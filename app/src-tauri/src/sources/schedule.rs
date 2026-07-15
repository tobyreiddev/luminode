//! Clock source: user-defined schedules (store's `schedules` table, edited
//! in the UI), checked whenever the local wall-clock minute changes.
//!
//! Two actions:
//! * `emit`  — put `time/<event_type>` on the bus. Pair with a trigger to
//!   show an animation at a time of day ("on time/evening show Night
//!   Breathe until time/morning").
//! * `idle`  — swap the idle animation ("at 18:00, idle = Night Breathe").
//!   This is the one source that touches light config directly: swapping a
//!   *setting* isn't an overlay, so routing it through a trigger would be
//!   a lie. It still announces itself on the bus (`time/idle_changed`) so
//!   the event log tells the story.
//!
//! Schedules are daily, local time. Minutes that pass while the machine
//! sleeps are skipped, not replayed — a light change from three hours ago
//! is not worth firing late.

use std::sync::Arc;
use std::time::Duration;

use crate::events::{Bus, Event};
use crate::store::Store;
use crate::triggers::TriggerEngine;

pub fn spawn(store: Store, bus: Bus, engine: Arc<TriggerEngine>) {
    tauri::async_runtime::spawn(async move {
        let mut last_minute = current_minute();
        let mut interval = tokio::time::interval(Duration::from_secs(15));
        loop {
            interval.tick().await;
            let minute = current_minute();
            if minute == last_minute {
                continue;
            }
            last_minute = minute.clone();

            for s in store
                .list_schedules()
                .into_iter()
                .filter(|s| s.enabled && s.time == minute)
            {
                match s.action.as_str() {
                    "emit" => {
                        if let Some(event_type) = &s.event_type {
                            let _ = bus.send(Event::new(
                                "time",
                                event_type,
                                serde_json::json!({ "schedule": s.name }),
                            ));
                        }
                    }
                    "idle" => {
                        if let Some(id) = s.animation_id {
                            store.set_setting("idle_animation_id", &id.to_string());
                            // A scheduled swap must be visible even if idle
                            // was showing the Claude usage gauge.
                            store.set_setting("idle_mode", "animation");
                            engine.recompute();
                            let _ = bus.send(Event::new(
                                "time",
                                "idle_changed",
                                serde_json::json!({ "schedule": s.name }),
                            ));
                        }
                    }
                    _ => {}
                }
            }
        }
    });
}

fn current_minute() -> String {
    chrono::Local::now().format("%H:%M").to_string()
}
