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
use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tauri::Emitter;

use crate::animation::{AnimSpec, EngineShared};
use crate::events::Event;
use crate::store::Store;

pub const MANUAL_PRIORITY: i32 = i32::MAX;

/// Idle-dim target: how far to pull brightness down after the inactivity
/// window elapses (≈25% of the set brightness). Restored to full on the next
/// activity.
const IDLE_DIM_SCALE: u8 = 64;

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
    pub quiet_hours_active: bool,
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
    /// session_id of the most recently active Claude Code session — the one you
    /// last prompted in (set on each claude/active). The usage gauge follows
    /// this session only, so an idle background session on a different account
    /// can't clobber the live numbers. None until the first prompt after start,
    /// during which any session's usage is accepted.
    active_claude_session: Mutex<Option<String>>,
    /// Idle-dimming: minutes of inactivity before the strip dims (0 = never),
    /// cached from the `idle_dim_minutes` setting so `recompute` (every 250ms)
    /// doesn't hit the DB; and the last time anything happened (a bus event or
    /// a user action) that should reset the dim.
    idle_dim_minutes: AtomicU32,
    last_activity: Mutex<Instant>,
}

struct Inner {
    overlays: Vec<Overlay>,
    snooze_until: Option<Instant>,
    last_fired: HashMap<i64, Instant>,
}

impl TriggerEngine {
    pub fn new(store: Store, engine: Arc<EngineShared>, app: tauri::AppHandle) -> Arc<Self> {
        let idle_dim_minutes = store
            .setting("idle_dim_minutes")
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
        let me = Arc::new(Self {
            inner: Mutex::new(Inner {
                overlays: Vec::new(),
                snooze_until: None,
                last_fired: HashMap::new(),
            }),
            store,
            engine,
            app,
            claude_usage: Mutex::new(None),
            active_claude_session: Mutex::new(None),
            idle_dim_minutes: AtomicU32::new(idle_dim_minutes),
            last_activity: Mutex::new(Instant::now()),
        });
        me.recompute();
        me
    }

    /// Record that something happened (a bus event or a user action) so the
    /// idle-dim timer resets. The following `recompute` restores full
    /// brightness if it had dimmed.
    fn mark_active(&self) {
        *self.last_activity.lock().unwrap() = Instant::now();
    }

    /// User interacted (moved brightness, toggled power, etc.): reset the
    /// idle-dim timer and reflect it immediately.
    pub fn note_activity(&self) {
        self.mark_active();
        self.recompute();
    }

    /// Update the cached inactivity window (minutes; 0 = never dim).
    pub fn set_idle_dim_minutes(&self, minutes: u32) {
        self.idle_dim_minutes.store(minutes, Ordering::Relaxed);
        self.note_activity();
    }

    /// Called for every event on the bus.
    pub fn on_event(&self, ev: &Event) {
        // Any event is activity: keep the strip at full brightness while
        // things are happening. The recompute at the end restores it.
        self.mark_active();
        if ev.source == "claude" && ev.event_type == "active" {
            // Whichever session just took a prompt becomes the one the usage
            // gauge follows. Prompts without a session_id (older bridge) leave
            // the current owner in place rather than un-pinning it.
            if let Some(sid) = ev.payload.get("session_id").and_then(|v| v.as_str()) {
                *self.active_claude_session.lock().unwrap() = Some(sid.to_string());
            }
        }
        if ev.source == "claude" && ev.event_type == "usage" {
            // Only the active session drives the gauge. With several sessions
            // open (possibly on different accounts), this stops an idle one
            // from yanking the strip back to its stale numbers. Before the
            // first prompt (owner still None), accept any session so the gauge
            // isn't blank at startup.
            let owner = self.active_claude_session.lock().unwrap();
            let from = ev.payload.get("session_id").and_then(|v| v.as_str());
            let owned = usage_belongs_to_active(owner.as_deref(), from);
            drop(owner);
            if owned {
                let session = ev.payload.get("session").and_then(|v| v.as_f64());
                let weekly = ev.payload.get("weekly").and_then(|v| v.as_f64());
                if session.is_some() || weekly.is_some() {
                    *self.claude_usage.lock().unwrap() = Some((
                        (session.unwrap_or(0.0) / 100.0) as f32,
                        (weekly.unwrap_or(0.0) / 100.0) as f32,
                    ));
                }
            }
            // Falls through: the recompute below refreshes the idle bar,
            // and any user trigger on claude/usage still gets its shot.
        }

        let triggers = self.store.list_triggers();
        let active_profile = self
            .store
            .setting("active_profile")
            .unwrap_or_else(|| "Default".into());
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
            if quiet_hours_active(&self.store) {
                continue;
            }
            if trigger.policy.profile != "*" && trigger.policy.profile != active_profile {
                continue;
            }
            if !payload_matches(ev, &trigger.policy) {
                continue;
            }
            if trigger.policy.cooldown_ms.is_some_and(|ms| {
                inner
                    .last_fired
                    .get(&trigger.id)
                    .is_some_and(|last| last.elapsed() < Duration::from_millis(ms.max(0) as u64))
            }) {
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
            inner.last_fired.insert(trigger.id, Instant::now());
        }
        drop(inner);
        self.recompute();
    }

