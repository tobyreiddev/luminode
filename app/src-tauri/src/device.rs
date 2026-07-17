//! Device manager — owns the serial port and the full connection lifecycle
//! from §3 of the architecture plan:
//!
//! 1. Discovery: scan for USB serial ports matching known Arduino VID/PIDs.
//! 2. Identity: devices are remembered by USB **serial number**, never port
//!    path — the Micro (Leonardo-class USB) can re-enumerate under a new
//!    path after replug.
//! 3. Auto-reconnect: while disconnected, rescan every 2s; a port matching
//!    the stored serial number is adopted without prompting. If no identity
//!    is stored and exactly one candidate exists, it is probed (PING→PONG)
//!    and adopted. Multiple candidates are surfaced to the UI to pick from.
//! 4. Graceful disconnect: read/write errors flip state to disconnected and
//!    resume discovery; the animation engine keeps rendering previews.
//! 5. Health: PING every 3s; missing PONGs for 10s counts as a hang.
//!
//! Everything runs on one dedicated thread; the rest of the app talks to it
//! through a bounded mpsc channel of [`DeviceMsg`].

use serde::Serialize;
use serialport::{SerialPort, SerialPortType};
use std::io::Read;
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tauri::Emitter;

use crate::animation::{AnimSpec, EngineShared};
use crate::events::{Bus, Event};
use crate::store::Store;

/// USB vendor IDs we consider "could be our Arduino": Arduino LLC,
/// Arduino.org, SparkFun (Pro Micro clones).
const KNOWN_VIDS: [u16; 3] = [0x2341, 0x2A03, 0x1B4F];
const BAUD: u32 = 115_200;
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(3);
const HEARTBEAT_TIMEOUT: Duration = Duration::from_secs(10);
const RESCAN_INTERVAL: Duration = Duration::from_secs(2);
/// A healthy probe resolves in ≤ ~3.5s (300ms settle + 3s pong wait); a
/// worker silent past this is stuck in a kernel open()/close() and gets
/// abandoned so discovery keeps running.
const PROBE_TIMEOUT: Duration = Duration::from_secs(10);
/// Back off re-probing a port whose worker got stuck, so wedged kernel
/// state doesn't accumulate one stranded thread per rescan.
const WEDGED_RETRY: Duration = Duration::from_secs(30);
/// Minimum spacing between writes to the board. The 32U4's CDC RX is known
/// to lock up under back-to-back USB packets on macOS (arduino/
/// ArduinoCore-avr#53), and our deaths correlate with command writes that
/// can land adjacent to a heartbeat ping — never with the pings themselves,
/// which are naturally 3s apart. 50ms is orders of magnitude more than the
/// board needs to drain a packet, and the device thread can afford the nap.
const MIN_WRITE_GAP: Duration = Duration::from_millis(50);
/// No single write may span two full 64-byte USB packets: event-log data
/// (2026-07-18) shows every wedge followed the one command whose line is
/// ≥128 bytes (dual_progress effect, exactly 2×64 — two completely full
/// back-to-back bulk packets into the 32U4's dual-bank receiver), while
/// 65–68-byte writes (full packet + short packet) ran for 38 minutes
/// straight. MIN_WRITE_GAP can't help there — the split happens inside one
/// write_all — so long lines go out in chunks small enough that each is a
/// single short packet, with a pause between chunks so the driver can't
/// coalesce them back into full ones.
const WRITE_CHUNK: usize = 48;
const CHUNK_GAP: Duration = Duration::from_millis(15);

pub enum DeviceMsg {
    /// Raw frame as hex string ("RRGGBB" × led count).
    Frame(String),
    /// Run a firmware-native effect.
    Effect(AnimSpec),
    /// Update just the progress fractions of the running effect (proto 2).
    Progress(f32, f32),
    Brightness(u8),
    /// User picked a specific port in the UI (first-time setup with
    /// multiple candidates).
    Adopt(String),
    /// Drop the stored identity and current connection (UI "forget device").
    Forget,
}

#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DeviceStatus {
    pub connected: bool,
    pub port: Option<String>,
    pub serial_number: Option<String>,
    pub fw_version: Option<String>,
    pub led_count: Option<u32>,
    pub protocol_version: Option<u8>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PortCandidate {
    pub port: String,
    pub serial_number: Option<String>,
    pub product: Option<String>,
}

