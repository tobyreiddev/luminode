//! The animation engine — single owner of "what is currently on the strip".
//!
//! A dedicated thread ticks at 30fps. Each tick it renders the current
//! [`AnimSpec`] into a frame:
//!
//! * **Firmware-native effects** (off/solid/breathe/rainbow/chase/sparkle)
//!   are sent
//!   once as a `RUN_EFFECT` command and the Arduino animates them itself —
//!   no serial bandwidth spent, and they keep running if the app hangs.
//! * **App-rendered effects** (flash/gradient/progress, later the keyframe
//!   editor) are streamed as raw frames at the tick rate.
//!
//! Either way the locally rendered frame is emitted to the UI (~10fps) so the
//! simulated strip preview works with no Arduino attached.

use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering};
use std::sync::mpsc::TrySendError;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tauri::Emitter;

use crate::device::DeviceMsg;

// Must match the firmware's NUM_LEDS exactly — the firmware rejects frames
// whose px length isn't NUM_LEDS * 6 hex chars ("bad frame").
pub const NUM_LEDS: usize = 33;

/// One renderable animation. Serialized into animation storage and across
/// the UI bridge, so field names are part of the app's persisted format.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AnimSpec {
    /// off | solid | breathe | rainbow | chase | sparkle | flash |
    /// gradient | progress | dual_progress
    pub effect: String,
    #[serde(default = "default_color")]
    pub color: [u8; 3],
    /// Secondary color: flash off-phase, gradient end, progress background.
    #[serde(default)]
    pub color2: Option<[u8; 3]>,
    /// 0.0 (slowest) .. 1.0 (fastest); same period mapping as the firmware.
    #[serde(default = "default_speed")]
    pub speed: f32,
    /// Only used by the "progress" effect: fraction complete, 0.0..1.0.
    /// Injected from event payloads by the rule engine.
    #[serde(default)]
    pub progress: Option<f32>,
    /// Only used by "dual_progress": the second bar (right half of the
    /// strip), 0.0..1.0. Injected from event payloads like `progress`.
    #[serde(default)]
    pub progress2: Option<f32>,
    /// Only used by "keyframes": color stops the whole strip fades through
    /// over one cycle (wrapping last → first). The v1 keyframe editor.
    #[serde(default)]
    pub keyframes: Option<Vec<[u8; 3]>>,
}

fn default_color() -> [u8; 3] {
    [0, 80, 255]
}
fn default_speed() -> f32 {
    0.3
}

impl Default for AnimSpec {
    fn default() -> Self {
        Self {
            effect: "rainbow".into(),
            color: default_color(),
            color2: None,
            speed: 0.15,
            progress: None,
            progress2: None,
            keyframes: None,
        }
    }
}

impl AnimSpec {
    pub fn validate(&self) -> Result<(), String> {
        const EFFECTS: &[&str] = &[
            "off",
            "solid",
            "breathe",
            "rainbow",
            "chase",
            "sparkle",
            "flash",
            "gradient",
            "progress",
            "dual_progress",
            "keyframes",
        ];
        if !EFFECTS.contains(&self.effect.as_str()) {
            return Err(format!("unknown animation effect: {}", self.effect));
        }
        if !self.speed.is_finite() || !(0.0..=1.0).contains(&self.speed) {
            return Err("animation speed must be between 0 and 1".into());
        }
        for (name, value) in [("progress", self.progress), ("progress2", self.progress2)] {
            if value.is_some_and(|v| !v.is_finite() || !(0.0..=1.0).contains(&v)) {
                return Err(format!("{name} must be between 0 and 1"));
            }
        }
        if let Some(stops) = &self.keyframes {
            if stops.len() > 16 {
                return Err("keyframe animations support at most 16 stops".into());
            }
            if self.effect == "keyframes" && stops.is_empty() {
                return Err("keyframe animations need at least one stop".into());
            }
        } else if self.effect == "keyframes" {
            return Err("keyframe animations need at least one stop".into());
        }
        Ok(())
    }

    pub fn off() -> Self {
        Self {
            effect: "off".into(),
            color: [0, 0, 0],
            color2: None,
            speed: 0.0,
            progress: None,
            progress2: None,
            keyframes: None,
        }
    }

