//! Agent hook wiring — the "is this actually plugged in?" half of the
//! Claude Code and Codex integrations.
//!
//! Both bridges are just `lightctl` invocations the agent runs on lifecycle
//! events (see cli/lightctl and the README's Integrations section), which
//! means setup lives in *the agent's* config file, not ours:
//!
//!   Claude Code  ~/.claude/settings.json   hooks.{UserPromptSubmit,Stop,
//!                                          SessionEnd} + statusLine
//!   Codex        ~/.codex/config.toml      [[hooks.UserPromptSubmit]],
//!                                          [[hooks.Stop]]
//!
//! This module reads those files to report what is wired, and writes the
//! missing entries on request. Two rules shape the whole thing: we only ever
//! touch entries that are ours (a `lightctl <agent>` command), and we back up
//! before writing, because these are the user's files, not the app's.

use serde::Serialize;
use std::path::{Path, PathBuf};

/// One hook the integration wants, and what the config currently says.
#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct HookItem {
    /// Config key: "UserPromptSubmit", "Stop", "SessionEnd", "statusLine".
    pub key: String,
    /// What it buys the user, in UI words.
    pub label: String,
    /// "ours" (a lightctl command we recognise), "missing", or "foreign"
    /// (something else already owns this key — we leave it alone).
    pub state: String,
    /// Whether the integration works without it.
    pub optional: bool,
    /// The command found in the config, if any.
    pub command: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HookStatus {
    /// "claude" | "codex" — also the event-bus source these hooks feed.
    pub agent: String,
    pub name: String,
    pub config_path: String,
    pub config_exists: bool,
    /// "installed" | "partial" | "missing" | "stale" | "error".
    pub status: String,
    pub items: Vec<HookItem>,
    /// The lightctl we would install (absent = nothing to install with).
    pub binary_path: Option<String>,
    /// The lightctl the config currently points at, if any.
    pub installed_binary: Option<String>,
    /// True when `installed_binary` is set but no longer on disk.
    pub binary_missing: bool,
    /// Last event actually received from this agent — the end-to-end proof
    /// that the wiring works, which no amount of config reading can give.
    pub last_event_ms: Option<i64>,
    pub message: Option<String>,
}

const CLAUDE_KEYS: &[(&str, &str, bool)] = &[
    ("UserPromptSubmit", "Turn starts → claude/active", false),
    ("Stop", "Turn ends → claude/stopped", false),
    ("SessionEnd", "Session ends → claude/stopped", false),
    ("statusLine", "Usage bars → claude/usage", true),
];

const CODEX_KEYS: &[(&str, &str, bool)] = &[
    ("UserPromptSubmit", "Turn starts → codex/active", false),
    ("Stop", "Turn ends → codex/stopped", false),
];

fn home() -> PathBuf {
    std::env::var("HOME").map(PathBuf::from).unwrap_or_default()
}

pub fn claude_config_path() -> PathBuf {
    home().join(".claude/settings.json")
}

pub fn codex_config_path() -> PathBuf {
    home().join(".codex/config.toml")
}

// ---------------------------------------------------------------- lightctl

fn is_executable(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path)
            .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
    }
    #[cfg(not(unix))]
    {
        path.is_file()
    }
}