pub struct DeviceCtx {
    pub status: Arc<Mutex<DeviceStatus>>,
    pub candidates: Arc<Mutex<Vec<PortCandidate>>>,
    pub store: Store,
    pub bus: Bus,
    pub engine: Arc<EngineShared>,
    pub app: tauri::AppHandle,
}

pub fn spawn(ctx: DeviceCtx, rx: Receiver<DeviceMsg>) {
    std::thread::Builder::new()
        .name("device-manager".into())
        .spawn(move || run(ctx, rx))
        .expect("failed to spawn device manager thread");
}

struct Connection {
    port: Box<dyn SerialPort>,
    port_name: String,
    read_buf: String,
    last_ping_sent: Instant,
    last_pong: Instant,
    last_write: Instant,
}

/// Discovery bookkeeping for ports that aren't answering.
#[derive(Default)]
struct ProbeState {
    /// Ports that already produced a probe_failed event — we keep re-probing
    /// them (firmware might get flashed any moment) but only log once, so a
    /// foreign Arduino sitting on USB doesn't fill the event log.
    logged: std::collections::HashSet<String>,
    /// Consecutive probe failures per port, driving the USB-reset remedy.
    fail_counts: std::collections::HashMap<String, u32>,
    /// Ports we've issued a 1200bps reset to and not yet reconnected. Cleared
    /// on a successful adopt, so the remedy re-arms for the *next* mute
    /// episode instead of firing only once per app run — a board that
    /// connects, drops, then goes mute again still gets reset. A genuinely
    /// foreign device never adopts, so its entry never clears: still one
    /// reset, exactly as before.
    touched: std::collections::HashSet<String>,
    /// Ports already reported as wedged (one port_wedged event per episode).
    wedge_logged: std::collections::HashSet<String>,
}

/// A probe worker's report back to the device-manager loop.
struct ProbeResult {
    id: u64,
    port: String,
    outcome: Option<(Connection, serde_json::Value)>,
}

struct InflightProbe {
    id: u64,
    port: String,
    started: Instant,
    user_initiated: bool,
}

/// Runs `probe()` on disposable worker threads. Opening this CDC port can
/// block in the kernel indefinitely — observed live: a wedged close(2) kept
/// the next open(2) stuck for 35+ minutes, which (when probing ran inline)
/// silently killed discovery for the rest of the app run. The loop tracks
/// one probe at a time, abandons workers that miss PROBE_TIMEOUT, and backs
/// off the affected port via `wedged_until`.
struct Prober {
    tx: std::sync::mpsc::Sender<ProbeResult>,
    rx: std::sync::mpsc::Receiver<ProbeResult>,
    inflight: Option<InflightProbe>,
    next_id: u64,
    wedged_until: std::collections::HashMap<String, Instant>,
}

impl Prober {
    fn new() -> Self {
        let (tx, rx) = std::sync::mpsc::channel();
        Self {
            tx,
            rx,
            inflight: None,
            next_id: 0,
            wedged_until: std::collections::HashMap::new(),
        }
    }

    fn start(&mut self, port: &str, user_initiated: bool) {
        let id = self.next_id;
        self.next_id += 1;
        self.inflight = Some(InflightProbe {
            id,
            port: port.to_string(),
            started: Instant::now(),
            user_initiated,
        });
        let tx = self.tx.clone();
        let port = port.to_string();
        std::thread::spawn(move || {
            let outcome = probe(&port);
            let _ = tx.send(ProbeResult { id, port, outcome });
        });
    }

    fn wedged(&self, port: &str) -> bool {
        self.wedged_until
            .get(port)
            .is_some_and(|until| Instant::now() < *until)
    }
}

