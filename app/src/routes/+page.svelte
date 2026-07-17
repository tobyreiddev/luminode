<script lang="ts">
  import { invoke } from "@tauri-apps/api/core";
  import { listen, type UnlistenFn } from "@tauri-apps/api/event";
  import { open as openDialog, save as saveDialog } from "@tauri-apps/plugin-dialog";
  import { onMount } from "svelte";
  import {
    rgbToHex,
    hexToRgb,
    type ActiveState,
    type AnimSpec,
    type Animation,
    type BusEvent,
    type DeviceStatus,
    type PortCandidate,
    type Schedule,
    type Trigger,
  } from "$lib/types";

  const NUM_LEDS = 33;
  const EFFECTS = ["off", "solid", "breathe", "rainbow", "chase", "sparkle", "flash", "gradient", "progress", "dual_progress"];
  // "keyframes" needs its stop editor, so it's only offered where one exists.
  const EDITOR_EFFECTS = [...EFFECTS, "keyframes"];
  const TWO_COLOR_EFFECTS = new Set(["flash", "gradient", "progress", "dual_progress"]);
  type View = "home" | "automations" | "integrations";
  let view = $state<View>("home");
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
  let active = $state<ActiveState>({ activeName: "…", snoozedUntilMs: null, overlays: [] });
  let events = $state<BusEvent[]>([]);

  // --- persisted collections ---
  let animations = $state<Animation[]>([]);
  let triggers = $state<Trigger[]>([]);
  let schedules = $state<Schedule[]>([]);
  // Animation id, or "claude_usage" for the live session/weekly gauge.
  let idleSelection = $state<number | "claude_usage" | null>(null);

  // --- manual control form ---
  let effect = $state("solid");
  let colorHex = $state("#0050ff");
  let color2Hex = $state("#000000");
  let speed = $state(0.5);
  let progressPct = $state(50);
  let progress2Pct = $state(50);
  let brightness = $state(64);
  let manualActive = $derived(active.overlays.some((o) => o.key === "manual"));
  // Non-null while the "save as animation" name field is showing. window.prompt()
  // is a no-op in the Tauri webview, so the name has to be collected inline.
  let manualSaveName = $state<string | null>(null);

  // --- animation editor (create from scratch or edit any animation, builtins included) ---
  let editingAnimation = $state<{ id: number; name: string } | null>(null);
  let eaEffect = $state("solid");
  let eaColorHex = $state("#0050ff");
  let eaColor2Hex = $state("#000000");
  let eaSpeed = $state(0.5);
  let eaKeyframes = $state<string[]>([]);
  let eaDurationS = $state(""); // seconds as text; "" = runs forever

  // --- trigger editor ---
  let editingTrigger = $state<Trigger | null>(null);

  // --- simulate form ---
  let simSource = $state("cli");
  let simType = $state("run_failed");
  let simPayload = $state("");

  // --- integrations (secrets live in the OS keychain, never the DB) ---
  let slackSet = $state(false);
  let calendarSet = $state(false);
  let slackInput = $state("");
  let calendarInput = $state("");
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
  }

  function manualSpec(): AnimSpec {
    return {
      effect,
      color: hexToRgb(colorHex),
      color2: TWO_COLOR_EFFECTS.has(effect) ? hexToRgb(color2Hex) : null,
      speed,
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

  // -- animations --
  function startEditAnimation(a: Animation | null) {
    if (a) {
      editingAnimation = { id: a.id, name: a.name };
      eaEffect = a.spec.effect;
      eaColorHex = rgbToHex(a.spec.color);
      eaColor2Hex = a.spec.color2 ? rgbToHex(a.spec.color2) : "#000000";
      eaSpeed = a.spec.speed;
      eaKeyframes = (a.spec.keyframes ?? []).map(rgbToHex);
      eaDurationS = a.durationMs != null ? String(a.durationMs / 1000) : "";
    } else {
      editingAnimation = { id: 0, name: "" };
      eaEffect = "solid";
      eaColorHex = "#0050ff";
      eaColor2Hex = "#000000";
      eaSpeed = 0.5;
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
      progress: null,
      progress2: null,
      keyframes: eaEffect === "keyframes" ? eaKeyframes.map(hexToRgb) : null,
    };
  }
  async function saveAnimationEdit() {
    if (!editingAnimation || !editingAnimation.name.trim()) return;
    try {
      if (editingAnimation.id === 0) {
        await invoke("save_animation", {
          name: editingAnimation.name.trim(),
          spec: editorSpec(),
          durationMs: eaDurationMs(),
        });
      } else {
        await invoke("update_animation", {
          id: editingAnimation.id,
          name: editingAnimation.name.trim(),
          spec: editorSpec(),
          durationMs: eaDurationMs(),
        });
      }
    } catch (e) {
      alert(String(e));
      return;
    }
    editingAnimation = null;
    animations = await invoke("list_animations");
  }
  async function tryEditorOnStrip() {
    await invoke("set_manual", { spec: editorSpec() });
  }
  async function saveManualAsAnimation() {
    const name = manualSaveName?.trim();
    if (!name) return;
    try {
      await invoke("save_animation", { name, spec: manualSpec(), durationMs: null });
    } catch (e) {
      alert(String(e));
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
      // Id first, mode second: the swap becomes visible in one recompute.
      await invoke("set_idle_animation", { id: idleSelection });
      await invoke("set_idle_mode", { mode: "animation" });
    }
  }

  // -- triggers --
  // Priority is list order: top wins, idle is the immovable floor. New
  // triggers land on top (most visible; drag down to demote).
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
    };
  }
  async function saveTrigger() {
    if (!editingTrigger || !editingTrigger.name.trim() || !editingTrigger.eventType.trim()) return;
    const trigger = {
      ...editingTrigger,
      clearEventType: editingTrigger.clearEventType?.trim() || null,
    };
    await invoke("save_trigger", { trigger });
    editingTrigger = null;
    triggers = await invoke("list_triggers");
  }
  async function toggleTrigger(trigger: Trigger) {
    await invoke("save_trigger", { trigger: { ...trigger, enabled: !trigger.enabled } });
    triggers = await invoke("list_triggers");
  }
  async function deleteTrigger(id: number) {
    if (!confirm("Delete this trigger?")) return;
    await invoke("delete_trigger", { id });
    triggers = await invoke("list_triggers");
  }

  // Drag to reorder: rows swap live while dragging for feedback; the new
  // order is committed once on dragend (fires on drop *and* cancel).
  let dragIndex = $state<number | null>(null);
  function onTriggerDragStart(e: DragEvent, i: number) {
    dragIndex = i;
    if (e.dataTransfer) {
      e.dataTransfer.effectAllowed = "move";
      e.dataTransfer.setData("text/plain", String(i)); // Safari needs data set
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

  // -- schedules --
  let editingSchedule = $state<Schedule | null>(null);
  function newSchedule(): Schedule {
    return {
      id: 0,
      name: "",
      time: "18:00",
      action: "emit",
      eventType: "",
      animationId: animations[0]?.id ?? 1,
      enabled: true,
    };
  }
  async function saveSchedule() {
    if (!editingSchedule || !editingSchedule.name.trim()) return;
    if (editingSchedule.action === "emit" && !editingSchedule.eventType?.trim()) return;
    const schedule = {
      ...editingSchedule,
      eventType: editingSchedule.action === "emit" ? editingSchedule.eventType!.trim() : null,
      animationId: editingSchedule.action === "idle" ? editingSchedule.animationId : null,
    };
    await invoke("save_schedule", { schedule });
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
    const path = await saveDialog({
      defaultPath: "luminode-config.json",
      filters: [{ name: "Luminode config", extensions: ["json"] }],
    });
    if (!path) return;
    try {
      await invoke("export_config", { path });
    } catch (e) {
      reportError(e);
    }
  }
  async function importConfig() {
    const path = await openDialog({
      multiple: false,
      filters: [{ name: "Luminode config", extensions: ["json"] }],
    });
    if (typeof path !== "string") return;
    try {
      const summary: string = await invoke("import_config", { path });
      notify(summary);
    } catch (e) {
      reportError(e);
      return;
    }
    animations = await invoke("list_animations");
    triggers = await invoke("list_triggers");
    schedules = await invoke("list_schedules");
    await loadIdle();
  }
  async function exportDiagnostics() {
    const path = await saveDialog({ defaultPath: "luminode-diagnostics.json", filters: [{ name: "JSON", extensions: ["json"] }] });
    if (!path) return;
    try {
      await invoke("export_diagnostics", { path });
      notify("Redacted diagnostics exported");
    } catch (e) {
      reportError(e);
    }
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
      try {
        payload = JSON.parse(simPayload);
      } catch {
        alert("Payload is not valid JSON");
        return;
      }
    }
    await invoke("simulate_event", { source: simSource, eventType: simType, payload });
    notify("Event fired");
  }

  async function clearHistory() {
    if (!confirm("Clear the complete event history?")) return;
    try {
      await invoke("clear_events");
      events = [];
      notify("Event history cleared");
    } catch (e) {
      reportError(e);
    }
  }

  async function replayEvent(event: BusEvent) {
    try {
      await invoke("simulate_event", { source: event.source, eventType: event.type, payload: event.payload });
      notify(`Replayed ${event.source}/${event.type}`);
    } catch (e) {
      reportError(e);
    }
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

  onMount(() => {
    const unlisteners: Promise<UnlistenFn>[] = [
      listen<DeviceStatus>("device:status", (e) => (status = e.payload)),
      listen<PortCandidate[]>("device:candidates", (e) => (candidates = e.payload)),
      listen<string>("engine:frame", (e) => (frame = e.payload)),
      listen<ActiveState>("engine:active", (e) => (active = e.payload)),
      listen<BusEvent>("bus:event", (e) => {
        events = [e.payload, ...events].slice(0, 100);
        // A schedule may have swapped the idle animation — keep the
        // dropdown truthful.
        if (e.payload.source === "time") {
          loadIdle();
        }
      }),
    ];
    (async () => {
      try {
      status = await invoke("get_status");
      candidates = await invoke("list_candidates");
      animations = await invoke("list_animations");
      triggers = await invoke("list_triggers");
      schedules = await invoke("list_schedules");
      brightness = await invoke("get_brightness");
      await loadIdle();
      events = await invoke("recent_events", { limit: 50 });
      active = await invoke("get_active");
      slackSet = await invoke("has_secret", { name: "slack_token" });
      calendarSet = await invoke("has_secret", { name: "calendar_ics_url" });
      } catch (e) {
        reportError(`Could not load Luminode: ${e}`);
      }
    })();
    return () => {
      unlisteners.forEach((p) => p.then((un) => un()));
    };
  });
</script>

<main data-view={view}>
  <!-- ======= header: preview strip + status ======= -->
  <header>
    <div class="strip" title="Live preview of the LED strip">
      {#each pixels(frame) as px}
        <span class="led" style="background:{px}; box-shadow: 0 0 6px {px}"></span>
      {/each}
    </div>
    <div class="statusbar">
      <span class="pill {status.connected ? 'ok' : 'bad'}">
        {status.connected ? `Connected · ${status.port} · fw ${status.fwVersion} · protocol ${status.protocolVersion} · ${status.ledCount} LEDs` : "Disconnected — searching…"}
      </span>
      <span class="pill">Now showing: <strong>{active.activeName}</strong></span>
      {#if active.snoozedUntilMs}
        <button onclick={() => snooze(0)}>End snooze</button>
      {:else}
        <button onclick={() => snooze(30)}>Snooze 30 min</button>
      {/if}
      {#if status.connected}
        <button class="subtle" onclick={() => invoke("forget_device")}>Forget device</button>
      {/if}
    </div>
    {#if !status.connected && candidates.length > 1}
      <div class="picker">
        Multiple Arduinos found — pick yours:
        {#each candidates as c}
          <button onclick={() => adopt(c.port)}>{c.product ?? "Unknown"} · {c.port}</button>
        {/each}
      </div>
    {/if}
  </header>
  <nav class="tabs" aria-label="Main sections">
    <button class:active={view === "home"} aria-current={view === "home" ? "page" : undefined} onclick={() => (view = "home")}>Home</button>
    <button class:active={view === "automations"} aria-current={view === "automations" ? "page" : undefined} onclick={() => (view = "automations")}>Automations</button>
    <button class:active={view === "integrations"} aria-current={view === "integrations" ? "page" : undefined} onclick={() => (view = "integrations")}>Integrations & history</button>
  </nav>
  {#if !status.connected && candidates.length === 0}
    <aside class="onboarding">
      <strong>Connect your Luminode</strong>
      <span>Plug in the Arduino over USB. The app will discover it automatically, then you can test brightness and choose an idle animation.</span>
    </aside>
  {/if}

  <div class="columns">
    <!-- ======= manual control + animations ======= -->
    <section class="home-view">
      <h2>Manual control {#if manualActive}<button class="subtle" onclick={releaseManual}>release</button>{/if}</h2>
      <div class="field">
        <label for="effect">Effect</label>
        <select id="effect" bind:value={effect}>
          {#each EFFECTS as e}<option value={e}>{e}</option>{/each}
        </select>
      </div>
      <div class="field">
        <label for="color">Color</label>
        <input id="color" type="color" bind:value={colorHex} />
        {#if TWO_COLOR_EFFECTS.has(effect)}
          <label for="color2">2nd</label>
          <input id="color2" type="color" bind:value={color2Hex} />
        {/if}
      </div>
      <div class="field">
        <label for="speed">Speed</label>
        <input id="speed" type="range" min="0" max="1" step="0.05" bind:value={speed} />
      </div>
      {#if effect === "progress" || effect === "dual_progress"}
        <div class="field">
          <label for="pct">{effect === "dual_progress" ? "Left %" : "Fill %"}</label>
          <input id="pct" type="range" min="0" max="100" step="1" bind:value={progressPct} />
          <span>{progressPct}%</span>
        </div>
      {/if}
      {#if effect === "dual_progress"}
        <div class="field">
          <label for="pct2">Right %</label>
          <input id="pct2" type="range" min="0" max="100" step="1" bind:value={progress2Pct} />
          <span>{progress2Pct}%</span>
        </div>
      {/if}
      <div class="field">
        <label for="bright">Brightness</label>
        <input id="bright" type="range" min="0" max="255" step="1" bind:value={brightness} onchange={onBrightness} />
        <span>{brightness}</span>
      </div>
      <div class="field">
        <button class="primary" onclick={applyManual}>Apply</button>
        {#if manualSaveName === null}
          <button onclick={() => (manualSaveName = "")}>Save as animation</button>
        {:else}
          <!-- svelte-ignore a11y_autofocus -->
          <input
            placeholder="animation name…"
            autofocus
            bind:value={manualSaveName}
            onkeydown={(e) => e.key === "Enter" && saveManualAsAnimation()}
          />
          <button class="primary" onclick={saveManualAsAnimation} disabled={!manualSaveName.trim()}>Save</button>
          <button onclick={() => (manualSaveName = null)}>Cancel</button>
        {/if}
      </div>

      <h2>Animations <button class="subtle" onclick={() => startEditAnimation(null)}>+ new</button></h2>
      <ul class="list">
        {#each animations as a (a.id)}
          <li>
            <span class="swatch" style="background:{rgbToHex(a.spec.color)}"></span>
            <span class="grow">{a.name} <small>({a.spec.effect}{a.durationMs != null ? ` · ${a.durationMs / 1000}s` : ""})</small></span>
            <button onclick={() => applyAnimation(a.id)}>Apply</button>
            <button class="subtle" onclick={() => startEditAnimation(a)}>edit</button>
            {#if !a.builtin}<button class="subtle" onclick={() => deleteAnimation(a.id)}>✕</button>{/if}
          </li>
        {/each}
      </ul>

      {#if editingAnimation}
        <div class="editor">
          <h3>{editingAnimation.id === 0 ? "New animation" : "Edit animation"}</h3>
          <div class="field">
            <label for="an">Name</label>
            <input id="an" bind:value={editingAnimation.name} placeholder="animation name…" />
          </div>
          <div class="field">
            <label for="ae">Effect</label>
            <select id="ae" bind:value={eaEffect}>
              {#each EDITOR_EFFECTS as e}<option value={e}>{e}</option>{/each}
            </select>
          </div>
          {#if eaEffect === "keyframes"}
            <div class="field">
              <label for="kf0">Stops</label>
              <span class="stops">
                {#each eaKeyframes as _, i}
                  <span class="stop">
                    <input id={i === 0 ? "kf0" : undefined} type="color" bind:value={eaKeyframes[i]} />
                    <button class="subtle" title="Remove stop"
                      onclick={() => (eaKeyframes = eaKeyframes.filter((_, j) => j !== i))}>✕</button>
                  </span>
                {/each}
                <button onclick={() => (eaKeyframes = [...eaKeyframes, eaKeyframes.at(-1) ?? "#ff4000"])}>+ stop</button>
              </span>
            </div>
            <small>The whole strip fades through the stops in order, looping back to the first. Speed sets the loop time.</small>
          {:else}
            <div class="field">
              <label for="ac">Color</label>
              <input id="ac" type="color" bind:value={eaColorHex} />
              {#if TWO_COLOR_EFFECTS.has(eaEffect)}
                <label for="ac2">2nd</label>
                <input id="ac2" type="color" bind:value={eaColor2Hex} />
              {/if}
            </div>
          {/if}
          <div class="field">
            <label for="as">Speed</label>
            <input id="as" type="range" min="0" max="1" step="0.05" bind:value={eaSpeed} />
          </div>
          <div class="field">
            <label for="adur">Length (s)</label>
            <input id="adur" type="number" class="narrow" min="0" step="0.5" placeholder="forever" bind:value={eaDurationS} />
            <small>How long it plays when shown — then the previous state returns. Empty = runs until outranked.</small>
          </div>
          <div class="field">
            <button class="primary" onclick={saveAnimationEdit} disabled={!editingAnimation.name.trim()}>Save</button>
            <button onclick={tryEditorOnStrip}>Try on strip</button>
            <button onclick={() => (editingAnimation = null)}>Cancel</button>
          </div>
          <small>“Try on strip” takes manual control — hit “release” up top when done.</small>
        </div>
      {/if}
    </section>

    <!-- ======= triggers ======= -->
    <section class="automations-view">
      <h2>Triggers <button class="subtle" onclick={() => (editingTrigger = newTrigger())}>+ add</button></h2>
      <small class="hint">Drag to reorder — higher wins when several fire at once.</small>
      <ul class="list">
        {#each triggers as t, i (t.id)}
          <li
            class:disabled={!t.enabled}
            class:dragging={dragIndex === i}
            draggable="true"
            ondragstart={(e) => onTriggerDragStart(e, i)}
            ondragover={(e) => onTriggerDragOver(e, i)}
            ondragend={onTriggerDragEnd}
          >
            <span class="handle" title="Drag to reorder">⠿</span>
            <span class="grow">
              <strong>{t.name}</strong><br />
              <small>
                on {t.source}/{t.eventType}
                {#if t.clearEventType}until {t.clearEventType}{/if}
                → {animations.find((a) => a.id === t.animationId)?.name ?? "?"}
                {#if t.durationMs}· {t.durationMs / 1000}s{/if}
              </small>
            </span>
            <label class="switch" title={t.enabled ? "Active — click to disable" : "Inactive — click to enable"}>
              <input type="checkbox" checked={t.enabled} onchange={() => toggleTrigger(t)} />
              <span class="slider"></span>
            </label>
            <button class="subtle" onclick={() => (editingTrigger = { ...t })}>edit</button>
            <button class="subtle" onclick={() => deleteTrigger(t.id)}>✕</button>
          </li>
        {/each}
        <!-- Idle: the floor of the priority stack. Pinned, not draggable. -->
        <li class="idle-row">
          <span class="handle spacer"></span>
          <span class="grow">
            <strong>Idle</strong><br />
            <small>when nothing above is active</small>
          </span>
          <select bind:value={idleSelection} onchange={onIdleChange} title="What the strip shows when idle">
            {#if idleSelection == null}
              <option value={null}>Built-in rainbow</option>
            {/if}
            {#each animations as a (a.id)}<option value={a.id}>{a.name}</option>{/each}
            <option value="claude_usage">Claude usage (session | weekly)</option>
          </select>
        </li>
      </ul>

      {#if editingTrigger}
        <div class="editor">
          <h3>{editingTrigger.id === 0 ? "New trigger" : "Edit trigger"}</h3>
          <div class="field"><label for="tn">Name</label><input id="tn" bind:value={editingTrigger.name} /></div>
          <div class="field">
            <label for="ts">On event</label>
            <input id="ts" class="narrow" placeholder="source" list="known-sources" bind:value={editingTrigger.source} />
            <input class="narrow" placeholder="type" bind:value={editingTrigger.eventType} />
            <datalist id="known-sources">
              <option value="cli">lightctl / terminal</option>
              <option value="system">screen lock etc.</option>
              <option value="device">the strip's connection</option>
            </datalist>
          </div>
          <div class="field">
            <label for="tc">Clear on</label>
            <input id="tc" placeholder="event type (optional)" bind:value={editingTrigger.clearEventType} />
          </div>
          <div class="field">
            <label for="ta">Show</label>
            <select id="ta" bind:value={editingTrigger.animationId}>
              {#each animations as a (a.id)}<option value={a.id}>{a.name}</option>{/each}
            </select>
          </div>
          <div class="field">
            <label for="tdur">Expires (s)</label>
            <input
              id="tdur" type="number" class="narrow" placeholder="animation default"
              value={editingTrigger.durationMs != null ? editingTrigger.durationMs / 1000 : ""}
              onchange={(e) => {
                const v = (e.target as HTMLInputElement).value;
                editingTrigger!.durationMs = v === "" ? null : Math.round(parseFloat(v) * 1000);
              }}
            />
          </div>
          <div class="field">
            <button class="primary" onclick={saveTrigger}>Save trigger</button>
            <button onclick={() => (editingTrigger = null)}>Cancel</button>
          </div>
          <small>Priority comes from the list — drag the trigger up or down after saving. Without “clear on” or “expires”, the trigger stays active until something outranks it.</small>
        </div>
      {/if}

      <h2>Schedules <button class="subtle" onclick={() => (editingSchedule = newSchedule())}>+ add</button></h2>
      {#if schedules.length === 0 && !editingSchedule}
        <small class="hint">Daily clock actions: emit a time/* event (pair it with a trigger), or swap the idle animation at a set time.</small>
      {/if}
      <ul class="list">
        {#each schedules as s (s.id)}
          <li class:disabled={!s.enabled}>
            <span class="grow">
              <strong>{s.name}</strong><br />
              <small>
                at {s.time} →
                {#if s.action === "emit"}emit time/{s.eventType}{:else}idle = {animations.find((a) => a.id === s.animationId)?.name ?? "?"}{/if}
              </small>
            </span>
            <label class="switch" title={s.enabled ? "Active — click to disable" : "Inactive — click to enable"}>
              <input type="checkbox" checked={s.enabled} onchange={() => toggleSchedule(s)} />
              <span class="slider"></span>
            </label>
            <button class="subtle" onclick={() => (editingSchedule = { ...s })}>edit</button>
            <button class="subtle" onclick={() => deleteSchedule(s.id)}>✕</button>
          </li>
        {/each}
      </ul>

      {#if editingSchedule}
        <div class="editor">
          <h3>{editingSchedule.id === 0 ? "New schedule" : "Edit schedule"}</h3>
          <div class="field"><label for="sn">Name</label><input id="sn" bind:value={editingSchedule.name} /></div>
          <div class="field">
            <label for="st">At</label>
            <input id="st" type="time" bind:value={editingSchedule.time} />
            <select bind:value={editingSchedule.action}>
              <option value="emit">emit an event</option>
              <option value="idle">swap idle animation</option>
            </select>
          </div>
          {#if editingSchedule.action === "emit"}
            <div class="field">
              <label for="se">Event type</label>
              <input id="se" class="narrow" placeholder="evening" bind:value={editingSchedule.eventType} />
              <small>fires as time/&lt;type&gt; — add a trigger for it</small>
            </div>
          {:else}
            <div class="field">
              <label for="sa">Idle becomes</label>
              <select id="sa" bind:value={editingSchedule.animationId}>
                {#each animations as a (a.id)}<option value={a.id}>{a.name}</option>{/each}
              </select>
            </div>
          {/if}
          <div class="field">
            <button class="primary" onclick={saveSchedule}>Save schedule</button>
            <button onclick={() => (editingSchedule = null)}>Cancel</button>
          </div>
        </div>
      {/if}

      <h2>Why is the light doing that?</h2>
      <ul class="list">
        {#if active.overlays.length === 0}
          <li><small>No active overrides — showing the idle animation.</small></li>
        {/if}
        {#each active.overlays as o (o.key)}
          <li>
            <span class="grow">
              {#if o.winning}👑{/if}
              {o.name} <small>{o.key === "manual" ? "manual — beats all triggers" : `prio ${o.priority}`}{#if o.expiresInMs != null} · expires in {Math.ceil(o.expiresInMs / 1000)}s{/if}</small>
            </span>
          </li>
        {/each}
      </ul>
    </section>

    <!-- ======= events ======= -->
    <section class="integrations-view">
      <h2>Integrations</h2>
      <div class="field">
        <label for="slacktok">Slack {slackSet ? "✓" : ""}</label>
        <input id="slacktok" type="password" placeholder={slackSet ? "token saved — paste to replace, save empty to clear" : "xoxp- user token"} bind:value={slackInput} />
        <button onclick={() => saveSecret("slack_token", slackInput)}>Save</button>
      </div>
      <div class="field">
        <label for="calurl">Calendar {calendarSet ? "✓" : ""}</label>
        <input id="calurl" type="password" placeholder={calendarSet ? "URL saved — paste to replace, save empty to clear" : "secret iCal (.ics) URL"} bind:value={calendarInput} />
        <button onclick={() => saveSecret("calendar_ics_url", calendarInput)}>Save</button>
      </div>
      <small class="hint">Setup steps are in the README ("Integrations"). Secrets go to the macOS keychain. Claude Code and mic/camera call detection need no setup here.</small>
      <div class="field">
        <label for="cfg">Config</label>
        <button id="cfg" onclick={exportConfig}>Export…</button>
        <button onclick={importConfig}>Import…</button>
        <button onclick={exportDiagnostics}>Diagnostics…</button>
        <small>animations, triggers, schedules & idle — as JSON, by name</small>
      </div>

      <h2>Simulate an event</h2>
      <div class="field">
        <input class="narrow" placeholder="source" bind:value={simSource} />
        <input class="narrow" placeholder="type" bind:value={simType} />
        <input placeholder={'payload JSON (e.g. {"percent": 60})'} bind:value={simPayload} />
        <button onclick={simulate}>Fire</button>
      </div>

      <h2>Event log <button class="subtle" onclick={clearHistory} disabled={events.length === 0}>Clear history</button></h2>
      <ul class="list log">
        {#each events as ev}
          <li>
            <small class="dim">{fmtTime(ev.ts)}</small>
            <span class="grow"><strong>{ev.source}</strong>/{ev.type} <small class="dim">{fmtPayload(ev.payload)}</small></span>
            <button class="subtle" aria-label={`Replay ${ev.source}/${ev.type}`} onclick={() => replayEvent(ev)}>Replay</button>
          </li>
        {/each}
      </ul>
    </section>
  </div>
</main>
{#if toast}
  <div class="toast {toast.kind}" role={toast.kind === "error" ? "alert" : "status"}>{toast.message}</div>
{/if}

<style>
  :global(body) {
    --bg: #111318;
    --surface: #1a1d24;
    --surface-raised: #232732;
    --border: #39404d;
    --text: #f0f2f5;
    --muted: #aeb5c1;
    --accent: #4a7cf0;
    margin: 0;
    font-family: ui-sans-serif, -apple-system, "Segoe UI", sans-serif;
    background: var(--bg);
    color: var(--text);
    font-size: 14px;
  }
  main { padding: 20px 24px 32px; max-width: 1280px; margin: 0 auto; }
  header { margin-bottom: 14px; }

  .strip {
    display: flex;
    gap: 4px;
    padding: 12px;
    background: #0a0b0d;
    border-radius: 10px;
    justify-content: center;
  }
  .led {
    width: 16px;
    height: 16px;
    border-radius: 50%;
    background: #000;
    flex: 0 1 auto;
  }

  .statusbar { display: flex; gap: 8px; align-items: center; margin-top: 10px; flex-wrap: wrap; }
  .tabs { display: flex; gap: 4px; margin: 0 0 18px; border-bottom: 1px solid var(--border); }
  .tabs button { border: 0; border-radius: 7px 7px 0 0; padding: 10px 14px; background: transparent; color: var(--muted); }
  .tabs button.active { color: var(--text); background: var(--surface); box-shadow: inset 0 -2px var(--accent); }
  main[data-view="home"] section:not(.home-view),
  main[data-view="automations"] section:not(.automations-view),
  main[data-view="integrations"] section:not(.integrations-view) { display: none; }
  main[data-view="home"] .columns,
  main[data-view="automations"] .columns,
  main[data-view="integrations"] .columns { grid-template-columns: minmax(0, 760px); justify-content: center; }
  .onboarding { display: flex; gap: 6px; flex-direction: column; padding: 14px 16px; margin-bottom: 18px; border: 1px solid #655126; background: #2a2417; border-radius: 10px; }
  .onboarding span { color: #d7c99e; }
  .pill {
    padding: 3px 10px;
    border-radius: 99px;
    background: #23262c;
    font-size: 12px;
  }
  .pill.ok { background: #123c22; color: #7ce8a5; }
  .pill.bad { background: #40191c; color: #ff9aa0; }
  .picker { margin-top: 8px; display: flex; gap: 8px; align-items: center; }

  .columns { display: grid; grid-template-columns: 1fr 1fr 1fr; gap: 20px; }
  @media (max-width: 980px) { .columns { grid-template-columns: 1fr; } }

  h2 { font-size: 13px; text-transform: uppercase; letter-spacing: 0.08em; color: #9aa1ad; margin: 18px 0 8px; }
  h2:first-child { margin-top: 0; }
  h3 { font-size: 13px; margin: 8px 0; }

  .field { display: flex; gap: 8px; align-items: center; margin-bottom: 8px; flex-wrap: wrap; }
  .field label { min-width: 68px; color: #9aa1ad; font-size: 12px; }
  .field input[type="range"] { flex: 1; min-width: 80px; }

  input, select, button {
    background: #23262c;
    color: #e6e8ec;
    border: 1px solid #333842;
    border-radius: 6px;
    padding: 8px 10px;
    min-height: 36px;
    font-size: 13px;
    font-family: inherit;
  }
  input.narrow { width: 90px; }
  input[type="color"] { padding: 1px; width: 40px; height: 28px; }
  button { cursor: pointer; }
  button:hover { border-color: #5b6472; }
  button:focus-visible, input:focus-visible, select:focus-visible, [draggable="true"]:focus-visible {
    outline: 3px solid color-mix(in srgb, var(--accent) 65%, white);
    outline-offset: 2px;
  }
  button.primary { background: #1f4fd8; border-color: #1f4fd8; }
  button.subtle { background: transparent; border-color: transparent; color: #9aa1ad; }
  button.subtle:hover { color: #e6e8ec; }
  button:disabled { opacity: 0.45; cursor: default; }

  .list { list-style: none; margin: 0; padding: 0; }
  .list li {
    display: flex;
    gap: 8px;
    align-items: center;
    padding: 6px 8px;
    border-radius: 6px;
  }
  .list li:nth-child(odd) { background: #1a1d22; }
  .list li.disabled { opacity: 0.5; }
  .grow { flex: 1; }
  .swatch { width: 14px; height: 14px; border-radius: 4px; flex: none; }
  .log { max-height: 320px; overflow-y: auto; }
  .dim { color: #6f7683; }
  small { color: var(--muted); }
  .toast { position: fixed; right: 20px; bottom: 20px; max-width: 420px; padding: 12px 16px; border-radius: 9px; background: #17472a; color: #aaf0c0; box-shadow: 0 8px 32px #0008; z-index: 100; }
  .toast.error { background: #541f25; color: #ffc2c6; }

  .editor {
    background: #1a1d22;
    border: 1px solid #333842;
    border-radius: 8px;
    padding: 10px;
    margin-top: 8px;
  }

  .hint { display: block; margin: -4px 0 6px; }

  /* keyframe stop editor */
  .stops { display: flex; gap: 6px; align-items: center; flex-wrap: wrap; flex: 1; }
  .stop { display: inline-flex; align-items: center; }
  .stop .subtle { padding: 2px 4px; }

  /* drag-to-reorder */
  .handle { cursor: grab; color: #6f7683; flex: none; user-select: none; }
  .handle.spacer { visibility: hidden; }
  .list li.dragging { opacity: 0.4; background: #23262c; }
  .list li.idle-row {
    background: transparent;
    border-top: 1px dashed #333842;
    border-radius: 0;
    margin-top: 4px;
  }

  /* active/inactive toggle */
  .switch { position: relative; width: 30px; height: 17px; flex: none; }
  .switch input {
    position: absolute; inset: 0; width: 100%; height: 100%;
    margin: 0; opacity: 0; cursor: pointer;
  }
  .slider {
    position: absolute; inset: 0;
    background: #333842; border-radius: 99px;
    transition: background 0.15s; pointer-events: none;
  }
  .slider::before {
    content: ""; position: absolute; width: 13px; height: 13px;
    border-radius: 50%; background: #9aa1ad; top: 2px; left: 2px;
    transition: transform 0.15s, background 0.15s;
  }
  .switch input:checked + .slider { background: #1f4fd8; }
  .switch input:checked + .slider::before { transform: translateX(13px); background: #fff; }
  @media (max-width: 640px) {
    main { padding: 14px; }
    .tabs { overflow-x: auto; }
    .tabs button { white-space: nowrap; }
    .field { align-items: stretch; }
    .field label { width: 100%; }
    input, select, button { min-height: 42px; }
  }
  @media (prefers-reduced-motion: reduce) {
    *, *::before, *::after { transition-duration: 0.001ms !important; animation-duration: 0.001ms !important; }
    .led { box-shadow: none !important; }
  }
</style>