    /// Effects the firmware can run on its own, given the protocol version
    /// it reported in its pong. Proto 1 firmwares only know the original
    /// six; proto 2 runs everything, so steady-state frame streaming (which
    /// wedges macOS's CDC driver — see device.rs) disappears entirely.
    pub fn is_firmware_native(&self, proto: u8) -> bool {
        match self.effect.as_str() {
            "off" | "solid" | "breathe" | "rainbow" | "chase" | "sparkle" => true,
            "flash" | "gradient" | "progress" | "dual_progress" | "keyframes" => proto >= 2,
            _ => false,
        }
    }
}

/// True when only the progress fields moved (a usage gauge ticking along):
/// the effect re-sends without a crossfade, so the bar updates promptly.
fn progress_only_change(a: &AnimSpec, b: &AnimSpec) -> bool {
    a.effect == b.effect
        && a.color == b.color
        && a.color2 == b.color2
        && a.speed == b.speed
        && a.keyframes == b.keyframes
}

/// State shared between the engine thread and the rest of the app.
/// Writers bump the generation counters; the engine thread compares them
/// against what it last sent to know when a re-send is due.
pub struct EngineShared {
    spec: Mutex<(AnimSpec, u64)>,
    pub brightness: AtomicU8,
    brightness_gen: AtomicU64,
    /// Bumped by the device manager on every successful (re)connect so the
    /// engine re-sends the current effect + brightness to the fresh device.
    pub connection_epoch: AtomicU64,
    /// Protocol version from the connected device's pong (set by the device
    /// manager before it bumps `connection_epoch`). Defaults to the current
    /// version optimistically: worst case an old firmware answers new
    /// effect names with an `err` event instead of us streaming at it.
    pub device_proto: AtomicU8,
    pub preview_visible: AtomicBool,
    /// Mirror the animation onto the tray icon (menu-bar strip preview).
    /// Toggled from the tray menu, persisted as the `tray_animation` setting.
    pub tray_preview: AtomicBool,
}

impl EngineShared {
    pub fn new(initial_brightness: u8, tray_preview: bool) -> Self {
        Self {
            spec: Mutex::new((AnimSpec::default(), 1)),
            brightness: AtomicU8::new(initial_brightness),
            brightness_gen: AtomicU64::new(1),
            connection_epoch: AtomicU64::new(0),
            device_proto: AtomicU8::new(2),
            preview_visible: AtomicBool::new(true),
            tray_preview: AtomicBool::new(tray_preview),
        }
    }

    pub fn set_spec(&self, spec: AnimSpec) {
        let mut guard = self.spec.lock().unwrap();
        if guard.0 != spec {
            guard.1 += 1;
            guard.0 = spec;
        }
    }

    pub fn current_spec(&self) -> AnimSpec {
        self.spec.lock().unwrap().0.clone()
    }

    pub fn set_brightness(&self, value: u8) {
        self.brightness.store(value, Ordering::Relaxed);
        self.brightness_gen.fetch_add(1, Ordering::Relaxed);
    }
}

/// Spawn the 30fps engine thread.
pub fn spawn(
    shared: Arc<EngineShared>,
    device_tx: std::sync::mpsc::SyncSender<DeviceMsg>,
    app: tauri::AppHandle,
) {
    std::thread::Builder::new()
        .name("animation-engine".into())
        .spawn(move || run(shared, device_tx, app))
        .expect("failed to spawn animation engine thread");
}

