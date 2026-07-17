# Event sources & integrations

Every integration follows one pattern: it is a module under
`app/src-tauri/src/sources/` that pushes uniform events onto the in-process
bus —

```rust
Event { source: "slack", event_type: "status_changed", payload: json!({…}), ts }
```

— and knows **nothing** about lights. What an event does to the strip is
decided entirely by user-editable triggers (see the Triggers section of the
UI and `triggers.rs`). This is the design note from the architecture plan:
priority logic lives in one place, never per-integration.

## Implemented (no credentials needed)

### `codex` — Codex command hooks (via the lightctl socket)

`lightctl codex` reads Codex hook JSON from stdin. `UserPromptSubmit` emits
`codex/active`; `Stop` emits `codex/stopped`. The root README contains the
supported `config.toml` hook blocks and documents the seeded working/finished
animations. The bridge always exits zero so it cannot break an agent turn.

### `cli` — the lightctl socket (`sources/lightctl.rs`)

The app listens on a unix socket
(`~/Library/Application Support/com.luminode.app/lightctl.sock`, override
with `LIGHTCTL_SOCK` on both sides). The `lightctl` binary (cli/lightctl)
writes newline-JSON events to it. Lines may carry an optional `"source"`
(defaults to `"cli"`) — that's how `lightctl claude` emits under the
`claude` source.

| Command | Events emitted |
|---|---|
| `lightctl progress 42` | `cli/progress` `{"percent":42}` |
| `lightctl progress done` | `cli/progress_done` |
| `lightctl event deploy_started '{"env":"prod"}'` | `cli/deploy_started` |
| `lightctl run -- make test` | `cli/run_started`, then `cli/run_succeeded` or `cli/run_failed` `{"command","exit_code","duration_secs"}` |
| `lightctl claude` (stdin bridge) | `claude/active`, `claude/stopped`, `claude/usage` `{"session","weekly"}` — see README "Integrations" for the hooks/statusline wiring |

`lightctl run` passes the wrapped command's exit code through and still runs
the command when the app is down — safe to bake into build scripts. Handy
shell helper:

```sh
# ~/.zshrc — flash the strip when any long command finishes
alias watchme='lightctl run --'
```

### `system` — macOS screen lock (`sources/screenlock.rs`)

Distributed notifications `com.apple.screenIsLocked/Unlocked` →
`system/screen_locked`, `system/screen_unlocked`. No permissions needed.
Windows (WTS session notifications) and Linux (logind DBus `Lock`/`Unlock`)
equivalents belong in the same module behind the same event types.

### `device` — the connection itself

The device manager emits `device/connected`, `device/disconnected`,
`device/probe_failed`, `device/firmware_error`. Useful for a trigger like
"strip just reconnected → brief green blink" (add it in the UI; no code).

### `system` — mic/camera call detection (`sources/call.rs`)

Polls CoreAudio's default input device and every CoreMediaIO camera for
"running somewhere" (any app using mic or camera) every 2 s; no capture, no
permission prompt. Emits `system/call_started` / `system/call_ended` on
transitions.

### `system` — display sleep (`sources/display.rs`)

Polls `CGDisplayIsAsleep` on the main display every 5 s (a display can
sleep without the screen locking). Emits `system/display_slept` /
`system/display_woke`.

### `time` — schedules (`sources/schedule.rs`)

User-defined daily times (Schedules section of the UI, stored in SQLite).
`action: emit` puts `time/<event_type>` on the bus; `action: idle` swaps
the idle animation and announces `time/idle_changed`. The idle action is
the one deliberate exception to "sources never touch light config" — a
setting swap isn't an overlay, so a trigger can't express it.

## Implemented (one secret to paste — see README "Integrations")

### `slack` — status/presence polling (`sources/slack.rs`)

User token (scopes `users.profile:read`, `users:read`) in the keychain as
`slack_token`; loop idles until it exists. Polls `users.profile.get` +
`users.getPresence` every 30 s, emits transitions only:
`slack/status_set {text, emoji}`, `slack/status_cleared`,
`slack/presence_active`, `slack/presence_away`.

### `calendar` — secret iCal URL polling (`sources/calendar.rs`)

ICS URL in the keychain as `calendar_ics_url`. Polls every 2 min, emits
`calendar/meeting_soon {summary, minutes_until}`,
`calendar/meeting_started {summary}`, `calendar/meeting_ended`. Skips
all-day/free/cancelled events; recurring (RRULE) events are not expanded —
the honest v1 limitation, see README.

### `claude` — Claude Code bridge (via the lightctl socket)

Not a module here: Claude Code hooks and the statusline pipe JSON to
`lightctl claude`, which emits over the socket with `source: "claude"`.
Wiring instructions live in the README because they edit
`~/.claude/settings.json`, not this repo.

Besides triggers, `claude/usage` feeds the **"Claude usage" idle mode**
(idle dropdown in the app): when nothing else is active the strip shows a
dual gauge — session usage growing from the left edge, weekly from the
right, each colored green → amber → red by utilization. The gauge renders
from the last usage event received, so it's empty until Claude Code's
statusline first reports in after app start.

## Planned — implementation notes

Common requirements:

1. **Token storage:** OS keychain via the `keyring` crate (or
   `tauri-plugin-stronghold`) — never SQLite or JSON. The settings table may
   hold non-secret config (workspace name, calendar id).
2. **Module shape:** own polling/webhook loop in
   `sources/<name>.rs`, spawned from `lib.rs` only when credentials exist;
   emit events and nothing else.
3. **UI:** a "connect" button per integration that runs the OAuth device
   flow or a localhost redirect flow.

| Source | API | Notes |
|---|---|---|
| Teams presence | MS Graph `/me/presence`, poll | Azure AD app registration + `Presence.Read` delegated scope + device-code flow; most setup friction of the lot. Emit `teams/presence_changed` `{availability}`. Step-by-step in README "Not built yet". |
| New mail | IMAP IDLE (Gmail: app password) | Simpler than the Gmail API for a local app. Emit `mail/new_message` `{from, subject}` with a short-duration flash trigger. |
| Windows/Linux screen lock | WTS session notifications / logind DBus | Same module and event types as macOS (`sources/screenlock.rs`). |

## Testing triggers without any integration

The UI's **Simulate an event** box injects any `{source, type, payload}`
onto the bus, exercising exactly the same path as a real integration.
Triggers can be built and demoed before a single OAuth app exists. From a
terminal, `lightctl event whatever '{"any":"payload"}'` does the same.
