//! Tauri commands — the UI's entire surface area into the core. The UI
//! stays a thin layer: every command delegates to the store, trigger
//! engine, or device manager.

use std::sync::atomic::Ordering;
use tauri::State;

use crate::animation::AnimSpec;
use crate::device::{DeviceMsg, DeviceStatus, PortCandidate};
use crate::events::Event;
use crate::store::{Animation, Schedule, Trigger, TriggerPolicy};
use crate::triggers::ActiveState;
use crate::AppState;

const MAX_NAME_LEN: usize = 100;
const MAX_EVENT_LEN: usize = 64;
const MAX_DURATION_MS: i64 = 86_400_000;

fn validate_text(value: &str, label: &str, max: usize) -> Result<(), String> {
    let len = value.chars().count();
    if value.trim().is_empty() {
        return Err(format!("{label} cannot be empty"));
    }
    if len > max || value.chars().any(char::is_control) {
        return Err(format!(
            "{label} must be at most {max} printable characters"
        ));
    }
    Ok(())
}

fn validate_duration(value: Option<i64>, label: &str) -> Result<(), String> {
    if value.is_some_and(|v| !(1..=MAX_DURATION_MS).contains(&v)) {
        return Err(format!("{label} must be between 1 ms and 24 hours"));
    }
    Ok(())
}

fn validate_animation_input(
    name: &str,
    spec: &AnimSpec,
    duration: Option<i64>,
) -> Result<(), String> {
    validate_text(name, "animation name", MAX_NAME_LEN)?;
    validate_duration(duration, "animation duration")?;
    spec.validate()
}

fn validate_trigger_input(state: &AppState, trigger: &Trigger) -> Result<(), String> {
    validate_text(&trigger.name, "trigger name", MAX_NAME_LEN)?;
    validate_text(&trigger.source, "event source", MAX_EVENT_LEN)?;
    validate_text(&trigger.event_type, "event type", MAX_EVENT_LEN)?;
    if let Some(clear) = trigger.clear_event_type.as_deref() {
        validate_text(clear, "clear event type", MAX_EVENT_LEN)?;
    }
    validate_duration(trigger.duration_ms, "trigger duration")?;
    validate_text(&trigger.policy.profile, "profile", MAX_NAME_LEN)?;
    if let Some(path) = trigger.policy.payload_path.as_deref() {
        validate_text(path, "payload path", 200)?;
        if trigger.policy.payload_equals.is_none() {
            return Err("payload conditions need an expected value".into());
        }
    }
    validate_duration(trigger.policy.cooldown_ms, "trigger cooldown")?;
    if !(-10_000..=10_000).contains(&trigger.priority) {
        return Err("trigger priority is outside the supported range".into());
    }
    if state.store.animation(trigger.animation_id).is_none() {
        return Err("trigger references an unknown animation".into());
    }
    Ok(())
}

#[tauri::command]
pub fn list_profiles(state: State<AppState>) -> Vec<String> {
    let mut profiles = vec!["Default".to_string()];
    profiles.extend(
        state
            .store
            .list_triggers()
            .into_iter()
            .map(|t| t.policy.profile)
            .filter(|p| p != "*"),
    );
    profiles.sort();
    profiles.dedup();
    profiles
}

#[tauri::command]
pub fn get_active_profile(state: State<AppState>) -> String {
    state
        .store
        .setting("active_profile")
        .unwrap_or_else(|| "Default".into())
}

#[tauri::command]
pub fn set_active_profile(state: State<AppState>, profile: String) -> Result<(), String> {
    validate_text(&profile, "profile", MAX_NAME_LEN)?;
    state.store.set_setting("active_profile", &profile);
    state.triggers.sync_with_store();
    Ok(())
}

