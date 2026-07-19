// Luminode firmware — APA102 strip driver for Arduino Micro (ATmega32U4).
//
// The firmware is intentionally dumb: it renders frames or runs one of a
// handful of built-in effects on command. All business logic ("why is the
// light red") lives in the desktop app. See docs/protocol.md for the full
// serial protocol specification.
//
// Wiring (software SPI on DATA_PIN/CLOCK_PIN below; bit-banging is plenty
// fast at 33 LEDs):
//   APA102 DATA  -> DATA_PIN
//   APA102 CLOCK -> CLOCK_PIN
//   APA102 GND   -> GND, APA102 5V -> 5V supply. 33 LEDs at full white want
//   ~2A, but FastLED caps total draw at POWER_BUDGET_MA, sized below for
//   USB bus power. On an external supply, raise the budget to match it.

#include <ArduinoJson.h>
#include <FastLED.h>

#define FW_VERSION "0.4.0"
#define PROTO_VERSION 4
#define NUM_LEDS 33
#define DATA_PIN 5
#define CLOCK_PIN 6
#define BAUD 115200

// Power budget enforced by FastLED at show() time: output is scaled so the
// strip never draws more than this, whatever brightness/frames the app asks
// for. 450mA suits USB bus power (500mA budget minus the 32U4's ~50mA);
// raise it when the strip has an external 5V supply.
#define POWER_BUDGET_MA 450

// No frames from the app for this long while in frame mode -> assume the app
// hung or died and fall back to the idle effect instead of freezing.
#define FRAME_TIMEOUT_MS 10000UL

// CDC self-heal: on macOS 26 the USB serial pipe to this board sporadically
// wedges mid-connection (host writes vanish, our replies stop arriving,
// even the host's open()/close() hang) and nothing but a re-enumeration —
// physical replug, bootloader entry — ever clears it. So if a host that was
// actively talking (DTR up, bytes flowed) goes silent for this long, we
// detach/attach USB ourselves: a software replug. This MUST fire before the
// app gives up: the app pings every 3s and closes the port 10s after the
// last pong, and closing drops DTR, which disarms this check — and every
// reconnect probe afterwards opens a fresh DTR session that restarts the
// clock, so the silence window can never accumulate again while the app is
// running. (The original 15s threshold was unreachable dead code for
// exactly that reason — observed 2026-07-17/18: wedged board sat mute for
// 30+ min, self-heal never fired.) 8s = two missed ping intervals, well
// clear of the ≤3.1s healthy gap, and 2s inside the app's 10s give-up.
// Armed only after the host has sent at least one byte this DTR session,
// so a human watching a serial monitor without typing never gets kicked;
// fires once per silence episode.
#define CDC_STALL_MS 8000UL

// Serial input line buffer. The largest command is a frame:
// {"cmd":"frame","px":"<198 hex chars>"}\n  ≈ 222 bytes.
#define LINE_BUF_SIZE 240

// Keyframe effect: max color stops (RAM: 3 bytes each).
#define MAX_KEYFRAMES 16

// Two buffers so device calibration never compounds: effects and frames
// render into the linear working `leds`; `present()` copies leds -> out
// applying per-channel gain and orientation, and FastLED is bound to `out`,
// so `out` is what's displayed. Keeping `leds` a clean linear source is what
// lets a constant gain be re-applied every frame without accumulating — the
// same reason FX_SPARKLE (which accumulates in `leds`) must not be scaled in
// place. ~99 bytes for the extra buffer, well within the 32U4's 2.5KB RAM.
CRGB leds[NUM_LEDS];
CRGB out[NUM_LEDS];
char lineBuf[LINE_BUF_SIZE];
uint8_t lineLen = 0;

// Proto 4: persistent device calibration (the `calibrate` command). Per-
// channel gain is a white-balance trim (the app's R/G/B sliders); `reversed`
// flips strip direction. Unlike `level`/`progress` these persist across
// commands until re-set — they're device tuning, not part of an effect's
// look — but live only in RAM, so the app re-sends them on every reconnect.
uint8_t gainR = 255, gainG = 255, gainB = 255;
bool reversed = false;

enum Mode : uint8_t {
  MODE_EFFECT,  // running a built-in effect
  MODE_FRAME,   // app is streaming raw frames
};

