//! The in-process event bus.
//!
//! Every integration (CLI socket, screen lock, future OAuth sources) emits a
//! uniform [`Event`] onto one tokio broadcast channel. Subscribers: the rule
//! engine (decides what the lights do), the event logger (SQLite), and the
//! UI (live event feed). Integrations never talk to the animation engine
//! directly — priority decisions belong to the rule engine alone.

use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Event {
    /// Which integration produced this, e.g. "cli", "system", "device".
    pub source: String,
    /// What happened, e.g. "progress", "run_failed", "screen_locked".
    #[serde(rename = "type")]
    pub event_type: String,
    /// Source-specific data, e.g. {"percent": 42}.
    #[serde(default)]
    pub payload: serde_json::Value,
    /// Milliseconds since the unix epoch.
    #[serde(default = "now_ms")]
    pub ts: i64,
}

impl Event {
    pub fn new(source: &str, event_type: &str, payload: serde_json::Value) -> Self {
        Self {
            source: source.to_string(),
            event_type: event_type.to_string(),
            payload,
            ts: now_ms(),
        }
    }
}

pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

pub type Bus = broadcast::Sender<Event>;

pub fn new_bus() -> Bus {
    // 256 of backlog: a slow subscriber lags (and is told so) rather than
    // blocking publishers.
    broadcast::channel(256).0
}
