# Luminode

Ambient LED status light: a Tauri desktop app that drives a 33-LED APA102
strip through an Arduino Micro, turning events from your digital life —
CLI progress, finished builds, screen lock, later calendars and Slack —
into light.

```
┌─────────────────────────────┐
│  Tray / menu-bar UI         │  manual control, animations, triggers, event log
├─────────────────────────────┤
│  Event Bus (in-process)     │  every source emits {source, type, payload, ts}
├─────────────────────────────┤
│  Trigger Engine             │  event → animation, priority-ordered overlays
├─────────────────────────────┤
│  Animation Engine           │  30fps tick; native effects delegated to firmware
├─────────────────────────────┤
│  Device Manager             │  discovery, handshake, reconnect, heartbeat
└─────────────────────────────┘
             │ USB serial, newline-JSON (docs/protocol.md)
        Arduino Micro + APA102×33 (firmware/)
```

## Repository layout

| Path                   | What                                                                             |
| ---------------------- | -------------------------------------------------------------------------------- |
| `firmware/luminode/`   | Arduino sketch (C++, FastLED + ArduinoJson)                                      |
| `app/`                 | Tauri 2 app — SvelteKit/Svelte 5 frontend (`src/`), Rust core (`src-tauri/src/`) |
| `cli/lightctl/`        | Terminal companion: pipe progress/command results into the app                   |
| `docs/protocol.md`     | Serial protocol spec                                                             |
| `docs/integrations.md` | Event-source catalog: what exists, how to add more                               |

Rust code is one cargo workspace rooted here (`Cargo.toml`); the firmware
builds with `arduino-cli`, not cargo.

## Prerequisites

