# Luminode serial protocol

Newline-delimited JSON over USB serial at **115200 baud**. One command or
reply per line. JSON was chosen over a binary framing deliberately: at 32
LEDs the bandwidth is trivial, and being able to debug the device from any
serial monitor by typing commands is worth far more than the parsing cost.
Revisit only if a longer strip actually saturates the link.

Protocol version: **3** (reported in `pong`; bump when making a breaking
change and gate app behavior on it).

Version history:

* **3** — `level` param on `effect`: per-animation brightness 0.0–1.0,
  applied on-device over the finished frame (and composing with the global
  `brightness`). It must live on the device because `rainbow` synthesizes
  its own colors; for every other effect the app pre-scales the colors it
  sends when talking to proto-2 firmware, so only rainbow degrades (to full
  brightness) there.
* **2** — every app effect became firmware-native (`flash`, `gradient`,
  `progress`, `dual_progress`, `keyframes`, plus `color2`/`progress`/
  `progress2`/`kf` params on `effect`). Motivation: sustained frame
  streaming wedges macOS 26's CDC driver (writes vanish, reads stop,
  `close(2)`/`open(2)` hang in the kernel until the board is replugged), so
  steady-state operation must not stream. The app checks the pong's `proto`
  and falls back to streaming these effects on proto-1 firmware.
* **1** — initial protocol.

## App → device

### `ping`

```json
{"cmd":"ping"}
```

Used for the connection handshake ("is this port really a Luminode
device?") and as a heartbeat every 3 s. Reply:

```json
{"evt":"pong","fw":"0.4.0","proto":3,"leds":33}
```

### `frame` — raw pixel data

```json
{"cmd":"frame","px":"ff0000ff0000…"}
```

`px` is a hex string of `RRGGBB` per LED — exactly `leds × 6` characters
(198 for 33 LEDs). **Why hex and not a JSON array:** a 33-element nested
array blows past the ATmega32U4's 2.5 KB RAM once ArduinoJson expands it;
the flat string parses in place.

Receiving a frame switches the device to *frame mode*: it displays exactly
what it was sent until the next frame arrives. Frames get **no reply** (at
30 fps replies would waste half the bandwidth). If frames stop for 10 s, or
the host closes the port, the device falls back to the built-in idle
sparkle effect rather than freezing on the last frame.

### `effect` — run a built-in effect

```json
{"cmd":"effect","name":"breathe","color":[0,120,255],"speed":0.5}
{"cmd":"effect","name":"dual_progress","color":[255,160,40],"color2":[120,90,255],"progress":0.4,"progress2":0.7}
{"cmd":"effect","name":"keyframes","speed":0.2,"kf":"ff000000ff000000ff"}
```

* `name`: `off` | `solid` | `breathe` | `rainbow` | `chase` | `sparkle` |
  `flash` | `gradient` | `progress` | `dual_progress` | `keyframes`
* `color`: `[r,g,b]`, optional (keeps previous)
* `color2`: `[r,g,b]`, optional — flash off-phase, gradient end, progress
  background, dual_progress right bar. Defaults per effect when omitted:
  progress `[30,30,30]`, dual_progress = `color`, otherwise black.
* `speed`: 0.0 (slowest, 8 s cycle) … 1.0 (fastest, 0.5 s cycle), optional
  — for `sparkle` (stochastic, not cycle-based) speed sets twinkle density
  and fade rate: slow is a sparse sparkle whose sparks linger ~2 s, fast is
  a dense quick twinkle
* `level`: 0.0–1.0, optional — per-animation brightness, applied on-device
  as a final scale over the rendered frame (so it also dims `rainbow`,
  whose colors the device synthesizes). Composes with the global
  `brightness` command rather than replacing it. **Resets to 1.0 when
  omitted** — it's part of the effect's look, restated with each `effect`
  command, not persistent device tuning.
* `progress`, `progress2`: 0.0–1.0 — progress bar fill / dual_progress left
  and right bars. **Reset to 0 when omitted** (they're state injected per
  update, not persistent tuning). Updating a gauge = re-send the whole
  `effect` command with new fractions; it's ~150 bytes a few times a
  minute, versus the 6.7 KB/s a streamed gauge used to cost.
* `kf`: keyframe color stops as one hex string, `RRGGBB` per stop, max 16
  stops (a nested JSON array would blow the 32U4's RAM — same trick as
  `frame`'s `px`). Required for `keyframes`, ignored otherwise.

The device animates on its own — no further serial traffic needed. Since
proto 2 **every** app effect is firmware-native, so the steady state sends
no serial traffic at all; the app streams frames only for effects the
connected firmware doesn't know (proto-1 boards, future experiments), and
then only when the frame content changes (with a 5 s keepalive to hold off
the frame-mode watchdog). Reply: `{"evt":"ok"}`.

### `progress` — nudge a running gauge (proto 2)

```json
{"cmd":"progress","a":0.64,"b":0.57}
```

Updates the progress fractions (`a` = `progress`, `b` = `progress2`) of the
running effect without re-stating it; omitted fields reset to 0. The app
uses this for every gauge movement on an already-running effect. **Why it
exists:** it stays under 64 bytes — one USB packet. Multi-packet writes are
the traffic class observed to wedge macOS 26's CDC driver mid-connection
(a ~130-byte `effect` re-send killed the link seconds later, twice in a
row, while minutes of small heartbeat pings ran clean), so the most
frequent mid-connection write gets the single-packet fast path. Reply:
`{"evt":"ok"}`.

### `brightness`

```json
{"cmd":"brightness","value":64}
```

Global brightness 0–255, applied on the device (FastLED scaling) so it
affects both streamed frames and built-in effects. Reply: `{"evt":"ok"}`.

## Device → app

| Line | Meaning |
|---|---|
| `{"evt":"pong","fw":…,"proto":…,"leds":…}` | ping reply; identity + capabilities |
| `{"evt":"ok"}` | command accepted (`effect`, `brightness`) |
| `{"evt":"err","msg":"…"}` | rejected line: `bad json`, `bad frame`, `bad kf`, `unknown cmd`, `line too long`, … |

The app surfaces `err` replies as `device/firmware_error` events on its
event bus, so they show up in the UI's event log.

## Device-side behaviors (not commands)

* **Boot default:** a white progress bar fills the strip over ~2 s (a
  power-on self-test: no bar means firmware/wiring/LED power is broken),
  then a slow, gentle white sparkle at brightness 64 — a few lingering
  sparks per second — runs until the app takes over.
* **Frame-mode watchdog:** see `frame` above. Effects (`effect` mode) are
  *not* subject to the watchdog; they run until told otherwise.
* **Input limits:** lines over 239 bytes are discarded (with an `err`
  reply) until the next newline.

## Trying it by hand

```
$ arduino-cli monitor -p /dev/cu.usbmodemXXXX --config 115200
{"cmd":"ping"}
{"evt":"pong","fw":"0.4.0","proto":3,"leds":33}
{"cmd":"effect","name":"solid","color":[255,0,0],"level":0.4}
{"evt":"ok"}
{"cmd":"effect","name":"progress","color":[0,200,120],"progress":0.66}
{"evt":"ok"}
```