enum Effect : uint8_t {
  FX_OFF,
  FX_SOLID,
  FX_BREATHE,
  FX_RAINBOW,
  FX_CHASE,
  FX_SPARKLE,
  // Proto 2: previously app-streamed, now firmware-native so the app can go
  // quiet after one command (sustained frame streaming wedges macOS's CDC
  // driver). The math mirrors render() in the app's animation.rs — keep the
  // two in sync so the UI preview matches the strip.
  FX_FLASH,
  FX_GRADIENT,
  FX_PROGRESS,
  FX_DUAL_PROGRESS,
  FX_KEYFRAMES,
};

Mode mode = MODE_EFFECT;
Effect effect = FX_SPARKLE;  // boot default: runs while waiting for the app
CRGB effectColor = CRGB::White;
// Secondary color: flash off-phase, gradient end, progress background,
// dual_progress right bar. Per-effect default resolved in handleEffect.
CRGB effectColor2 = CRGB::Black;
float effectSpeed = 0.05;  // 0.0 (slowest) .. 1.0 (fastest)
// Proto 3: per-effect brightness (the `level` param, 0.0..1.0, stored as
// 0..255). Applied on top of the global `brightness` — it exists on-device
// because rainbow synthesizes its own colors, so the app can't pre-scale it.
uint8_t effectLevel = 255;
float progressA = 0.0;     // progress / dual_progress left bar, 0.0..1.0
float progressB = 0.0;     // dual_progress right bar, 0.0..1.0
CRGB kfStops[MAX_KEYFRAMES];
uint8_t kfCount = 0;
uint8_t brightness = 64;
unsigned long lastFrameMs = 0;

// CDC self-heal state (see CDC_STALL_MS above).
bool dtrWasUp = false;
bool hostSpoke = false;
bool cdcCycled = false;
unsigned long lastRxMs = 0;

void setup() {
  Serial.begin(BAUD);
  // REQUIRES FastLED 3.7.x — do not upgrade to 3.10.x. FastLED 3.10's AVR
  // backend routes SPI chipsets through the hardware SPI peripheral (ICSP
  // MOSI/SCK = pins 16/15) regardless of the pins requested here, so on
  // DATA=5/CLOCK=6 nothing ever reaches the strip. 3.7.x bit-bangs any
  // non-SPI pin pair as expected. 1MHz explicit rate: ~1ms per 33-LED
  // frame, far more than the 60Hz loop needs, and gentle on jumper wiring.
  // APA102HD = gamma-corrected driver: burns the APA102's 5-bit per-pixel
  // brightness field for smooth dim colors; everything upstream stays linear.
  FastLED.addLeds<APA102HD, DATA_PIN, CLOCK_PIN, BGR, DATA_RATE_MHZ(1)>(out, NUM_LEDS);
  FastLED.setMaxPowerInVoltsAndMilliamps(5, POWER_BUDGET_MA);
  FastLED.setBrightness(brightness);
  bootProgressBar();
}

// Boot animation: a white progress bar fills the strip over ~2s, holds a
// beat, then hands over to the sparkle idle in loop(). Doubles as a
// power-on self-test: if the bar doesn't show, firmware/wiring/LED power is
// broken regardless of what the app says. Blocking is fine — serial input
// buffers in the CDC stack and gets handled on the first loop() pass, well
// within the app's 3s probe window.
void bootProgressBar() {
  const unsigned long FILL_MS = 2000;
  unsigned long start = millis();
  unsigned long elapsed;
  while ((elapsed = millis() - start) < FILL_MS) {
    float filled = (float)elapsed * NUM_LEDS / FILL_MS;
    for (uint8_t i = 0; i < NUM_LEDS; i++) {
      float remain = filled - i;
      if (remain >= 1.0f) {
        leds[i] = CRGB::White;
      } else if (remain > 0.0f) {
        // Partial head pixel fades in, same as the app's progress effect.
        uint8_t v = (uint8_t)(remain * 255);
        leds[i] = CRGB(v, v, v);
      } else {
        leds[i] = CRGB::Black;
      }
    }
    presentAndShow();
    delay(1000 / 60);
  }
  fill_solid(leds, NUM_LEDS, CRGB::White);
  presentAndShow();
  delay(250);  // full-white beat before the sparkle takes over
}