- Rust (stable, via [rustup](https://rustup.rs))
- Node 20+ (frontend tooling)
- [arduino-cli](https://arduino.github.io/arduino-cli/) with
  `arduino:avr` core and libraries: `arduino-cli lib install "FastLED@3.7.8" "ArduinoJson@6.21.5"`
  — FastLED must stay on 3.7.x: 3.10.x's AVR backend ignores the requested
  data/clock pins for SPI chipsets (drives hardware SPI on the ICSP pins
  instead), so the strip on pins 5/6 receives nothing.

## Hardware

Arduino Micro → APA102 strip (APA102 is clocked SPI, so **two** signal wires,
unlike WS2812). Pins are `DATA_PIN`/`CLOCK_PIN` in the sketch — software SPI,
plenty fast at 33 LEDs:

| APA102     | Arduino Micro                                                                                 |
| ---------- | --------------------------------------------------------------------------------------------- |
| DATA (DI)  | digital 5 (`DATA_PIN`)                                                                        |
| CLOCK (CI) | digital 6 (`CLOCK_PIN`)                                                                       |
| GND        | GND (common with the supply)                                                                  |
| 5V         | 5 V supply — USB bus power works out of the box: firmware caps total draw at `POWER_BUDGET_MA` (450 mA). Full white at full brightness wants ≈ 2 A, so for that, use an external supply and raise the budget in the sketch |

## Build & run

```sh
# 1. Firmware — compile and flash (see "Flashing" below for macOS caveat)
arduino-cli compile --fqbn arduino:avr:micro firmware/luminode
arduino-cli upload -p /dev/cu.usbmodemXXXX --fqbn arduino:avr:micro firmware/luminode

# 2. Desktop app
cd app && npm install
npm run tauri dev          # development (hot reload)
npm run tauri build        # release bundle (.app / .dmg)

# 3. CLI (optional but fun)
cargo build --release -p lightctl
cp target/release/lightctl ~/bin/   # or anywhere on PATH
```

The app is tray-first: closing the window hides it; quit from the tray menu.
On macOS it runs as an accessory (no dock icon).

## Distributing to another Mac

Ship a **Developer ID-signed, notarized .dmg** — recipients drag to
Applications and it opens with no Gatekeeper warnings. (Don't bother with
the App Store for this app: the sandbox blocks raw serial-port access.)

One-time setup:

1. **Certificate** — in the [developer portal](https://developer.apple.com/account/resources/certificates/add)
   create a **Developer ID Application** certificate (only the Account
   Holder can; easiest via Xcode → Settings → Accounts → Manage
   Certificates → + → "Developer ID Application"). It must show up in
   `security find-identity -v -p codesigning`.
2. **Notarization credential** — create an app-specific password at
   [appleid.apple.com](https://account.apple.com/account/manage) → Sign-In
   & Security → App-Specific Passwords.

Then build (Tauri signs, notarizes, and staples automatically when these
env vars are present):

```sh
export APPLE_SIGNING_IDENTITY="Developer ID Application: Toby Reid (53QYT47BNT)"
export APPLE_ID="you@example.com"           # your Apple ID email
export APPLE_PASSWORD="xxxx-xxxx-xxxx-xxxx" # the app-specific password
export APPLE_TEAM_ID="53QYT47BNT"

cd app && npm run tauri build
```

Output: `target/release/bundle/dmg/Luminode_<version>_aarch64.dmg`. For a
dmg that also runs on Intel Macs, `rustup target add x86_64-apple-darwin
aarch64-apple-darwin` once, then append `-- --target
universal-apple-darwin` to the build command (output lands under
`target/universal-apple-darwin/`).

Notes:

- Notarization uploads to Apple and usually takes 1–5 minutes; the build
  blocks until it finishes. Check status on failure with
  `xcrun notarytool log`.
- The dmg bundling script drives Finder via AppleScript — it needs a real
  GUI terminal session and may prompt once for Finder automation
  permission. If it keeps failing, the signed+notarized `.app` in
  `target/release/bundle/macos/` is fine to ship zipped:
  `ditto -c -k --keepParent Luminode.app Luminode.zip`.
- `lightctl` is not inside the app bundle. On another machine, copy
  `target/release/lightctl` alongside or build it there — the app works
  fully without it; only terminal integration needs it.

### Flashing caveat (macOS 26 / Tahoe)

On this machine, the Micro's **bootloader** enumerates on USB (VID 0x2341,
PID 0x0037) but macOS never creates its `/dev/cu.*` node, so avrdude can't
reach it and `arduino-cli upload` fails with `butterfly_recv(): programmer
is not responding` (it ends up talking to the running sketch instead — if
you see `Found programmer: Id = "…"` with sketch output in it, this is what
happened). The sketch-mode port works fine; only the bootloader CDC is
affected. Workarounds, in order of preference:

1. Flash from another machine (Linux/older macOS) or a VM with USB
   passthrough.
2. Double-tap the reset button to hold the bootloader open while retrying
   `arduino-cli upload` — if the node ever appears, upload within ~8 s.
3. Flash over ISP with a second Arduino as programmer, bypassing the
   bootloader entirely.

Once our firmware is on the board, day-to-day use never needs the
bootloader — the app talks to the sketch-mode port, which works.

A related quirk: after the host closes and reopens the sketch-mode port
(e.g. the app restarts), the board sometimes goes **mute** — the port node
exists and writes succeed, but nothing ever comes back. The device manager
handles this itself: after 3 consecutive failed probes it performs the
1200 bps reset touch (once per port per run), the board re-enumerates, and
the next probe connects. You'll see a `device/usb_reset` event in the log
when this fires. If you're ever debugging by hand, the same remedy is
`stty -f /dev/cu.usbmodemXXXX 1200` and wait ~10 s.

## Using it

- **Manual control** — pick an effect/color/speed, Apply. Manual control is
  itself a priority-100 overlay; "release" hands the strip back to triggers.
- **Animations** — named visuals (effect + colors + speed). Create them from
  the current manual state ("Save as animation"), from scratch ("+ new"), or
  edit any existing one — built-ins included — with "Try on strip" to
  preview before saving. Effects include two-bar `dual_progress` (two gauges
  meeting in the middle — built for Claude usage) and `keyframes` (the strip
  fades through a list of color stops you edit — the v1 keyframe editor).
  An animation can carry its own **Length** (e.g. Success Flash = 2 s): it
  plays for that long wherever it's shown — trigger fire or manual Apply —
  then falls away and **whatever was running underneath returns**. That
  interrupt-and-return is the overlay stack doing its job, not a special
  case. Transitions crossfade (~250 ms), so nothing ever hard-cuts.
- **Triggers** — `on <source>/<type> [until <type>] show <animation>
[expires after S seconds]`. **Priority is list order: drag rows to
  reorder, higher wins** when several fire at once. Each row has an
  active/inactive toggle — switching one off also kills its live overlay.
  **Idle is pinned at the bottom** as the immovable floor of the stack; its
  dropdown picks what shows when nothing above is active. Manual control
  always outranks every trigger (an explicit click beats an ambient rule).
  A trigger's "Expires" is optional — blank means "use the animation's own
  length". Ships with defaults: CLI progress bar, command succeeded/failed
  flashes, screen locked → off, display asleep → off, Claude Code
  working/finished/usage, meeting/call → red.
- **Schedules** — daily clock actions, local time. Two kinds: **emit** puts
  `time/<type>` on the bus at HH:MM (pair with a trigger: "at 22:00 emit
  `evening`" + "on time/evening show Night Breathe until time/morning"),
  and **swap idle** changes the idle animation at a set time ("at 18:00,
  idle = Night Breathe"). Minutes that pass while the Mac sleeps are
  skipped, not replayed.
- **Why is the light doing that?** — live list of active overlays with the
  winner crowned. This plus the event log is the debugging story.
- **Snooze** — tray menu or header button; forces the strip dark for 30 min
  without touching your triggers. The tray also has **Start at login**
  (registers the app as a launch agent — flip it once and forget the app
  exists, which is the point).
- **Export / Import** — Integrations panel → Config. One JSON file with
  animations, triggers, schedules, and the idle choice; references are by
  *name*, so it restores onto a fresh machine. Import upserts by name and
  never deletes.
- **Simulate an event** — fire any {source, type, payload} at the trigger
  engine without needing the real integration.

From a terminal:

```sh
lightctl progress 42          # LED bar fills to 42%
lightctl progress done        # clears it
lightctl run -- cargo test    # green/red flash by exit code, code passed through
lightctl event my_thing '{"n":1}'   # anything; write a trigger for it
lightctl claude               # stdin bridge for Claude Code (see Integrations)
lightctl codex                # stdin bridge for Codex hooks (see Integrations)
```

## Integrations

Full catalog and module conventions: `docs/integrations.md`. Everything
below emits events onto the bus; what the lights *do* is always a trigger
you can edit, reorder, or switch off in the UI.

### Codex (manual setup required)

`lightctl codex` turns supported Codex lifecycle hooks into
`codex/active` and `codex/stopped` events. Luminode seeds a blue breathing
**Codex Working** animation, a working trigger that clears on Stop, and a
green finished flash.

Build `lightctl`, then add these command hooks to `~/.codex/config.toml`.
Replace the command path if your checkout lives elsewhere:

```toml
[[hooks.UserPromptSubmit]]

[[hooks.UserPromptSubmit.hooks]]
type = "command"
command = "/Users/toby/Workspace/luminode/target/release/lightctl codex"
timeout = 5

[[hooks.Stop]]

[[hooks.Stop.hooks]]
type = "command"
command = "/Users/toby/Workspace/luminode/target/release/lightctl codex"
timeout = 5
```

Codex sends hook JSON to the command on stdin. The bridge always exits zero,
so an unavailable Luminode app cannot interrupt a Codex turn. Restart Codex
after changing its configuration. Remove the two hook blocks to uninstall.

### Claude Code (manual setup required)

`lightctl claude` reads whatever Claude Code pipes to it and tells the two
input shapes apart on its own: **hook** JSON becomes `claude/active`
(UserPromptSubmit) or `claude/stopped` (Stop/SessionEnd); **statusline**
JSON becomes `claude/usage {"session": %, "weekly": %}` from the
`rate_limits` block (Pro/Max subscribers only; also prints
`Opus 4.8 · 5h 42% · 7d 61%` so it doubles as your statusline). Usage
events are emitted only when the rounded percentages change, and the
command always exits 0 — a dead light never breaks Claude Code.

Seeded triggers: **Claude working** (breathing clay while a turn runs),
**Claude finished** (green flash on Stop), **Claude usage bars**
(`dual_progress`: session fills amber from the left edge toward the middle,
weekly fills violet from the right — surfaces for 15 s whenever Claude goes
quiet).

Wire it up by hand — add this to `~/.claude/settings.json` (top level,
alongside `"model"` etc.), with the path pointing at your built binary:

```json
"statusLine": {
  "type": "command",
  "command": "/Users/toby/Workspace/luminode/target/release/lightctl claude"
},
"hooks": {
  "UserPromptSubmit": [
    { "hooks": [{ "type": "command", "command": "/Users/toby/Workspace/luminode/target/release/lightctl claude" }] }
  ],
  "Stop": [
    { "hooks": [{ "type": "command", "command": "/Users/toby/Workspace/luminode/target/release/lightctl claude" }] }
  ],
  "SessionEnd": [
    { "hooks": [{ "type": "command", "command": "/Users/toby/Workspace/luminode/target/release/lightctl claude" }] }
  ]
}
```

Rebuild with `cargo build --release -p lightctl` after pulling changes —
the hooks run the binary at that absolute path. Remove the same keys to
uninstall.

### Slack status/presence (manual setup required)

1. [api.slack.com/apps](https://api.slack.com/apps) → **Create New App** →
   From scratch → any name, your workspace.
2. **OAuth & Permissions** → *User Token Scopes* (not Bot!) → add
   `users.profile:read` and `users:read`.
3. **Install to Workspace**, approve, copy the **User OAuth Token**
   (`xoxp-…`).
4. Paste it into the app's **Integrations** panel → Save. It lands in the
   macOS keychain, never on disk.

Polls every 30 s and emits transitions only: `slack/status_set
{text, emoji}`, `slack/status_cleared`, `slack/presence_active`,
`slack/presence_away`. Add triggers for them in the UI. Caveat: triggers
match source+type, not payload contents, so `status_set` fires for *any*
status — payload matching is a known gap (see roadmap).

### Calendar (manual setup required — no OAuth!)

Uses a **secret iCal URL** instead of a Google/Microsoft app registration:

- **Google:** calendar.google.com → ⚙ Settings → your calendar under
  "Settings for my calendars" → **Integrate calendar** → copy **Secret
  address in iCal format**.
- **Outlook:** Settings → Calendar → **Shared calendars** → Publish a
  calendar → copy the **ICS** link.

Paste into **Integrations** → Save (kept in the keychain — the URL grants
read access to your whole calendar, treat it as a secret). Polls every
2 min; emits `calendar/meeting_soon {summary, minutes_until}` (≤5 min out),
`calendar/meeting_started {summary}`, `calendar/meeting_ended`. Seeded
trigger: meeting → Meeting Red until it ends.

**Limitations:** recurring events (RRULE) are **not expanded** — the feed
ships the rule, not instances, and expanding RFC 5545 correctly is its own
project. Recurring meetings are ignored for now (implementation path: the
`rrule` crate in `sources/calendar.rs::parse_ics`). All-day, "free", and
cancelled events are skipped on purpose. `TZID` times are read in the
Mac's local timezone — correct whenever calendar TZ = machine TZ.

### Mic/camera call detection (no setup)

`sources/call.rs` polls CoreAudio ("default input device running
somewhere") and CoreMediaIO (any camera running) every 2 s — true whenever
*any* app has the mic **or a camera** open, so camera-only meetings with
the mic muted at OS level still count. No mic/camera permission needed
(nothing is captured). Emits `system/call_started` / `system/call_ended`;
seeded trigger shows Meeting Red. Voice memos count as calls — flip the
trigger's toggle if that bothers you.

### Display sleep (no setup)

`sources/display.rs` polls `CGDisplayIsAsleep` every 5 s — distinct from
screen *lock*, since a display can sleep without locking. Emits
`system/display_slept` / `system/display_woke`; seeded trigger turns the
strip off until the display wakes.

### Not built yet — how to add them

Explicit instructions for the remaining catalog; each is a module in
`app/src-tauri/src/sources/` that emits events and touches nothing else
(spawn it in `lib.rs`, copy the shape of `slack.rs`):

- **MS Teams presence** — portal.azure.com → App registrations → New
  (personal + work accounts) → API permissions → Microsoft Graph →
  delegated `Presence.Read` → enable *Allow public client flows* for the
  device-code flow. Module: device-code sign-in, poll
  `graph.microsoft.com/v1.0/me/presence` ~30 s, emit
  `teams/available|busy|do_not_disturb|away` on change. Token in the
  keychain (`secrets.rs`); the friction is entirely Azure-side.
- **New mail** — for Gmail prefer an **app password + IMAP** over the
  Gmail API (no Cloud Console project needed): imap.gmail.com:993, IDLE on
  INBOX, emit `mail/new_message {from, subject}` with a short-duration
  flash trigger. Crates: `imap` + `native-tls` (or `async-imap`).
- **Windows/Linux screen lock** — same module and event types as macOS
  (`sources/screenlock.rs`): Windows `WTSRegisterSessionNotification`
  (WM_WTSSESSION_CHANGE → lock/unlock), Linux logind DBus
  `org.freedesktop.login1.Session` `Lock`/`Unlock` signals via `zbus`.
- **Payload matching in triggers** — the known gap above: add an optional
  `payload_match` JSON column to `triggers`, compare subset-equality in
  `TriggerEngine::on_event`, expose a field in the trigger editor. Then
  "status_emoji == :spiral_calendar_pad: → Meeting Red" becomes possible.
- **Tray icon artwork** — replace the generated placeholder set under
  `app/src-tauri/icons/` (`npx @tauri-apps/cli icon path/to/1024.png`
  regenerates every size from one master).

## Architecture decisions (and why)

Decisions made during initial implementation, 2026-07-11:

- **Tauri 2 + Svelte 5 + SQLite (rusqlite, bundled)** — chosen interactively
  at kickoff. Svelte for the smallest reactive layer over Tauri events;
  SQLite over JSON-store for the queryable event log.
- **Identity by USB serial number, not port path** — the Micro re-enumerates
  with a new path after replug on some OSes (`device.rs`).
- **Newline-JSON serial protocol** — debuggable from any serial monitor;
  binary framing is not worth it at 33 LEDs (`docs/protocol.md`). Frames use
  a hex string because a JSON pixel array doesn't fit in the 32U4's RAM.
- **Firmware owns zero business logic** but implements every effect
  natively (proto 2), so the app sends one small `effect` command per change
  and goes silent — steady-state frame streaming is gone entirely. This is
  load-bearing on macOS 26: sustained streaming wedges the CDC driver
  (writes vanish, replies stop, `close(2)`/`open(2)` hang in the kernel
  until the board is replugged). Frames remain only as a fallback for
  proto-1 firmware and future experiments, sent on-change with a keepalive.
  Both sides implement the same rendering math so the UI preview matches
  the strip.
- **One event bus, one trigger engine** — integrations emit uniform events
  and never touch the lights; priority logic exists in exactly one place
  (`triggers.rs`). The alternative decays into per-source "if meeting AND
  NOT building…" spaghetti.
- **Overlay model** — fired triggers become (priority, expiry) overlays;
  the winner is recomputed on every event, expiry tick, and manual change.
  Manual control and snooze are just special overlays/states in the same
  model, so there's no second code path.
- **Trigger priority is list order** — users drag rows instead of typing
  numbers; `reorder_triggers` writes priorities spaced by 10 in one
  transaction. Manual control sits at `i32::MAX`: once no number is
  user-visible, "an explicit click always wins" is the only rule that
  doesn't surprise. Trigger mutations run through
  `TriggerEngine::sync_with_store`, so disabling/deleting a trigger kills
  its live overlay immediately.
- **Claude Code integration is just another lightctl caller** — hooks and
  the statusline pipe their JSON to `lightctl claude`, which emits ordinary
  bus events (`claude/*`). No Claude-specific code in the app core; the
  socket protocol grew an optional `source` field so bridge events aren't
  conflated with terminal ones.
- **Calendar via secret ICS URL, not OAuth** — one pasted URL replaces a
  Google Cloud project, consent screen, and token refresh. The cost
  (recurring events unexpanded, see Integrations) is a fair v1 trade.
- **Threads**: device manager and animation engine are dedicated OS threads
  (serial I/O is blocking; the render tick wants steady timing); event
  sources and the bus subscriber run on tokio via `tauri::async_runtime`.
  Frames flow through a bounded channel and are _dropped_ under backpressure
  — a stale frame is worse than a skipped one.
- **Secrets policy** (for future integrations): OAuth tokens go in the OS
  keychain (`keyring` crate), never SQLite/JSON — see docs/integrations.md.

## Current status / roadmap

Implemented: firmware + protocol, device lifecycle, manual control UI with
live preview, SQLite persistence, trigger engine with drag-ordered
priorities/expiry/snooze, `lightctl` CLI, event log & overlay debugger,
keyframe editor v1 (whole-strip color timeline), and sources: macOS screen
lock, Claude Code (hooks + statusline bridge), Slack status/presence,
calendar via secret ICS URL, mic-based call detection.

Not yet: see **`docs/roadmap.md`** for the full gap analysis and phased
plan (v0.2 polish → v0.3 time/matching → v0.4 zones + bias lighting →
v0.5 ecosystem). Manual-implementation notes for missing integrations are
under "Integrations → Not built yet" above.

## Development notes

- `cargo check --workspace` and `cd app && npm run check` are the fast
  correctness loops.
- `npm run tauri dev` needs the vite dev server it spawns; plain
  `cargo build -p luminode` embeds whatever is in `app/build` from the last
  `npm run build`.
- App data (SQLite DB, lightctl socket) lives at
  `~/Library/Application Support/com.luminode.app/`. Delete the DB to re-seed
  the default animations/triggers.
- The firmware compiles to ~75% flash / 35% RAM on the 32U4, so there is
  room, but remember frames already push the serial line buffer near its
  240-byte cap — grow `LINE_BUF_SIZE` if you grow `NUM_LEDS`.
