<!--
  Animated thumbnail for one animation: plays one pre-rendered loop of the
  actual effect (rendered by the Rust engine via preview_animation), holds
  the first frame for a beat, then plays again. Static looks (single-frame
  clips, or reduced-motion users) just draw once.
-->
<script lang="ts">
  import { invoke } from "@tauri-apps/api/core";
  import type { AnimSpec } from "$lib/types";

  let { spec }: { spec: AnimSpec } = $props();

  const PAUSE_MS = 1000;
  let canvas = $state<HTMLCanvasElement | null>(null);

  function draw(hex: string) {
    const ctx = canvas?.getContext("2d");
    if (!canvas || !ctx) return;
    const leds = hex.length / 6;
    const w = canvas.width / leds;
    ctx.fillStyle = "#000";
    ctx.fillRect(0, 0, canvas.width, canvas.height);
    for (let i = 0; i < leds; i++) {
      ctx.fillStyle = `#${hex.slice(i * 6, i * 6 + 6)}`;
      ctx.fillRect(i * w, 0, Math.ceil(w), canvas.height);
    }
  }

  $effect(() => {
    // JSON round-trip: tracks every field of the spec deeply, and hands
    // invoke a plain object rather than the $state proxy.
    const plain = JSON.parse(JSON.stringify(spec)) as AnimSpec;
    if (!canvas) return;

    let raf = 0;
    let cancelled = false;
    const reducedMotion = matchMedia("(prefers-reduced-motion: reduce)").matches;

    invoke<{ frames: string[]; durationMs: number }>("preview_animation", { spec: plain })
      .then(({ frames, durationMs }) => {
        if (cancelled || frames.length === 0) return;
        draw(frames[0]);
        if (frames.length === 1 || reducedMotion) return;
        let loopStart = performance.now();
        let shown = 0;
        const tick = (now: number) => {
          const elapsed = now - loopStart;
          let idx: number;
          if (elapsed < durationMs) {
            idx = Math.min(frames.length - 1, Math.floor((elapsed / durationMs) * frames.length));
          } else if (elapsed < durationMs + PAUSE_MS) {
            idx = 0; // the pause: hold the first frame between plays
          } else {
            loopStart = now;
            idx = 0;
          }
          if (idx !== shown) {
            shown = idx;
            draw(frames[idx]);
          }
          raf = requestAnimationFrame(tick);
        };
        raf = requestAnimationFrame(tick);
      })
      .catch(() => {
        /* invalid/unknown spec: leave the thumbnail black */
      });

    return () => {
      cancelled = true;
      cancelAnimationFrame(raf);
    };
  });
</script>

<canvas bind:this={canvas} width="66" height="12" aria-hidden="true"></canvas>

<style>
  canvas {
    width: 66px;
    height: 12px;
    border-radius: 4px;
    background: #000;
    flex: none;
  }
</style>