void loop() {
  // CDC self-heal bookkeeping: a fresh DTR session starts un-armed. Clear
  // any stale TX error too — the core sets it even for writes attempted
  // while DTR was down (e.g. a reply that raced the host closing the port),
  // and that isn't evidence this session's pipe is wedged.
  bool dtrUp = (bool)Serial;
  if (dtrUp && !dtrWasUp) {
    hostSpoke = false;
    lastRxMs = millis();
    Serial.clearWriteError();
  }
  dtrWasUp = dtrUp;

  readSerial();

  // CDC self-heal: host was talking and still holds DTR, but either it went
  // silent past any plausible healthy gap (host->device direction wedged) or
  // our own replies are timing out inside the USB stack (device->host
  // direction wedged — USB_Send gave up after 250ms of the host not
  // draining, which a healthy host kernel always does regardless of what
  // the app is up to). Both mean the pipe is gone; re-enumerate.
  bool rxSilent = millis() - lastRxMs > CDC_STALL_MS;
  bool txStuck = Serial.getWriteError() != 0;
  if (dtrUp && hostSpoke && !cdcCycled && (rxSilent || txStuck)) {
    cdcCycled = true;
    Serial.clearWriteError();
    USBDevice.detach();
    delay(250);
    USBDevice.attach();
  }

  // Watchdog: if the app was streaming frames and went silent (or closed the
  // port — !Serial checks DTR on the 32U4), drop to a calm idle effect.
  if (mode == MODE_FRAME &&
      (!Serial || millis() - lastFrameMs > FRAME_TIMEOUT_MS)) {
    mode = MODE_EFFECT;
    effect = FX_SPARKLE;
    effectColor = CRGB::White;
    effectSpeed = 0.05;
    effectLevel = 255;
  }

  // Render/show at ~60Hz without ever blocking serial draining: the loop
  // spins freely so CDC input is consumed as fast as the host sends it.
  // The old delay(16) between drains let the host back up mid-frame while
  // streaming at 30fps, and on macOS 26 that stall can wedge the CDC pipe
  // entirely (the "mute board" failure — one pong, then silence).
  static unsigned long lastShowMs = 0;
  unsigned long now = millis();
  if (now - lastShowMs >= 1000 / 60) {
    lastShowMs = now;
    if (mode == MODE_EFFECT) {
      renderEffect();
    }
    presentAndShow();
  }
}

// Copy the linear working buffer to the FastLED-bound display buffer,
// applying persistent calibration: per-channel gain, then orientation. Done
// as a copy (not in place) so the constant gain never compounds across
// frames and FX_SPARKLE's accumulation in `leds` stays untouched. Runs every
// shown frame; at 33 LEDs it's a few hundred cheap ops.
void presentAndShow() {
  for (uint8_t i = 0; i < NUM_LEDS; i++) {
    CRGB c = leds[i];
    c.r = scale8(c.r, gainR);
    c.g = scale8(c.g, gainG);
    c.b = scale8(c.b, gainB);
    out[reversed ? (NUM_LEDS - 1 - i) : i] = c;
  }
  FastLED.show();
}

// ---------------------------------------------------------------------------
// Serial protocol
// ---------------------------------------------------------------------------

void readSerial() {
  while (Serial.available() > 0) {
    char c = Serial.read();
    lastRxMs = millis();
    hostSpoke = true;
    cdcCycled = false;  // healthy traffic re-arms the self-heal
    if (c == '\n') {
      lineBuf[lineLen] = '\0';
      if (lineLen > 0) handleLine();
      lineLen = 0;
    } else if (c != '\r') {
      if (lineLen < LINE_BUF_SIZE - 1) {
        lineBuf[lineLen++] = c;
      } else {
        // Overlong line: discard until the next newline.
        lineLen = 0;
        sendErr(F("line too long"));
        while (Serial.available() > 0 && Serial.read() != '\n') {}
      }
    }
  }
}

