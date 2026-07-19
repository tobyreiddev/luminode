<script lang="ts">
  import { invoke } from "@tauri-apps/api/core";
  import { listen, type UnlistenFn } from "@tauri-apps/api/event";
  import { open as openDialog, save as saveDialog } from "@tauri-apps/plugin-dialog";
  import { onMount } from "svelte";
  import AnimThumb from "$lib/AnimThumb.svelte";
  import Toggle from "$lib/Toggle.svelte";
  import {
    rgbToHex,
    hexToRgb,
    type ActiveState,
    type AnimSpec,
    type Animation,
    type AppSettings,
    type BusEvent,
    type DeviceStatus,
    type PortCandidate,
    type Schedule,
    type Trigger,
    type IntegrationHealth,
    type IntegrationDescriptor,
    type KnownDevice,
  } from "$lib/types";

  const NUM_LEDS = 33;
  const EFFECTS = ["off", "solid", "breathe", "rainbow", "chase", "sparkle", "flash", "gradient", "progress", "dual_progress"];
  // "keyframes" needs its stop editor, so it's only offered where one exists.
  const EDITOR_EFFECTS = [...EFFECTS, "keyframes"];
  const TWO_COLOR_EFFECTS = new Set(["flash", "gradient", "progress", "dual_progress"]);
  const COLOR_EFFECTS = new Set(["solid", "breathe", "chase", "sparkle", "flash", "gradient", "progress", "dual_progress"]);
  const SPEED_LABELS: Record<string, string> = {
    breathe: "Breath rate",
    rainbow: "Scroll speed",
    chase: "Speed",
    sparkle: "Twinkle speed",
    flash: "Blink rate",
    gradient: "Rotation speed",
    keyframes: "Loop speed",
  };
  // Curated palette for the accent picker.
  const RULE_PALETTE = ["#5ec8f2", "#4ade80", "#f5a94e", "#f87171", "#c084fc", "#f472b6"];

  type View = "overview" | "rules" | "devices" | "settings";
  let view = $state<View>("overview");
  let theme = $state<"system" | "dark" | "light">("system");
  let toast = $state<{ message: string; kind: "ok" | "error" } | null>(null);
  let toastTimer: ReturnType<typeof setTimeout> | null = null;

  function notify(message: string, kind: "ok" | "error" = "ok") {
    toast = { message, kind };
    if (toastTimer) clearTimeout(toastTimer);
    toastTimer = setTimeout(() => (toast = null), 4000);
  }

  function reportError(error: unknown) {
    notify(String(error), "error");
  }

  // --- live state pushed from Rust ---
  let status = $state<DeviceStatus>({ connected: false, port: null, serialNumber: null, fwVersion: null, ledCount: null, protocolVersion: null });
  let candidates = $state<PortCandidate[]>([]);
  let frame = $state<string>("000000".repeat(NUM_LEDS));
  let active = $state<ActiveState>({ activeName: "…", snoozedUntilMs: null, overlays: [], quietHoursActive: false });
  let events = $state<BusEvent[]>([]);
  let integrationHealth = $state<IntegrationHealth[]>([]);
  let integrationCatalog = $state<IntegrationDescriptor[]>([]);
  let knownDevices = $state<KnownDevice[]>([]);
  let firmwareCompatibility = $state<{ currentProtocol: number | null; requiredProtocol: number; compatible: boolean; guidance: string } | null>(null);

  // --- persisted collections ---
  let animations = $state<Animation[]>([]);
  let triggers = $state<Trigger[]>([]);
  let profiles = $state<string[]>(["Default"]);
  let activeProfile = $state("Default");
  let quietEnabled = $state(false);
  let quietStart = $state("22:00");
  let quietEnd = $state("07:00");
  let conditionValue = $state("");
  let schedules = $state<Schedule[]>([]);
  // Animation id, or "claude_usage" for the live session/weekly gauge.
  let idleSelection = $state<number | "claude_usage" | null>(null);

  // --- app-level settings + device calibration (redesign) ---
  let power = $state(true);
  let brightnessCap = $state(255);
  let gain = $state<[number, number, number]>([255, 255, 255]);
  let reversed = $state(false);
  let idleDimMinutes = $state(0);
  let startMinimized = $state(false);
  let autostart = $state(false);
  let accent = $state("#5ec8f2");

  // --- manual control form ---
  let effect = $state("solid");
  let colorHex = $state("#0050ff");
  let color2Hex = $state("#000000");
  let speed = $state(0.5);
  let levelPct = $state(100);
  let progressPct = $state(50);
  let progress2Pct = $state(50);
  let brightness = $state(64);
  let manualActive = $derived(active.overlays.some((o) => o.key === "manual"));
  let manualSaveName = $state<string | null>(null);

  // --- animation editor ---
  let editingAnimation = $state<{ id: number; name: string } | null>(null);
  let eaEffect = $state("solid");
  let eaColorHex = $state("#0050ff");
  let eaColor2Hex = $state("#000000");
  let eaSpeed = $state(0.5);
  let eaLevelPct = $state(100);
  let eaKeyframes = $state<string[]>([]);
  let eaDurationS = $state("");
  // "Try on strip" pins a manual overlay (priority i32::MAX, no expiry) so the
  // preview holds while editing. Track it so closing the editor releases it —
  // otherwise the pin outlives the editor and silently blocks every trigger
  // (Claude working, meetings, …) until the app restarts.
  let editorPreviewActive = $state(false);

  // --- trigger + animation editors, expanded inline into their cards ---
  let editingTrigger = $state<Trigger | null>(null);
  let expandedRuleId = $state<number | null>(null);
  let expandedAnimId = $state<number | null>(null);

  // --- simulate form ---
  let simSource = $state("cli");
  let simType = $state("run_failed");
  let simPayload = $state("");

  // --- integrations ---
  let slackSet = $state(false);
  let calendarSet = $state(false);
  let canUndoImport = $state(false);
  let slackInput = $state("");
  let calendarInput = $state("");

  // Derived preview helpers.
  let framePixels = $derived(pixels(frame));
  let litFraction = $derived(framePixels.filter((p) => p !== "#000000").length / NUM_LEDS);
  let winningOverlay = $derived(active.overlays.find((o) => o.winning) ?? null);

  const NAV: { id: View; label: string }[] = [
    { id: "overview", label: "Overview" },
    { id: "rules", label: "Rules" },
    { id: "devices", label: "Devices" },
    { id: "settings", label: "Settings" },
  ];

  async function saveSecret(name: string, value: string) {
    try {
      await invoke("set_secret", { name, value: value.trim() });
    } catch (e) {
      reportError(e);
      return;
    }
    if (name === "slack_token") {
      slackSet = await invoke("has_secret", { name });
      slackInput = "";
    } else if (name === "calendar_ics_url") {
      calendarSet = await invoke("has_secret", { name });
      calendarInput = "";
    }
    notify(value.trim() ? "Integration secret saved" : "Integration disconnected");
    integrationHealth = await invoke("integration_health");
  }

  function manualSpec(): AnimSpec {
    return {
      effect,
      color: hexToRgb(colorHex),
      color2: TWO_COLOR_EFFECTS.has(effect) ? hexToRgb(color2Hex) : null,
      speed,
      level: levelPct / 100,
      progress: effect === "progress" || effect === "dual_progress" ? progressPct / 100 : null,
      progress2: effect === "dual_progress" ? progress2Pct / 100 : null,
      keyframes: null,
    };
  }

  async function applyManual() {
    await invoke("set_manual", { spec: manualSpec() });
  }
  async function releaseManual() {
    await invoke("clear_manual");
  }
  async function onBrightness() {
    await invoke("set_brightness", { value: brightness });
  }

  // --- redesign settings handlers ---
  async function loadSettings() {
    const s: AppSettings = await invoke("get_app_settings");
    power = s.power;
    brightness = s.brightness;
    brightnessCap = s.brightnessCap;
    gain = s.gain;
    reversed = s.reversed;
    idleDimMinutes = s.idleDimMinutes;
    startMinimized = s.startMinimized;
    autostart = s.autostart;
    accent = s.accent;
    applyAccent();
  }
  function applyAccent() {
    document.documentElement.style.setProperty("--accent", accent);
  }
  async function togglePower() {
    power = !power;
    await invoke("set_power", { on: power });
  }
  async function onBrightnessCap() {
    try {
      await invoke("set_brightness_cap", { value: brightnessCap });
      if (brightness > brightnessCap) brightness = brightnessCap;
    } catch (e) {
      reportError(e);
    }
  }
  async function onCalibration() {
    await invoke("set_calibration", { gain, reversed });
  }
  async function setOrientation(rtl: boolean) {
    reversed = rtl;
    await onCalibration();
  }
  async function setGain(channel: number, value: number) {
    gain[channel] = value;
    await onCalibration();
  }
  async function identify() {
    try {
      await invoke("identify");
    } catch (e) {
      reportError(e);
    }
  }
  async function onIdleDim() {
    await invoke("set_idle_dim", { minutes: idleDimMinutes });
  }
  async function toggleStartMinimized() {
    startMinimized = !startMinimized;
    await invoke("set_start_minimized", { enabled: startMinimized });
  }
  async function toggleAutostart() {
    try {
      autostart = await invoke("set_autostart", { enabled: !autostart });
    } catch (e) {
      reportError(e);
    }
  }
  async function onAccent(hex: string) {
    accent = hex;
    applyAccent();
    try {
      await invoke("set_accent", { hex });
    } catch (e) {
      reportError(e);
    }
  }

  // --- rules: the collapsed row shows the referenced animation; the expanded
  // body is the full rule editor (bound to the `editingTrigger` buffer). ---
  function ruleAnim(t: Trigger): Animation | undefined {
    return animations.find((a) => a.id === t.animationId);
  }
  function ruleColor(t: Trigger): string {
    const a = ruleAnim(t);
    return a ? rgbToHex(a.spec.color) : "#5ec8f2";
  }
  function rulePattern(t: Trigger): string {
    return ruleAnim(t)?.spec.effect ?? "—";
  }
  // Effect name of whatever a winning overlay is currently showing, for the
  // Overview status line.
  function rulePatternFromKey(key: string): string {
    if (key === "manual") return "manual";
    const id = Number(key.replace("trigger:", ""));
    const t = triggers.find((x) => x.id === id);
    return t ? rulePattern(t) : "idle";
  }
  // Expand a rule into edit mode (one editor open at a time), or collapse it.
  function toggleExpandRule(t: Trigger) {
    if (expandedRuleId === t.id) {
      cancelRuleEdit();
      return;
    }
    editingTrigger = structuredClone($state.snapshot(t)) as Trigger;
    conditionValue = t.policy.payloadEquals == null ? "" : JSON.stringify(t.policy.payloadEquals);
    expandedRuleId = t.id;
    editingAnimation = null;
    expandedAnimId = null;
  }
  function addRule() {
    editingTrigger = newTrigger();
    conditionValue = "";
    expandedRuleId = null; // the new-rule card is keyed on id 0
  }
  function cancelRuleEdit() {
    editingTrigger = null;
    expandedRuleId = null;
  }
  // Same expand-to-configure pattern for animations.
  function toggleExpandAnim(a: Animation) {
    if (expandedAnimId === a.id) {
      cancelAnimEdit();
      return;
    }
    startEditAnimation(a);
    expandedAnimId = a.id;
    editingTrigger = null;
    expandedRuleId = null;
  }
  function addAnimation() {
    startEditAnimation(null);
    expandedAnimId = null; // the new-animation card is keyed on id 0
  }
  function cancelAnimEdit() {
    releaseEditorPreview();
    editingAnimation = null;
    expandedAnimId = null;
  }

  // -- animations --
  function startEditAnimation(a: Animation | null) {
    releaseEditorPreview(); // drop any preview from the editor we're leaving
    if (a) {
      editingAnimation = { id: a.id, name: a.name };
      eaEffect = a.spec.effect;
      eaColorHex = rgbToHex(a.spec.color);
      eaColor2Hex = a.spec.color2 ? rgbToHex(a.spec.color2) : "#000000";
      eaSpeed = a.spec.speed;
      eaLevelPct = Math.round((a.spec.level ?? 1) * 100);
      eaKeyframes = (a.spec.keyframes ?? []).map(rgbToHex);
      eaDurationS = a.durationMs != null ? String(a.durationMs / 1000) : "";
    } else {
      editingAnimation = { id: 0, name: "" };
      eaEffect = "solid";
      eaColorHex = "#0050ff";
      eaColor2Hex = "#000000";
      eaSpeed = 0.5;
      eaLevelPct = 100;
      eaKeyframes = [];
      eaDurationS = "";
    }
  }
  function eaDurationMs(): number | null {
    const v = parseFloat(eaDurationS);
    return Number.isFinite(v) && v > 0 ? Math.round(v * 1000) : null;
  }
  function editorSpec(): AnimSpec {
    return {
      effect: eaEffect,
      color: hexToRgb(eaColorHex),
      color2: TWO_COLOR_EFFECTS.has(eaEffect) ? hexToRgb(eaColor2Hex) : null,
      speed: eaSpeed,
      level: eaLevelPct / 100,
      progress: null,
      progress2: null,
      keyframes: eaEffect === "keyframes" ? eaKeyframes.map(hexToRgb) : null,
    };
  }
  async function saveAnimationEdit() {
    if (!editingAnimation || !editingAnimation.name.trim()) return;
    try {
      if (editingAnimation.id === 0) {
        await invoke("save_animation", { name: editingAnimation.name.trim(), spec: editorSpec(), durationMs: eaDurationMs() });
      } else {
        await invoke("update_animation", { id: editingAnimation.id, name: editingAnimation.name.trim(), spec: editorSpec(), durationMs: eaDurationMs() });
      }
    } catch (e) {
      reportError(e);
      return;
    }
    await releaseEditorPreview();
    editingAnimation = null;
    expandedAnimId = null;
    animations = await invoke("list_animations");
  }
  async function tryEditorOnStrip() {
    await invoke("set_manual", { spec: editorSpec() });
    editorPreviewActive = true;
  }
  // Release a "Try on strip" preview when the editor closes so it can't linger
  // as a max-priority overlay masking every trigger.
  async function releaseEditorPreview() {
    if (!editorPreviewActive) return;
    editorPreviewActive = false;
    await invoke("clear_manual");
  }
  async function saveManualAsAnimation() {
    const name = manualSaveName?.trim();
    if (!name) return;
    try {
      await invoke("save_animation", { name, spec: manualSpec(), durationMs: null });
    } catch (e) {
      reportError(e);
      return;
    }
    manualSaveName = null;
    animations = await invoke("list_animations");
  }
  async function applyAnimation(id: number) {
    await invoke("apply_animation", { id });
  }
  async function deleteAnimation(id: number) {
    const animation = animations.find((a) => a.id === id);
    const affected = triggers.filter((t) => t.animationId === id).length;
    if (!confirm(`Delete “${animation?.name ?? "animation"}”?${affected ? ` This also removes ${affected} trigger(s).` : ""}`)) return;
    try { await invoke("delete_animation", { id }); } catch (e) { reportError(e); return; }
    animations = await invoke("list_animations");
    triggers = await invoke("list_triggers");
  }
  async function loadIdle() {
    const [mode, id] = await Promise.all([
      invoke<string>("get_idle_mode"),
      invoke<number | null>("get_idle_animation"),
    ]);
    idleSelection = mode === "claude_usage" ? "claude_usage" : id;
  }
  async function onIdleChange() {
    if (idleSelection === "claude_usage") {
      await invoke("set_idle_mode", { mode: "claude_usage" });
    } else if (idleSelection != null) {
      await invoke("set_idle_animation", { id: idleSelection });
      await invoke("set_idle_mode", { mode: "animation" });
    }
  }

  // -- triggers --
  function newTrigger(): Trigger {
    return {
      id: 0,
      name: "",
      source: "cli",
      eventType: "",
      clearEventType: null,
      animationId: animations[0]?.id ?? 1,
      priority: (triggers[0]?.priority ?? 0) + 10,
      durationMs: null,
      enabled: true,
      policy: { profile: activeProfile, payloadPath: null, payloadEquals: null, cooldownMs: null },
    };
  }
  async function saveTrigger() {
    if (!editingTrigger || !editingTrigger.name.trim() || !editingTrigger.eventType.trim()) return;
    const trigger = {
      ...editingTrigger,
      clearEventType: editingTrigger.clearEventType?.trim() || null,
      policy: {
        ...editingTrigger.policy,
        payloadPath: editingTrigger.policy.payloadPath?.trim() || null,
        payloadEquals: editingTrigger.policy.payloadPath ? parseConditionValue(conditionValue) : null,
      },
    };
    try {
      await invoke("save_trigger", { trigger });
    } catch (e) {
      reportError(e);
      return;
    }
    editingTrigger = null;
    expandedRuleId = null;
    triggers = await invoke("list_triggers");
    profiles = await invoke("list_profiles");
  }
  function parseConditionValue(value: string): unknown {
    if (!value.trim()) return "";
    try { return JSON.parse(value); } catch { return value; }
  }
  async function changeProfile() {
    await invoke("set_active_profile", { profile: activeProfile });
    active = await invoke("get_active");
    notify(`Scene changed to ${activeProfile}`);
  }
  async function saveQuietHours() {
    try {
      await invoke("set_quiet_hours", { enabled: quietEnabled, start: quietStart, end: quietEnd });
      notify(quietEnabled ? `Quiet hours saved (${quietStart}–${quietEnd})` : "Quiet hours disabled");
    } catch (e) { reportError(e); }
  }
  async function toggleTrigger(trigger: Trigger) {
    await invoke("save_trigger", { trigger: { ...trigger, enabled: !trigger.enabled } });
    triggers = await invoke("list_triggers");
  }
  async function deleteTrigger(id: number) {
    if (!confirm("Delete this trigger?")) return;
    await invoke("delete_trigger", { id });
    editingTrigger = null;
    expandedRuleId = null;
    triggers = await invoke("list_triggers");
  }

  // Drag to reorder.
  let dragIndex = $state<number | null>(null);
  function onTriggerDragStart(e: DragEvent, i: number) {
    dragIndex = i;
    if (e.dataTransfer) {
      e.dataTransfer.effectAllowed = "move";
      e.dataTransfer.setData("text/plain", String(i));
    }
  }
  function onTriggerDragOver(e: DragEvent, i: number) {
    e.preventDefault();
    if (dragIndex === null || i === dragIndex) return;
    const arr = [...triggers];
    const [moved] = arr.splice(dragIndex, 1);
    arr.splice(i, 0, moved);
    triggers = arr;
    dragIndex = i;
  }
  async function onTriggerDragEnd() {
    if (dragIndex === null) return;
    dragIndex = null;
    await invoke("reorder_triggers", { ids: triggers.map((t) => t.id) });
    triggers = await invoke("list_triggers");
  }
  async function moveTrigger(index: number, delta: number) {
    const target = index + delta;
    if (target < 0 || target >= triggers.length) return;
    const reordered = [...triggers];
    [reordered[index], reordered[target]] = [reordered[target], reordered[index]];
    triggers = reordered;
    await invoke("reorder_triggers", { ids: triggers.map((t) => t.id) });
    triggers = await invoke("list_triggers");
  }
  function applyTheme(value: "system" | "dark" | "light") {
    theme = value;
    document.documentElement.dataset.theme = value;
    localStorage.setItem("luminode-theme", value);
  }
  async function refreshHealth() {
    integrationHealth = await invoke("integration_health");
    notify("Integration health refreshed");
  }

  // -- schedules --
  let editingSchedule = $state<Schedule | null>(null);
  function newSchedule(): Schedule {
    return { id: 0, name: "", time: "18:00", action: "emit", eventType: "", animationId: animations[0]?.id ?? 1, enabled: true };
  }
  async function saveSchedule() {
    if (!editingSchedule || !editingSchedule.name.trim()) return;
    if (editingSchedule.action === "emit" && !editingSchedule.eventType?.trim()) return;
    const schedule = {
      ...editingSchedule,
      eventType: editingSchedule.action === "emit" ? editingSchedule.eventType!.trim() : null,
      animationId: editingSchedule.action === "idle" ? editingSchedule.animationId : null,
    };
    try {
      await invoke("save_schedule", { schedule });
    } catch (e) {
      reportError(e);
      return;
    }
    editingSchedule = null;
    schedules = await invoke("list_schedules");
  }
  async function toggleSchedule(s: Schedule) {
    await invoke("save_schedule", { schedule: { ...s, enabled: !s.enabled } });
    schedules = await invoke("list_schedules");
  }
  async function deleteSchedule(id: number) {
    if (!confirm("Delete this schedule?")) return;
    await invoke("delete_schedule", { id });
    schedules = await invoke("list_schedules");
  }

  // -- config export/import --
  async function exportConfig() {
    const path = await saveDialog({ defaultPath: "luminode-config.json", filters: [{ name: "Luminode config", extensions: ["json"] }] });
    if (!path) return;
    try { await invoke("export_config", { path }); notify("Config exported"); } catch (e) { reportError(e); }
  }
  async function importConfig() {
    const path = await openDialog({ multiple: false, filters: [{ name: "Luminode config", extensions: ["json"] }] });
    if (typeof path !== "string") return;
    try {
      const summary: string = await invoke("import_config", { path });
      notify(summary);
      canUndoImport = true;
    } catch (e) { reportError(e); return; }
    animations = await invoke("list_animations");
    triggers = await invoke("list_triggers");
    schedules = await invoke("list_schedules");
    await loadIdle();
  }
  async function undoImport() {
    try {
      await invoke("undo_import");
      canUndoImport = false;
      animations = await invoke("list_animations"); triggers = await invoke("list_triggers"); schedules = await invoke("list_schedules");
      await loadIdle();
      notify("Import undone");
    } catch (e) { reportError(e); }
  }
  async function exportDiagnostics() {
    const path = await saveDialog({ defaultPath: "luminode-diagnostics.json", filters: [{ name: "JSON", extensions: ["json"] }] });
    if (!path) return;
    try { await invoke("export_diagnostics", { path }); notify("Redacted diagnostics exported"); } catch (e) { reportError(e); }
  }

  // -- device / misc --
  async function adopt(port: string) {
    await invoke("adopt_device", { port });
  }
  async function snooze(minutes: number) {
    await invoke("snooze", { minutes });
  }
  async function simulate() {
    let payload: unknown = null;
    if (simPayload.trim()) {
      try { payload = JSON.parse(simPayload); } catch { notify("Payload is not valid JSON", "error"); return; }
    }
    await invoke("simulate_event", { source: simSource, eventType: simType, payload });
    notify("Event fired");
  }
  async function clearHistory() {
    if (!confirm("Clear the complete event history?")) return;
    try { await invoke("clear_events"); events = []; notify("Event history cleared"); } catch (e) { reportError(e); }
  }
  async function replayEvent(event: BusEvent) {
    try {
      await invoke("simulate_event", { source: event.source, eventType: event.type, payload: event.payload });
      notify(`Replayed ${event.source}/${event.type}`);
    } catch (e) { reportError(e); }
  }

  function pixels(hex: string): string[] {
    const out: string[] = [];
    for (let i = 0; i < NUM_LEDS; i++) out.push(`#${hex.slice(i * 6, i * 6 + 6)}`);
    return out;
  }
  function fmtTime(ts: number): string {
    return new Date(ts).toLocaleTimeString();
  }
  function fmtPayload(p: unknown): string {
    if (p == null) return "";
    const s = JSON.stringify(p);
    return s === "null" ? "" : s;
  }
  function pct(v: number, max = 255): number {
    return Math.round((v / max) * 100);
  }

  onMount(() => {
    const savedTheme = localStorage.getItem("luminode-theme");
    if (savedTheme === "dark" || savedTheme === "light" || savedTheme === "system") applyTheme(savedTheme);
    const unlisteners: Promise<UnlistenFn>[] = [
      listen<DeviceStatus>("device:status", (e) => {
        status = e.payload;
        invoke<KnownDevice[]>("known_devices").then((value) => (knownDevices = value));
        invoke<typeof firmwareCompatibility>("firmware_compatibility").then((value) => (firmwareCompatibility = value));
      }),
      listen<PortCandidate[]>("device:candidates", (e) => (candidates = e.payload)),
      listen<string>("engine:frame", (e) => (frame = e.payload)),
      listen<ActiveState>("engine:active", (e) => (active = e.payload)),
      listen<BusEvent>("bus:event", (e) => {
        events = [e.payload, ...events].slice(0, 100);
        if (e.payload.source === "time") loadIdle();
        if (e.payload.source === "slack" || e.payload.source === "calendar") {
          invoke<IntegrationHealth[]>("integration_health").then((value) => (integrationHealth = value));
        }
      }),
    ];
    const onVisibility = () => invoke("set_preview_visible", { visible: !document.hidden });
    document.addEventListener("visibilitychange", onVisibility);
    onVisibility();
    (async () => {
      try {
        status = await invoke("get_status");
        candidates = await invoke("list_candidates");
        animations = await invoke("list_animations");
        triggers = await invoke("list_triggers");
        profiles = await invoke("list_profiles");
        activeProfile = await invoke("get_active_profile");
        const quiet = await invoke<{ enabled: boolean; start: string; end: string }>("get_quiet_hours");
        quietEnabled = quiet.enabled; quietStart = quiet.start; quietEnd = quiet.end;
        schedules = await invoke("list_schedules");
        brightness = await invoke("get_brightness");
        await loadSettings();
        await loadIdle();
        events = await invoke("recent_events", { limit: 50 });
        active = await invoke("get_active");
        slackSet = await invoke("has_secret", { name: "slack_token" });
        calendarSet = await invoke("has_secret", { name: "calendar_ics_url" });
        integrationHealth = await invoke("integration_health");
        integrationCatalog = await invoke("integration_catalog");
        knownDevices = await invoke("known_devices");
        firmwareCompatibility = await invoke("firmware_compatibility");
        canUndoImport = await invoke("can_undo_import");
      } catch (e) {
        reportError(`Could not load Luminode: ${e}`);
      }
    })();
    return () => {
      document.removeEventListener("visibilitychange", onVisibility);
      invoke("set_preview_visible", { visible: false });
      unlisteners.forEach((p) => p.then((un) => un()));
    };
  });