    /// `expires_in`: None pins manual control until released; Some lets a
    /// manually applied animation play out and then fall away on its own
    /// (used by Apply on animations that carry their own length).
    pub fn set_manual(&self, spec: AnimSpec, expires_in: Option<Duration>) {
        self.mark_active();
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
        self.mark_active();
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
        let active_profile = self
            .store
            .setting("active_profile")
            .unwrap_or_else(|| "Default".into());
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
                Some(t)
                    if t.enabled
                        && (t.policy.profile == "*" || t.policy.profile == active_profile) =>
                {
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
                    level: 1.0,
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
        let quiet = quiet_hours_active(&self.store);
        let manual_wins = winner.as_ref().is_some_and(|o| o.key == "manual");

        let (active_name, spec) = if snoozed {
            ("Snoozed".to_string(), AnimSpec::off())
        } else if quiet && !manual_wins {
            ("Quiet hours".to_string(), AnimSpec::off())
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
                    winning: !snoozed && winner.as_ref().map(|w| w.key == o.key).unwrap_or(false),
                })
                .collect(),
            quiet_hours_active: quiet,
        };
        drop(inner);

        // Idle dimming: after the configured inactivity window, pull the
        // brightness down; full brightness returns on the next activity. Only
        // bumps the engine's send generation on a real change, so calling it
        // every 250ms recompute is cheap.
        let minutes = self.idle_dim_minutes.load(Ordering::Relaxed);
        let dim = minutes > 0
            && self.last_activity.lock().unwrap().elapsed()
                >= Duration::from_secs(minutes as u64 * 60);
        self.engine
            .set_idle_scale(if dim { IDLE_DIM_SCALE } else { 255 });

        self.engine.set_spec(spec);
        let _ = self.app.emit("engine:active", &state);
        state
    }

    pub fn active_state_snapshot(&self) -> ActiveState {
        self.recompute()
    }
}

fn quiet_hours_active(store: &Store) -> bool {
    if store.setting("quiet_enabled").as_deref() != Some("true") {
        return false;
    }
    let start = store
        .setting("quiet_start")
        .unwrap_or_else(|| "22:00".into());
    let end = store.setting("quiet_end").unwrap_or_else(|| "07:00".into());
    let now = chrono::Local::now().format("%H:%M").to_string();
    if start <= end {
        now >= start && now < end
    } else {
        now >= start || now < end
    }
}

fn payload_matches(ev: &Event, policy: &crate::store::TriggerPolicy) -> bool {
    let (Some(path), Some(expected)) = (&policy.payload_path, &policy.payload_equals) else {
        return true;
    };
    let pointer = if path.starts_with('/') {
        path.clone()
    } else {
        format!(
            "/{}",
            path.split('.')
                .map(|part| part.replace('~', "~0").replace('/', "~1"))
                .collect::<Vec<_>>()
                .join("/")
        )
    };
    ev.payload.pointer(&pointer) == Some(expected)
}

/// Bar color for a usage fraction: green while comfortable, amber past 60%,
/// red as it approaches the cap. Continuous, so the drift is visible.
/// Whether a claude/usage event should drive the idle gauge, given the
/// currently-active session. The active session (the one last prompted in)
/// owns the gauge; other sessions are ignored so an idle one can't overwrite
/// the live numbers. Until a session is pinned (owner None) or when either
/// side lacks a session_id (older bridge), the event is accepted.
fn usage_belongs_to_active(active: Option<&str>, from: Option<&str>) -> bool {
    match (active, from) {
        (Some(active), Some(sid)) => active == sid,
        _ => true,
    }
}

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::TriggerPolicy;

    #[test]
    fn payload_conditions_support_dot_and_pointer_paths() {
        let event = Event::new(
            "test",
            "event",
            serde_json::json!({"build": {"status": "failed"}}),
        );
        for path in ["build.status", "/build/status"] {
            let policy = TriggerPolicy {
                payload_path: Some(path.into()),
                payload_equals: Some(serde_json::json!("failed")),
                ..TriggerPolicy::default()
            };
            assert!(payload_matches(&event, &policy));
        }
    }

    #[test]
    fn payload_condition_rejects_a_different_value() {
        let event = Event::new("test", "event", serde_json::json!({"percent": 50}));
        let policy = TriggerPolicy {
            payload_path: Some("percent".into()),
            payload_equals: Some(serde_json::json!(100)),
            ..TriggerPolicy::default()
        };
        assert!(!payload_matches(&event, &policy));
    }

    #[test]
    fn usage_gauge_follows_only_the_active_session() {
        // The active session's own usage drives the gauge.
        assert!(usage_belongs_to_active(Some("A"), Some("A")));
        // An idle session on a different account cannot.
        assert!(!usage_belongs_to_active(Some("A"), Some("B")));
        // Before any prompt, or from an older bridge without a session_id,
        // accept the event so the gauge isn't blank.
        assert!(usage_belongs_to_active(None, Some("B")));
        assert!(usage_belongs_to_active(Some("A"), None));
        assert!(usage_belongs_to_active(None, None));
    }
}
