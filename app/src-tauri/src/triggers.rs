//! Trigger engine — turns bus events into "what the strip should show".
//!
//! State is a set of **overlays**: (name, spec, priority, optional expiry).
//! The highest-priority live overlay wins; with none, the idle animation
//! shows. Sources of overlays:
//!
//! * fired triggers (keyed by trigger id, so a re-fire refreshes rather
//!   than stacks)
//! * manual control from the UI (fixed key "manual", beats every trigger —
//!   trigger priorities come from list order in the UI, and an explicit
//!   click must always win over an ambient rule)
//! * snooze from the tray (doesn't add an overlay — it forces "off" and
//!   suppresses everything below until it ends)
//!
//! Every recompute is pushed to the UI as `engine:active`, which is exactly
//! the "why is the light red" debugger: the winning overlay plus everything
//! it beat.

use serde::Serialize;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tauri::Emitter;

use crate::animation::{AnimSpec, EngineShared};
use crate::events::Event;
use crate::store::Store;

pub const MANUAL_PRIORITY: i32 = i32::MAX;

#[derive(Clone)]
struct Overlay {
    /// "trigger:<id>" or "manual".
    key: String,
    name: String,
    spec: AnimSpec,
    priority: i32,
    expires_at: Option<Instant>,
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct OverlayInfo {
    pub key: String,
    pub name: String,
    pub priority: i32,
    pub expires_in_ms: Option<u64>,
    pub winning: bool,
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct ActiveState {
    pub active_name: String,
    pub snoozed_until_ms: Option<i64>,
    pub overlays: Vec<OverlayInfo>,
}

pub struct TriggerEngine {
    inner: Mutex<Inner>,
    store: Store,
    engine: Arc<EngineShared>,
    app: tauri::AppHandle,
    /// Last claude/usage payload as (session, weekly) fractions, kept so the
    /// "Claude usage" idle mode can render without waiting for the next
    /// statusline update. In-memory only: stale-after-restart beats showing
    /// numbers from a previous day as if they were live.
    claude_usage: Mutex<Option<(f32, f32)>>,
}

struct Inner {
    overlays: Vec<Overlay>,
    snooze_until: Option<Instant>,
}

impl TriggerEngine {
    pub fn new(store: Store, engine: Arc<EngineShared>, app: tauri::AppHandle) -> Arc<Self> {
        let me = Arc::new(Self {
            inner: Mutex::new(Inner {
                overlays: Vec::new(),
                snooze_until: None,
            }),
            store,
            engine,
            app,
            claude_usage: Mutex::new(None),
        });
        me.recompute();
        me
    }

    /// Called for every event on the bus.
    pub fn on_event(&self, ev: &Event) {
        if ev.source == "claude" && ev.event_type == "usage" {
            let session = ev.payload.get("session").and_then(|v| v.as_f64());
            let weekly = ev.payload.get("weekly").and_then(|v| v.as_f64());
            if session.is_some() || weekly.is_some() {
                *self.claude_usage.lock().unwrap() = Some((
                    (session.unwrap_or(0.0) / 100.0) as f32,
                    (weekly.unwrap_or(0.0) / 100.0) as f32,
                ));
            }
            // Falls through: the recompute below refreshes the idle bar,
            // and any user trigger on claude/usage still gets its shot.
        }

        let triggers = self.store.list_triggers();
        let mut inner = self.inner.lock().unwrap();

        for trigger in triggers
            .iter()
            .filter(|t| t.enabled && t.source == ev.source)
        {
            // Deactivation first: a trigger's clear event removes its overlay.
            if trigger.clear_event_type.as_deref() == Some(ev.event_type.as_str()) {
                inner
                    .overlays
                    .retain(|o| o.key != format!("trigger:{}", trigger.id));
            }
            if trigger.event_type != ev.event_type {
                continue;
            }
            let Some(animation) = self.store.animation(trigger.animation_id) else {
                continue;
            };
            let mut spec = animation.spec.clone();
            // Progress effects are parameterized by the event itself.
            if spec.effect == "progress" {
                if let Some(percent) = ev.payload.get("percent").and_then(|v| v.as_f64()) {
                    spec.progress = Some((percent / 100.0) as f32);
                }
            }
            if spec.effect == "dual_progress" {
                // e.g. claude/usage {"session": 23.5, "weekly": 61.0}
                if let Some(pct) = ev.payload.get("session").and_then(|v| v.as_f64()) {
                    spec.progress = Some((pct / 100.0) as f32);
                }
                if let Some(pct) = ev.payload.get("weekly").and_then(|v| v.as_f64()) {
                    spec.progress2 = Some((pct / 100.0) as f32);
                }
            }
            let overlay = Overlay {
                key: format!("trigger:{}", trigger.id),
                name: trigger.name.clone(),
                spec,
                priority: trigger.priority,
                // Trigger's own expiry wins; else the animation's intrinsic
                // length ("a flash is 2s wherever it plays"); else forever.
                // When it ends, whatever is underneath is still there — the
                // stack *is* the interrupt-and-return behavior.
                expires_at: trigger
                    .duration_ms
                    .or(animation.duration_ms)
                    .map(|ms| Instant::now() + Duration::from_millis(ms.max(0) as u64)),
            };
            // Re-fire replaces (refreshing expiry) instead of stacking.
            inner.overlays.retain(|o| o.key != overlay.key);
            inner.overlays.push(overlay);
        }
        drop(inner);
        self.recompute();
    }

    /// `expires_in`: None pins manual control until released; Some lets a
    /// manually applied animation play out and then fall away on its own
    /// (used by Apply on animations that carry their own length).
    pub fn set_manual(&self, spec: AnimSpec, expires_in: Option<Duration>) {
        let mut inner = self.inner.lock().unwrap();
        inner.overlays.retain(|o| o.key != "manual");
        inner.overlays.push(Overlay {
            key: "manual".into(),
            name: "Manual control".into(),
            spec,
            priority: MANUAL_PRIORITY,
            expires_at: expires_in.map(|d| Instant::now() + d),
        });
        drop(inner);
        self.recompute();
    }

    pub fn clear_manual(&self) {
        self.inner
            .lock()
            .unwrap()
            .overlays
            .retain(|o| o.key != "manual");
        self.recompute();
    }

    /// minutes == 0 clears the snooze.
    pub fn snooze(&self, minutes: u64) {
        self.inner.lock().unwrap().snooze_until = if minutes == 0 {
            None
        } else {
            Some(Instant::now() + Duration::from_secs(minutes * 60))
        };
        self.recompute();
    }

    /// Reconcile live overlays with the trigger table after any mutation:
    /// a deleted or disabled trigger loses its overlay immediately, a
    /// reordered or renamed one keeps it with the fresh priority/name.
    /// Without this, toggling a trigger off leaves its light on until expiry.
    pub fn sync_with_store(&self) {
        let triggers = self.store.list_triggers();
        let mut inner = self.inner.lock().unwrap();
        inner.overlays.retain_mut(|o| {
            let Some(id) = o
                .key
                .strip_prefix("trigger:")
                .and_then(|s| s.parse::<i64>().ok())
            else {
                return true; // "manual" is not the store's to reconcile
            };
            match triggers.iter().find(|t| t.id == id) {
                Some(t) if t.enabled => {
                    o.priority = t.priority;
                    o.name = t.name.clone();
                    true
                }
                _ => false,
            }
        });
        drop(inner);
        self.recompute();
    }

    fn idle_spec(&self) -> (String, AnimSpec) {
        // idle_mode "claude_usage": the resting state is a live gauge —
        // session usage growing from the left, weekly from the right —
        // fed by claude/usage events (lightctl claude statusline bridge).
        if self.store.setting("idle_mode").as_deref() == Some("claude_usage") {
            let (session, weekly) = self.claude_usage.lock().unwrap().unwrap_or((0.0, 0.0));
            return (
                "Idle (Claude usage)".into(),
                AnimSpec {
                    effect: "dual_progress".into(),
                    color: usage_color(session),
                    color2: Some(usage_color(weekly)),
                    speed: 0.0,
                    progress: Some(session),
                    progress2: Some(weekly),
                    keyframes: None,
                },
            );
        }
        let idle_id = self
            .store
            .setting("idle_animation_id")
            .and_then(|v| v.parse::<i64>().ok());
        if let Some(animation) = idle_id.and_then(|id| self.store.animation(id)) {
            (format!("Idle ({})", animation.name), animation.spec)
        } else {
            ("Idle (Rainbow)".into(), AnimSpec::default())
        }
    }

    /// Prune expired overlays, pick a winner, hand it to the animation
    /// engine, and tell the UI. Called on every event, on manual changes,
    /// and periodically (for expirations) from a timer task.
    pub fn recompute(&self) -> ActiveState {
        let now = Instant::now();
        let mut inner = self.inner.lock().unwrap();
        inner
            .overlays
            .retain(|o| o.expires_at.map(|t| t > now).unwrap_or(true));
        if inner.snooze_until.map(|t| t <= now).unwrap_or(false) {
            inner.snooze_until = None;
        }

        let snoozed = inner.snooze_until.is_some();
        let winner = inner.overlays.iter().max_by_key(|o| o.priority).cloned();

        let (active_name, spec) = if snoozed {
            ("Snoozed".to_string(), AnimSpec::off())
        } else {
            match &winner {
                Some(o) => (o.name.clone(), o.spec.clone()),
                None => self.idle_spec(),
            }
        };

        let state = ActiveState {
            active_name,
            snoozed_until_ms: inner.snooze_until.map(|t| {
                crate::events::now_ms() + t.saturating_duration_since(now).as_millis() as i64
            }),
            overlays: inner
                .overlays
                .iter()
                .map(|o| OverlayInfo {
                    key: o.key.clone(),
                    name: o.name.clone(),
                    priority: o.priority,
                    expires_in_ms: o
                        .expires_at
                        .map(|t| t.saturating_duration_since(now).as_millis() as u64),
                    winning: !snoozed
                        && winner.as_ref().map(|w| w.key == o.key).unwrap_or(false),
                })
                .collect(),
        };
        drop(inner);

        self.engine.set_spec(spec);
        let _ = self.app.emit("engine:active", &state);
        state
    }

    pub fn active_state_snapshot(&self) -> ActiveState {
        self.recompute()
    }
}

/// Bar color for a usage fraction: green while comfortable, amber past 60%,
/// red as it approaches the cap. Continuous, so the drift is visible.
fn usage_color(fraction: f32) -> [u8; 3] {
    const GREEN: [u8; 3] = [20, 200, 90];
    const AMBER: [u8; 3] = [255, 160, 0];
    const RED: [u8; 3] = [255, 40, 30];
    let f = fraction.clamp(0.0, 1.0);
    if f < 0.6 {
        crate::animation::lerp(GREEN, AMBER, f / 0.6)
    } else {
        crate::animation::lerp(AMBER, RED, (f - 0.6) / 0.4)
    }
}
