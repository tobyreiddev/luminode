# Roadmap

> **Status 2026-07-12 (later the same day):** most of v0.2/v0.3 shipped —
> ✅ marks below. Notable deltas from the plan: the rainbow bug was
> dropped (turned out to be user error, not a real bug); "trigger
> ergonomics" was superseded by
> **per-animation durations** (an animation carries its own length; a
> higher-priority overlay plays out and the previous state returns — the
> stack was already the interrupt-and-return mechanism); the schedule
> source shipped as **user-defined daily times** (emit event / swap idle)
> rather than sunrise/sunset — sun math remains future work.

Gap analysis + planned direction as of 2026-07-12. Ordering favors (a)
visible quality of what exists over new features, (b) features that exploit
the strip's placement above a monitor. Each item names where it lands in
the codebase; the architecture (bus → triggers → animation → device) is not
expected to change until v0.4's zones.

## v0.2 — polish the core (make what exists feel finished)

| Item | Why / where |
| --- | --- |
| ✅ **Crossfade between overlays** | Done: ~250 ms blend in `animation.rs::run`; fades stream frames even into firmware-native effects, then hand over with one Effect command. |
| ✅ **Gamma correction + APA102 5-bit brightness** | Done via FastLED's `APA102HD` controller in the firmware — gamma + 5-bit HD output for both firmware effects and streamed frames; app stays linear. |
| ✅ **Start at login** | Done: `tauri-plugin-autostart`, "Start at login" check item in the tray menu. |
| ✅→ **Trigger ergonomics** | Superseded: shipped **per-animation durations** instead (animation carries its own length; trigger "Expires" falls back to it; manual Apply respects it; overlay stack returns to the previous state). Test-fire/last-fired buttons remain nice-to-have. |
| ✅ **Export / import** | Done: animations + triggers + schedules + idle as one JSON file, referenced by name; native save/open dialogs; import upserts, never deletes. |

## v0.3 — time, matching, and truthfulness (the model catches up to real life)

| Item | Why / where |
| --- | --- |
| ✅ **Schedule source** | Done as user-defined daily times (`sources/schedule.rs` + `schedules` table + UI section): `emit` → `time/<type>` events for triggers, `idle` → swap the idle animation at HH:MM. Sunrise/sunset presets still future (computable offline from lat/long). |
| **Payload matching in triggers** | `payload_match` JSON column, subset-equality in `on_event`, editor field. Unlocks "Slack emoji 🗓 → Meeting Red" instead of any-status. |
| ✅ **Camera in-use detection** | Done: CoreMediaIO "running somewhere" across all cameras, OR'd with the mic check in `sources/call.rs`. |
| ✅ **Display sleep source** | Done: `sources/display.rs` polls `CGDisplayIsAsleep`; seeded "Display asleep → lights off" trigger. |
| **RRULE expansion** | Recurring meetings are ignored (documented v1 limitation). `rrule` crate in `calendar.rs::parse_ics`; most standups recur, so this matters more than it looks. |
| **Per-animation brightness** | `AnimSpec.brightness: Option<u8>` multiplied into render; night animations shouldn't need the global slider. |

## v0.4 — the monitor-strip release (features only this form factor can do)

| Item | Why / where |
| --- | --- |
| **Zones** | Split the strip into user-defined segments, each running its own overlay stack (left = Claude, middle = status, right = CI). Compositor change in `triggers.rs`/`animation.rs`: recompute per zone, concatenate frames. Winner-takes-all hides concurrent truths; this is the biggest architectural upgrade planned. |
| **Bias lighting / ambilight idle** | ScreenCaptureKit at ~10 fps → average edge colors → streamed frames (needs Screen Recording permission, macOS-only module). The classic reason a strip lives above a monitor; ships as an effect so it can be the idle animation. |
| **Focus timer** | `lightctl focus 25` → draining bar + break flash. Rides existing progress plumbing; pairs perfectly with a zone. |
| **macOS Focus/DND source** | Mirror Focus modes as `system/focus_on|off` events (watch the DND assertions plist, or a Shortcuts automation calling `lightctl event`). |

## v0.5 — ecosystem

- **CI/PR watcher** — native `sources/github.rs` (poll `gh`/REST for review-requested, CI status). Can be prototyped today as a cron script calling `lightctl event` — document before building.
- **MS Teams presence, IMAP new-mail** — manual-implementation guides already in README "Not built yet".
- **Windows/Linux** — screen lock sources, serial already cross-platform; then installers.
- **Auto-update** — `tauri-plugin-updater` once anyone besides the author runs it.
- **Multi-device** — second strip = second device row + per-device zone routing.
- **Audio-reactive mode** — fun, strictly last.

## Explicit non-goals

- Mac App Store distribution (sandbox cannot reach serial devices; Developer ID is the path — see README).
- Cloud/webhook relays: sources poll or listen locally; the app never needs a public endpoint.
- Binary serial protocol: newline-JSON stays until >100 LEDs makes it impossible.
