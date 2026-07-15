//! lightctl — feed events from scripts and terminals into Luminode.
//!
//! Usage:
//!   lightctl progress <0-100>          report progress (drives the LED bar)
//!   lightctl progress done             clear the progress bar
//!   lightctl event <type> [json]       emit an arbitrary event, e.g.
//!                                      lightctl event build_started '{"repo":"x"}'
//!   lightctl run -- <cmd> [args...]    run a command; emits run_started and
//!                                      run_succeeded/run_failed (with exit
//!                                      code and duration) around it
//!   lightctl claude                    Claude Code bridge: reads the JSON
//!                                      Claude Code pipes to hooks and
//!                                      statusline commands, emits claude/*
//!                                      events (see claude_bridge below)
//!
//! Transport: one JSON line per event over the app's unix socket
//! ({"type": ..., "payload": ...}), reply "ok". Socket path defaults to the
//! Luminode app-data dir and can be overridden with $LIGHTCTL_SOCK — keep in
//! sync with app/src-tauri/src/sources/lightctl.rs.
//!
//! `run` is deliberately forgiving: if the app isn't running, the wrapped
//! command still executes and its exit code is passed through — a dead
//! status light must never break a build script.

use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::{exit, Command};
use std::time::Instant;

fn socket_path() -> PathBuf {
    if let Ok(path) = std::env::var("LIGHTCTL_SOCK") {
        return PathBuf::from(path);
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    if cfg!(target_os = "macos") {
        PathBuf::from(home).join("Library/Application Support/com.luminode.app/lightctl.sock")
    } else {
        // Linux XDG data dir (Tauri's app_data_dir default).
        let base = std::env::var("XDG_DATA_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from(&home).join(".local/share"));
        base.join("com.luminode.app/lightctl.sock")
    }
}

/// Send one event line; returns false if the app isn't reachable.
fn send(event_type: &str, payload: serde_json::Value) -> bool {
    send_from("cli", event_type, payload)
}

fn send_from(source: &str, event_type: &str, payload: serde_json::Value) -> bool {
    let Ok(mut stream) = UnixStream::connect(socket_path()) else {
        return false;
    };
    let line = serde_json::json!({ "source": source, "type": event_type, "payload": payload });
    if writeln!(stream, "{line}").is_err() {
        return false;
    }
    let mut reply = String::new();
    let _ = BufReader::new(&stream).read_line(&mut reply);
    reply.trim() == "ok"
}

fn usage() -> ! {
    eprintln!(
        "usage:\n  lightctl progress <0-100|done>\n  lightctl event <type> [json-payload]\n  lightctl run -- <command> [args...]\n  lightctl claude    (stdin bridge for Claude Code hooks/statusline)"
    );
    exit(2);
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("progress") => {
            let value = args.get(1).unwrap_or_else(|| usage());
            let ok = if value == "done" {
                send("progress_done", serde_json::Value::Null)
            } else {
                let percent: f64 = value.parse().unwrap_or_else(|_| usage());
                send("progress", serde_json::json!({ "percent": percent }))
            };
            finish(ok);
        }
        Some("event") => {
            let event_type = args.get(1).unwrap_or_else(|| usage());
            let payload = match args.get(2) {
                Some(raw) => serde_json::from_str(raw).unwrap_or_else(|e| {
                    eprintln!("lightctl: invalid JSON payload: {e}");
                    exit(2);
                }),
                None => serde_json::Value::Null,
            };
            finish(send(event_type, payload));
        }
        Some("run") => {
            // Accept both `lightctl run -- cmd` and `lightctl run cmd`.
            let rest = if args.get(1).map(String::as_str) == Some("--") {
                &args[2..]
            } else {
                &args[1..]
            };
            if rest.is_empty() {
                usage();
            }
            run_wrapped(rest);
        }
        Some("claude") => claude_bridge(),
        _ => usage(),
    }
}

/// Bridge for Claude Code. Wire it up in ~/.claude/settings.json as both a
/// hook command and the statusline command — it tells the two apart by the
/// JSON on stdin:
///
/// * hook input (`hook_event_name` present):
///     UserPromptSubmit  → claude/active   (Claude started working)
///     Stop, SessionEnd  → claude/stopped  (clears "active", fires "finished")
/// * statusline input (everything else): emits claude/usage
///     {"session": <5h %>, "weekly": <7d %>} when the rate-limit numbers
///     changed since last tick, and prints a one-line statusline either way.
///
/// Always exits 0: a dead status light must never break Claude Code.
fn claude_bridge() -> ! {
    let mut input = String::new();
    if std::io::stdin().read_to_string(&mut input).is_err() {
        exit(0);
    }
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&input) else {
        exit(0);
    };

    if let Some(hook) = v.get("hook_event_name").and_then(|h| h.as_str()) {
        match hook {
            "UserPromptSubmit" => {
                send_from("claude", "active", serde_json::Value::Null);
            }
            "Stop" | "SessionEnd" => {
                send_from("claude", "stopped", serde_json::Value::Null);
            }
            _ => {}
        }
        exit(0);
    }

    // Statusline mode. Runs on every conversation update, so only emit an
    // event when the (rounded) percentages actually moved.
    let pct = |window: &str| -> Option<f64> {
        v.get("rate_limits")?
            .get(window)?
            .get("used_percentage")?
            .as_f64()
    };
    let session = pct("five_hour");
    let weekly = pct("seven_day");

    let mut line_parts: Vec<String> = Vec::new();
    if let Some(model) = v
        .pointer("/model/display_name")
        .and_then(|m| m.as_str())
    {
        line_parts.push(model.to_string());
    }
    if session.is_some() || weekly.is_some() {
        if let Some(p) = session {
            line_parts.push(format!("5h {:.0}%", p));
        }
        if let Some(p) = weekly {
            line_parts.push(format!("7d {:.0}%", p));
        }

        let fingerprint = format!(
            "{} {}",
            session.map(|p| p.round() as i64).unwrap_or(-1),
            weekly.map(|p| p.round() as i64).unwrap_or(-1)
        );
        let state_file = socket_path().with_file_name("claude-usage.last");
        if std::fs::read_to_string(&state_file).ok().as_deref() != Some(&fingerprint) {
            let mut payload = serde_json::Map::new();
            if let Some(p) = session {
                payload.insert("session".into(), p.into());
            }
            if let Some(p) = weekly {
                payload.insert("weekly".into(), p.into());
            }
            if send_from("claude", "usage", serde_json::Value::Object(payload)) {
                let _ = std::fs::write(&state_file, fingerprint);
            }
        }
    }

    println!("{}", line_parts.join(" · "));
    exit(0);
}

fn finish(sent: bool) -> ! {
    if sent {
        exit(0);
    }
    eprintln!("lightctl: could not reach Luminode (is the app running?)");
    exit(1);
}

fn run_wrapped(cmd: &[String]) -> ! {
    let name = cmd.join(" ");
    send("run_started", serde_json::json!({ "command": name }));
    let started = Instant::now();

    let status = Command::new(&cmd[0]).args(&cmd[1..]).status();
    let secs = started.elapsed().as_secs_f64();

    match status {
        Ok(status) => {
            let code = status.code().unwrap_or(-1);
            let payload = serde_json::json!({
                "command": name,
                "exit_code": code,
                "duration_secs": secs,
            });
            send(
                if status.success() { "run_succeeded" } else { "run_failed" },
                payload,
            );
            exit(code);
        }
        Err(e) => {
            eprintln!("lightctl: failed to launch {}: {e}", cmd[0]);
            send(
                "run_failed",
                serde_json::json!({ "command": name, "exit_code": -1, "duration_secs": secs }),
            );
            exit(127);
        }
    }
}