</script>

<div class="shell">
  <!-- ============================ sidebar ============================ -->
  <aside class="sidebar">
    <div>
      <div class="brand">
        <div class="logo"><span class="logo-dot"></span></div>
        <span class="brand-name">Luminode</span>
      </div>
      <nav aria-label="Sections">
        {#each NAV as item}
          <button class="nav-row" class:selected={view === item.id} aria-current={view === item.id ? "page" : undefined} onclick={() => (view = item.id)}>
            <span class="nav-dot"></span>
            <span class="nav-label">{item.label}</span>
          </button>
        {/each}
      </nav>
    </div>
    <div class="sidebar-footer">
      <div class="conn">
        <span class="conn-dot" class:ok={status.connected}></span>
        <span>{status.connected ? "Connected" : "Disconnected"}</span>
      </div>
      <div class="conn-detail">
        {#if status.connected}{status.port} · proto {status.protocolVersion} · {status.ledCount} LEDs{:else}searching for device…{/if}
      </div>
    </div>
  </aside>

  <!-- ============================ content ============================ -->
  <main class="content">
    {#if firmwareCompatibility && !firmwareCompatibility.compatible}
      <aside class="banner warn" role="alert"><strong>Firmware update required</strong><span>Protocol {firmwareCompatibility.currentProtocol} is older than required protocol {firmwareCompatibility.requiredProtocol}. {firmwareCompatibility.guidance}</span></aside>
    {/if}

    <!-- ============================ OVERVIEW ============================ -->
    {#if view === "overview"}
      <div class="tab">
        <div class="page-head">
          <h1>Overview</h1>
          <div class="power">
            <span class="power-label">{power ? "On" : "Off"}</span>
            <Toggle checked={power} onchange={togglePower} label="Master power" />
          </div>
        </div>

        {#if !status.connected && candidates.length === 0}
          <aside class="banner notice"><strong>Connect your Luminode</strong><span>Plug in the Arduino over USB — the app discovers it automatically. Everything below still previews without hardware.</span></aside>
        {/if}
        {#if !status.connected && candidates.length > 1}
          <div class="card picker">
            <span class="label">Multiple devices found — pick yours</span>
            <div class="row wrap">{#each candidates as c}<button onclick={() => adopt(c.port)}>{c.product ?? "Unknown"} · {c.port}</button>{/each}</div>
          </div>
        {/if}

        <div class="card">
          <div class="card-head">
            <span class="label">LIVE PREVIEW</span>
            <button class="pill-btn" onclick={identify}>Identify strip</button>
          </div>
          <div class="strip">
            {#each framePixels as px}
              <span class="led" class:lit={px !== "#000000"} style="--c:{px}"></span>
            {/each}
          </div>
        </div>

        <div class="card">
          <span class="label">ACTIVE TRIGGER</span>
          <div class="active-name">
            <span class="active-dot"></span>
            <strong>{active.activeName}</strong>
          </div>
          <div class="bar"><div class="bar-fill" style="width:{Math.round(litFraction * 100)}%"></div></div>
          <div class="row between">
            <span class="mono dim">{Math.round(litFraction * 100)}% · {winningOverlay ? rulePatternFromKey(winningOverlay.key) : "idle"} pattern{winningOverlay?.expiresInMs != null ? ` · ${Math.ceil(winningOverlay.expiresInMs / 1000)}s left` : ""}</span>
            {#if active.snoozedUntilMs}
              <button class="pill-btn" onclick={() => snooze(0)}>End snooze</button>
            {:else}
              <button class="pill-btn" onclick={() => snooze(30)}>Snooze 30m</button>
            {/if}
          </div>
        </div>

        <div class="card">
          <div class="card-head">
            <span class="label">BRIGHTNESS</span>
            <span class="value">{pct(brightness)}%</span>
          </div>
          <input class="slider" type="range" min="0" max={brightnessCap} step="1" bind:value={brightness} onchange={onBrightness} style="--fill:{pct(brightness, brightnessCap)}%" />
        </div>

        <div class="card">
          <span class="label">WHY IS THE LIGHT DOING THAT?</span>
          <ul class="mini-list">
            {#if active.overlays.length === 0}
              <li><span class="dim">No active overrides — showing the idle animation.</span></li>
            {/if}
            {#each active.overlays as o (o.key)}
              <li>
                <span class="grow">{#if o.winning}👑 {/if}{o.name}</span>
                <span class="mono dim">{o.key === "manual" ? "beats all" : `prio ${o.priority}`}{o.expiresInMs != null ? ` · ${Math.ceil(o.expiresInMs / 1000)}s` : ""}</span>
              </li>
            {/each}
          </ul>
        </div>
      </div>
    {/if}

    <!-- ============================ RULES ============================ -->
    {#if view === "rules"}
      <div class="tab">
        <div class="page-head">
          <h1>Rules</h1>
          <span class="subtitle">Map events to light behavior</span>
        </div>

        <div class="card">
          <div class="grid2">
            <label class="field">
              <span class="label">Scene</span>
              <div class="row">
                <input list="profiles" bind:value={activeProfile} />
                <datalist id="profiles">{#each profiles as p}<option value={p}></option>{/each}</datalist>
                <button onclick={changeProfile}>Activate</button>
              </div>
            </label>
            <div class="field">
              <span class="label">Quiet hours</span>
              <div class="row">
                <Toggle size="sm" checked={quietEnabled} onchange={() => { quietEnabled = !quietEnabled; saveQuietHours(); }} label="Quiet hours" />
                <input aria-label="Quiet hours start" type="time" bind:value={quietStart} onchange={saveQuietHours} />
                <span class="dim">to</span>
                <input aria-label="Quiet hours end" type="time" bind:value={quietEnd} onchange={saveQuietHours} />
              </div>
            </div>
          </div>
        </div>

        <div class="section-head">
          <h2>Triggers</h2>
          <button class="pill-btn" onclick={addRule}>+ Add rule</button>
        </div>
        <p class="hint">Drag to reorder — higher wins when several fire at once. Expand a rule to edit it.</p>

        <datalist id="known-sources"><option value="cli">lightctl / terminal</option><option value="system">screen lock etc.</option><option value="device">the strip's connection</option></datalist>

        {#snippet ruleEditor()}
          {#if editingTrigger}
            <label class="field"><span class="label">Name</span><input bind:value={editingTrigger.name} placeholder="rule name…" /></label>
            <label class="field"><span class="label">Animation</span><select bind:value={editingTrigger.animationId}>{#each animations as a (a.id)}<option value={a.id}>{a.name}</option>{/each}</select></label>
            <div class="field"><span class="label">On event</span><div class="row"><input class="narrow" placeholder="source" list="known-sources" bind:value={editingTrigger.source} /><input class="narrow" placeholder="type" bind:value={editingTrigger.eventType} /></div></div>
            <label class="field"><span class="label">Clear on</span><input placeholder="event type (optional)" bind:value={editingTrigger.clearEventType} /></label>
            <label class="field"><span class="label">Hold (s)</span><input class="narrow" type="number" placeholder="animation default" value={editingTrigger.durationMs != null ? editingTrigger.durationMs / 1000 : ""} onchange={(e) => { const v = (e.target as HTMLInputElement).value; editingTrigger!.durationMs = v === "" ? null : Math.round(parseFloat(v) * 1000); }} /></label>
            <label class="field"><span class="label">Scene</span><input list="profiles" bind:value={editingTrigger.policy.profile} /><small class="dim">Use * for every scene</small></label>
            <div class="field"><span class="label">Payload condition</span><div class="row"><input placeholder="status or user.name" bind:value={editingTrigger.policy.payloadPath} /><input placeholder="expected JSON/value" bind:value={conditionValue} /></div></div>
            <label class="field"><span class="label">Cooldown (s)</span><input class="narrow" type="number" min="0" value={editingTrigger.policy.cooldownMs != null ? editingTrigger.policy.cooldownMs / 1000 : ""} onchange={(e) => { const v = (e.target as HTMLInputElement).value; editingTrigger!.policy.cooldownMs = v ? Math.round(Number(v) * 1000) : null; }} /></label>
            <div class="row"><button class="primary" onclick={saveTrigger}>Save rule</button><button onclick={cancelRuleEdit}>Cancel</button>{#if editingTrigger.id !== 0}<button class="link danger" onclick={() => deleteTrigger(editingTrigger!.id)}>Delete</button>{/if}</div>
          {/if}
        {/snippet}

        <div class="rules-list">
          {#each triggers as t, i (t.id)}
            <!-- svelte-ignore a11y_no_static_element_interactions -->
            <div class="rule-card" class:disabled={!t.enabled} class:dragging={dragIndex === i} class:expanded={expandedRuleId === t.id}
              draggable="true" ondragstart={(e) => onTriggerDragStart(e, i)} ondragover={(e) => onTriggerDragOver(e, i)} ondragend={onTriggerDragEnd}>
              <div class="rule-row">
                <span class="handle" title="Drag to reorder">⠿</span>
                <Toggle size="sm" checked={t.enabled} onchange={() => toggleTrigger(t)} label={`Enable ${t.name}`} />
                <span class="color-dot" style="--c:{ruleColor(t)}"></span>
                <div class="rule-meta grow">
                  <div class="rule-name">{t.name}</div>
                  <div class="rule-desc dim">on {t.source}/{t.eventType}{t.clearEventType ? ` until ${t.clearEventType}` : ""} → {ruleAnim(t)?.name ?? "?"}</div>
                </div>
                <span class="pattern-pill">{rulePattern(t)}</span>
                <button class="chevron" aria-label="Edit rule" onclick={() => toggleExpandRule(t)}>{expandedRuleId === t.id ? "⌃" : "⌄"}</button>
              </div>
              {#if expandedRuleId === t.id}
                <div class="rule-expanded">{@render ruleEditor()}</div>
              {/if}
            </div>
          {/each}

          {#if editingTrigger && editingTrigger.id === 0}
            <div class="rule-card expanded">
              <div class="rule-row">
                <span class="handle spacer"></span>
                <span class="color-dot" style="--c:{ruleColor(editingTrigger)}"></span>
                <div class="rule-meta grow"><div class="rule-name">{editingTrigger.name.trim() || "New rule"}</div><div class="rule-desc dim">configure below</div></div>
              </div>
              <div class="rule-expanded">{@render ruleEditor()}</div>
            </div>
          {/if}

          <div class="rule-card idle-card">
            <div class="rule-row">
              <span class="handle spacer"></span>
              <div class="rule-meta grow"><div class="rule-name">Idle</div><div class="rule-desc dim">when nothing above is active</div></div>
              <select bind:value={idleSelection} onchange={onIdleChange} title="What the strip shows when idle">
                {#if idleSelection == null}<option value={null}>Built-in rainbow</option>{/if}
                {#each animations as a (a.id)}<option value={a.id}>{a.name}</option>{/each}
                <option value="claude_usage">Claude usage (session | weekly)</option>
              </select>
            </div>
          </div>
        </div>

        <!-- Animations library (same expand-to-configure pattern) + manual control. -->
        <div class="section-head"><h2>Animations</h2><button class="pill-btn" onclick={addAnimation}>+ New</button></div>
        <p class="hint">Expand an animation to configure it.</p>

        {#snippet animEditor()}
          {#if editingAnimation}
            <label class="field"><span class="label">Name</span><input bind:value={editingAnimation.name} placeholder="animation name…" /></label>
            <label class="field"><span class="label">Effect</span><select bind:value={eaEffect}>{#each EDITOR_EFFECTS as e}<option value={e}>{e}</option>{/each}</select></label>
            {#if eaEffect === "keyframes"}
              <div class="field"><span class="label">Stops</span>
                <div class="row wrap">
                  {#each eaKeyframes as _, i}
                    <span class="stop"><input type="color" bind:value={eaKeyframes[i]} /><button class="link" onclick={() => (eaKeyframes = eaKeyframes.filter((_, j) => j !== i))}>✕</button></span>
                  {/each}
                  <button onclick={() => (eaKeyframes = [...eaKeyframes, eaKeyframes.at(-1) ?? "#ff4000"])}>+ stop</button>
                </div>
              </div>
            {:else if COLOR_EFFECTS.has(eaEffect)}
              <div class="field"><span class="label">Color</span><div class="row"><input type="color" bind:value={eaColorHex} />{#if TWO_COLOR_EFFECTS.has(eaEffect)}<span class="dim">2nd</span><input type="color" bind:value={eaColor2Hex} />{/if}</div></div>
            {/if}
            {#if eaEffect in SPEED_LABELS}
              <label class="field"><span class="label">{SPEED_LABELS[eaEffect]}</span><input class="slider" type="range" min="0" max="1" step="0.05" bind:value={eaSpeed} style="--fill:{pct(eaSpeed, 1)}%" /></label>
            {/if}
            {#if eaEffect !== "off"}
              <label class="field"><span class="label">Brightness</span><input class="slider" type="range" min="0" max="100" step="1" bind:value={eaLevelPct} style="--fill:{pct(eaLevelPct, 100)}%" /><span class="value">{eaLevelPct}%</span></label>
            {/if}
            <label class="field"><span class="label">Length (s)</span><input class="narrow" type="number" min="0" step="0.5" placeholder="forever" bind:value={eaDurationS} /></label>
            <div class="row"><button class="primary" onclick={saveAnimationEdit} disabled={!editingAnimation.name.trim()}>Save</button><button onclick={tryEditorOnStrip}>Try on strip</button><button onclick={cancelAnimEdit}>Cancel</button></div>
          {/if}
        {/snippet}

        <div class="rules-list">
          {#each animations as a (a.id)}
            <div class="rule-card" class:expanded={expandedAnimId === a.id}>
              <div class="rule-row">
                <AnimThumb spec={a.spec} />
                <div class="rule-meta grow"><div class="rule-name">{a.name}</div><div class="rule-desc dim">{a.spec.effect}{a.durationMs != null ? ` · ${a.durationMs / 1000}s` : ""}{a.builtin ? " · built-in" : ""}</div></div>
                <button class="link" onclick={() => applyAnimation(a.id)}>Apply</button>
                {#if !a.builtin}<button class="link danger" aria-label={`Delete ${a.name}`} onclick={() => deleteAnimation(a.id)}>✕</button>{/if}
                <button class="chevron" aria-label="Edit animation" onclick={() => toggleExpandAnim(a)}>{expandedAnimId === a.id ? "⌃" : "⌄"}</button>
              </div>
              {#if expandedAnimId === a.id}
                <div class="rule-expanded">{@render animEditor()}</div>
              {/if}
            </div>
          {/each}

          {#if editingAnimation && editingAnimation.id === 0}
            <div class="rule-card expanded">
              <div class="rule-row"><span class="handle spacer"></span><div class="rule-meta grow"><div class="rule-name">{editingAnimation.name.trim() || "New animation"}</div><div class="rule-desc dim">configure below</div></div></div>
              <div class="rule-expanded">{@render animEditor()}</div>
            </div>
          {/if}
        </div>

        <div class="section-head"><h2>Manual control</h2>{#if manualActive}<button class="pill-btn" onclick={releaseManual}>Release</button>{/if}</div>
        <div class="card">
          <label class="field"><span class="label">Effect</span><select bind:value={effect}>{#each EFFECTS as e}<option value={e}>{e}</option>{/each}</select></label>
          {#if COLOR_EFFECTS.has(effect)}
            <div class="field"><span class="label">Color</span><div class="row"><input type="color" bind:value={colorHex} />{#if TWO_COLOR_EFFECTS.has(effect)}<span class="dim">2nd</span><input type="color" bind:value={color2Hex} />{/if}</div></div>
          {/if}
          {#if effect in SPEED_LABELS}
            <label class="field"><span class="label">{SPEED_LABELS[effect]}</span><input class="slider" type="range" min="0" max="1" step="0.05" bind:value={speed} style="--fill:{pct(speed, 1)}%" /></label>
          {/if}
          {#if effect !== "off"}
            <label class="field"><span class="label">Brightness</span><input class="slider" type="range" min="0" max="100" step="1" bind:value={levelPct} style="--fill:{pct(levelPct, 100)}%" /><span class="value">{levelPct}%</span></label>
          {/if}
          {#if effect === "progress" || effect === "dual_progress"}
            <label class="field"><span class="label">{effect === "dual_progress" ? "Left %" : "Fill %"}</span><input class="slider" type="range" min="0" max="100" step="1" bind:value={progressPct} style="--fill:{pct(progressPct, 100)}%" /><span class="value">{progressPct}%</span></label>
          {/if}
          {#if effect === "dual_progress"}
            <label class="field"><span class="label">Right %</span><input class="slider" type="range" min="0" max="100" step="1" bind:value={progress2Pct} style="--fill:{pct(progress2Pct, 100)}%" /><span class="value">{progress2Pct}%</span></label>
          {/if}
          <div class="row">
            <button class="primary" onclick={applyManual}>Apply</button>
            {#if manualSaveName === null}
              <button onclick={() => (manualSaveName = "")}>Save as animation</button>
            {:else}
              <!-- svelte-ignore a11y_autofocus -->
              <input placeholder="animation name…" autofocus bind:value={manualSaveName} onkeydown={(e) => e.key === "Enter" && saveManualAsAnimation()} />
              <button class="primary" onclick={saveManualAsAnimation} disabled={!manualSaveName.trim()}>Save</button>
              <button onclick={() => (manualSaveName = null)}>Cancel</button>
            {/if}
          </div>
        </div>

        <!-- Schedules fold in below rules. -->
        <div class="section-head"><h2>Schedules</h2><button class="pill-btn" onclick={() => (editingSchedule = newSchedule())}>+ Add</button></div>
        <div class="card">
          {#if schedules.length === 0 && !editingSchedule}
            <p class="hint">Daily clock actions: emit a time/* event (pair with a rule), or swap the idle animation at a set time.</p>
          {/if}
          <ul class="mini-list">
            {#each schedules as s (s.id)}
              <li class:disabled={!s.enabled}>
                <span class="grow"><strong>{s.name}</strong> <small class="dim">at {s.time} → {#if s.action === "emit"}emit time/{s.eventType}{:else}idle = {animations.find((a) => a.id === s.animationId)?.name ?? "?"}{/if}</small></span>
                <Toggle size="sm" checked={s.enabled} onchange={() => toggleSchedule(s)} label={`Enable ${s.name}`} />
                <button class="link" onclick={() => (editingSchedule = { ...s })}>Edit</button>
                <button class="link danger" onclick={() => deleteSchedule(s.id)}>✕</button>
              </li>
            {/each}
          </ul>
        </div>
        {#if editingSchedule}
          <div class="card editor">
            <h3>{editingSchedule.id === 0 ? "New schedule" : "Edit schedule"}</h3>
            <label class="field"><span class="label">Name</span><input bind:value={editingSchedule.name} /></label>
            <div class="field"><span class="label">At</span><div class="row"><input type="time" bind:value={editingSchedule.time} /><select bind:value={editingSchedule.action}><option value="emit">emit an event</option><option value="idle">swap idle animation</option></select></div></div>
            {#if editingSchedule.action === "emit"}
              <label class="field"><span class="label">Event type</span><input class="narrow" placeholder="evening" bind:value={editingSchedule.eventType} /></label>
            {:else}
              <label class="field"><span class="label">Idle becomes</span><select bind:value={editingSchedule.animationId}>{#each animations as a (a.id)}<option value={a.id}>{a.name}</option>{/each}</select></label>
            {/if}
            <div class="row"><button class="primary" onclick={saveSchedule}>Save schedule</button><button onclick={() => (editingSchedule = null)}>Cancel</button></div>
          </div>
        {/if}
      </div>
    {/if}

    <!-- ============================ DEVICES ============================ -->
    {#if view === "devices"}
      <div class="tab">
        <div class="page-head"><h1>Devices</h1></div>

        <div class="card">
          <div class="grid2">
            <label class="field">
              <span class="label">Serial port</span>
              {#if candidates.length > 0}
                <select value={status.port} onchange={(e) => adopt((e.target as HTMLSelectElement).value)}>
                  {#if status.port}<option value={status.port}>{status.port}</option>{/if}
                  {#each candidates.filter((c) => c.port !== status.port) as c}<option value={c.port}>{c.port}</option>{/each}
                </select>
              {:else}
                <input class="mono" value={status.port ?? "— none detected —"} readonly />
              {/if}
            </label>
            <div class="field">
              <span class="label">Orientation</span>
              <div class="row">
                <button class="seg" class:active={!reversed} onclick={() => setOrientation(false)}>Left → Right</button>
                <button class="seg" class:active={reversed} onclick={() => setOrientation(true)}>Right → Left</button>
              </div>
            </div>
          </div>
        </div>

        <div class="card">
          <span class="label">POWER &amp; CALIBRATION</span>
          <div class="cal-row">
            <div class="row between"><span>Brightness cap</span><span class="mono dim">{pct(brightnessCap)}%</span></div>
            <input class="slider" type="range" min="26" max="255" step="1" bind:value={brightnessCap} onchange={onBrightnessCap} style="--fill:{pct(brightnessCap)}%" />
          </div>
          {#each ["Red", "Green", "Blue"] as name, ch}
            <div class="cal-row">
              <div class="row between"><span>{name} gain</span><span class="mono dim">{pct(gain[ch])}%</span></div>
              <input class="slider" type="range" min="0" max="255" step="1" value={gain[ch]} oninput={(e) => (gain[ch] = Number((e.target as HTMLInputElement).value))} onchange={(e) => setGain(ch, Number((e.target as HTMLInputElement).value))} style="--fill:{pct(gain[ch])}%" />
            </div>
          {/each}
          <p class="hint">Calibration is stored on the app and re-sent to the strip on every connect. It applies to every effect.</p>
        </div>

        {#if status.connected}
          <div class="card row between"><span class="dim">Forget this device and stop auto-reconnecting.</span><button onclick={() => invoke("forget_device")}>Forget device</button></div>
        {/if}

        {#if knownDevices.length > 0}
          <div class="section-head"><h2>Known devices</h2></div>
          <div class="card">
            <ul class="mini-list">
              {#each knownDevices as device}
                <li><span class="grow"><strong>{device.serialNumber ?? device.lastPort}</strong> <small class="dim">fw {device.fwVersion} · {device.ledCount} LEDs · last seen {new Date(device.lastSeenMs).toLocaleString()}</small></span></li>
              {/each}
            </ul>
          </div>
        {/if}
      </div>
    {/if}

    <!-- ============================ SETTINGS ============================ -->
    {#if view === "settings"}
      <div class="tab">
        <div class="page-head"><h1>Settings</h1></div>

        <div class="card">
          <div class="setting-row">
            <div><div class="setting-name">Launch at login</div><div class="setting-desc dim">Start Luminode automatically when you sign in.</div></div>
            <Toggle checked={autostart} onchange={toggleAutostart} label="Launch at login" />
          </div>
          <div class="setting-row">
            <div><div class="setting-name">Start minimized</div><div class="setting-desc dim">Open directly to the menu bar without a window.</div></div>
            <Toggle checked={startMinimized} onchange={toggleStartMinimized} label="Start minimized" />
          </div>
        </div>

        <div class="card">
          <div class="setting-row">
            <div><div class="setting-name">Idle dimming</div><div class="setting-desc dim">Dim the strip after a stretch of inactivity.</div></div>
            <select bind:value={idleDimMinutes} onchange={onIdleDim}>
              <option value={5}>5 min</option><option value={10}>10 min</option><option value={20}>20 min</option><option value={30}>30 min</option><option value={0}>Never</option>
            </select>
          </div>
        </div>

        <div class="section-head"><h2>Appearance</h2></div>
        <div class="card">
          <div class="setting-row">
            <div><div class="setting-name">Theme</div></div>
            <select bind:value={theme} onchange={() => applyTheme(theme)}><option value="system">System</option><option value="dark">Dark</option><option value="light">Light</option></select>
          </div>
          <div class="setting-row">
            <div><div class="setting-name">Accent</div><div class="setting-desc dim">Used for selection, toggles, and sliders.</div></div>
            <div class="row">
              {#each RULE_PALETTE as hex}
                <button class="swatch" class:sel={accent.toLowerCase() === hex} style="--c:{hex}" aria-label={`Accent ${hex}`} onclick={() => onAccent(hex)}></button>
              {/each}
            </div>
          </div>
        </div>

        <div class="section-head"><h2>Integrations</h2><button class="pill-btn" onclick={refreshHealth}>Refresh</button></div>
        <div class="card">
          <div class="health-grid">
            {#each integrationHealth as item (item.source)}
              <article class="health-card {item.status}">
                <strong>{item.source}</strong>
                <span class="dim">{item.status === "healthy" ? "Connected" : item.status === "error" ? "Needs attention" : "Not configured"}</span>
                {#if item.message}<small class="dim">{item.message}</small>{/if}
              </article>
            {/each}
          </div>
          <label class="field"><span class="label">Slack {slackSet ? "✓" : ""}</span><div class="row"><input type="password" placeholder={slackSet ? "token saved — paste to replace" : "xoxp- user token"} bind:value={slackInput} /><button onclick={() => saveSecret("slack_token", slackInput)}>Save</button></div></label>
          <label class="field"><span class="label">Calendar {calendarSet ? "✓" : ""}</span><div class="row"><input type="password" placeholder={calendarSet ? "URL saved — paste to replace" : "secret iCal (.ics) URL"} bind:value={calendarInput} /><button onclick={() => saveSecret("calendar_ics_url", calendarInput)}>Save</button></div></label>
          <details><summary>Available event sources</summary><div class="catalog-grid">{#each integrationCatalog as item (item.source)}<article><strong>{item.name}</strong><small class="dim">{item.source} · {item.setup}</small><small class="dim">{item.events.join(", ")}</small></article>{/each}</div></details>
        </div>

        <div class="section-head"><h2>Config</h2></div>
        <div class="card">
          <div class="row wrap">
            <button onclick={exportConfig}>Export…</button>
            <button onclick={importConfig}>Import…</button>
            {#if canUndoImport}<button onclick={undoImport}>Undo last import</button>{/if}
            <button onclick={exportDiagnostics}>Diagnostics…</button>
          </div>
          <small class="dim">Animations, rules, schedules &amp; idle — as JSON, by name.</small>
        </div>

        <details class="card advanced">
          <summary>Advanced — simulate &amp; event log</summary>
          <div class="field"><span class="label">Simulate</span><div class="row"><input class="narrow" placeholder="source" bind:value={simSource} /><input class="narrow" placeholder="type" bind:value={simType} /><input placeholder={'payload JSON (e.g. {"percent": 60})'} bind:value={simPayload} /><button onclick={simulate}>Fire</button></div></div>
          <div class="section-head"><h3>Event log</h3><button class="link" onclick={clearHistory} disabled={events.length === 0}>Clear</button></div>
          <ul class="mini-list log">
            {#each events as ev}
              <li><small class="dim mono">{fmtTime(ev.ts)}</small><span class="grow"><strong>{ev.source}</strong>/{ev.type} <small class="dim">{fmtPayload(ev.payload)}</small></span><button class="link" onclick={() => replayEvent(ev)}>Replay</button></li>
            {/each}
          </ul>
        </details>

        <div class="about"><span class="dim">Luminode {status.fwVersion ? `· firmware ${status.fwVersion}` : ""}</span></div>
      </div>
    {/if}
  </main>
</div>

{#if toast}
  <div class="toast {toast.kind}" role={toast.kind === "error" ? "alert" : "status"}>{toast.message}</div>
{/if}

<style>
  /* ============================ design tokens ============================ */
  :global(body) {
    --bg: oklch(0.13 0.005 260);
    --window-bg: oklch(0.17 0.006 260);
    --sidebar-bg: oklch(0.15 0.006 260);
    --surface: oklch(0.21 0.006 260);
    --surface-raised: oklch(0.25 0.006 260);
    --border: rgba(255, 255, 255, 0.07);
    --border-strong: rgba(255, 255, 255, 0.11);
    --text: oklch(0.94 0.003 260);
    --muted: oklch(0.62 0.008 260);
    --text-dim: oklch(0.5 0.008 260);
    --text-faint: oklch(0.45 0.01 260);
    --accent: #5ec8f2;
    --on-accent: #0a0a0a;
    --success: oklch(0.72 0.17 150);
    --toggle-off: oklch(0.28 0.006 260);
    --danger: #f87171;
    --input-bg: oklch(0.15 0.006 260);
    --track: oklch(0.15 0.006 260);
    margin: 0;
    font-family: -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif;
    background: var(--window-bg);
    color: var(--text);
    font-size: 14px;
  }
  :global(html[data-theme="light"] body) {
    --bg: oklch(0.95 0.004 260);
    --window-bg: oklch(0.99 0.002 260);
    --sidebar-bg: oklch(0.97 0.004 260);
    --surface: oklch(1 0 0);
    --surface-raised: oklch(0.95 0.004 260);
    --border: rgba(0, 0, 0, 0.1);
    --border-strong: rgba(0, 0, 0, 0.16);
    --text: oklch(0.24 0.01 260);
    --muted: oklch(0.44 0.01 260);
    --text-dim: oklch(0.54 0.01 260);
    --text-faint: oklch(0.6 0.01 260);
    --on-accent: #ffffff;
    --toggle-off: oklch(0.82 0.008 260);
    --input-bg: oklch(0.97 0.004 260);
    --track: oklch(0.88 0.006 260);
  }
  @media (prefers-color-scheme: light) {
    :global(html[data-theme="system"] body), :global(html:not([data-theme]) body) {
      --bg: oklch(0.95 0.004 260);
      --window-bg: oklch(0.99 0.002 260);
      --sidebar-bg: oklch(0.97 0.004 260);
      --surface: oklch(1 0 0);
      --surface-raised: oklch(0.95 0.004 260);
      --border: rgba(0, 0, 0, 0.1);
      --border-strong: rgba(0, 0, 0, 0.16);
      --text: oklch(0.24 0.01 260);
      --muted: oklch(0.44 0.01 260);
      --text-dim: oklch(0.54 0.01 260);
      --text-faint: oklch(0.6 0.01 260);
      --on-accent: #ffffff;
      --toggle-off: oklch(0.82 0.008 260);
    }
  }

  .shell { display: flex; height: 100vh; overflow: hidden; }

  /* ============================ sidebar ============================ */
  .sidebar {
    width: 232px; flex-shrink: 0; background: var(--sidebar-bg);
    border-right: 1px solid var(--border);
    display: flex; flex-direction: column; justify-content: space-between;
    padding: 20px 14px;
  }
  .brand { display: flex; align-items: center; gap: 10px; padding: 6px 8px 22px; }
  .logo { width: 26px; height: 26px; border-radius: 8px; background: var(--accent); display: flex; align-items: center; justify-content: center; flex-shrink: 0; }
  .logo-dot { width: 8px; height: 8px; border-radius: 50%; background: rgba(255, 255, 255, 0.9); }
  .brand-name { font-size: 15px; font-weight: 700; }
  nav { display: flex; flex-direction: column; gap: 2px; }
  .nav-row { display: flex; align-items: center; gap: 10px; padding: 9px 12px; border: 0; border-radius: 8px; background: transparent; cursor: pointer; text-align: left; color: var(--muted); }
  .nav-row.selected { background: var(--surface-raised); color: var(--text); }
  .nav-dot { width: 7px; height: 7px; border-radius: 50%; background: oklch(0.4 0.01 260); flex-shrink: 0; }
  .nav-row.selected .nav-dot { background: var(--accent); }
  .nav-label { font-size: 13.5px; font-weight: 500; }
  .nav-row.selected .nav-label { font-weight: 600; }
  .sidebar-footer { border-top: 1px solid var(--border); padding-top: 14px; display: flex; flex-direction: column; gap: 6px; }
  .conn { display: flex; align-items: center; gap: 8px; font-size: 11.5px; color: var(--muted); }
  .conn-dot { width: 7px; height: 7px; border-radius: 50%; background: var(--text-faint); flex-shrink: 0; }
  .conn-dot.ok { background: var(--success); }
  .conn-detail { font-family: ui-monospace, "SF Mono", Menlo, monospace; font-size: 10.5px; color: var(--text-faint); padding-left: 15px; }

  /* ============================ content ============================ */
  .content { flex: 1; overflow-y: auto; padding: 32px 40px; }
  .tab { max-width: 720px; margin: 0 auto; display: flex; flex-direction: column; gap: 18px; }
  .page-head { display: flex; align-items: center; justify-content: space-between; }
  h1 { font-size: 22px; font-weight: 700; margin: 0; }
  .subtitle { font-size: 12px; color: var(--text-dim); }
  .power { display: flex; align-items: center; gap: 10px; }
  .power-label { font-size: 12.5px; color: var(--muted); }

  .section-head { display: flex; align-items: center; justify-content: space-between; margin-top: 6px; }
  h2 { font-size: 15px; font-weight: 700; margin: 0; }
  h3 { font-size: 13px; font-weight: 600; margin: 0; }
  .hint { font-size: 12px; color: var(--text-dim); margin: -6px 0 0; }

  .card { background: var(--surface); border: 1px solid var(--border); border-radius: 14px; padding: 20px; display: flex; flex-direction: column; gap: 14px; }
  .card-head { display: flex; align-items: center; justify-content: space-between; }
  .label { font-size: 11.5px; font-weight: 600; color: var(--text-dim); letter-spacing: 0.03em; text-transform: uppercase; }
  .value { font-size: 14px; font-weight: 700; }

  .banner { display: flex; flex-direction: column; gap: 5px; padding: 12px 16px; border-radius: 10px; border: 1px solid var(--border-strong); }
  .banner.warn { border-color: #9b3f45; background: rgba(155, 63, 69, 0.15); }
  .banner.notice { border-color: #655126; background: rgba(101, 81, 38, 0.15); }
  .banner strong { font-size: 13.5px; }
  .banner span { font-size: 12.5px; color: var(--muted); }

  /* preview strip */
  /* Single row that scales to the window width — the LEDs share the available
     space (capped so they stay dot-sized on a wide window), never wrapping. */
  .strip { display: flex; flex-wrap: nowrap; gap: clamp(2px, 0.7%, 6px); justify-content: center; align-items: center; padding: 8px 0; }
  .led { flex: 1 1 0; min-width: 0; max-width: 15px; aspect-ratio: 1; border-radius: 50%; background: oklch(0.3 0.006 260); opacity: 0.35; }
  .led.lit { background: var(--c); box-shadow: 0 0 9px var(--c); opacity: 1; }

  /* active trigger */
  .active-name { display: flex; align-items: center; gap: 8px; font-size: 14.5px; }
  .active-dot { width: 9px; height: 9px; border-radius: 50%; background: var(--accent); }
  .bar { height: 6px; border-radius: 3px; background: var(--track); overflow: hidden; }
  .bar-fill { height: 100%; background: var(--accent); border-radius: 3px; transition: width 0.2s ease; }

  .mini-list { list-style: none; margin: 0; padding: 0; display: flex; flex-direction: column; gap: 2px; }
  .mini-list li { display: flex; align-items: center; gap: 8px; padding: 6px 0; }
  .mini-list li.disabled { opacity: 0.5; }
  .mini-list.log { max-height: 260px; overflow-y: auto; }
  .grow { flex: 1; min-width: 0; }
  .dim { color: var(--text-dim); }
  .mono { font-family: ui-monospace, "SF Mono", Menlo, monospace; }
  .row { display: flex; align-items: center; gap: 10px; }
  .row.wrap { flex-wrap: wrap; }
  .row.between { justify-content: space-between; }

  /* rules */
  .rules-list { display: flex; flex-direction: column; gap: 12px; }
  .rule-card { background: var(--surface); border: 1px solid var(--border); border-radius: 12px; padding: 16px 18px; }
  .rule-card.disabled { opacity: 0.6; }
  .rule-card.dragging { opacity: 0.4; background: var(--surface-raised); }
  .rule-card.expanded { border-color: color-mix(in srgb, var(--accent) 45%, var(--border-strong)); }
  .rule-card.idle-card { border-style: dashed; }
  .rule-row { display: flex; align-items: center; gap: 14px; }
  .handle { cursor: grab; color: var(--text-faint); user-select: none; flex: none; }
  .handle.spacer { visibility: hidden; }
  .color-dot { width: 11px; height: 11px; border-radius: 50%; background: var(--c); box-shadow: 0 0 6px var(--c); flex: none; }
  .rule-meta { line-height: 1.35; }
  .rule-name { font-size: 14.5px; font-weight: 600; }
  .rule-desc { font-size: 12px; }
  .pattern-pill { font-size: 10.5px; font-weight: 700; letter-spacing: 0.04em; text-transform: uppercase; color: var(--muted); background: var(--surface-raised); padding: 5px 10px; border-radius: 6px; }
  .chevron { width: 26px; height: 26px; border: 0; border-radius: 7px; background: transparent; color: var(--muted); cursor: pointer; flex: none; }
  .rule-expanded { display: flex; flex-direction: column; gap: 16px; margin-top: 16px; padding-top: 16px; border-top: 1px solid var(--border); }
  .swatch { width: 22px; height: 22px; border-radius: 50%; border: 0; background: var(--c); cursor: pointer; padding: 0; }
  .swatch.sel { box-shadow: 0 0 0 2px var(--surface), 0 0 0 4px var(--c); }

  /* devices */
  .grid2 { display: grid; grid-template-columns: 1fr 1fr; gap: 20px 32px; }
  .cal-row { display: flex; flex-direction: column; gap: 8px; }
  .seg { flex: 1; padding: 8px 14px; border: 0; border-radius: 8px; font-size: 12.5px; font-weight: 600; cursor: pointer; background: var(--surface-raised); color: var(--muted); }
  .seg.active { background: var(--accent); color: var(--on-accent); }

  /* settings */
  .setting-row { display: flex; align-items: center; justify-content: space-between; gap: 16px; }
  .card > .setting-row + .setting-row { border-top: 1px solid var(--border); padding-top: 14px; }
  .setting-name { font-size: 13.5px; font-weight: 600; }
  .setting-desc { font-size: 12px; margin-top: 2px; }
  .health-grid { display: grid; grid-template-columns: repeat(auto-fit, minmax(150px, 1fr)); gap: 8px; }
  .health-card { display: flex; flex-direction: column; gap: 3px; padding: 12px; border: 1px solid var(--border); border-radius: 9px; }
  .health-card.healthy { border-color: var(--success); }
  .health-card.error { border-color: var(--danger); }
  .catalog-grid { display: grid; grid-template-columns: repeat(auto-fit, minmax(180px, 1fr)); gap: 8px; margin-top: 10px; }
  .catalog-grid article { display: flex; flex-direction: column; gap: 3px; padding: 10px; background: var(--surface-raised); border-radius: 8px; }
  .advanced summary { cursor: pointer; font-weight: 600; font-size: 13px; }
  .about { text-align: center; font-size: 12px; padding: 8px; }

  /* fields + form controls */
  .field { display: flex; flex-direction: column; gap: 6px; }
  .editor { gap: 12px; }
  .stop { display: inline-flex; align-items: center; gap: 2px; }

  input, select, button {
    font-family: inherit; font-size: 13px; color: var(--text);
    background: var(--input-bg); border: 1px solid var(--border-strong);
    border-radius: 8px; padding: 9px 10px;
  }
  input.mono { font-family: ui-monospace, "SF Mono", Menlo, monospace; }
  input.narrow { width: 110px; }
  input[type="color"] { padding: 1px; width: 42px; height: 30px; }
  input[type="time"] { width: auto; }
  button { background: var(--surface-raised); border-color: transparent; cursor: pointer; font-weight: 600; }
  button:hover { filter: brightness(1.12); }
  button:disabled { opacity: 0.45; cursor: default; }
  button.primary { background: var(--accent); color: var(--on-accent); }
  button.pill-btn { padding: 7px 14px; border-radius: 8px; font-size: 12.5px; }
  button.link { background: transparent; border: 0; padding: 4px 6px; color: var(--muted); font-size: 12.5px; }
  button.link:hover { color: var(--text); filter: none; }
  button.link.danger { color: var(--danger); }
  :global(button:focus-visible), :global(input:focus-visible), :global(select:focus-visible), :global([draggable="true"]:focus-visible) {
    outline: 3px solid color-mix(in srgb, var(--accent) 65%, white); outline-offset: 2px;
  }

  /* range sliders with accent fill */
  input.slider { -webkit-appearance: none; appearance: none; height: 4px; padding: 0; border: 0; border-radius: 2px; background: linear-gradient(to right, var(--accent) var(--fill, 50%), var(--toggle-off) var(--fill, 50%)); }
  input.slider::-webkit-slider-thumb { -webkit-appearance: none; appearance: none; width: 14px; height: 14px; border-radius: 50%; background: #fff; box-shadow: 0 1px 3px rgba(0, 0, 0, 0.4); cursor: pointer; }
  input.slider::-moz-range-thumb { width: 14px; height: 14px; border: 0; border-radius: 50%; background: #fff; cursor: pointer; }

  .picker .row { margin-top: 4px; }

  .toast { position: fixed; right: 20px; bottom: 20px; max-width: 420px; padding: 12px 16px; border-radius: 9px; background: var(--surface-raised); color: var(--text); box-shadow: 0 8px 32px rgba(0, 0, 0, 0.5); z-index: 100; border: 1px solid var(--border-strong); }
  .toast.error { border-color: var(--danger); }

  @media (prefers-reduced-motion: reduce) {
    .led { box-shadow: none !important; }
    .bar-fill { transition: none; }
  }
</style>
