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
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixListener;

use crate::events::{Bus, Event};

pub const SOCKET_NAME: &str = "lightctl.sock";

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
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                continue;
            };
            let bus = bus.clone();
            tauri::async_runtime::spawn(async move {
                let (read_half, mut write_half) = stream.into_split();
                let mut lines = BufReader::new(read_half).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    let reply = match parse_line(&line) {
                        Some(ev) => {
                            let _ = bus.send(ev);
                            "ok\n"
                        }
                        None => "err\n",
                    };
                    if write_half.write_all(reply.as_bytes()).await.is_err() {
                        break;
                    }
                }
            });
        }
    });
}

fn parse_line(line: &str) -> Option<Event> {
    let v: serde_json::Value = serde_json::from_str(line).ok()?;
    let event_type = v.get("type")?.as_str()?.to_string();
    let source = v.get("source").and_then(|s| s.as_str()).unwrap_or("cli");
    let payload = v.get("payload").cloned().unwrap_or(serde_json::Value::Null);
    Some(Event::new(source, &event_type, payload))
}