fn run(ctx: DeviceCtx, rx: Receiver<DeviceMsg>) {
    let mut conn: Option<Connection> = None;
    let mut last_scan = Instant::now() - RESCAN_INTERVAL;
    let mut probes = ProbeState::default();
    let mut prober = Prober::new();

    loop {
        // Wait briefly for work; timeout keeps the read/heartbeat/scan loops
        // ticking even when nobody is sending commands.
        match rx.recv_timeout(Duration::from_millis(15)) {
            Ok(msg) => handle_msg(&ctx, &mut conn, &mut prober, msg),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return,
        }
        // Drain anything queued behind the first message, keeping only the
        // newest frame — stale frames are dropped, commands all apply.
        let mut latest_frame: Option<String> = None;
        while let Ok(msg) = rx.try_recv() {
            if let DeviceMsg::Frame(hex) = msg {
                latest_frame = Some(hex);
            } else {
                handle_msg(&ctx, &mut conn, &mut prober, msg);
            }
        }
        if let Some(hex) = latest_frame {
            handle_msg(&ctx, &mut conn, &mut prober, DeviceMsg::Frame(hex));
        }

        // Probe workers report here, even long-abandoned ones.
        while let Ok(res) = prober.rx.try_recv() {
            handle_probe_result(&ctx, &mut conn, &mut probes, &mut prober, res);
        }
        // Abandon a worker stuck in a kernel open()/close() and back off its
        // port; discovery of other ports carries on.
        if let Some(p) = &prober.inflight {
            if p.started.elapsed() >= PROBE_TIMEOUT {
                let port = p.port.clone();
                prober.inflight = None;
                prober
                    .wedged_until
                    .insert(port.clone(), Instant::now() + WEDGED_RETRY);
                if probes.wedge_logged.insert(port.clone()) {
                    let _ = ctx.bus.send(Event::new(
                        "device",
                        "port_wedged",
                        serde_json::json!({
                            "port": port,
                            "hint": "kernel serial open() stuck; if this persists, unplug and replug the USB cable",
                        }),
                    ));
                }
            }
        }

        if let Some(c) = conn.as_mut() {
            // Pump incoming lines (pong/err) and run the heartbeat.
            let healthy = pump_reads(&ctx, c) && heartbeat(c);
            if !healthy {
                disconnect(&ctx, &mut conn, "device stopped responding");
            }
        } else if prober.inflight.is_none() && last_scan.elapsed() >= RESCAN_INTERVAL {
            last_scan = Instant::now();
            try_discover(&ctx, &mut probes, &mut prober);
        }
    }
}

