//! Luminode core — wires together the layers from the architecture plan:
//!
//!   tray/UI → commands.rs
//!   event bus → events.rs (sources push, subscribers below)
//!   trigger engine → triggers.rs (bus subscriber, owns priorities)
//!   animation engine → animation.rs (30fps thread, owns "current look")
//!   device manager → device.rs (serial lifecycle thread)
//!   persistence → store.rs (SQLite in the app data dir)
//!
//! Startup order matters only in that everything needs the AppHandle, so it
//! all happens in `.setup()`.

mod animation;
mod commands;
mod device;
mod events;
mod health;
mod secrets;
mod sources;
mod store;
mod triggers;

use std::sync::mpsc::SyncSender;
use std::sync::{Arc, Mutex};
use tauri::menu::{CheckMenuItem, Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::TrayIconBuilder;
use tauri::{Emitter, Manager, WindowEvent};
use tauri_plugin_autostart::ManagerExt as _;

use animation::EngineShared;
use device::{DeviceMsg, DeviceStatus, PortCandidate};
use events::Bus;
use store::Store;
use triggers::TriggerEngine;

pub struct AppState {
    pub store: Store,
    pub bus: Bus,
    pub device_tx: SyncSender<DeviceMsg>,
    pub engine: Arc<EngineShared>,
    pub triggers: Arc<TriggerEngine>,
    pub device_status: Arc<Mutex<DeviceStatus>>,
    pub candidates: Arc<Mutex<Vec<PortCandidate>>>,
    pub health: health::HealthRegistry,
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            None,
        ))
        .setup(|app| {
            // Regular app: dock icon + Cmd-Tab. Closing the window still
            // only hides it (see on_window_event) — the tray keeps the
            // lights running; quitting stays explicit.
            let data_dir = app.path().app_data_dir()?;
            std::fs::create_dir_all(&data_dir)?;
            let store = Store::open(&data_dir.join("luminode.db"))?;

            let initial_brightness = store
                .setting("brightness")
                .and_then(|v| v.parse().ok())
                .unwrap_or(64u8);

            let bus = events::new_bus();
            let engine = Arc::new(EngineShared::new(initial_brightness));
            let device_status = Arc::new(Mutex::new(DeviceStatus::default()));
            let candidates = Arc::new(Mutex::new(Vec::new()));
            let health = health::HealthRegistry::default();

            // Frames are droppable, so the channel stays small on purpose —
            // backpressure means "skip frames", never "stall the engine".
            let (device_tx, device_rx) = std::sync::mpsc::sync_channel::<DeviceMsg>(8);

            let trigger_engine =
                TriggerEngine::new(store.clone(), engine.clone(), app.handle().clone());

            device::spawn(
                device::DeviceCtx {
                    status: device_status.clone(),
                    candidates: candidates.clone(),
                    store: store.clone(),
                    bus: bus.clone(),
                    engine: engine.clone(),
                    app: app.handle().clone(),
                },
                device_rx,
            );
            animation::spawn(engine.clone(), device_tx.clone(), app.handle().clone());
            sources::lightctl::spawn(
                std::env::var("LIGHTCTL_SOCK")
                    .map(Into::into)
                    .unwrap_or_else(|_| data_dir.join(sources::lightctl::SOCKET_NAME)),
                bus.clone(),
            );
            sources::screenlock::spawn(bus.clone());
            sources::call::spawn(bus.clone());
            sources::display::spawn(bus.clone());
            sources::schedule::spawn(store.clone(), bus.clone(), trigger_engine.clone());
            // Poll-loop sources idle until their keychain secret exists, so
            // spawning unconditionally is free.
            sources::slack::spawn(bus.clone(), health.clone());
            sources::calendar::spawn(bus.clone(), health.clone());

            // Bus subscriber: feed the trigger engine, the event log, and
            // the UI's live feed from every event.
            {
                let mut rx = bus.subscribe();
                let triggers = trigger_engine.clone();
                let store = store.clone();
                let handle = app.handle().clone();
                tauri::async_runtime::spawn(async move {
                    loop {
                        match rx.recv().await {
                            Ok(ev) => {
                                store.log_event(&ev);
                                let _ = handle.emit("bus:event", &ev);
                                triggers.on_event(&ev);
                            }
                            // Lagged: we missed events; keep going.
                            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                            Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                        }
                    }
                });
            }

            // Timer: expire transient overlays (flash triggers etc.) even
            // when no new events arrive.
            {
                let triggers = trigger_engine.clone();
                tauri::async_runtime::spawn(async move {
                    let mut interval = tokio::time::interval(std::time::Duration::from_millis(250));
                    loop {
                        interval.tick().await;
                        triggers.recompute();
                    }
                });
            }

            app.manage(AppState {
                store,
                bus,
                device_tx,
                engine,
                triggers: trigger_engine.clone(),
                device_status,
                candidates,
                health,
            });

            setup_tray(app, trigger_engine)?;
            Ok(())
        })
        .on_window_event(|window, event| {
            // Close hides — the app lives in the tray, quitting is explicit.
            if let WindowEvent::CloseRequested { api, .. } = event {
                let _ = window.hide();
                api.prevent_close();
            }
        })
        .invoke_handler(tauri::generate_handler![
            commands::get_status,
            commands::list_candidates,
            commands::adopt_device,
            commands::forget_device,
            commands::set_manual,
            commands::clear_manual,
            commands::set_brightness,
            commands::get_brightness,
            commands::list_animations,
            commands::save_animation,
            commands::update_animation,
            commands::delete_animation,
            commands::apply_animation,
            commands::set_idle_animation,
            commands::get_idle_animation,
            commands::set_idle_mode,
            commands::get_idle_mode,
            commands::list_triggers,
            commands::list_profiles,
            commands::get_active_profile,
            commands::set_active_profile,
            commands::get_quiet_hours,
            commands::set_quiet_hours,
            commands::save_trigger,
            commands::reorder_triggers,
            commands::delete_trigger,
            commands::list_schedules,
            commands::save_schedule,
            commands::delete_schedule,
            commands::export_config,
            commands::export_diagnostics,
            commands::import_config,
            commands::recent_events,
            commands::clear_events,
            commands::get_active,
            commands::snooze,
            commands::simulate_event,
            commands::set_secret,
            commands::has_secret,
            commands::integration_health,
        ])
        .build(tauri::generate_context!())
        .expect("error while building tauri application")
        .run(|app, event| {
            // Dock icon click while the window is hidden: bring it back.
            #[cfg(target_os = "macos")]
            if let tauri::RunEvent::Reopen { .. } = event {
                if let Some(window) = app.get_webview_window("main") {
                    let _ = window.show();
                    let _ = window.set_focus();
                }
            }
            #[cfg(not(target_os = "macos"))]
            {
                let _ = (app, &event);
            }
        });
}

