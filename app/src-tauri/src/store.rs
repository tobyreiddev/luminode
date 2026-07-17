//! SQLite persistence: device identity, animations, triggers, settings, and
//! a rolling event log (the data behind "why did it turn red?").
//!
//! Vocabulary (matches the UI): an **animation** is a named visual (effect +
//! colors + speed); a **trigger** maps a bus event onto an animation with a
//! priority and optional expiry. Early versions called these "presets" and
//! "rules" — `migrate_legacy_names` renames old databases in place.
//!
//! One connection behind a mutex is plenty at this write rate. The database
//! lives in the platform app-data dir (macOS:
//! `~/Library/Application Support/com.luminode.app/luminode.db`).

use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::sync::{Arc, Mutex};

use crate::animation::AnimSpec;
use crate::events::Event;

#[derive(Clone)]
pub struct Store(Arc<Mutex<Connection>>);

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct KnownDevice {
    pub serial_number: Option<String>,
    pub last_port: String,
    pub led_count: u32,
    pub fw_version: String,
    pub last_seen_ms: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Animation {
    pub id: i64,
    pub name: String,
    pub spec: AnimSpec,
    pub builtin: bool,
    /// Default length when this animation is shown (trigger fires without
    /// its own expiry, manual Apply). None = runs until outranked/released.
    /// The overlay stack returns to whatever is underneath when it ends.
    pub duration_ms: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Trigger {
    #[serde(default)]
    pub id: i64,
    pub name: String,
    /// Event source this trigger listens to, e.g. "cli", "system".
    pub source: String,
    /// Event type that activates the trigger, e.g. "progress".
    pub event_type: String,
    /// Optional event type that deactivates it, e.g. "screen_unlocked".
    /// Triggers without one rely on duration_ms (or stay active forever).
    pub clear_event_type: Option<String>,
    pub animation_id: i64,
    /// Higher wins. The UI derives this from list order (drag to reorder →
    /// `reorder_triggers`); manual control sits above every trigger.
    pub priority: i32,
    /// Auto-expire after this long; refreshed each time the trigger re-fires.
    pub duration_ms: Option<i64>,
    pub enabled: bool,
    #[serde(default)]
    pub policy: TriggerPolicy,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TriggerPolicy {
    #[serde(default = "default_profile")]
    pub profile: String,
    #[serde(default)]
    pub payload_path: Option<String>,
    #[serde(default)]
    pub payload_equals: Option<serde_json::Value>,
    #[serde(default)]
    pub cooldown_ms: Option<i64>,
}

impl Default for TriggerPolicy {
    fn default() -> Self {
        Self {
            profile: default_profile(),
            payload_path: None,
            payload_equals: None,
            cooldown_ms: None,
        }
    }
}

fn default_profile() -> String {
    "Default".into()
}

/// A clock-driven action, checked once a minute (`sources/schedule.rs`):
/// `action: "emit"` puts `time/<event_type>` on the bus (pair it with a
/// trigger to show an animation at a time of day); `action: "idle"` swaps
/// the idle animation. Daily, local time.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Schedule {
    #[serde(default)]
    pub id: i64,
    pub name: String,
    /// "HH:MM", 24h, local timezone.
    pub time: String,
    /// "emit" | "idle"
    pub action: String,
    pub event_type: Option<String>,
    pub animation_id: Option<i64>,
    pub enabled: bool,
}

pub struct ImportAnimation {
    pub name: String,
    pub spec: AnimSpec,
    pub duration_ms: Option<i64>,
}
pub struct ImportTrigger {
    pub name: String,
    pub source: String,
    pub event_type: String,
    pub clear_event_type: Option<String>,
    pub animation: String,
    pub priority: i32,
    pub duration_ms: Option<i64>,
    pub enabled: bool,
    pub policy: TriggerPolicy,
}
pub struct ImportSchedule {
    pub name: String,
    pub time: String,
    pub action: String,
    pub event_type: Option<String>,
    pub animation: Option<String>,
    pub enabled: bool,
}
pub struct ImportBundle {
    pub animations: Vec<ImportAnimation>,
    pub triggers: Vec<ImportTrigger>,
    pub schedules: Vec<ImportSchedule>,
    pub idle_animation: Option<String>,
    pub idle_mode: Option<String>,
}

impl Store {
    pub fn import_bundle(&self, bundle: &ImportBundle) -> rusqlite::Result<(usize, usize, usize)> {
        let mut conn = self.0.lock().unwrap();
        let tx = conn.transaction()?;
        tx.execute_batch(
            "DROP TABLE IF EXISTS import_undo_animations;
             DROP TABLE IF EXISTS import_undo_triggers;
             DROP TABLE IF EXISTS import_undo_schedules;
             DROP TABLE IF EXISTS import_undo_settings;
             CREATE TABLE import_undo_animations AS SELECT * FROM animations;
             CREATE TABLE import_undo_triggers AS SELECT * FROM triggers;
             CREATE TABLE import_undo_schedules AS SELECT * FROM schedules;
             CREATE TABLE import_undo_settings AS SELECT * FROM settings;",
        )?;
        for a in &bundle.animations {
            tx.execute(
                "INSERT INTO animations(name, spec, builtin, duration_ms) VALUES (?1, ?2, 0, ?3)
                 ON CONFLICT(name) DO UPDATE SET spec=excluded.spec, duration_ms=excluded.duration_ms",
                params![a.name, serde_json::to_string(&a.spec).unwrap(), a.duration_ms],
            )?;
        }
        let animation_id = |name: &str| -> rusqlite::Result<i64> {
            tx.query_row(
                "SELECT id FROM animations WHERE name=?1",
                params![name],
                |row| row.get(0),
            )
        };
        let mut trigger_count = 0;
        for t in &bundle.triggers {
            let Ok(animation_id) = animation_id(&t.animation) else {
                continue;
            };
            let existing: Option<i64> = tx
                .query_row(
                    "SELECT id FROM triggers WHERE name=?1 LIMIT 1",
                    params![t.name],
                    |row| row.get(0),
                )
                .ok();
            let policy = serde_json::to_string(&t.policy).unwrap_or_else(|_| "{}".into());
            if let Some(id) = existing {
                tx.execute("UPDATE triggers SET source=?2,event_type=?3,clear_event_type=?4,animation_id=?5,priority=?6,duration_ms=?7,enabled=?8,policy=?9 WHERE id=?1",
                    params![id,t.source,t.event_type,t.clear_event_type,animation_id,t.priority,t.duration_ms,t.enabled as i64,policy])?;
            } else {
                tx.execute("INSERT INTO triggers(name,source,event_type,clear_event_type,animation_id,priority,duration_ms,enabled,policy) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)",
                    params![t.name,t.source,t.event_type,t.clear_event_type,animation_id,t.priority,t.duration_ms,t.enabled as i64,policy])?;
            }
            trigger_count += 1;
        }
        for s in &bundle.schedules {
            let animation_id = s
                .animation
                .as_deref()
                .and_then(|name| animation_id(name).ok());
            let existing: Option<i64> = tx
                .query_row(
                    "SELECT id FROM schedules WHERE name=?1 LIMIT 1",
                    params![s.name],
                    |row| row.get(0),
                )
                .ok();
            if let Some(id) = existing {
                tx.execute("UPDATE schedules SET time=?2,action=?3,event_type=?4,animation_id=?5,enabled=?6 WHERE id=?1",
                    params![id,s.time,s.action,s.event_type,animation_id,s.enabled as i64])?;
            } else {
                tx.execute("INSERT INTO schedules(name,time,action,event_type,animation_id,enabled) VALUES (?1,?2,?3,?4,?5,?6)",
                    params![s.name,s.time,s.action,s.event_type,animation_id,s.enabled as i64])?;
            }
        }
        if let Some(name) = &bundle.idle_animation {
            if let Ok(id) = animation_id(name) {
                tx.execute("INSERT INTO settings(key,value) VALUES ('idle_animation_id',?1) ON CONFLICT(key) DO UPDATE SET value=excluded.value", params![id.to_string()])?;
            }
        }
        if let Some(mode) = bundle
            .idle_mode
            .as_deref()
            .filter(|m| *m == "animation" || *m == "claude_usage")
        {
            tx.execute("INSERT INTO settings(key,value) VALUES ('idle_mode',?1) ON CONFLICT(key) DO UPDATE SET value=excluded.value", params![mode])?;
        }
        tx.commit()?;
        Ok((
            bundle.animations.len(),
            trigger_count,
            bundle.schedules.len(),
        ))
    }

    pub fn can_undo_import(&self) -> bool {
        self.0
            .lock()
            .unwrap()
            .query_row(
                "SELECT 1 FROM sqlite_master WHERE type='table' AND name='import_undo_animations'",
                [],
                |_| Ok(()),
            )
            .is_ok()
    }

    pub fn undo_import(&self) -> rusqlite::Result<()> {
        let mut conn = self.0.lock().unwrap();
        let tx = conn.transaction()?;
        tx.execute_batch(
            "DELETE FROM triggers;
             DELETE FROM schedules;
             DELETE FROM animations;
             DELETE FROM settings;
             INSERT INTO animations SELECT * FROM import_undo_animations;
             INSERT INTO triggers SELECT * FROM import_undo_triggers;
             INSERT INTO schedules SELECT * FROM import_undo_schedules;
             INSERT INTO settings SELECT * FROM import_undo_settings;
             DROP TABLE import_undo_animations;
             DROP TABLE import_undo_triggers;
             DROP TABLE import_undo_schedules;
             DROP TABLE import_undo_settings;",
        )?;
        tx.commit()
    }

    pub fn open(path: &Path) -> rusqlite::Result<Self> {
        let conn = Connection::open(path)?;
        migrate_legacy_names(&conn)?;
        // Additive column migrations (no-ops on fresh DBs, which get the
        // column from CREATE TABLE below — ALTER first so old DBs catch up).
        add_column_if_missing(&conn, "animations", "duration_ms", "INTEGER");
        add_column_if_missing(&conn, "triggers", "policy", "TEXT NOT NULL DEFAULT '{}'");
        conn.execute_batch(
            "
            PRAGMA foreign_keys = ON;
            PRAGMA busy_timeout = 5000;
            PRAGMA journal_mode = WAL;
            CREATE TABLE IF NOT EXISTS settings (
                key   TEXT PRIMARY KEY,
                value TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS device (
                id            INTEGER PRIMARY KEY CHECK (id = 1),
                serial_number TEXT,
                last_port     TEXT,
                led_count     INTEGER,
                fw_version    TEXT,
                last_seen_ms  INTEGER
            );
            CREATE TABLE IF NOT EXISTS devices (
                identity      TEXT PRIMARY KEY,
                serial_number TEXT,
                last_port     TEXT NOT NULL,
                led_count     INTEGER NOT NULL,
                fw_version    TEXT NOT NULL,
                last_seen_ms  INTEGER NOT NULL
            );
            CREATE TABLE IF NOT EXISTS animations (
                id          INTEGER PRIMARY KEY AUTOINCREMENT,
                name        TEXT NOT NULL UNIQUE,
                spec        TEXT NOT NULL,
                builtin     INTEGER NOT NULL DEFAULT 0,
                duration_ms INTEGER
            );
            CREATE TABLE IF NOT EXISTS schedules (
                id           INTEGER PRIMARY KEY AUTOINCREMENT,
                name         TEXT NOT NULL,
                time         TEXT NOT NULL,            -- 'HH:MM' local, daily
                action       TEXT NOT NULL,            -- 'emit' | 'idle'
                event_type   TEXT,                     -- for 'emit'
                animation_id INTEGER REFERENCES animations(id) ON DELETE SET NULL,
                enabled      INTEGER NOT NULL DEFAULT 1
            );
            CREATE TABLE IF NOT EXISTS triggers (
                id               INTEGER PRIMARY KEY AUTOINCREMENT,
                name             TEXT NOT NULL,
                source           TEXT NOT NULL,
                event_type       TEXT NOT NULL,
                clear_event_type TEXT,
                animation_id     INTEGER NOT NULL REFERENCES animations(id) ON DELETE CASCADE,
                priority         INTEGER NOT NULL DEFAULT 10,
                duration_ms      INTEGER,
                enabled          INTEGER NOT NULL DEFAULT 1
                ,policy          TEXT NOT NULL DEFAULT '{}'
            );
            CREATE TABLE IF NOT EXISTS event_log (
                id         INTEGER PRIMARY KEY AUTOINCREMENT,
                ts_ms      INTEGER NOT NULL,
                source     TEXT NOT NULL,
                event_type TEXT NOT NULL,
                payload    TEXT
            );
            CREATE INDEX IF NOT EXISTS idx_event_log_ts ON event_log(ts_ms);
            CREATE TRIGGER IF NOT EXISTS prune_event_log
            AFTER INSERT ON event_log
            WHEN NEW.id % 100 = 0
            BEGIN
                DELETE FROM event_log
                WHERE id <= (SELECT MAX(id) FROM event_log) - 5000;
            END;
            ",
        )?;
        let store = Self(Arc::new(Mutex::new(conn)));
        store.seed_if_new();
        store.seed_claude();
        store.seed_codex();
        store.seed_meetings();
        store.seed_display();
        Ok(store)
    }

    /// First-run seed: built-in animations plus default triggers wiring up
    /// the zero-auth event sources, so the pipeline demonstrably works on
    /// day one. Users can edit or disable all of it.
    fn seed_if_new(&self) {
        if self.setting("seeded").is_some() {
            return;
        }
        // `..AnimSpec::off()` fills the fields a seed doesn't care about, so
        // adding a field to AnimSpec doesn't touch every literal here. The
        // third tuple field is the animation's own default length (flashes
        // are self-limiting; steady states run until outranked).
        let animations: Vec<(&str, AnimSpec, Option<i64>)> = vec![
            ("Off", AnimSpec::off(), None),
            ("Idle Rainbow", AnimSpec::default(), None),
            (
                "Focus Blue",
                AnimSpec {
                    effect: "solid".into(),
                    color: [0, 80, 255],
                    speed: 0.3,
                    ..AnimSpec::off()
                },
                None,
            ),
            (
                "Meeting Red",
                AnimSpec {
                    effect: "solid".into(),
                    color: [255, 30, 0],
                    speed: 0.3,
                    ..AnimSpec::off()
                },
                None,
            ),
            (
                "Night Breathe",
                AnimSpec {
                    effect: "breathe".into(),
                    color: [0, 40, 120],
                    speed: 0.2,
                    ..AnimSpec::off()
                },
                None,
            ),
            (
                "Success Flash",
                AnimSpec {
                    effect: "flash".into(),
                    color: [0, 200, 60],
                    color2: Some([0, 0, 0]),
                    speed: 0.75,
                    ..AnimSpec::off()
                },
                Some(2_000),
            ),
            (
                "Failure Flash",
                AnimSpec {
                    effect: "flash".into(),
                    color: [255, 0, 0],
                    color2: Some([0, 0, 0]),
                    speed: 0.85,
                    ..AnimSpec::off()
                },
                Some(3_000),
            ),
            (
                "Progress Bar",
                AnimSpec {
                    effect: "progress".into(),
                    color: [0, 200, 120],
                    color2: Some([40, 40, 60]),
                    speed: 0.3,
                    progress: Some(0.0),
                    ..AnimSpec::off()
                },
                None,
            ),
        ];
        for (name, spec, duration) in &animations {
            let _ = self.save_animation(name, spec, true, *duration);
        }

        let animation_id = |name: &str| -> i64 {
            self.list_animations()
                .iter()
                .find(|a| a.name == name)
                .map(|a| a.id)
                .unwrap_or(1)
        };
        let triggers = vec![
            Trigger {
                id: 0,
                name: "CLI progress bar".into(),
                source: "cli".into(),
                event_type: "progress".into(),
                clear_event_type: Some("progress_done".into()),
                animation_id: animation_id("Progress Bar"),
                priority: 70,
                duration_ms: Some(120_000),
                enabled: true,
                policy: TriggerPolicy::default(),
            },
            Trigger {
                id: 0,
                name: "Command succeeded".into(),
                source: "cli".into(),
                event_type: "run_succeeded".into(),
                clear_event_type: None,
                animation_id: animation_id("Success Flash"),
                priority: 90,
                duration_ms: Some(4_000),
                enabled: true,
                policy: TriggerPolicy::default(),
            },
            Trigger {
                id: 0,
                name: "Command failed".into(),
                source: "cli".into(),
                event_type: "run_failed".into(),
                clear_event_type: None,
                animation_id: animation_id("Failure Flash"),
                priority: 95,
                duration_ms: Some(6_000),
                enabled: true,
                policy: TriggerPolicy::default(),
            },
            Trigger {
                id: 0,
                name: "Screen locked → lights off".into(),
                source: "system".into(),
                event_type: "screen_locked".into(),
                clear_event_type: Some("screen_unlocked".into()),
                animation_id: animation_id("Off"),
                priority: 80,
                duration_ms: None,
                enabled: true,
                policy: TriggerPolicy::default(),
            },
        ];
        for trigger in triggers {
            let _ = self.save_trigger(&trigger);
        }
        self.set_setting("seeded", "1");
    }

    /// Later seed wave (own flag, so databases created before it exist get
    /// it too): Claude Code integration — the `lightctl claude` bridge emits
    /// claude/active, claude/stopped, and claude/usage {session, weekly}.
    fn seed_claude(&self) {
        if self.setting("seeded_claude").is_some() {
            return;
        }
        let working = self.save_animation(
            "Claude Working",
            // Anthropic clay, breathing: unmistakably "Claude is thinking".
            &AnimSpec {
                effect: "breathe".into(),
                color: [217, 119, 87],
                speed: 0.35,
                ..AnimSpec::off()
            },
            true,
            None,
        );
        let usage = self.save_animation(
            "Claude Usage",
            // Session fills amber from the left, weekly violet from the right.
            &AnimSpec {
                effect: "dual_progress".into(),
                color: [255, 160, 40],
                color2: Some([120, 90, 255]),
                speed: 0.3,
                progress: Some(0.0),
                progress2: Some(0.0),
                ..AnimSpec::off()
            },
            true,
            None,
        );
        let done_id = self
            .list_animations()
            .iter()
            .find(|a| a.name == "Success Flash")
            .map(|a| a.id);

        let mut triggers = Vec::new();
        if let Some(id) = done_id {
            triggers.push(Trigger {
                id: 0,
                name: "Claude finished".into(),
                source: "claude".into(),
                event_type: "stopped".into(),
                clear_event_type: None,
                animation_id: id,
                priority: 85,
                duration_ms: Some(4_000),
                enabled: true,
                policy: TriggerPolicy::default(),
            });
        }
        if let Ok(id) = working {
            triggers.push(Trigger {
                id: 0,
                name: "Claude working".into(),
                source: "claude".into(),
                event_type: "active".into(),
                clear_event_type: Some("stopped".into()),
                animation_id: id,
                priority: 60,
                // Safety net: if the Stop hook never fires (crash, kill -9),
                // don't glow clay forever.
                duration_ms: Some(30 * 60_000),
                enabled: true,
                policy: TriggerPolicy::default(),
            });
        }
        if let Ok(id) = usage {
            triggers.push(Trigger {
                id: 0,
                name: "Claude usage bars".into(),
                source: "claude".into(),
                event_type: "usage".into(),
                clear_event_type: None,
                animation_id: id,
                // Below "working": ambient info that surfaces for a moment
                // once Claude goes quiet, refreshed by each statusline tick.
                priority: 50,
                duration_ms: Some(15_000),
                enabled: true,
                policy: TriggerPolicy::default(),
            });
        }
        for trigger in triggers {
            let _ = self.save_trigger(&trigger);
        }
        self.set_setting("seeded_claude", "1");
    }

    /// Codex command hooks emit codex/active and codex/stopped through
    /// `lightctl codex`. Keep this visually distinct from Claude's clay tone.
    fn seed_codex(&self) {
        if self.setting("seeded_codex").is_some() {
            return;
        }
        let working = self.save_animation(
            "Codex Working",
            &AnimSpec {
                effect: "breathe".into(),
                color: [74, 144, 226],
                speed: 0.42,
                ..AnimSpec::off()
            },
            true,
            None,
        );
        let done_id = self
            .list_animations()
            .iter()
            .find(|a| a.name == "Success Flash")
            .map(|a| a.id);
        if let Ok(animation_id) = working {
            let _ = self.save_trigger(&Trigger {
                id: 0,
                name: "Codex working".into(),
                source: "codex".into(),
                event_type: "active".into(),
                clear_event_type: Some("stopped".into()),
                animation_id,
                priority: 61,
                duration_ms: Some(30 * 60_000),
                enabled: true,
                policy: TriggerPolicy::default(),
            });
        }
        if let Some(animation_id) = done_id {
            let _ = self.save_trigger(&Trigger {
                id: 0,
                name: "Codex finished".into(),
                source: "codex".into(),
                event_type: "stopped".into(),
                clear_event_type: None,
                animation_id,
                priority: 86,
                duration_ms: Some(4_000),
                enabled: true,
                policy: TriggerPolicy::default(),
            });
        }
        self.set_setting("seeded_codex", "1");
    }

    /// Seed wave 3: meeting/call awareness (calendar + mic sources).
    fn seed_meetings(&self) {
        if self.setting("seeded_meetings").is_some() {
            return;
        }
        let meeting_red = self
            .list_animations()
            .iter()
            .find(|a| a.name == "Meeting Red")
            .map(|a| a.id);
        if let Some(id) = meeting_red {
            for (name, source, event_type, clear, priority) in [
                (
                    "In a meeting (calendar)",
                    "calendar",
                    "meeting_started",
                    Some("meeting_ended"),
                    75,
                ),
                (
                    "On a call (mic in use)",
                    "system",
                    "call_started",
                    Some("call_ended"),
                    72,
                ),
            ] {
                let _ = self.save_trigger(&Trigger {
                    id: 0,
                    name: name.into(),
                    source: source.into(),
                    event_type: event_type.into(),
                    clear_event_type: clear.map(Into::into),
                    animation_id: id,
                    priority,
                    duration_ms: None,
                    enabled: true,
                    policy: TriggerPolicy::default(),
                });
            }
        }
        self.set_setting("seeded_meetings", "1");
    }

    /// Seed wave 4: display sleep → dark strip (distinct from screen lock —
    /// a display can sleep without locking).
    fn seed_display(&self) {
        if self.setting("seeded_display").is_some() {
            return;
        }
        if let Some(off) = self.list_animations().iter().find(|a| a.name == "Off") {
            let _ = self.save_trigger(&Trigger {
                id: 0,
                name: "Display asleep → lights off".into(),
                source: "system".into(),
                event_type: "display_slept".into(),
                clear_event_type: Some("display_woke".into()),
                animation_id: off.id,
                priority: 82,
                duration_ms: None,
                enabled: true,
                policy: TriggerPolicy::default(),
            });
        }
        self.set_setting("seeded_display", "1");
    }

    // -- settings -----------------------------------------------------------

    pub fn setting(&self, key: &str) -> Option<String> {
        let conn = self.0.lock().unwrap();
        conn.query_row(
            "SELECT value FROM settings WHERE key = ?1",
            params![key],
            |row| row.get(0),
        )
        .ok()
    }

    pub fn set_setting(&self, key: &str, value: &str) {
        let conn = self.0.lock().unwrap();
        let _ = conn.execute(
            "INSERT INTO settings(key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        );
    }

    // -- device identity ----------------------------------------------------

    pub fn device_serial_number(&self) -> Option<String> {
        let conn = self.0.lock().unwrap();
        conn.query_row("SELECT serial_number FROM device WHERE id = 1", [], |row| {
            row.get::<_, Option<String>>(0)
        })
        .ok()
        .flatten()
    }

    pub fn save_device_identity(
        &self,
        serial_number: Option<&str>,
        port: &str,
        led_count: u32,
        fw_version: &str,
    ) {
        let conn = self.0.lock().unwrap();
        let _ = conn.execute(
            "INSERT INTO device(id, serial_number, last_port, led_count, fw_version, last_seen_ms)
             VALUES (1, ?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(id) DO UPDATE SET
               serial_number = excluded.serial_number,
               last_port     = excluded.last_port,
               led_count     = excluded.led_count,
               fw_version    = excluded.fw_version,
               last_seen_ms  = excluded.last_seen_ms",
            params![
                serial_number,
                port,
                led_count,
                fw_version,
                crate::events::now_ms()
            ],
        );
        let identity = serial_number.unwrap_or(port);
        let _ = conn.execute(
            "INSERT INTO devices(identity,serial_number,last_port,led_count,fw_version,last_seen_ms)
             VALUES (?1,?2,?3,?4,?5,?6) ON CONFLICT(identity) DO UPDATE SET
             serial_number=excluded.serial_number,last_port=excluded.last_port,
             led_count=excluded.led_count,fw_version=excluded.fw_version,last_seen_ms=excluded.last_seen_ms",
            params![identity, serial_number, port, led_count, fw_version, crate::events::now_ms()],
        );
    }

    pub fn known_devices(&self) -> Vec<KnownDevice> {
        let conn = self.0.lock().unwrap();
        let Ok(mut stmt) = conn.prepare("SELECT serial_number,last_port,led_count,fw_version,last_seen_ms FROM devices ORDER BY last_seen_ms DESC") else { return Vec::new() };
        stmt.query_map([], |row| {
            Ok(KnownDevice {
                serial_number: row.get(0)?,
                last_port: row.get(1)?,
                led_count: row.get(2)?,
                fw_version: row.get(3)?,
                last_seen_ms: row.get(4)?,
            })
        })
        .map(|rows| rows.filter_map(Result::ok).collect())
        .unwrap_or_default()
    }

    pub fn clear_device_identity(&self) {
        let conn = self.0.lock().unwrap();
        let _ = conn.execute("DELETE FROM device WHERE id = 1", []);
    }

    // -- animations -----------------------------------------------------------

    pub fn list_animations(&self) -> Vec<Animation> {
        let conn = self.0.lock().unwrap();
        let Ok(mut stmt) = conn.prepare(
            "SELECT id, name, spec, builtin, duration_ms FROM animations
             ORDER BY builtin DESC, name",
        ) else {
            return Vec::new();
        };
        stmt.query_map([], |row| {
            let spec_json: String = row.get(2)?;
            Ok(Animation {
                id: row.get(0)?,
                name: row.get(1)?,
                spec: serde_json::from_str(&spec_json).unwrap_or(AnimSpec::off()),
                builtin: row.get::<_, i64>(3)? != 0,
                duration_ms: row.get(4)?,
            })
        })
        .map(|rows| rows.filter_map(Result::ok).collect())
        .unwrap_or_default()
    }

    pub fn animation(&self, id: i64) -> Option<Animation> {
        let conn = self.0.lock().unwrap();
        conn.query_row(
            "SELECT id, name, spec, builtin, duration_ms FROM animations WHERE id = ?1",
            params![id],
            |row| {
                let spec_json: String = row.get(2)?;
                Ok(Animation {
                    id: row.get(0)?,
                    name: row.get(1)?,
                    spec: serde_json::from_str(&spec_json).unwrap_or_else(|_| AnimSpec::off()),
                    builtin: row.get::<_, i64>(3)? != 0,
                    duration_ms: row.get(4)?,
                })
            },
        )
        .ok()
    }

    pub fn save_animation(
        &self,
        name: &str,
        spec: &AnimSpec,
        builtin: bool,
        duration_ms: Option<i64>,
    ) -> rusqlite::Result<i64> {
        let conn = self.0.lock().unwrap();
        conn.execute(
            "INSERT INTO animations(name, spec, builtin, duration_ms) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(name) DO UPDATE SET
               spec = excluded.spec, duration_ms = excluded.duration_ms",
            params![
                name,
                serde_json::to_string(spec).unwrap(),
                builtin as i64,
                duration_ms
            ],
        )?;
        conn.query_row(
            "SELECT id FROM animations WHERE name = ?1",
            params![name],
            |row| row.get(0),
        )
    }

    /// Rename and/or change an existing animation. Builtins are editable on
    /// purpose — they're seeds, not gospel; only deletion is restricted.
    pub fn update_animation(
        &self,
        id: i64,
        name: &str,
        spec: &AnimSpec,
        duration_ms: Option<i64>,
    ) -> rusqlite::Result<()> {
        let conn = self.0.lock().unwrap();
        conn.execute(
            "UPDATE animations SET name = ?2, spec = ?3, duration_ms = ?4 WHERE id = ?1",
            params![id, name, serde_json::to_string(spec).unwrap(), duration_ms],
        )?;
        Ok(())
    }

    pub fn delete_animation(&self, id: i64) -> rusqlite::Result<()> {
        let mut conn = self.0.lock().unwrap();
        let tx = conn.transaction()?;
        tx.execute("DELETE FROM triggers WHERE animation_id = ?1", params![id])?;
        tx.execute(
            "DELETE FROM animations WHERE id = ?1 AND builtin = 0",
            params![id],
        )?;
        tx.commit()
    }

    // -- triggers ---------------------------------------------------------------

    pub fn list_triggers(&self) -> Vec<Trigger> {
        let conn = self.0.lock().unwrap();
        let Ok(mut stmt) = conn.prepare(
            "SELECT id, name, source, event_type, clear_event_type, animation_id,
                    priority, duration_ms, enabled, policy
             FROM triggers ORDER BY priority DESC, name",
        ) else {
            return Vec::new();
        };
        stmt.query_map([], |row| {
            Ok(Trigger {
                id: row.get(0)?,
                name: row.get(1)?,
                source: row.get(2)?,
                event_type: row.get(3)?,
                clear_event_type: row.get(4)?,
                animation_id: row.get(5)?,
                priority: row.get(6)?,
                duration_ms: row.get(7)?,
                enabled: row.get::<_, i64>(8)? != 0,
                policy: row
                    .get::<_, String>(9)
                    .ok()
                    .and_then(|v| serde_json::from_str(&v).ok())
                    .unwrap_or_default(),
            })
        })
        .map(|rows| rows.filter_map(Result::ok).collect())
        .unwrap_or_default()
    }

    /// Insert (id == 0) or update (id != 0); returns the trigger id.
    pub fn save_trigger(&self, trigger: &Trigger) -> rusqlite::Result<i64> {
        let conn = self.0.lock().unwrap();
        if trigger.id == 0 {
            conn.execute(
                "INSERT INTO triggers(name, source, event_type, clear_event_type,
                                      animation_id, priority, duration_ms, enabled, policy)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                params![
                    trigger.name,
                    trigger.source,
                    trigger.event_type,
                    trigger.clear_event_type,
                    trigger.animation_id,
                    trigger.priority,
                    trigger.duration_ms,
                    trigger.enabled as i64,
                    serde_json::to_string(&trigger.policy).unwrap_or_else(|_| "{}".into())
                ],
            )?;
            Ok(conn.last_insert_rowid())
        } else {
            conn.execute(
                "UPDATE triggers SET name = ?2, source = ?3, event_type = ?4,
                                     clear_event_type = ?5, animation_id = ?6,
                                     priority = ?7, duration_ms = ?8, enabled = ?9, policy = ?10
                 WHERE id = ?1",
                params![
                    trigger.id,
                    trigger.name,
                    trigger.source,
                    trigger.event_type,
                    trigger.clear_event_type,
                    trigger.animation_id,
                    trigger.priority,
                    trigger.duration_ms,
                    trigger.enabled as i64,
                    serde_json::to_string(&trigger.policy).unwrap_or_else(|_| "{}".into())
                ],
            )?;
            Ok(trigger.id)
        }
    }

    /// Reassign priorities from an explicit ordering, first = highest.
    /// Spaced by 10 so events arriving mid-drag never see two triggers tied.
    pub fn reorder_triggers(&self, ids: &[i64]) -> rusqlite::Result<()> {
        let mut conn = self.0.lock().unwrap();
        let tx = conn.transaction()?;
        let n = ids.len() as i32;
        for (i, id) in ids.iter().enumerate() {
            tx.execute(
                "UPDATE triggers SET priority = ?2 WHERE id = ?1",
                params![id, (n - i as i32) * 10],
            )?;
        }
        tx.commit()
    }

    pub fn delete_trigger(&self, id: i64) -> rusqlite::Result<()> {
        let conn = self.0.lock().unwrap();
        conn.execute("DELETE FROM triggers WHERE id = ?1", params![id])?;
        Ok(())
    }

    // -- schedules ------------------------------------------------------------

    pub fn list_schedules(&self) -> Vec<Schedule> {
        let conn = self.0.lock().unwrap();
        let Ok(mut stmt) = conn.prepare(
            "SELECT id, name, time, action, event_type, animation_id, enabled
             FROM schedules ORDER BY time, name",
        ) else {
            return Vec::new();
        };
        stmt.query_map([], |row| {
            Ok(Schedule {
                id: row.get(0)?,
                name: row.get(1)?,
                time: row.get(2)?,
                action: row.get(3)?,
                event_type: row.get(4)?,
                animation_id: row.get(5)?,
                enabled: row.get::<_, i64>(6)? != 0,
            })
        })
        .map(|rows| rows.filter_map(Result::ok).collect())
        .unwrap_or_default()
    }

    /// Insert (id == 0) or update; returns the schedule id.
    pub fn save_schedule(&self, s: &Schedule) -> rusqlite::Result<i64> {
        let conn = self.0.lock().unwrap();
        if s.id == 0 {
            conn.execute(
                "INSERT INTO schedules(name, time, action, event_type, animation_id, enabled)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    s.name,
                    s.time,
                    s.action,
                    s.event_type,
                    s.animation_id,
                    s.enabled as i64
                ],
            )?;
            Ok(conn.last_insert_rowid())
        } else {
            conn.execute(
                "UPDATE schedules SET name = ?2, time = ?3, action = ?4,
                                      event_type = ?5, animation_id = ?6, enabled = ?7
                 WHERE id = ?1",
                params![
                    s.id,
                    s.name,
                    s.time,
                    s.action,
                    s.event_type,
                    s.animation_id,
                    s.enabled as i64
                ],
            )?;
            Ok(s.id)
        }
    }

    pub fn delete_schedule(&self, id: i64) -> rusqlite::Result<()> {
        let conn = self.0.lock().unwrap();
        conn.execute("DELETE FROM schedules WHERE id = ?1", params![id])?;
        Ok(())
    }

    // -- event log ------------------------------------------------------------

    pub fn log_event(&self, ev: &Event) {
        let conn = self.0.lock().unwrap();
        let _ = conn.execute(
            "INSERT INTO event_log(ts_ms, source, event_type, payload) VALUES (?1, ?2, ?3, ?4)",
            params![ev.ts, ev.source, ev.event_type, ev.payload.to_string()],
        );
    }

    pub fn clear_events(&self) -> rusqlite::Result<()> {
        self.0
            .lock()
            .unwrap()
            .execute("DELETE FROM event_log", [])?;
        Ok(())
    }

    pub fn recent_events(&self, limit: u32) -> Vec<Event> {
        let conn = self.0.lock().unwrap();
        let Ok(mut stmt) = conn.prepare(
            "SELECT ts_ms, source, event_type, payload FROM event_log
             ORDER BY id DESC LIMIT ?1",
        ) else {
            return Vec::new();
        };
        stmt.query_map(params![limit], |row| {
            let payload: Option<String> = row.get(3)?;
            Ok(Event {
                ts: row.get(0)?,
                source: row.get(1)?,
                event_type: row.get(2)?,
                payload: payload
                    .and_then(|p| serde_json::from_str(&p).ok())
                    .unwrap_or(serde_json::Value::Null),
            })
        })
        .map(|rows| rows.filter_map(Result::ok).collect())
        .unwrap_or_default()
    }
}

/// ALTER TABLE ADD COLUMN, tolerating "already exists" — SQLite has no
/// IF NOT EXISTS for columns, so probe table_info first.
fn add_column_if_missing(conn: &Connection, table: &str, column: &str, decl: &str) {
    let exists = conn
        .prepare(&format!(
            "SELECT 1 FROM pragma_table_info('{table}') WHERE name = ?1"
        ))
        .and_then(|mut stmt| stmt.query_row(params![column], |_| Ok(())))
        .is_ok();
    if !exists {
        let _ = conn.execute(
            &format!("ALTER TABLE {table} ADD COLUMN {column} {decl}"),
            [],
        );
    }
}

/// Databases created before the animations/triggers vocabulary used tables
/// named `presets` and `rules`. Rename in place so existing data survives.
fn migrate_legacy_names(conn: &Connection) -> rusqlite::Result<()> {
    let has_table = |name: &str| -> bool {
        conn.query_row(
            "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1",
            params![name],
            |_| Ok(()),
        )
        .is_ok()
    };
    if has_table("presets") && !has_table("animations") {
        conn.execute_batch(
            "
            ALTER TABLE presets RENAME TO animations;
            ALTER TABLE rules RENAME TO triggers;
            ALTER TABLE triggers RENAME COLUMN preset_id TO animation_id;
            UPDATE settings SET key = 'idle_animation_id' WHERE key = 'idle_preset_id';
            ",
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn store_seeds_and_queries_animation_directly() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("test.db")).unwrap();
        let animation = store.list_animations().into_iter().next().unwrap();
        assert_eq!(store.animation(animation.id).unwrap().name, animation.name);
    }

    #[test]
    fn deleting_custom_animation_cascades_triggers() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("test.db")).unwrap();
        let id = store
            .save_animation("Temporary", &AnimSpec::default(), false, None)
            .unwrap();
        store
            .save_trigger(&Trigger {
                id: 0,
                name: "Temporary trigger".into(),
                source: "cli".into(),
                event_type: "test".into(),
                clear_event_type: None,
                animation_id: id,
                priority: 1,
                duration_ms: None,
                enabled: true,
                policy: TriggerPolicy::default(),
            })
            .unwrap();
        store.delete_animation(id).unwrap();
        assert!(store.list_triggers().iter().all(|t| t.animation_id != id));
    }
}