void handleLine() {
  StaticJsonDocument<384> doc;
  DeserializationError err = deserializeJson(doc, lineBuf);
  if (err) {
    sendErr(F("bad json"));
    return;
  }

  const char* cmd = doc["cmd"];
  if (cmd == nullptr) {
    sendErr(F("missing cmd"));
    return;
  }

  if (strcmp(cmd, "ping") == 0) {
    Serial.print(F("{\"evt\":\"pong\",\"fw\":\"" FW_VERSION "\",\"proto\":"));
    Serial.print(PROTO_VERSION);
    Serial.print(F(",\"leds\":"));
    Serial.print(NUM_LEDS);
    Serial.println(F("}"));
  } else if (strcmp(cmd, "frame") == 0) {
    handleFrame(doc["px"]);
  } else if (strcmp(cmd, "effect") == 0) {
    handleEffect(doc);
  } else if (strcmp(cmd, "progress") == 0) {
    // Lightweight gauge update: adjusts the progress fractions without
    // re-stating the whole effect. Kept under 64 bytes (one USB packet) —
    // it's the most frequent mid-connection write, and small single-packet
    // writes are the traffic class that has never wedged the macOS CDC pipe.
    progressA = constrain((float)(doc["a"] | 0.0f), 0.0f, 1.0f);
    progressB = constrain((float)(doc["b"] | 0.0f), 0.0f, 1.0f);
    sendOk();
  } else if (strcmp(cmd, "brightness") == 0) {
    int v = doc["value"] | -1;
    if (v < 0 || v > 255) {
      sendErr(F("bad brightness"));
      return;
    }
    brightness = v;
    FastLED.setBrightness(brightness);
    sendOk();
  } else if (strcmp(cmd, "calibrate") == 0) {
    // Persistent device tuning: per-channel gain + strip direction. Both
    // fields optional; an omitted field keeps its current value (so the app
    // can push either independently). Applied in present() each frame, so it
    // covers native effects and streamed frames alike.
    JsonArray gain = doc["gain"];
    if (!gain.isNull() && gain.size() == 3) {
      gainR = (uint8_t)gain[0];
      gainG = (uint8_t)gain[1];
      gainB = (uint8_t)gain[2];
    }
    reversed = doc["reversed"] | reversed;
    sendOk();
  } else {
    sendErr(F("unknown cmd"));
  }
}

// Frames arrive as a hex string ("RRGGBB" x NUM_LEDS) rather than a JSON
// array — a 33-element nested array would not fit in the 32U4's 2.5KB RAM
// once ArduinoJson expands it.
void handleFrame(const char* px) {
  if (px == nullptr || strlen(px) != NUM_LEDS * 6) {
    sendErr(F("bad frame"));
    return;
  }
  for (uint8_t i = 0; i < NUM_LEDS; i++) {
    leds[i] = CRGB(hexByte(px + i * 6), hexByte(px + i * 6 + 2),
                   hexByte(px + i * 6 + 4));
  }
  mode = MODE_FRAME;
  lastFrameMs = millis();
  // Deliberately no reply: at 30fps an ok per frame would waste bandwidth.
}

void handleEffect(JsonDocument& doc) {
  const char* name = doc["name"];
  if (name == nullptr) {
    sendErr(F("missing name"));
    return;
  }

  Effect fx;
  if (strcmp(name, "off") == 0) fx = FX_OFF;
  else if (strcmp(name, "solid") == 0) fx = FX_SOLID;
  else if (strcmp(name, "breathe") == 0) fx = FX_BREATHE;
  else if (strcmp(name, "rainbow") == 0) fx = FX_RAINBOW;
  else if (strcmp(name, "chase") == 0) fx = FX_CHASE;
  else if (strcmp(name, "sparkle") == 0) fx = FX_SPARKLE;
  else if (strcmp(name, "flash") == 0) fx = FX_FLASH;
  else if (strcmp(name, "gradient") == 0) fx = FX_GRADIENT;
  else if (strcmp(name, "progress") == 0) fx = FX_PROGRESS;
  else if (strcmp(name, "dual_progress") == 0) fx = FX_DUAL_PROGRESS;
  else if (strcmp(name, "keyframes") == 0) fx = FX_KEYFRAMES;
  else {
    sendErr(F("unknown effect"));
    return;
  }

  // Keyframe stops arrive as a hex string like frames do ("RRGGBB" per
  // stop) — a nested JSON array of arrays would not fit the 32U4's RAM.
  const char* kf = doc["kf"];
  if (kf != nullptr) {
    size_t len = strlen(kf);
    if (len == 0 || len % 6 != 0 || len / 6 > MAX_KEYFRAMES) {
      sendErr(F("bad kf"));
      return;
    }
    kfCount = len / 6;
    for (uint8_t i = 0; i < kfCount; i++) {
      kfStops[i] = CRGB(hexByte(kf + i * 6), hexByte(kf + i * 6 + 2),
                        hexByte(kf + i * 6 + 4));
    }
  } else if (fx == FX_KEYFRAMES) {
    sendErr(F("missing kf"));
    return;
  }

  JsonArray color = doc["color"];
  if (!color.isNull() && color.size() == 3) {
    effectColor = CRGB((uint8_t)color[0], (uint8_t)color[1], (uint8_t)color[2]);
  }
  // color2 defaults per effect, matching the app's render():
  // progress background dim gray, dual_progress mirrors color, else black.
  JsonArray color2 = doc["color2"];
  if (!color2.isNull() && color2.size() == 3) {
    effectColor2 =
        CRGB((uint8_t)color2[0], (uint8_t)color2[1], (uint8_t)color2[2]);
  } else if (fx == FX_PROGRESS) {
    effectColor2 = CRGB(30, 30, 30);
  } else if (fx == FX_DUAL_PROGRESS) {
    effectColor2 = effectColor;
  } else {
    effectColor2 = CRGB::Black;
  }
  float speed = doc["speed"] | -1.0f;
  if (speed >= 0.0f && speed <= 1.0f) effectSpeed = speed;
  // Absent level resets to full — it's part of the effect's look, restated
  // with every effect command, not persistent device tuning.
  float level = constrain((float)(doc["level"] | 1.0f), 0.0f, 1.0f);
  effectLevel = (uint8_t)(level * 255.0f + 0.5f);
  // Absent progress fields reset to 0, same as the app's unwrap_or(0.0).
  progressA = constrain((float)(doc["progress"] | 0.0f), 0.0f, 1.0f);
  progressB = constrain((float)(doc["progress2"] | 0.0f), 0.0f, 1.0f);

  effect = fx;
  mode = MODE_EFFECT;
  sendOk();
}