fn valid_clock(value: &str) -> bool {
    value.len() == 5
        && value.as_bytes().get(2) == Some(&b':')
        && value[0..2].parse::<u8>().is_ok_and(|h| h < 24)
        && value[3..5].parse::<u8>().is_ok_and(|m| m < 60)
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct QuietHours {
    enabled: bool,
    start: String,
    end: String,
}

#[tauri::command]
pub fn get_quiet_hours(state: State<AppState>) -> QuietHours {
    QuietHours {
        enabled: state.store.setting("quiet_enabled").as_deref() == Some("true"),
        start: state
            .store
            .setting("quiet_start")
            .unwrap_or_else(|| "22:00".into()),
        end: state
            .store
            .setting("quiet_end")
            .unwrap_or_else(|| "07:00".into()),
    }
}

#[tauri::command]
pub fn set_quiet_hours(
    state: State<AppState>,
    enabled: bool,
    start: String,
    end: String,
) -> Result<(), String> {
    if !valid_clock(&start) || !valid_clock(&end) {
        return Err("quiet hours must use valid HH:MM times".into());
    }
    state
        .store
        .set_setting("quiet_enabled", if enabled { "true" } else { "false" });
    state.store.set_setting("quiet_start", &start);
    state.store.set_setting("quiet_end", &end);
    state.triggers.recompute();
    Ok(())
}

fn validate_schedule_input(state: &AppState, schedule: &Schedule) -> Result<(), String> {
    validate_text(&schedule.name, "schedule name", MAX_NAME_LEN)?;
    let valid_time = schedule.time.len() == 5
        && schedule.time.as_bytes()[2] == b':'
        && schedule.time[0..2].parse::<u8>().is_ok_and(|h| h < 24)
        && schedule.time[3..5].parse::<u8>().is_ok_and(|m| m < 60);
    if !valid_time {
        return Err("schedule time must be HH:MM in 24-hour time".into());
    }
    match schedule.action.as_str() {
        "emit" => validate_text(
            schedule.event_type.as_deref().unwrap_or(""),
            "schedule event type",
            MAX_EVENT_LEN,
        )?,
        "idle" => {
            let id = schedule
                .animation_id
                .ok_or("idle schedules need an animation")?;
            if state.store.animation(id).is_none() {
                return Err("schedule references an unknown animation".into());
            }
        }
        _ => return Err("schedule action must be 'emit' or 'idle'".into()),
    }
    Ok(())
}

#[tauri::command]
pub fn get_status(state: State<AppState>) -> DeviceStatus {
    state.device_status.lock().unwrap().clone()
}

#[tauri::command]
pub fn list_candidates(state: State<AppState>) -> Vec<PortCandidate> {
    state.candidates.lock().unwrap().clone()
}

#[tauri::command]
pub fn adopt_device(state: State<AppState>, port: String) -> Result<(), String> {
    validate_text(&port, "device port", 512)?;
    let _ = state.device_tx.try_send(DeviceMsg::Adopt(port));
    Ok(())
}

#[tauri::command]
pub fn forget_device(state: State<AppState>) {
    let _ = state.device_tx.try_send(DeviceMsg::Forget);
}

#[tauri::command]
pub fn set_manual(state: State<AppState>, spec: AnimSpec) -> Result<(), String> {
    spec.validate()?;
    // Hand-built manual state pins until released; only animation Apply
    // (below) picks up an intrinsic duration.
    state.triggers.set_manual(spec, None);
    Ok(())
}

#[tauri::command]
pub fn clear_manual(state: State<AppState>) {
    state.triggers.clear_manual();
}

#[tauri::command]
pub fn set_brightness(state: State<AppState>, value: u8) {
    state.engine.set_brightness(value);
    state.store.set_setting("brightness", &value.to_string());
}

#[tauri::command]
pub fn get_brightness(state: State<AppState>) -> u8 {
    state.engine.brightness.load(Ordering::Relaxed)
}

#[tauri::command]
pub fn list_animations(state: State<AppState>) -> Vec<Animation> {
    state.store.list_animations()
}

#[tauri::command]
pub fn save_animation(
    state: State<AppState>,
    name: String,
    spec: AnimSpec,
    duration_ms: Option<i64>,
) -> Result<i64, String> {
    validate_animation_input(&name, &spec, duration_ms)?;
    state
        .store
        .save_animation(&name, &spec, false, duration_ms)
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub fn update_animation(
    state: State<AppState>,
    id: i64,
    name: String,
    spec: AnimSpec,
    duration_ms: Option<i64>,
) -> Result<(), String> {
    validate_animation_input(&name, &spec, duration_ms)?;
    state
        .store
        .update_animation(id, &name, &spec, duration_ms)
        .map_err(|e| e.to_string())?;
    // The edited animation may be the idle animation or feed an active
    // overlay next time its trigger fires; recompute picks up the former now.
    state.triggers.recompute();
    Ok(())
}

#[tauri::command]
pub fn delete_animation(state: State<AppState>, id: i64) -> Result<(), String> {
    state.store.delete_animation(id).map_err(|e| e.to_string())
}

#[tauri::command]
pub fn apply_animation(state: State<AppState>, id: i64) -> Result<(), String> {
    let animation = state.store.animation(id).ok_or("no such animation")?;
    // An animation with its own length plays out and falls away, revealing
    // whatever was underneath — Apply on "Success Flash" is a 2s flash, not
    // a mode you have to release.
    let expires = animation
        .duration_ms
        .map(|ms| std::time::Duration::from_millis(ms.max(0) as u64));
    state.triggers.set_manual(animation.spec, expires);
    Ok(())
}

#[tauri::command]
pub fn set_idle_animation(state: State<AppState>, id: i64) -> Result<(), String> {
    if state.store.animation(id).is_none() {
        return Err("unknown idle animation".into());
    }
    state
        .store
        .set_setting("idle_animation_id", &id.to_string());
    state.triggers.recompute();
    Ok(())
}

#[tauri::command]
pub fn get_idle_animation(state: State<AppState>) -> Option<i64> {
    state
        .store
        .setting("idle_animation_id")
        .and_then(|v| v.parse().ok())
}

/// "animation" (default) shows the idle animation; "claude_usage" shows the
/// live Claude session/weekly gauge fed by the lightctl statusline bridge.
#[tauri::command]
pub fn set_idle_mode(state: State<AppState>, mode: String) -> Result<(), String> {
    if mode != "animation" && mode != "claude_usage" {
        return Err(format!("unknown idle mode: {mode}"));
    }
    state.store.set_setting("idle_mode", &mode);
    state.triggers.recompute();
    Ok(())
}

#[tauri::command]
pub fn get_idle_mode(state: State<AppState>) -> String {
    state
        .store
        .setting("idle_mode")
        .unwrap_or_else(|| "animation".into())
}

#[tauri::command]
pub fn list_triggers(state: State<AppState>) -> Vec<Trigger> {
    state.store.list_triggers()
}

#[tauri::command]
pub fn save_trigger(state: State<AppState>, trigger: Trigger) -> Result<i64, String> {
    validate_trigger_input(&state, &trigger)?;
    let id = state
        .store
        .save_trigger(&trigger)
        .map_err(|e| e.to_string())?;
    // Disabling a trigger must kill its live overlay, not just future fires.
    state.triggers.sync_with_store();
    Ok(id)
}

/// Persist a drag-reorder: `ids` in display order, first = highest priority.
#[tauri::command]
pub fn reorder_triggers(state: State<AppState>, ids: Vec<i64>) -> Result<(), String> {
    state
        .store
        .reorder_triggers(&ids)
        .map_err(|e| e.to_string())?;
    state.triggers.sync_with_store();
    Ok(())
}

#[tauri::command]
pub fn delete_trigger(state: State<AppState>, id: i64) -> Result<(), String> {
    state.store.delete_trigger(id).map_err(|e| e.to_string())?;
    state.triggers.sync_with_store();
    Ok(())
}

#[tauri::command]
pub fn recent_events(state: State<AppState>, limit: u32) -> Vec<Event> {
    state.store.recent_events(limit.min(500))
}

#[tauri::command]
pub fn clear_events(state: State<AppState>) -> Result<(), String> {
    state.store.clear_events().map_err(|e| e.to_string())
}

#[tauri::command]
pub fn get_active(state: State<AppState>) -> ActiveState {
    state.triggers.active_state_snapshot()
}

#[tauri::command]
pub fn snooze(state: State<AppState>, minutes: u64) {
    state.triggers.snooze(minutes.min(24 * 60));
}

// -- schedules ---------------------------------------------------------------

#[tauri::command]
pub fn list_schedules(state: State<AppState>) -> Vec<Schedule> {
    state.store.list_schedules()
}

#[tauri::command]
pub fn save_schedule(state: State<AppState>, schedule: Schedule) -> Result<i64, String> {
    validate_schedule_input(&state, &schedule)?;
    state
        .store
        .save_schedule(&schedule)
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub fn delete_schedule(state: State<AppState>, id: i64) -> Result<(), String> {
    state.store.delete_schedule(id).map_err(|e| e.to_string())
}

// -- export / import ----------------------------------------------------------
// Animations are referenced by *name* in the file, so a config moves between
// machines whose row ids differ. Import upserts by name — nothing is deleted.

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct ConfigFile {
    version: u32,
    animations: Vec<ConfigAnimation>,
    triggers: Vec<ConfigTrigger>,
    #[serde(default)]
    schedules: Vec<ConfigSchedule>,
    idle_animation: Option<String>,
    #[serde(default)]
    idle_mode: Option<String>,
}

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct ConfigAnimation {
    name: String,
    spec: AnimSpec,
    builtin: bool,
    duration_ms: Option<i64>,
}

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct ConfigTrigger {
    name: String,
    source: String,
    event_type: String,
    clear_event_type: Option<String>,
    animation: String,
    priority: i32,
    duration_ms: Option<i64>,
    enabled: bool,
    #[serde(default)]
    policy: TriggerPolicy,
}

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct ConfigSchedule {
    name: String,
    time: String,
    action: String,
    event_type: Option<String>,
    animation: Option<String>,
    enabled: bool,
}

#[tauri::command]
pub fn export_config(state: State<AppState>, path: String) -> Result<(), String> {
    let animations = state.store.list_animations();
    let name_of = |id: i64| {
        animations
            .iter()
            .find(|a| a.id == id)
            .map(|a| a.name.clone())
    };
    let file = ConfigFile {
        version: 1,
        animations: animations
            .iter()
            .map(|a| ConfigAnimation {
                name: a.name.clone(),
                spec: a.spec.clone(),
                builtin: a.builtin,
                duration_ms: a.duration_ms,
            })
            .collect(),
        triggers: state
            .store
            .list_triggers()
            .into_iter()
            .filter_map(|t| {
                Some(ConfigTrigger {
                    animation: name_of(t.animation_id)?,
                    name: t.name,
                    source: t.source,
                    event_type: t.event_type,
                    clear_event_type: t.clear_event_type,
                    priority: t.priority,
                    duration_ms: t.duration_ms,
                    enabled: t.enabled,
                    policy: t.policy.clone(),
                })
            })
            .collect(),
        schedules: state
            .store
            .list_schedules()
            .into_iter()
            .map(|s| ConfigSchedule {
                animation: s.animation_id.and_then(name_of),
                name: s.name,
                time: s.time,
                action: s.action,
                event_type: s.event_type,
                enabled: s.enabled,
            })
            .collect(),
        idle_animation: state
            .store
            .setting("idle_animation_id")
            .and_then(|v| v.parse().ok())
            .and_then(name_of),
        idle_mode: state.store.setting("idle_mode"),
    };
    let json = serde_json::to_string_pretty(&file).map_err(|e| e.to_string())?;
    std::fs::write(&path, json).map_err(|e| e.to_string())
}

#[tauri::command]
pub fn export_diagnostics(state: State<AppState>, path: String) -> Result<(), String> {
    let status = state
        .device_status
        .lock()
        .map_err(|_| "device status unavailable")?
        .clone();
    let diagnostics = serde_json::json!({
        "appVersion": env!("CARGO_PKG_VERSION"),
        "platform": std::env::consts::OS,
        "architecture": std::env::consts::ARCH,
        "device": {
            "connected": status.connected,
            "firmwareVersion": status.fw_version,
            "protocolVersion": status.protocol_version,
            "ledCount": status.led_count
        },
        "counts": {
            "animations": state.store.list_animations().len(),
            "triggers": state.store.list_triggers().len(),
            "schedules": state.store.list_schedules().len()
        },
        "recentEvents": state.store.recent_events(100).into_iter().map(|event| serde_json::json!({
            "source": event.source,
            "type": event.event_type,
            "ts": event.ts
        })).collect::<Vec<_>>()
    });
    let json = serde_json::to_string_pretty(&diagnostics).map_err(|e| e.to_string())?;
    std::fs::write(path, json).map_err(|e| e.to_string())
}

#[tauri::command]
pub fn import_config(state: State<AppState>, path: String) -> Result<String, String> {
    const MAX_CONFIG_BYTES: u64 = 2 * 1024 * 1024;
    if std::fs::metadata(&path).map_err(|e| e.to_string())?.len() > MAX_CONFIG_BYTES {
        return Err("config file is larger than 2 MiB".into());
    }
    let raw = std::fs::read_to_string(&path).map_err(|e| e.to_string())?;
    let file: ConfigFile =
        serde_json::from_str(&raw).map_err(|e| format!("not a Luminode config: {e}"))?;
    if file.version != 1 {
        return Err(format!("unsupported config version: {}", file.version));
    }
    if file.animations.len() > 500 || file.triggers.len() > 1_000 || file.schedules.len() > 500 {
        return Err("config contains too many items".into());
    }
    for a in &file.animations {
        validate_animation_input(&a.name, &a.spec, a.duration_ms)?;
    }
    let animation_names: std::collections::HashSet<&str> =
        file.animations.iter().map(|a| a.name.as_str()).collect();
    for t in &file.triggers {
        validate_text(&t.name, "trigger name", MAX_NAME_LEN)?;
        validate_text(&t.source, "event source", MAX_EVENT_LEN)?;
        validate_text(&t.event_type, "event type", MAX_EVENT_LEN)?;
        if let Some(clear) = t.clear_event_type.as_deref() {
            validate_text(clear, "clear event type", MAX_EVENT_LEN)?;
        }
        validate_duration(t.duration_ms, "trigger duration")?;
        if !animation_names.contains(t.animation.as_str())
            && state
                .store
                .list_animations()
                .iter()
                .all(|a| a.name != t.animation)
        {
            return Err(format!(
                "trigger '{}' references an unknown animation",
                t.name
            ));
        }
    }
    for s in &file.schedules {
        validate_text(&s.name, "schedule name", MAX_NAME_LEN)?;
        let valid_time = s.time.len() == 5
            && s.time.as_bytes()[2] == b':'
            && s.time[0..2].parse::<u8>().is_ok_and(|h| h < 24)
            && s.time[3..5].parse::<u8>().is_ok_and(|m| m < 60);
        if !valid_time {
            return Err(format!("schedule '{}' has an invalid time", s.name));
        }
        match s.action.as_str() {
            "emit" => validate_text(
                s.event_type.as_deref().unwrap_or(""),
                "schedule event type",
                MAX_EVENT_LEN,
            )?,
            "idle" if s.animation.is_some() => {}
            "idle" => return Err(format!("schedule '{}' needs an animation", s.name)),
            _ => return Err(format!("schedule '{}' has an invalid action", s.name)),
        }
    }

    for a in &file.animations {
        state
            .store
            .save_animation(&a.name, &a.spec, false, a.duration_ms)
            .map_err(|e| e.to_string())?;
    }
    let animations = state.store.list_animations();
    let id_of = |name: &str| animations.iter().find(|a| a.name == name).map(|a| a.id);

    let existing_triggers = state.store.list_triggers();
    let mut trigger_count = 0;
    for t in &file.triggers {
        let Some(animation_id) = id_of(&t.animation) else {
            continue;
        };
        let id = existing_triggers
            .iter()
            .find(|e| e.name == t.name)
            .map(|e| e.id)
            .unwrap_or(0);
        state
            .store
            .save_trigger(&Trigger {
                id,
                name: t.name.clone(),
                source: t.source.clone(),
                event_type: t.event_type.clone(),
                clear_event_type: t.clear_event_type.clone(),
                animation_id,
                priority: t.priority,
                duration_ms: t.duration_ms,
                enabled: t.enabled,
                policy: t.policy.clone(),
            })
            .map_err(|e| e.to_string())?;
        trigger_count += 1;
    }

    let existing_schedules = state.store.list_schedules();
    for s in &file.schedules {
        let id = existing_schedules
            .iter()
            .find(|e| e.name == s.name)
            .map(|e| e.id)
            .unwrap_or(0);
        state
            .store
            .save_schedule(&Schedule {
                id,
                name: s.name.clone(),
                time: s.time.clone(),
                action: s.action.clone(),
                event_type: s.event_type.clone(),
                animation_id: s.animation.as_deref().and_then(id_of),
                enabled: s.enabled,
            })
            .map_err(|e| e.to_string())?;
    }

    if let Some(idle_id) = file.idle_animation.as_deref().and_then(id_of) {
        state
            .store
            .set_setting("idle_animation_id", &idle_id.to_string());
    }
    if let Some(mode) = file
        .idle_mode
        .as_deref()
        .filter(|m| *m == "animation" || *m == "claude_usage")
    {
        state.store.set_setting("idle_mode", mode);
    }
    state.triggers.sync_with_store();
    Ok(format!(
        "Imported {} animations, {} triggers, {} schedules",
        file.animations.len(),
        trigger_count,
        file.schedules.len()
    ))
}

/// Store (or, with an empty value, delete) an integration secret in the OS
/// keychain. Known names: "slack_token", "calendar_ics_url".
#[tauri::command]
pub fn set_secret(name: String, value: String) -> Result<(), String> {
    crate::secrets::set(&name, &value)
}

#[tauri::command]
pub fn has_secret(name: String) -> bool {
    crate::secrets::get(&name).is_some()
}

#[tauri::command]
pub fn integration_health(state: State<AppState>) -> Vec<crate::health::IntegrationHealth> {
    state.health.snapshot()
}

/// Inject a synthetic event — lets users test triggers from the UI without
/// wiring up a real integration first.
#[tauri::command]
pub fn simulate_event(
    state: State<AppState>,
    source: String,
    event_type: String,
    payload: serde_json::Value,
) -> Result<(), String> {
    validate_text(&source, "event source", MAX_EVENT_LEN)?;
    validate_text(&event_type, "event type", MAX_EVENT_LEN)?;
    if serde_json::to_vec(&payload)
        .map_err(|e| e.to_string())?
        .len()
        > 65_536
    {
        return Err("event payload is larger than 64 KiB".into());
    }
    let _ = state.bus.send(Event::new(&source, &event_type, payload));
    Ok(())
}