/// Locate the `lightctl` binary to point hooks at. The hooks store an
/// absolute path (the agent runs them with an unknown PATH), so this has to
/// resolve to a real file — order is "shipped beside this app", then the
/// cargo dev outputs, then the usual install dirs and $PATH.
pub fn find_lightctl() -> Option<PathBuf> {
    let mut candidates: Vec<PathBuf> = Vec::new();

    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            candidates.push(dir.join("lightctl"));
            // `npm run tauri dev` runs target/debug/luminode; a release
            // lightctl next door is the one the README tells you to build.
            if let Some(target) = dir.parent() {
                candidates.push(target.join("release/lightctl"));
                candidates.push(target.join("debug/lightctl"));
            }
        }
    }

    let home = home();
    candidates.push(home.join(".cargo/bin/lightctl"));
    candidates.push(home.join("bin/lightctl"));
    candidates.push(PathBuf::from("/opt/homebrew/bin/lightctl"));
    candidates.push(PathBuf::from("/usr/local/bin/lightctl"));

    if let Ok(path) = std::env::var("PATH") {
        candidates.extend(std::env::split_paths(&path).map(|dir| dir.join("lightctl")));
    }

    candidates
        .into_iter()
        .find(|p| is_executable(p))
        .and_then(|p| std::fs::canonicalize(&p).ok().or(Some(p)))
}

/// Does this command line look like our bridge for `agent`?
fn is_ours(command: &str, agent: &str) -> bool {
    let trimmed = command.trim();
    trimmed.contains("lightctl") && trimmed.split_whitespace().last() == Some(agent)
}

/// The binary half of a `… /path/to/lightctl claude` command line.
fn binary_of(command: &str) -> Option<String> {
    let (head, _agent) = command.trim().rsplit_once(char::is_whitespace)?;
    let head = head.trim().trim_matches(['"', '\'']);
    (!head.is_empty()).then(|| head.to_string())
}

/// Should an entry we already own be repointed at the binary we just found?
/// Only when what it names is gone: a hook the user built somewhere else
/// keeps working, and a config that is already fine is left untouched.
fn needs_repair(command: &str) -> bool {
    binary_of(command).is_none_or(|b| !is_executable(Path::new(&b)))
}

fn hook_command(binary: &Path, agent: &str) -> String {
    format!("{} {agent}", binary.display())
}

// ------------------------------------------------------------------ saving

/// Write `contents` over `path`, keeping a `.luminode.bak` copy of what was
/// there and the original's permissions (config.toml is often 0600 — a
/// rewrite must not widen it).
fn write_config(path: &Path, contents: &str) -> Result<(), String> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    let perms = std::fs::metadata(path).ok().map(|m| m.permissions());
    if path.exists() {
        let backup = PathBuf::from(format!("{}.luminode.bak", path.display()));
        std::fs::copy(path, &backup).map_err(|e| format!("backup failed: {e}"))?;
    }
    let tmp = PathBuf::from(format!("{}.luminode.tmp", path.display()));
    std::fs::write(&tmp, contents).map_err(|e| format!("{}: {e}", tmp.display()))?;
    if let Some(perms) = perms {
        let _ = std::fs::set_permissions(&tmp, perms);
    }
    std::fs::rename(&tmp, path).map_err(|e| format!("{}: {e}", path.display()))
}

// ------------------------------------------------------------ claude code