fn setup_tray(app: &tauri::App, triggers: Arc<TriggerEngine>) -> tauri::Result<()> {
    let open = MenuItem::with_id(app, "open", "Open Luminode", true, None::<&str>)?;
    let snooze30 = MenuItem::with_id(app, "snooze30", "Snooze 30 min", true, None::<&str>)?;
    let snooze_off = MenuItem::with_id(app, "snooze_off", "End snooze", true, None::<&str>)?;
    let autostart = CheckMenuItem::with_id(
        app,
        "autostart",
        "Start at login",
        true,
        app.autolaunch().is_enabled().unwrap_or(false),
        None::<&str>,
    )?;
    let quit = MenuItem::with_id(app, "quit", "Quit Luminode", true, None::<&str>)?;
    let menu = Menu::with_items(
        app,
        &[
            &open,
            &PredefinedMenuItem::separator(app)?,
            &snooze30,
            &snooze_off,
            &PredefinedMenuItem::separator(app)?,
            &autostart,
            &quit,
        ],
    )?;

    TrayIconBuilder::with_id("main")
        .icon(app.default_window_icon().unwrap().clone())
        .menu(&menu)
        .show_menu_on_left_click(true)
        .on_menu_event(move |app, event| match event.id.as_ref() {
            "open" => {
                if let Some(window) = app.get_webview_window("main") {
                    let _ = window.show();
                    let _ = window.set_focus();
                }
            }
            "snooze30" => triggers.snooze(30),
            "snooze_off" => triggers.snooze(0),
            "autostart" => {
                // Toggle, then reflect the *actual* state back onto the
                // menu item (enable/disable can fail, e.g. dev builds).
                let launcher = app.autolaunch();
                let _ = if launcher.is_enabled().unwrap_or(false) {
                    launcher.disable()
                } else {
                    launcher.enable()
                };
                let _ = autostart.set_checked(launcher.is_enabled().unwrap_or(false));
            }
            "quit" => app.exit(0),
            _ => {}
        })
        .build(app)?;
    Ok(())
}
