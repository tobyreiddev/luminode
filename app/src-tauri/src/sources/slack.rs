//! Slack status/presence source. Polls every 30s with the user token from
//! the keychain (`slack_token`, set in the UI's Integrations panel); does
//! nothing until one exists, so the loop is always safe to spawn. Polling
//! (not the Events API) because Events needs a public webhook endpoint —
//! the wrong trade for a local desktop app.
//!
//! Emitted on transitions only (and once at startup to reflect reality):
//!   slack/status_set      {"text": "...", "emoji": ":calendar:"}
//!   slack/status_cleared
//!   slack/presence_active
//!   slack/presence_away
//!
//! Manual setup (token scopes etc.): README "Integrations".

use std::time::Duration;

use crate::events::{Bus, Event};
use crate::secrets;

pub fn spawn(bus: Bus, health: crate::health::HealthRegistry) {
    tauri::async_runtime::spawn(async move {
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(15))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .expect("Slack HTTP client configuration is valid");
        // None = never observed; used to emit initial state exactly once.
        let mut last_status: Option<(String, String)> = None;
        let mut last_presence: Option<String> = None;
        let mut interval = tokio::time::interval(Duration::from_secs(30));
        loop {
            interval.tick().await;
            let Some(token) = secrets::get("slack_token") else {
                health.idle("slack", "Add a Slack token to connect");
                continue;
            };

            let profile_result = api(&client, &token, "users.profile.get").await;
            if let Some(v) = profile_result {
                health.success("slack");
                let profile = v.get("profile").cloned().unwrap_or_default();
                let text = str_field(&profile, "status_text");
                let emoji = str_field(&profile, "status_emoji");
                let current = (text.clone(), emoji.clone());
                if last_status.as_ref() != Some(&current) {
                    if !text.is_empty() || !emoji.is_empty() {
                        let _ = bus.send(Event::new(
                            "slack",
                            "status_set",
                            serde_json::json!({ "text": text, "emoji": emoji }),
                        ));
                    } else if last_status.is_some() {
                        let _ = bus.send(Event::new(
                            "slack",
                            "status_cleared",
                            serde_json::Value::Null,
                        ));
                    }
                    last_status = Some(current);
                }
            } else {
                health.failure("slack", "Slack API request failed");
            }

            if let Some(v) = api(&client, &token, "users.getPresence").await {
                let presence = str_field(&v, "presence"); // "active" | "away"
                if !presence.is_empty() && last_presence.as_deref() != Some(&presence) {
                    let _ = bus.send(Event::new(
                        "slack",
                        if presence == "active" {
                            "presence_active"
                        } else {
                            "presence_away"
                        },
                        serde_json::Value::Null,
                    ));
                    last_presence = Some(presence);
                }
            }
        }
    });
}

fn str_field(v: &serde_json::Value, key: &str) -> String {
    v.get(key)
        .and_then(|s| s.as_str())
        .unwrap_or("")
        .to_string()
}

/// Call a Slack Web API method; None on transport errors or `"ok": false`
/// (bad token, missing scope) — the loop just tries again next tick.
async fn api(client: &reqwest::Client, token: &str, method: &str) -> Option<serde_json::Value> {
    let response = client
        .get(format!("https://slack.com/api/{method}"))
        .bearer_auth(token)
        .send()
        .await
        .ok()?
        .error_for_status()
        .ok()?;
    if response.content_length().is_some_and(|n| n > 1_048_576) {
        return None;
    }
    let bytes = response.bytes().await.ok()?;
    if bytes.len() > 1_048_576 {
        return None;
    }
    let v: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    v.get("ok")?.as_bool()?.then_some(v)
}