/// Every command string Claude Code would run for `key`, in config order.
fn claude_commands(root: &serde_json::Value, key: &str) -> Vec<String> {
    if key == "statusLine" {
        return root
            .get("statusLine")
            .and_then(|s| s.get("command"))
            .and_then(|c| c.as_str())
            .map(|c| vec![c.to_string()])
            .unwrap_or_default();
    }
    root.get("hooks")
        .and_then(|h| h.get(key))
        .and_then(|v| v.as_array())
        .map(|groups| {
            groups
                .iter()
                .filter_map(|g| g.get("hooks").and_then(|h| h.as_array()))
                .flatten()
                .filter_map(|h| h.get("command").and_then(|c| c.as_str()))
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

fn claude_status_at(path: &Path, binary: Option<&Path>) -> HookStatus {
    let mut status = blank_status("claude", "Claude Code", path, binary);
    let raw = match std::fs::read_to_string(path) {
        Ok(text) => {
            status.config_exists = true;
            text
        }
        Err(_) => {
            status.items = missing_items(CLAUDE_KEYS);
            status.status = "missing".into();
            return status;
        }
    };
    let root: serde_json::Value = match serde_json::from_str(&raw) {
        Ok(v) => v,
        Err(e) => {
            status.status = "error".into();
            status.message = Some(format!("{} is not valid JSON: {e}", path.display()));
            status.items = missing_items(CLAUDE_KEYS);
            return status;
        }
    };
    status.items = CLAUDE_KEYS
        .iter()
        .map(|&(key, label, optional)| {
            let commands = claude_commands(&root, key);
            let ours = commands.iter().find(|c| is_ours(c, "claude"));
            HookItem {
                key: key.into(),
                label: label.into(),
                state: match (ours, commands.first()) {
                    (Some(_), _) => "ours",
                    // Only statusLine can be claimed by someone else; a
                    // foreign entry under hooks.* is simply another hook.
                    (None, Some(_)) if key == "statusLine" => "foreign",
                    _ => "missing",
                }
                .into(),
                optional,
                command: ours.cloned().or_else(|| commands.first().cloned()),
            }
        })
        .collect();
    finish(status)
}

fn install_claude_at(path: &Path, binary: &Path) -> Result<(), String> {
    let command = hook_command(binary, "claude");
    let mut root: serde_json::Value = match std::fs::read_to_string(path) {
        Ok(text) if !text.trim().is_empty() => serde_json::from_str(&text)
            .map_err(|e| format!("{} is not valid JSON: {e}", path.display()))?,
        _ => serde_json::json!({}),
    };
    if !root.is_object() {
        return Err(format!("{} is not a JSON object", path.display()));
    }

    let hooks = root
        .as_object_mut()
        .unwrap()
        .entry("hooks")
        .or_insert_with(|| serde_json::json!({}));
    if !hooks.is_object() {
        return Err("\"hooks\" in settings.json is not an object".into());
    }
    for &(key, _, _) in CLAUDE_KEYS.iter().filter(|(k, _, _)| *k != "statusLine") {
        let entry = hooks
            .as_object_mut()
            .unwrap()
            .entry(key)
            .or_insert_with(|| serde_json::json!([]));
        let Some(groups) = entry.as_array_mut() else {
            return Err(format!("\"hooks.{key}\" in settings.json is not an array"));
        };
        // Repair in place if we're already there, so a moved binary doesn't
        // accumulate duplicate hook entries.
        let existing = groups
            .iter_mut()
            .filter_map(|g| g.get_mut("hooks").and_then(|h| h.as_array_mut()))
            .flatten()
            .find(|h| {
                h.get("command")
                    .and_then(|c| c.as_str())
                    .is_some_and(|c| is_ours(c, "claude"))
            });
        match existing {
            Some(hook) => {
                let current = hook["command"].as_str().unwrap_or_default().to_string();
                if needs_repair(&current) {
                    hook["command"] = serde_json::json!(command);
                }
            }
            None => groups.push(serde_json::json!({
                "hooks": [{ "type": "command", "command": command }]
            })),
        }
    }

    // The status line is someone's personal real estate: claim it only when
    // it's free or already ours.
    let statusline_free = root
        .get("statusLine")
        .and_then(|s| s.get("command"))
        .and_then(|c| c.as_str())
        .is_none_or(|c| is_ours(c, "claude"));
    if statusline_free {
        root["statusLine"] = serde_json::json!({ "type": "command", "command": command });
    }

    let json = serde_json::to_string_pretty(&root).map_err(|e| e.to_string())?;
    write_config(path, &format!("{json}\n"))
}

// ------------------------------------------------------------------ codex

fn codex_commands(doc: &toml_edit::DocumentMut, key: &str) -> Vec<String> {
    let Some(entries) = doc
        .get("hooks")
        .and_then(|h| h.get(key))
        .and_then(|v| v.as_array_of_tables())
    else {
        return Vec::new();
    };
    entries
        .iter()
        .filter_map(|t| t.get("hooks").and_then(|h| h.as_array_of_tables()))
        .flatten()
        .filter_map(|h| h.get("command").and_then(|c| c.as_str()))
        .map(str::to_string)
        .collect()
}

fn codex_status_at(path: &Path, binary: Option<&Path>) -> HookStatus {
    let mut status = blank_status("codex", "Codex", path, binary);
    let raw = match std::fs::read_to_string(path) {
        Ok(text) => {
            status.config_exists = true;
            text
        }
        Err(_) => {
            status.items = missing_items(CODEX_KEYS);
            status.status = "missing".into();
            return status;
        }
    };
    let doc: toml_edit::DocumentMut = match raw.parse() {
        Ok(d) => d,
        Err(e) => {
            status.status = "error".into();
            status.message = Some(format!("{} is not valid TOML: {e}", path.display()));
            status.items = missing_items(CODEX_KEYS);
            return status;
        }
    };
    status.items = CODEX_KEYS
        .iter()
        .map(|&(key, label, optional)| {
            let commands = codex_commands(&doc, key);
            let ours = commands.iter().find(|c| is_ours(c, "codex"));
            HookItem {
                key: key.into(),
                label: label.into(),
                state: if ours.is_some() { "ours" } else { "missing" }.into(),
                optional,
                command: ours.cloned(),
            }
        })
        .collect();
    finish(status)
}

fn install_codex_at(path: &Path, binary: &Path) -> Result<(), String> {
    let command = hook_command(binary, "codex");
    let raw = std::fs::read_to_string(path).unwrap_or_default();
    let mut doc: toml_edit::DocumentMut = raw
        .parse()
        .map_err(|e| format!("{} is not valid TOML: {e}", path.display()))?;

    if doc.get("hooks").is_none() {
        let mut table = toml_edit::Table::new();
        table.set_implicit(true);
        doc.insert("hooks", toml_edit::Item::Table(table));
    }
    let hooks = doc
        .get_mut("hooks")
        .and_then(|h| h.as_table_mut())
        .ok_or("\"hooks\" in config.toml is not a table")?;

    for &(key, _, _) in CODEX_KEYS {
        if hooks.get(key).is_none() {
            hooks.insert(key, toml_edit::Item::ArrayOfTables(Default::default()));
        }
        let entries = hooks
            .get_mut(key)
            .and_then(|v| v.as_array_of_tables_mut())
            .ok_or_else(|| format!("\"hooks.{key}\" in config.toml is not a table array"))?;

        let existing = entries
            .iter_mut()
            .filter_map(|t| t.get_mut("hooks").and_then(|h| h.as_array_of_tables_mut()))
            .flat_map(|inner| inner.iter_mut())
            .find(|h| {
                h.get("command")
                    .and_then(|c| c.as_str())
                    .is_some_and(|c| is_ours(c, "codex"))
            });
        match existing {
            Some(hook) => {
                let current = hook["command"].as_str().unwrap_or_default().to_string();
                if needs_repair(&current) {
                    hook["command"] = toml_edit::value(command.clone());
                }
            }
            None => {
                let mut hook = toml_edit::Table::new();
                hook["type"] = toml_edit::value("command");
                hook["command"] = toml_edit::value(command.clone());
                hook["timeout"] = toml_edit::value(5);
                let mut inner = toml_edit::ArrayOfTables::new();
                inner.push(hook);
                let mut entry = toml_edit::Table::new();
                entry.insert("hooks", toml_edit::Item::ArrayOfTables(inner));
                entries.push(entry);
            }
        }
    }

    write_config(path, &doc.to_string())
}

// ------------------------------------------------------------------ shared

fn blank_status(agent: &str, name: &str, path: &Path, binary: Option<&Path>) -> HookStatus {
    HookStatus {
        agent: agent.into(),
        name: name.into(),
        config_path: path.display().to_string(),
        config_exists: false,
        status: "missing".into(),
        items: Vec::new(),
        binary_path: binary.map(|b| b.display().to_string()),
        installed_binary: None,
        binary_missing: false,
        last_event_ms: None,
        message: None,
    }
}

fn missing_items(keys: &[(&str, &str, bool)]) -> Vec<HookItem> {
    keys.iter()
        .map(|&(key, label, optional)| HookItem {
            key: key.into(),
            label: label.into(),
            state: "missing".into(),
            optional,
            command: None,
        })
        .collect()
}

/// Roll the per-key states up into one verdict, and check that whatever the
/// config points at still exists — a hook naming a deleted binary looks
/// installed and does nothing, which is the failure mode worth catching.
fn finish(mut status: HookStatus) -> HookStatus {
    let required: Vec<&HookItem> = status.items.iter().filter(|i| !i.optional).collect();
    let present = required.iter().filter(|i| i.state == "ours").count();

    status.installed_binary = status
        .items
        .iter()
        .filter(|i| i.state == "ours")
        .find_map(|i| i.command.as_deref().and_then(binary_of));
    status.binary_missing = status
        .installed_binary
        .as_deref()
        .is_some_and(|b| !is_executable(Path::new(b)));

    status.status = if present == 0 {
        "missing"
    } else if present < required.len() {
        "partial"
    } else if status.binary_missing {
        "stale"
    } else {
        "installed"
    }
    .into();

    if status.binary_missing {
        let installed = status.installed_binary.clone().unwrap_or_default();
        status.message = Some(match &status.binary_path {
            Some(found) => format!("Hooks point at {installed}, which no longer exists. Repair to use {found}."),
            None => format!("Hooks point at {installed}, which no longer exists. Build it with `cargo build --release -p lightctl`."),
        });
    } else if status.binary_path.is_none() && status.status != "installed" {
        status.message =
            Some("lightctl not found — build it with `cargo build --release -p lightctl`.".into());
    }
    status
}

/// Status of both agents. `last_event` supplies the "have we ever heard from
/// it" timestamp per source.
pub fn status_all(last_event: impl Fn(&str) -> Option<i64>) -> Vec<HookStatus> {
    let binary = find_lightctl();
    let binary = binary.as_deref();
    let mut all = vec![
        claude_status_at(&claude_config_path(), binary),
        codex_status_at(&codex_config_path(), binary),
    ];
    for status in &mut all {
        status.last_event_ms = last_event(&status.agent);
    }
    all
}

/// Write the missing hooks for one agent.
pub fn install(agent: &str) -> Result<(), String> {
    let binary = find_lightctl().ok_or(
        "lightctl not found. Build it with `cargo build --release -p lightctl`, then try again.",
    )?;
    match agent {
        "claude" => install_claude_at(&claude_config_path(), &binary),
        "codex" => install_codex_at(&codex_config_path(), &binary),
        other => Err(format!("unknown agent \"{other}\"")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A path that exists and is executable, for binary_missing checks.
    fn fake_binary(dir: &Path) -> PathBuf {
        let path = dir.join("lightctl");
        std::fs::write(&path, "#!/bin/sh\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        path
    }

    #[test]
    fn recognises_our_commands_only() {
        assert!(is_ours("/opt/lightctl claude", "claude"));
        assert!(!is_ours("/opt/lightctl claude", "codex"));
        assert!(!is_ours("/opt/other-tool claude", "claude"));
        // A statusline of someone else's that merely mentions the word.
        assert!(!is_ours("/opt/lightctl claude --json | jq", "claude"));
        assert_eq!(
            binary_of("/opt/bin/lightctl claude").as_deref(),
            Some("/opt/bin/lightctl")
        );
    }

    #[test]
    fn missing_claude_config_reports_missing() {
        let dir = tempfile::tempdir().unwrap();
        let status = claude_status_at(&dir.path().join("settings.json"), None);
        assert_eq!(status.status, "missing");
        assert!(!status.config_exists);
        assert_eq!(status.items.len(), CLAUDE_KEYS.len());
        assert!(status.items.iter().all(|i| i.state == "missing"));
    }

    #[test]
    fn claude_install_is_idempotent_and_keeps_foreign_settings() {
        let dir = tempfile::tempdir().unwrap();
        let binary = fake_binary(dir.path());
        let config = dir.path().join("settings.json");
        std::fs::write(
            &config,
            r#"{
              "model": "opus",
              "hooks": { "Stop": [{ "hooks": [{ "type": "command", "command": "/bin/echo hi" }] }] }
            }"#,
        )
        .unwrap();

        install_claude_at(&config, &binary).unwrap();
        let first = std::fs::read_to_string(&config).unwrap();
        install_claude_at(&config, &binary).unwrap();
        assert_eq!(first, std::fs::read_to_string(&config).unwrap());

        let root: serde_json::Value = serde_json::from_str(&first).unwrap();
        assert_eq!(root["model"], "opus");
        // The pre-existing Stop hook survives alongside ours.
        assert_eq!(claude_commands(&root, "Stop").len(), 2);
        assert!(first.contains("\"model\""));

        let status = claude_status_at(&config, Some(&binary));
        assert_eq!(status.status, "installed");
        assert!(status.items.iter().all(|i| i.state == "ours"));
        assert_eq!(status.installed_binary.as_deref(), binary.to_str());
        assert!(std::fs::read_to_string(format!("{}.luminode.bak", config.display())).is_ok());
    }

    #[test]
    fn claude_install_leaves_a_foreign_statusline_alone() {
        let dir = tempfile::tempdir().unwrap();
        let binary = fake_binary(dir.path());
        let config = dir.path().join("settings.json");
        std::fs::write(
            &config,
            r#"{ "statusLine": { "type": "command", "command": "/opt/mine.sh" } }"#,
        )
        .unwrap();

        install_claude_at(&config, &binary).unwrap();
        let status = claude_status_at(&config, Some(&binary));
        let statusline = status.items.iter().find(|i| i.key == "statusLine").unwrap();
        assert_eq!(statusline.state, "foreign");
        assert_eq!(statusline.command.as_deref(), Some("/opt/mine.sh"));
        // …and the required hooks still went in, so the verdict is "installed".
        assert_eq!(status.status, "installed");
    }

    #[test]
    fn claude_install_repairs_a_moved_binary_without_duplicating() {
        let dir = tempfile::tempdir().unwrap();
        let binary = fake_binary(dir.path());
        let config = dir.path().join("settings.json");
        std::fs::write(
            &config,
            r#"{ "hooks": { "UserPromptSubmit": [{ "hooks": [
                 { "type": "command", "command": "/gone/lightctl claude" }] }] } }"#,
        )
        .unwrap();

        let stale = claude_status_at(&config, Some(&binary));
        assert_eq!(stale.status, "partial");
        assert!(stale.binary_missing);

        install_claude_at(&config, &binary).unwrap();
        let root: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&config).unwrap()).unwrap();
        assert_eq!(claude_commands(&root, "UserPromptSubmit").len(), 1);
        assert_eq!(claude_status_at(&config, Some(&binary)).status, "installed");
    }

    #[test]
    fn claude_install_leaves_a_working_custom_path_alone() {
        let dir = tempfile::tempdir().unwrap();
        let binary = fake_binary(dir.path());
        let elsewhere = dir.path().join("mine");
        std::fs::create_dir(&elsewhere).unwrap();
        let custom = fake_binary(&elsewhere);
        let config = dir.path().join("settings.json");
        std::fs::write(
            &config,
            format!(
                r#"{{ "hooks": {{ "Stop": [{{ "hooks": [
                     {{ "type": "command", "command": "{} claude" }}] }}] }} }}"#,
                custom.display()
            ),
        )
        .unwrap();

        install_claude_at(&config, &binary).unwrap();
        let root: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&config).unwrap()).unwrap();
        assert_eq!(
            claude_commands(&root, "Stop"),
            vec![format!("{} claude", custom.display())]
        );
        // …while the keys that were absent got the binary we resolved.
        assert_eq!(
            claude_commands(&root, "SessionEnd"),
            vec![format!("{} claude", binary.display())]
        );
    }

    #[test]
    fn claude_status_is_stale_when_the_binary_is_gone() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("settings.json");
        let hook = r#"{ "hooks": [{ "type": "command", "command": "/gone/lightctl claude" }] }"#;
        std::fs::write(
            &config,
            format!(
                r#"{{ "hooks": {{ "UserPromptSubmit": [{hook}], "Stop": [{hook}], "SessionEnd": [{hook}] }} }}"#
            ),
        )
        .unwrap();
        let status = claude_status_at(&config, None);
        assert_eq!(status.status, "stale");
        assert!(status.message.unwrap().contains("no longer exists"));
    }

    #[test]
    fn invalid_claude_config_is_an_error_not_a_rewrite() {
        let dir = tempfile::tempdir().unwrap();
        let binary = fake_binary(dir.path());
        let config = dir.path().join("settings.json");
        std::fs::write(&config, "{ not json").unwrap();
        assert_eq!(claude_status_at(&config, None).status, "error");
        assert!(install_claude_at(&config, &binary).is_err());
        assert_eq!(std::fs::read_to_string(&config).unwrap(), "{ not json");
    }

    #[test]
    fn codex_install_is_idempotent_and_preserves_the_rest_of_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let binary = fake_binary(dir.path());
        let config = dir.path().join("config.toml");
        std::fs::write(
            &config,
            "# my codex config\nmodel = \"gpt-5\"\n\n[hooks.state]\ntrusted = \"x\"\n",
        )
        .unwrap();

        install_codex_at(&config, &binary).unwrap();
        let first = std::fs::read_to_string(&config).unwrap();
        install_codex_at(&config, &binary).unwrap();
        assert_eq!(first, std::fs::read_to_string(&config).unwrap());

        assert!(first.starts_with("# my codex config\nmodel = \"gpt-5\"\n"));
        assert!(first.contains("trusted = \"x\""));
        assert!(first.contains("[[hooks.UserPromptSubmit.hooks]]"));
        assert!(first.contains("[[hooks.Stop.hooks]]"));

        let status = codex_status_at(&config, Some(&binary));
        assert_eq!(status.status, "installed");
        assert_eq!(status.installed_binary.as_deref(), binary.to_str());
    }

    #[test]
    fn codex_install_creates_a_missing_config() {
        let dir = tempfile::tempdir().unwrap();
        let binary = fake_binary(dir.path());
        let config = dir.path().join("nested/config.toml");
        install_codex_at(&config, &binary).unwrap();
        assert_eq!(codex_status_at(&config, Some(&binary)).status, "installed");
    }

    #[test]
    fn codex_install_keeps_the_original_file_mode() {
        let dir = tempfile::tempdir().unwrap();
        let binary = fake_binary(dir.path());
        let config = dir.path().join("config.toml");
        std::fs::write(&config, "model = \"gpt-5\"\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&config, std::fs::Permissions::from_mode(0o600)).unwrap();
            install_codex_at(&config, &binary).unwrap();
            let mode = std::fs::metadata(&config).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }

    #[test]
    fn codex_partial_install_is_reported() {
        let dir = tempfile::tempdir().unwrap();
        let binary = fake_binary(dir.path());
        let config = dir.path().join("config.toml");
        std::fs::write(
            &config,
            format!(
                "[[hooks.Stop]]\n\n[[hooks.Stop.hooks]]\ntype = \"command\"\ncommand = \"{} codex\"\n",
                binary.display()
            ),
        )
        .unwrap();
        assert_eq!(codex_status_at(&config, Some(&binary)).status, "partial");
    }
}