fn run(
    shared: Arc<EngineShared>,
    device_tx: std::sync::mpsc::SyncSender<DeviceMsg>,
    app: tauri::AppHandle,
) {
    const TICK: Duration = Duration::from_millis(33); // ~30fps
    /// Crossfade length when the active spec changes — long enough to kill
    /// the hard cut, short enough that a 2s flash still reads as a flash.
    /// Preview-only: the strip cuts straight to the new effect, because a
    /// 250ms frame burst on every change is exactly the streaming traffic
    /// that wedges macOS's CDC driver (see device.rs).
    const FADE_MS: u64 = 250;
    /// Streamed (non-native) effects re-send an unchanged frame this often
    /// so the firmware's 10s FRAME_TIMEOUT watchdog doesn't fire.
    const FRAME_KEEPALIVE: Duration = Duration::from_secs(5);

    let start = Instant::now();
    let mut last_seen_gen: u64 = 0;
    let mut last_sent_epoch: u64 = u64::MAX;
    let mut last_sent_brightness_gen: u64 = 0;
    let mut tick_count: u64 = 0;
    // Crossfade state: the frame we're fading *from* and when it started.
    let mut fade: Option<(Vec<[u8; 3]>, Instant)> = None;
    let mut last_frame: Vec<[u8; 3]> = vec![[0, 0, 0]; NUM_LEDS];
    let mut prev_spec = shared.current_spec();
    // Whether the device is running the current effect spec. Cleared by
    // spec changes and reconnects, set when an Effect send is accepted.
    let mut effect_synced = false;
    // A progress-only nudge is owed to an already-synced effect. Sent as the
    // tiny single-packet `progress` command: full effect re-sends are
    // multi-packet writes, and those are what kill the CDC pipe (observed:
    // connections die seconds after a gauge-update re-send, never during
    // minutes of small heartbeat traffic).
    let mut progress_dirty = false;
    // Send-on-change state for the streamed (non-native) path.
    let mut last_sent_frame: Option<String> = None;
    let mut last_frame_sent_at = Instant::now();
    // RGBA currently painted on the tray icon; None = the default app icon.
    let mut tray_rgba_shown: Option<Vec<u8>> = None;

    loop {
        let tick_started = Instant::now();
        let (spec, generation) = {
            let guard = shared.spec.lock().unwrap();
            guard.clone()
        };
        let epoch = shared.connection_epoch.load(Ordering::Relaxed);
        let t_ms = start.elapsed().as_millis() as u64;

        if generation != last_seen_gen {
            // Spec changed: fade the preview from whatever was last shown,
            // unless only a progress fraction moved. (At startup last_frame
            // is black, so power-on is a fade-in — a feature.)
            if progress_only_change(&prev_spec, &spec) {
                if effect_synced {
                    progress_dirty = true;
                }
                // Not synced yet: the pending full Effect send carries the
                // new fractions anyway.
            } else {
                fade = Some((last_frame.clone(), Instant::now()));
                effect_synced = false;
            }
            last_seen_gen = generation;
        }
        prev_spec = spec.clone();

        // `target` is the pure render of the current spec (what the device
        // shows); `frame` additionally applies the preview-only crossfade.
        let target = render(&spec, t_ms, NUM_LEDS);
        let frame = match &fade {
            Some((from, started)) => {
                let t = started.elapsed().as_millis() as f32 / FADE_MS as f32;
                if t >= 1.0 {
                    fade = None;
                    target.clone()
                } else {
                    from.iter()
                        .zip(&target)
                        .map(|(a, b)| lerp(*a, *b, t))
                        .collect()
                }
            }
            None => target.clone(),
        };
        last_frame = frame.clone();

        // Preview for the UI at ~10fps (every 3rd tick) to keep IPC cheap.
        // The tray mirror shares the cadence but not the preview_visible
        // gate — the menu bar is exactly the place that should keep
        // animating while the window is hidden.
        if tick_count.is_multiple_of(3) {
            if shared.preview_visible.load(Ordering::Relaxed) {
                let _ = app.emit("engine:frame", frame_to_hex(&frame));
            }
            update_tray(&app, &shared, &frame, &mut tray_rgba_shown);
        }

        if epoch != last_sent_epoch {
            // Fresh (re)connect: re-send everything to the new device.
            effect_synced = false;
            last_sent_frame = None;
            // brightness_gen starts at 1 and only increments, so 0 forces a
            // resend that retries every tick until the send is accepted.
            last_sent_brightness_gen = 0;
        }
        let brightness_gen = shared.brightness_gen.load(Ordering::Relaxed);
        if brightness_gen != last_sent_brightness_gen {
            let value = shared.brightness.load(Ordering::Relaxed);
            if device_tx.try_send(DeviceMsg::Brightness(value)).is_ok() {
                last_sent_brightness_gen = brightness_gen;
            }
        }

        let proto = shared.device_proto.load(Ordering::Relaxed);
        if spec.is_firmware_native(proto) {
            // Hand the effect to the firmware once and stop talking
            // (survives app hangs, no serial traffic in steady state).
            if !effect_synced {
                if device_tx.try_send(DeviceMsg::Effect(spec.clone())).is_ok() {
                    effect_synced = true;
                    progress_dirty = false;
                }
            } else if progress_dirty {
                let a = spec.progress.unwrap_or(0.0);
                let b = spec.progress2.unwrap_or(0.0);
                if device_tx.try_send(DeviceMsg::Progress(a, b)).is_ok() {
                    progress_dirty = false;
                }
            }
        } else {
            // Effect this firmware can't run: stream frames, but only when
            // the frame actually changed (plus a keepalive under the
            // firmware's FRAME_TIMEOUT), and never the preview crossfade —
            // pounding a stalled CDC pipe is what wedges the driver.
            let hex = frame_to_hex(&target);
            if last_sent_frame.as_deref() != Some(hex.as_str())
                || last_frame_sent_at.elapsed() >= FRAME_KEEPALIVE
            {
                // Drop rather than block if the device thread is busy — a
                // stale frame is worthless once the next one exists.
                match device_tx.try_send(DeviceMsg::Frame(hex.clone())) {
                    Ok(()) => {
                        last_sent_frame = Some(hex);
                        last_frame_sent_at = Instant::now();
                    }
                    Err(TrySendError::Full(_)) | Err(TrySendError::Disconnected(_)) => {}
                }
            }
        }
        last_sent_epoch = epoch;

        tick_count += 1;
        let elapsed = tick_started.elapsed();
        if elapsed < TICK {
            std::thread::sleep(TICK - elapsed);
        }
    }
}

