//! OS-keychain storage for integration secrets — the secrets policy from
//! the architecture plan: tokens never land in SQLite or JSON. Known names:
//!
//!   slack_token       xoxp user token (users.profile:read, users:read)
//!   calendar_ics_url  secret iCal address (Google/Outlook "publish" URL)
//!
//! Set from the UI's Integrations panel via the `set_secret` command.

use keyring::Entry;

const SERVICE: &str = "com.luminode.app";
const ALLOWED_NAMES: &[&str] = &["slack_token", "calendar_ics_url"];

fn allowed(name: &str) -> bool {
    ALLOWED_NAMES.contains(&name)
}

pub fn get(name: &str) -> Option<String> {
    if !allowed(name) {
        return None;
    }
    Entry::new(SERVICE, name)
        .ok()?
        .get_password()
        .ok()
        .filter(|s| !s.is_empty())
}

/// Empty value deletes the entry.
pub fn set(name: &str, value: &str) -> Result<(), String> {
    if !allowed(name) {
        return Err("unknown secret name".into());
    }
    let entry = Entry::new(SERVICE, name).map_err(|e| e.to_string())?;
    if value.is_empty() {
        let _ = entry.delete_credential();
        Ok(())
    } else {
        entry.set_password(value).map_err(|e| e.to_string())
    }
}
