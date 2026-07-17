//! Unix-socket listener for the `lightctl` CLI (cli/lightctl).
//!
//! Protocol: newline-delimited JSON, one event per line:
//!     {"type": "progress", "payload": {"percent": 42}}
//!     {"source": "claude", "type": "usage", "payload": {"session": 24}}
//! `source` is optional and defaults to "cli" (the `lightctl claude` bridge
//! sets it so Claude events aren't conflated with terminal ones). Replies
//! `ok\n`, or `err\n` for unparseable lines. The socket lives in the app
//! data dir; lightctl computes the same path, overridable via
//! $LIGHTCTL_SOCK on both sides.

use std::path::PathBuf;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixListener;
use tokio::sync::Semaphore;

use crate::events::{Bus, Event};

pub const SOCKET_NAME: &str = "lightctl.sock";
const MAX_LINE_BYTES: usize = 65_536;
const MAX_CONNECTIONS: usize = 32;
const IO_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

pub fn spawn(sock_path: PathBuf, bus: Bus) {
    tauri::async_runtime::spawn(async move {
        // A previous run's socket file would make bind fail.
        let _ = std::fs::remove_file(&sock_path);
        let listener = match UnixListener::bind(&sock_path) {
            Ok(l) => l,
            Err(e) => {
                eprintln!("lightctl: cannot bind {}: {e}", sock_path.display());
                return;
            }
        };
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if let Err(e) =
                std::fs::set_permissions(&sock_path, std::fs::Permissions::from_mode(0o600))
            {
                eprintln!("lightctl: cannot secure {}: {e}", sock_path.display());
                let _ = std::fs::remove_file(&sock_path);
                return;
            }
        }
        let permits = std::sync::Arc::new(Semaphore::new(MAX_CONNECTIONS));
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                continue;
            };
            let Ok(permit) = permits.clone().try_acquire_owned() else {
                continue;
            };
            let bus = bus.clone();
            tauri::async_runtime::spawn(async move {
                let _permit = permit;
                let (read_half, mut write_half) = stream.into_split();
                let mut reader = BufReader::new(read_half);
                loop {
                    let read =
                        tokio::time::timeout(IO_TIMEOUT, read_bounded_line(&mut reader)).await;
                    let Ok(Ok(Some(line))) = read else { break };
                    let reply = match line.as_deref() {
                        Some(line) => match parse_line(line) {
                            Some(ev) => {
                                let _ = bus.send(ev);
                                "ok\n"
                            }
                            None => "err\n",
                        },
                        None => "err\n",
                    };
                    if tokio::time::timeout(IO_TIMEOUT, write_half.write_all(reply.as_bytes()))
                        .await
                        .is_err()
                    {
                        break;
                    }
                    if line.is_none() {
                        break;
                    }
                }
            });
        }
    });
}

/// Read one newline-delimited UTF-8 message without allocating beyond the
/// protocol limit. `Some(None)` is a rejected line; `None` is clean EOF.
async fn read_bounded_line<R: AsyncBufRead + Unpin>(
    reader: &mut R,
) -> std::io::Result<Option<Option<String>>> {
    let mut out = Vec::new();
    loop {
        let buf = reader.fill_buf().await?;
        if buf.is_empty() {
            return Ok(if out.is_empty() { None } else { Some(None) });
        }
        let newline = buf.iter().position(|b| *b == b'\n');
        let take = newline.map_or(buf.len(), |i| i + 1);
        if out.len() + take > MAX_LINE_BYTES {
            reader.consume(take);
            return Ok(Some(None));
        }
        out.extend_from_slice(&buf[..take]);
        reader.consume(take);
        if newline.is_some() {
            out.pop();
            if out.last() == Some(&b'\r') {
                out.pop();
            }
            return Ok(Some(String::from_utf8(out).ok()));
        }
    }
}

fn parse_line(line: &str) -> Option<Event> {
    if line.len() > MAX_LINE_BYTES {
        return None;
    }
    let v: serde_json::Value = serde_json::from_str(line).ok()?;
    let event_type = v.get("type")?.as_str()?.to_string();
    let source = v.get("source").and_then(|s| s.as_str()).unwrap_or("cli");
    if source != "cli" && source != "claude" && source != "codex" {
        return None;
    }
    if event_type.is_empty() || event_type.len() > 64 || event_type.chars().any(char::is_control) {
        return None;
    }
    let payload = v.get("payload").cloned().unwrap_or(serde_json::Value::Null);
    Some(Event::new(source, &event_type, payload))
}