// ---------------------------------------------------------------------------
// Tray icon mirror
// ---------------------------------------------------------------------------
// The strip drawn as a horizontal bar in the menu bar: 2px per LED inside a
// 1-2px dark frame, on a transparent canvas. macOS scales tray images to a
// fixed height preserving aspect ratio, so the canvas height (not the bar
// height) controls how wide the icon renders — 70×36 shows as ~35pt wide.

const TRAY_W: usize = NUM_LEDS * 2 + 4;
const TRAY_H: usize = 36;
const TRAY_BAR_H: usize = 10;

fn tray_rgba(frame: &[[u8; 3]]) -> Vec<u8> {
    let mut buf = vec![0u8; TRAY_W * TRAY_H * 4];
    let bar_top = (TRAY_H - TRAY_BAR_H) / 2;
    let border = [58u8, 58, 64, 200];
    for y in (bar_top - 1)..(bar_top + TRAY_BAR_H + 1) {
        for x in 0..TRAY_W {
            let in_bar = y >= bar_top && y < bar_top + TRAY_BAR_H && (2..TRAY_W - 2).contains(&x);
            let px = if in_bar {
                let c = frame[(x - 2) / 2];
                [c[0], c[1], c[2], 255]
            } else {
                border
            };
            let o = (y * TRAY_W + x) * 4;
            buf[o..o + 4].copy_from_slice(&px);
        }
    }
    buf
}

