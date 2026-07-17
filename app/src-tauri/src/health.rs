use serde::Serialize;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct IntegrationHealth {
    pub source: String,
    pub status: String,
    pub message: Option<String>,
    pub last_attempt_ms: Option<i64>,
    pub last_success_ms: Option<i64>,
}

#[derive(Clone, Default)]
pub struct HealthRegistry(Arc<Mutex<HashMap<String, IntegrationHealth>>>);

impl HealthRegistry {
    pub fn success(&self, source: &str) {
        let now = crate::events::now_ms();
        self.update(source, "healthy", None, now, Some(now));
    }

    pub fn failure(&self, source: &str, message: impl Into<String>) {
        let now = crate::events::now_ms();
        let last_success = self
            .0
            .lock()
            .ok()
            .and_then(|map| map.get(source).and_then(|h| h.last_success_ms));
        self.update(source, "error", Some(message.into()), now, last_success);
    }

    pub fn idle(&self, source: &str, message: impl Into<String>) {
        self.update(
            source,
            "not_configured",
            Some(message.into()),
            crate::events::now_ms(),
            None,
        );
    }

    fn update(
        &self,
        source: &str,
        status: &str,
        message: Option<String>,
        attempt: i64,
        success: Option<i64>,
    ) {
        if let Ok(mut map) = self.0.lock() {
            map.insert(
                source.into(),
                IntegrationHealth {
                    source: source.into(),
                    status: status.into(),
                    message,
                    last_attempt_ms: Some(attempt),
                    last_success_ms: success,
                },
            );
        }
    }

    pub fn snapshot(&self) -> Vec<IntegrationHealth> {
        let mut items: Vec<_> = self
            .0
            .lock()
            .map(|m| m.values().cloned().collect())
            .unwrap_or_default();
        items.sort_by(|a, b| a.source.cmp(&b.source));
        items
    }
}