uint8_t hexByte(const char* s) {
  return (hexNibble(s[0]) << 4) | hexNibble(s[1]);
}

uint8_t hexNibble(char c) {
  if (c >= '0' && c <= '9') return c - '0';
  if (c >= 'a' && c <= 'f') return c - 'a' + 10;
  if (c >= 'A' && c <= 'F') return c - 'A' + 10;
  return 0;
}

void sendOk() { Serial.println(F("{\"evt\":\"ok\"}")); }

void sendErr(const __FlashStringHelper* msg) {
  Serial.print(F("{\"evt\":\"err\",\"msg\":\""));
  Serial.print(msg);
  Serial.println(F("\"}"));
}

// ---------------------------------------------------------------------------
// Built-in effects
// ---------------------------------------------------------------------------
// Speed 0..1 maps to a cycle period between SLOW_MS and FAST_MS; phase is
// derived from millis() so effects stay smooth regardless of loop jitter.

uint16_t cyclePeriodMs() {
  const uint16_t SLOW_MS = 8000, FAST_MS = 500;
  return SLOW_MS - (uint16_t)((SLOW_MS - FAST_MS) * effectSpeed);
}

void renderEffect() {
  // 0..255 position within the current cycle.
  uint8_t phase = (uint8_t)((millis() % cyclePeriodMs()) * 256UL / cyclePeriodMs());
  // Same position as a 0.0..1.0 float, for effects needing finer math.
  float ph = (float)(millis() % cyclePeriodMs()) / cyclePeriodMs();

  switch (effect) {
    case FX_OFF:
      fill_solid(leds, NUM_LEDS, CRGB::Black);
      break;
    case FX_SOLID:
      fill_solid(leds, NUM_LEDS, effectColor);
      break;
    case FX_BREATHE: {
      // sin8 gives a smooth 0..255..0 sweep over the cycle.
      uint8_t level = sin8(phase);
      CRGB c = effectColor;
      c.nscale8_video(level);
      fill_solid(leds, NUM_LEDS, c);
      break;
    }
    case FX_RAINBOW:
      fill_rainbow(leds, NUM_LEDS, phase, 256 / NUM_LEDS);
      break;
    case FX_CHASE: {
      fill_solid(leds, NUM_LEDS, CRGB::Black);
      uint8_t head = (uint16_t)phase * NUM_LEDS / 256;
      for (uint8_t t = 0; t < 5; t++) {  // head + fading 4-pixel tail
        int8_t idx = (head - t + NUM_LEDS) % NUM_LEDS;
        CRGB c = effectColor;
        c.nscale8_video(255 - t * 55);
        leds[idx] = c;
      }
      break;
    }
    case FX_SPARKLE: {
      // Twinkle: fade the whole strip a little each frame, then now and then
      // ignite a random pixel to the effect color. Stochastic rather than
      // phase-driven, so it never looks metronomic. Unlike the other effects
      // this accumulates across frames — fine, since nothing else writes
      // `leds` while an effect is running. Speed scales both ignition rate
      // and fade, so low speed is a sparse sparkle whose sparks linger for
      // a couple of seconds, and high speed is a dense fast twinkle. At the
      // boot default (0.05): ~4 sparks/s, each fading out over ~2 s.
      fadeToBlackBy(leds, NUM_LEDS, 5 + (uint8_t)(effectSpeed * 50));
      if (random8() < 8 + (uint8_t)(effectSpeed * 176)) {
        // Level is applied here at ignition, not in the whole-frame pass
        // below: sparkle accumulates in `leds` across frames, so a
        // per-frame scale would compound into extra decay.
        CRGB c = effectColor;
        c.nscale8(effectLevel);
        leds[random8(NUM_LEDS)] = c;
      }
      break;
    }
    case FX_FLASH:
      // Hard square wave between color and color2.
      fill_solid(leds, NUM_LEDS, ph < 0.5f ? effectColor : effectColor2);
      break;
    case FX_GRADIENT: {
      // Static color -> color2 gradient, slowly rotating around the strip,
      // mirrored so it wraps without a hard seam.
      float offset = ph * NUM_LEDS;
      for (uint8_t i = 0; i < NUM_LEDS; i++) {
        float pos = i + offset;
        if (pos >= NUM_LEDS) pos -= NUM_LEDS;  // offset < NUM_LEDS, so once
        pos /= NUM_LEDS;
        float t = pos < 0.5f ? pos * 2.0f : (1.0f - pos) * 2.0f;
        leds[i] = blend(effectColor, effectColor2, (fract8)(t * 255));
      }
      break;
    }
    case FX_PROGRESS: {
      // Fill from LED 0; partial head pixel fades in. Background is color2
      // dimmed to a quarter so the bar reads clearly.
      float filled = progressA * NUM_LEDS;
      CRGB bg = effectColor2;
      bg.nscale8(64);
      for (uint8_t i = 0; i < NUM_LEDS; i++) {
        float remain = filled - i;
        leds[i] = remain >= 1.0f ? effectColor
                  : remain > 0.0f
                      ? blend(bg, effectColor, (fract8)(remain * 255))
                      : bg;
      }
      break;
    }
    case FX_DUAL_PROGRESS: {
      // Two gauges meeting in the middle: progressA fills from the left
      // edge in color, progressB from the right edge in color2.
      const CRGB bg(10, 10, 14);
      const uint8_t half = NUM_LEDS / 2;
      float left = progressA * half;
      float right = progressB * (NUM_LEDS - half);
      for (uint8_t i = 0; i < NUM_LEDS; i++) {
        CRGB c;
        float remain;
        if (i < half) {
          c = effectColor;
          remain = left - i;
        } else {
          c = effectColor2;
          remain = right - (NUM_LEDS - 1 - i);
        }
        leds[i] = remain >= 1.0f ? c
                  : remain > 0.0f ? blend(bg, c, (fract8)(remain * 255))
                                  : bg;
      }
      break;
    }
    case FX_KEYFRAMES: {
      // Whole-strip color timeline: fade through the stops in order,
      // wrapping smoothly back to the first. speed sets the cycle.
      if (kfCount == 0) {
        fill_solid(leds, NUM_LEDS, CRGB::Black);
      } else if (kfCount == 1) {
        fill_solid(leds, NUM_LEDS, kfStops[0]);
      } else {
        float pos = ph * kfCount;
        uint8_t i = (uint8_t)pos % kfCount;
        float t = pos - (uint8_t)pos;
        fill_solid(leds, NUM_LEDS,
                   blend(kfStops[i], kfStops[(i + 1) % kfCount], (fract8)(t * 255)));
      }
      break;
    }
  }
  // Per-effect brightness: one scale pass over the finished frame, matching
  // the app's render(). Every effect except sparkle re-renders from scratch
  // each frame, so scaling here is safe; sparkle scales at ignition above.
  if (effectLevel < 255 && effect != FX_SPARKLE) {
    nscale8(leds, NUM_LEDS, effectLevel);
  }
}