/// Push the current frame onto the tray icon when it changed. An all-black
/// frame (effect off / snoozed) and a disabled toggle both fall back to the
/// default app icon — a black bar in the menu bar reads as a glitch.
fn update_tray(
    app: &tauri::AppHandle,
    shared: &EngineShared,
    frame: &[[u8; 3]],
    shown: &mut Option<Vec<u8>>,
) {
    let desired = if shared.tray_preview.load(Ordering::Relaxed)
        && frame.iter().any(|px| *px != [0, 0, 0])
    {
        Some(tray_rgba(frame))
    } else {
        None
    };
    // The tray may not be built yet during startup — leave `shown` untouched
    // so the next tick retries instead of believing the icon was painted.
    let Some(tray) = app.tray_by_id("main") else {
        return;
    };
    if desired != *shown {
        let icon = match &desired {
            Some(rgba) => Some(tauri::image::Image::new_owned(
                rgba.clone(),
                TRAY_W as u32,
                TRAY_H as u32,
            )),
            None => app.default_window_icon().cloned(),
        };
        let _ = tray.set_icon(icon);
        *shown = desired;
    }
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------
// The firmware-native effects intentionally mirror the math in
// firmware/luminode/luminode.ino so the UI preview matches the physical strip.

/// Cycle period for a given speed; keep in sync with `cyclePeriodMs()` in the
/// firmware (8000ms at speed 0 down to 500ms at speed 1).
fn cycle_period_ms(speed: f32) -> u64 {
    let speed = speed.clamp(0.0, 1.0);
    (8000.0 - 7500.0 * speed) as u64
}

/// Position within the current cycle, 0.0..1.0.
fn phase(t_ms: u64, speed: f32) -> f32 {
    let period = cycle_period_ms(speed);
    (t_ms % period) as f32 / period as f32
}

pub fn render(spec: &AnimSpec, t_ms: u64, n: usize) -> Vec<[u8; 3]> {
    let mut frame = vec![[0u8, 0, 0]; n];
    let ph = phase(t_ms, spec.speed);

    match spec.effect.as_str() {
        "solid" => frame.fill(spec.color),
        "breathe" => {
            let level = ((ph * std::f32::consts::TAU).sin() * 0.5 + 0.5).powf(1.5);
            frame.fill(scale(spec.color, level));
        }
        "rainbow" => {
            for (i, px) in frame.iter_mut().enumerate() {
                let hue = (ph + i as f32 / n as f32) % 1.0;
                *px = hsv_to_rgb(hue, 1.0, 1.0);
            }
        }
        "chase" => {
            let head = (ph * n as f32) as usize % n;
            for t in 0..5usize {
                let idx = (head + n - t) % n;
                frame[idx] = scale(spec.color, 1.0 - t as f32 * 0.22);
            }
        }
        "sparkle" => {
            // Deterministic stand-in for the firmware's random twinkle (the
            // preview must be a pure function of t_ms): each pixel sparks
            // once per hashed time slot, then decays. The firmware fades by
            // fadeToBlackBy(5 + speed*50) at 60fps; 13600/fade ms is a close
            // fit for that exponential's visible lifetime (~2s when slow,
            // ~0.25s at full speed). The per-pixel spark rate tracks the
            // firmware's ignition probability, so preview density matches
            // the strip.
            let speed = spec.speed.clamp(0.0, 1.0);
            let decay_ms = 13600.0 / (5.0 + speed * 50.0);
            let rate = 0.06 + speed * 1.25; // sparks/pixel/sec
            let slot_ms = (1000.0 / rate) as u64;
            for (i, px) in frame.iter_mut().enumerate() {
                // Stagger each pixel's slot boundaries so sparks never align.
                let t = t_ms + splitmix(i as u64) % slot_ms;
                let slot = t / slot_ms;
                let spark_at = splitmix(slot.wrapping_mul(n as u64) + i as u64) % slot_ms;
                let since = t % slot_ms;
                if since >= spark_at {
                    let level = 1.0 - (since - spark_at) as f32 / decay_ms;
                    if level > 0.0 {
                        *px = scale(spec.color, level * level);
                    }
                }
            }
        }
        "flash" => {
            // Hard square wave between color and color2 (default black).
            let on = ph < 0.5;
            let off_color = spec.color2.unwrap_or([0, 0, 0]);
            frame.fill(if on { spec.color } else { off_color });
        }
        "gradient" => {
            // Static gradient color -> color2, slowly rotating around the strip.
            let c2 = spec.color2.unwrap_or([0, 0, 0]);
            let offset = ph * n as f32;
            for (i, px) in frame.iter_mut().enumerate() {
                let pos = ((i as f32 + offset) % n as f32) / n as f32;
                // Mirror so the gradient wraps without a hard seam.
                let t = if pos < 0.5 {
                    pos * 2.0
                } else {
                    (1.0 - pos) * 2.0
                };
                *px = lerp(spec.color, c2, t);
            }
        }
        "progress" => {
            // Fill from LED 0; partial last pixel fades in. Background is
            // color2, dimmed so the bar reads clearly.
            let fraction = spec.progress.unwrap_or(0.0).clamp(0.0, 1.0);
            let filled = fraction * n as f32;
            let background = scale(spec.color2.unwrap_or([30, 30, 30]), 0.25);
            for (i, px) in frame.iter_mut().enumerate() {
                let remain = filled - i as f32;
                *px = if remain >= 1.0 {
                    spec.color
                } else if remain > 0.0 {
                    lerp(background, spec.color, remain)
                } else {
                    background
                };
            }
        }
        "dual_progress" => {
            // Two gauges meeting in the middle: `progress` fills from the
            // left edge toward the center in `color`, `progress2` from the
            // right edge toward the center in `color2`. Built for Claude
            // usage (session | weekly) but generic.
            let background = [10u8, 10, 14];
            let half = n / 2;
            let left = spec.progress.unwrap_or(0.0).clamp(0.0, 1.0) * half as f32;
            let right = spec.progress2.unwrap_or(0.0).clamp(0.0, 1.0) * (n - half) as f32;
            let c2 = spec.color2.unwrap_or(spec.color);
            for (i, px) in frame.iter_mut().enumerate() {
                let (color, remain) = if i < half {
                    (spec.color, left - i as f32)
                } else {
                    // Distance in from the right edge, so the bar grows inward.
                    (c2, right - (n - 1 - i) as f32)
                };
                *px = if remain >= 1.0 {
                    color
                } else if remain > 0.0 {
                    lerp(background, color, remain)
                } else {
                    background
                };
            }
        }
        "keyframes" => {
            // Whole-strip color timeline: fade through the stops in order,
            // wrapping smoothly back to the first. speed sets the cycle.
            if let Some(stops) = spec.keyframes.as_deref().filter(|s| !s.is_empty()) {
                if stops.len() == 1 {
                    frame.fill(stops[0]);
                } else {
                    let pos = ph * stops.len() as f32;
                    let i = pos as usize % stops.len();
                    let t = pos - pos.floor();
                    frame.fill(lerp(stops[i], stops[(i + 1) % stops.len()], t));
                }
            }
        }
        // "off" and anything unknown render black.
        _ => {}
    }
    frame
}

pub fn frame_to_hex(frame: &[[u8; 3]]) -> String {
    let mut s = String::with_capacity(frame.len() * 6);
    for px in frame {
        s.push_str(&format!("{:02x}{:02x}{:02x}", px[0], px[1], px[2]));
    }
    s
}

/// SplitMix64 mix — cheap stateless hash for the sparkle preview.
fn splitmix(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9e37_79b9_7f4a_7c15);
    x = (x ^ (x >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    x ^ (x >> 31)
}

fn scale(c: [u8; 3], level: f32) -> [u8; 3] {
    let level = level.clamp(0.0, 1.0);
    [
        (c[0] as f32 * level) as u8,
        (c[1] as f32 * level) as u8,
        (c[2] as f32 * level) as u8,
    ]
}

pub fn lerp(a: [u8; 3], b: [u8; 3], t: f32) -> [u8; 3] {
    let t = t.clamp(0.0, 1.0);
    [
        (a[0] as f32 + (b[0] as f32 - a[0] as f32) * t) as u8,
        (a[1] as f32 + (b[1] as f32 - a[1] as f32) * t) as u8,
        (a[2] as f32 + (b[2] as f32 - a[2] as f32) * t) as u8,
    ]
}

fn hsv_to_rgb(h: f32, s: f32, v: f32) -> [u8; 3] {
    let h = (h % 1.0) * 6.0;
    let i = h.floor();
    let f = h - i;
    let p = v * (1.0 - s);
    let q = v * (1.0 - s * f);
    let t = v * (1.0 - s * (1.0 - f));
    let (r, g, b) = match i as i32 % 6 {
        0 => (v, t, p),
        1 => (q, v, p),
        2 => (p, v, t),
        3 => (p, q, v),
        4 => (t, p, v),
        _ => (v, p, q),
    };
    [(r * 255.0) as u8, (g * 255.0) as u8, (b * 255.0) as u8]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validation_rejects_untrusted_values() {
        assert!(AnimSpec {
            effect: "shell".into(),
            ..AnimSpec::default()
        }
        .validate()
        .is_err());
        assert!(AnimSpec {
            speed: f32::NAN,
            ..AnimSpec::default()
        }
        .validate()
        .is_err());
        assert!(AnimSpec {
            progress: Some(2.0),
            ..AnimSpec::default()
        }
        .validate()
        .is_err());
    }

    #[test]
    fn keyframes_are_bounded() {
        let spec = AnimSpec {
            effect: "keyframes".into(),
            keyframes: Some(vec![[1, 2, 3]; 17]),
            ..AnimSpec::default()
        };
        assert!(spec.validate().is_err());
    }

    #[test]
    fn every_supported_effect_renders_requested_led_count() {
        for effect in [
            "off",
            "solid",
            "breathe",
            "rainbow",
            "chase",
            "sparkle",
            "flash",
            "gradient",
            "progress",
            "dual_progress",
            "keyframes",
        ] {
            let spec = AnimSpec {
                effect: effect.into(),
                keyframes: (effect == "keyframes").then_some(vec![[0, 0, 0], [255, 0, 0]]),
                ..AnimSpec::default()
            };
            assert_eq!(render(&spec, 1234, 33).len(), 33, "{effect}");
        }
    }
}