fn handle_msg(ctx: &DeviceCtx, conn: &mut Option<Connection>, prober: &mut Prober, msg: DeviceMsg) {
    match msg {
        DeviceMsg::Frame(hex) => {
            write_line(ctx, conn, &format!(r#"{{"cmd":"frame","px":"{hex}"}}"#));
        }
        DeviceMsg::Effect(spec) => {
            let mut cmd = serde_json::json!({
                "cmd": "effect",
                "name": spec.effect,
                "color": spec.color,
                "speed": round3(spec.speed),
            });
            if let Some(c2) = spec.color2 {
                cmd["color2"] = serde_json::json!(c2);
            }
            if let Some(p) = spec.progress {
                cmd["progress"] = serde_json::json!(round3(p));
            }
            if let Some(p) = spec.progress2 {
                cmd["progress2"] = serde_json::json!(round3(p));
            }
            if let Some(kf) = &spec.keyframes {
                cmd["kf"] = serde_json::json!(crate::animation::frame_to_hex(kf));
            }
            write_line(ctx, conn, &cmd.to_string());
        }
        DeviceMsg::Progress(a, b) => {
            // Stays under 64 bytes — a single USB packet (see the DeviceMsg
            // variant's rationale in animation.rs).
            write_line(
                ctx,
                conn,
                &format!(
                    r#"{{"cmd":"progress","a":{},"b":{}}}"#,
                    round3(a),
                    round3(b)
                ),
            );
        }
        DeviceMsg::Brightness(value) => {
            write_line(
                ctx,
                conn,
                &format!(r#"{{"cmd":"brightness","value":{value}}}"#),
            );
        }
        DeviceMsg::Adopt(port_name) => {
            disconnect(ctx, conn, "switching device");
            // User insists: lift any wedge backoff and probe now. If a
            // worker is already on that port, just upgrade it to
            // user-initiated (its result will report without dedup).
            prober.wedged_until.remove(&port_name);
            match &mut prober.inflight {
                Some(p) if p.port == port_name => p.user_initiated = true,
                _ => prober.start(&port_name, true),
            }
        }
        DeviceMsg::Forget => {
            ctx.store.clear_device_identity();
            disconnect(ctx, conn, "device forgotten");
        }
    }
}

/// f32→JSON produces 17-digit floats; 3 decimals is plenty for a 33-pixel
/// strip and keeps command lines short (single USB packet where possible).
fn round3(v: f32) -> f64 {
    (v as f64 * 1000.0).round() / 1000.0
}

fn write_line(_ctx: &DeviceCtx, conn: &mut Option<Connection>, line: &str) {
    let Some(c) = conn.as_mut() else { return };
    let mut data = line.as_bytes().to_vec();
    data.push(b'\n');
    let since_last = c.last_write.elapsed();
    if since_last < MIN_WRITE_GAP {
        std::thread::sleep(MIN_WRITE_GAP - since_last);
    }
    // Best-effort: a failed write is NOT a disconnect. While streaming frames
    // at 30fps the board periodically stops draining its USB buffer for a few
    // ms mid-render, so a write stalls past the port timeout and returns
    // TimedOut/WouldBlock — the link is fine, that frame is simply dropped and
    // the next one proceeds. Treating write errors as fatal is what turned an
    // ordinary streaming hiccup into a disconnect → close() wedge → replug
    // loop. The read side is the authority on a genuinely gone device: a real
    // unplug makes the next `pump_reads` return a hard error (→ disconnect)
    // within one loop pass, and a mute-but-present board trips the pong
    // timeout in `heartbeat` after HEARTBEAT_TIMEOUT.
    for (i, chunk) in data.chunks(WRITE_CHUNK).enumerate() {
        if i > 0 {
            std::thread::sleep(CHUNK_GAP);
        }
        let _ = c.port.write_all(chunk);
    }
    c.last_write = Instant::now();
}

/// Read whatever is available, handling complete lines. Returns false on a
/// hard I/O error (i.e. the port is gone).
fn pump_reads(ctx: &DeviceCtx, c: &mut Connection) -> bool {
    let mut buf = [0u8; 256];
    loop {
        match c.port.read(&mut buf) {
            Ok(0) => return true,
            Ok(n) => {
                c.read_buf.push_str(&String::from_utf8_lossy(&buf[..n]));
                while let Some(idx) = c.read_buf.find('\n') {
                    let line: String = c.read_buf.drain(..=idx).collect();
                    handle_device_line(ctx, c, line.trim());
                }
                // Defensive: a device spewing garbage without newlines must
                // not grow the buffer forever.
                if c.read_buf.len() > 4096 {
                    c.read_buf.clear();
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::TimedOut => return true,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => return true,
            Err(_) => return false,
        }
    }
}

fn handle_device_line(ctx: &DeviceCtx, c: &mut Connection, line: &str) {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
        return;
    };
    match value["evt"].as_str() {
        Some("pong") => c.last_pong = Instant::now(),
        Some("err") => {
            let msg = value["msg"].as_str().unwrap_or("unknown").to_string();
            let _ = ctx.bus.send(Event::new(
                "device",
                "firmware_error",
                serde_json::json!({ "msg": msg }),
            ));
        }
        _ => {}
    }
}

/// Send periodic pings; returns false when pongs stopped coming back.
fn heartbeat(c: &mut Connection) -> bool {
    // Defer the ping (rather than sleep) if another write just went out —
    // same MIN_WRITE_GAP rationale as write_line, and a ping is never urgent.
    if c.last_ping_sent.elapsed() >= HEARTBEAT_INTERVAL && c.last_write.elapsed() >= MIN_WRITE_GAP {
        c.last_ping_sent = Instant::now();
        c.last_write = Instant::now();
        let mut data = br#"{"cmd":"ping"}"#.to_vec();
        data.push(b'\n');
        // Ignore a failed ping write for the same reason as write_line; the
        // pong timeout below is the authority on a dead link.
        let _ = c.port.write_all(&data);
    }
    c.last_pong.elapsed() < HEARTBEAT_TIMEOUT
}

// ---------------------------------------------------------------------------
// Discovery / adoption
// ---------------------------------------------------------------------------

fn list_candidates() -> Vec<PortCandidate> {
    let Ok(ports) = serialport::available_ports() else {
        return Vec::new();
    };
    ports
        .into_iter()
        .filter_map(|p| {
            // macOS lists each device as both /dev/tty.* and /dev/cu.*;
            // cu (call-up) is the right one for host-initiated connections.
            if cfg!(target_os = "macos") && p.port_name.starts_with("/dev/tty.") {
                return None;
            }
            match p.port_type {
                SerialPortType::UsbPort(info) if KNOWN_VIDS.contains(&info.vid) => {
                    Some(PortCandidate {
                        port: p.port_name,
                        serial_number: info.serial_number,
                        product: info.product,
                    })
                }
                _ => None,
            }
        })
        .collect()
}

fn try_discover(ctx: &DeviceCtx, probes: &mut ProbeState, prober: &mut Prober) {
    let candidates = list_candidates();
    *ctx.candidates.lock().unwrap() = candidates.clone();
    // A port that vanished gets a fresh chance to log/count/probe when it
    // returns (deliberately not `touched` — that is once per app run).
    probes
        .logged
        .retain(|port| candidates.iter().any(|c| &c.port == port));
    probes
        .fail_counts
        .retain(|port, _| candidates.iter().any(|c| &c.port == port));
    probes
        .wedge_logged
        .retain(|port| candidates.iter().any(|c| &c.port == port));
    prober
        .wedged_until
        .retain(|port, _| candidates.iter().any(|c| &c.port == port));

    if candidates.is_empty() {
        return;
    }

    let stored_serial = ctx.store.device_serial_number();
    let pick = match &stored_serial {
        // Known device: only ever auto-connect to the matching serial number.
        Some(serial) => candidates
            .iter()
            .find(|c| c.serial_number.as_deref() == Some(serial.as_str())),
        // First-time setup: adopt a lone candidate; with several, let the
        // user choose in the UI (device:candidates event).
        None => {
            if candidates.len() == 1 {
                candidates.first()
            } else {
                let _ = ctx.app.emit("device:candidates", &candidates);
                None
            }
        }
    };

    if let Some(candidate) = pick {
        if !prober.wedged(&candidate.port) {
            prober.start(&candidate.port, false);
        }
    }
}

fn handle_probe_result(
    ctx: &DeviceCtx,
    conn: &mut Option<Connection>,
    probes: &mut ProbeState,
    prober: &mut Prober,
    res: ProbeResult,
) {
    let user_initiated = match &prober.inflight {
        Some(p) if p.id == res.id => {
            let user = p.user_initiated;
            prober.inflight = None;
            user
        }
        // A previously abandoned worker finally reported: its port isn't
        // wedged (anymore), and a late success is still a perfectly good
        // connection — adopt it below if we're not already connected.
        _ => false,
    };
    prober.wedged_until.remove(&res.port);
    probes.wedge_logged.remove(&res.port);

    match res.outcome {
        Some(pair) => {
            if conn.is_some() {
                // Already connected meanwhile (e.g. a user Adopt raced a
                // late auto-probe): close the surplus port off-thread, like
                // disconnect() does — its close(2) may block for minutes.
                std::thread::spawn(move || drop(pair));
                return;
            }
            probes.logged.remove(&res.port);
            probes.fail_counts.remove(&res.port);
            // Re-arm the reset remedy: this device just proved it's ours,
            // so if it goes mute again later it should be reset again.
            probes.touched.remove(&res.port);
            adopt(ctx, conn, pair);
        }
        None => {
            // User-initiated probes always report, no dedup.
            if user_initiated || probes.logged.insert(res.port.clone()) {
                let _ = ctx.bus.send(Event::new(
                    "device",
                    "probe_failed",
                    serde_json::json!({ "port": res.port }),
                ));
            }
            let count = probes.fail_counts.entry(res.port.clone()).or_insert(0);
            *count += 1;
            // Observed on macOS 26: after the host closes and reopens the
            // CDC port, the 32U4 sometimes goes mute (writes vanish, no
            // replies) until the MCU is reset. Remedy: the standard
            // 1200bps "touch" — the board reboots and re-enumerates with
            // working CDC. Once per port per run keeps this from
            // harassing a device that's simply not ours.
            if *count >= 3 && probes.touched.insert(res.port.clone()) {
                let _ = ctx.bus.send(Event::new(
                    "device",
                    "usb_reset",
                    serde_json::json!({ "port": res.port }),
                ));
                reset_via_1200bps_touch(&res.port);
            }
        }
    }
}

/// Open at 1200 baud, drop DTR, close — the Leonardo-class reset signal.
/// The board vanishes for ~8s (bootloader) and re-enumerates fresh.
///
/// Runs on a detached thread: opening and (especially) closing this flaky CDC
/// port can each block in the kernel for many seconds — the same close(2)
/// pathology handled in `disconnect`. Doing it inline stalled the whole
/// discovery loop, so probes and further reset attempts came minutes late
/// (which in turn made device events look like they'd stopped logging). Fire
/// and forget; the next rescan picks up the re-enumerated board.
fn reset_via_1200bps_touch(port_name: &str) {
    let port_name = port_name.to_string();
    std::thread::spawn(move || {
        if let Ok(mut port) = serialport::new(&port_name, 1200)
            .timeout(Duration::from_millis(200))
            .open()
        {
            let _ = port.write_data_terminal_ready(false);
            // Dropping `port` closes it; the bootloader triggers on close.
        }
    });
}

/// Open a port and confirm it speaks our protocol (PING → PONG within 3s).
fn probe(port_name: &str) -> Option<(Connection, serde_json::Value)> {
    let mut port = serialport::new(port_name, BAUD)
        .timeout(Duration::from_millis(50))
        .open()
        .ok()?;

    // Give CDC a moment to settle after open, then flush any stale bytes.
    std::thread::sleep(Duration::from_millis(300));
    let _ = port.clear(serialport::ClearBuffer::All);

    port.write_all(b"{\"cmd\":\"ping\"}\n").ok()?;

    let deadline = Instant::now() + Duration::from_secs(3);
    let mut acc = String::new();
    let mut buf = [0u8; 256];
    while Instant::now() < deadline {
        match port.read(&mut buf) {
            Ok(n) if n > 0 => {
                acc.push_str(&String::from_utf8_lossy(&buf[..n]));
                for line in acc.split('\n') {
                    if let Ok(v) = serde_json::from_str::<serde_json::Value>(line.trim()) {
                        if v["evt"] == "pong" {
                            let now = Instant::now();
                            return Some((
                                Connection {
                                    port,
                                    port_name: port_name.to_string(),
                                    read_buf: String::new(),
                                    last_ping_sent: now,
                                    last_pong: now,
                                    last_write: now,
                                },
                                v,
                            ));
                        }
                    }
                }
            }
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::TimedOut => {}
            Err(_) => return None,
        }
    }
    None
}

fn adopt(
    ctx: &DeviceCtx,
    conn: &mut Option<Connection>,
    (c, pong): (Connection, serde_json::Value),
) {
    let serial_number = list_candidates()
        .iter()
        .find(|cand| cand.port == c.port_name)
        .and_then(|cand| cand.serial_number.clone());
    let fw = pong["fw"].as_str().unwrap_or("unknown").to_string();
    let led_count = pong["leds"].as_u64().unwrap_or(0) as u32;
    let proto = pong["proto"].as_u64().unwrap_or(1).min(u8::MAX as u64) as u8;

    ctx.store
        .save_device_identity(serial_number.as_deref(), &c.port_name, led_count, &fw);

    {
        let mut status = ctx.status.lock().unwrap();
        status.connected = true;
        status.port = Some(c.port_name.clone());
        status.serial_number = serial_number;
        status.fw_version = Some(fw);
        status.led_count = Some(led_count);
        status.protocol_version = Some(proto);
    }
    *conn = Some(c);

    // Publish the protocol version first: the engine reads it after seeing
    // the epoch change, and it gates native-effect vs frame-stream sending.
    ctx.engine
        .device_proto
        .store(proto, std::sync::atomic::Ordering::Relaxed);
    // A new epoch makes the animation engine re-send effect + brightness.
    ctx.engine
        .connection_epoch
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);

    emit_status(ctx);
    let _ = ctx.bus.send(Event::new(
        "device",
        "connected",
        serde_json::to_value(&*ctx.status.lock().unwrap()).unwrap_or_default(),
    ));
}

fn disconnect(ctx: &DeviceCtx, conn: &mut Option<Connection>, reason: &str) {
    let Some(c) = conn.take() else {
        return;
    };
    // Closing the CDC tty can block indefinitely in the kernel on macOS: a
    // hung close(2) has been observed wedging this whole thread for minutes,
    // freezing discovery/reconnect and every effect+frame write, so the strip
    // sits on the firmware watchdog fallback while the UI still says
    // "connected". Hand the port to a detached thread to close on its own
    // time; the device manager moves on immediately.
    std::thread::spawn(move || drop(c));
    {
        let mut status = ctx.status.lock().unwrap();
        status.connected = false;
        status.port = None;
        status.fw_version = None;
        status.protocol_version = None;
    }
    emit_status(ctx);
    let _ = ctx.bus.send(Event::new(
        "device",
        "disconnected",
        serde_json::json!({ "reason": reason }),
    ));
}

fn emit_status(ctx: &DeviceCtx) {
    let status = ctx.status.lock().unwrap().clone();
    let _ = ctx.app.emit("device:status", &status);
}
