<!--
  Pill switch used across the redesigned UI (master power, per-rule enable,
  settings rows). Presentational: the parent owns the boolean and reacts to
  `onchange`. Accent-on, gray-off, thumb slides via transform.
-->
<script lang="ts">
  let {
    checked,
    onchange,
    size = "md",
    disabled = false,
    label = "toggle",
  }: {
    checked: boolean;
    onchange: () => void;
    size?: "sm" | "md";
    disabled?: boolean;
    label?: string;
  } = $props();
</script>

<button
  type="button"
  class="toggle {size}"
  class:on={checked}
  {disabled}
  role="switch"
  aria-checked={checked}
  aria-label={label}
  onclick={() => !disabled && onchange()}
>
  <span class="thumb"></span>
</button>

<style>
  .toggle {
    --w: 38px;
    --h: 22px;
    --thumb: 18px;
    display: inline-flex;
    align-items: center;
    flex: none;
    width: var(--w);
    height: var(--h);
    padding: 2px;
    border: 0;
    border-radius: 999px;
    background: var(--toggle-off);
    cursor: pointer;
    transition: background 0.15s ease;
  }
  .toggle.sm {
    --w: 34px;
    --h: 20px;
    --thumb: 16px;
  }
  .toggle.on {
    background: var(--accent);
  }
  .toggle:disabled {
    cursor: default;
    opacity: 0.5;
  }
  .thumb {
    width: var(--thumb);
    height: var(--thumb);
    border-radius: 50%;
    background: #fff;
    box-shadow: 0 1px 2px rgba(0, 0, 0, 0.35);
    transform: translateX(0);
    transition: transform 0.15s ease;
  }
  .toggle.on .thumb {
    transform: translateX(calc(var(--w) - var(--thumb) - 4px));
  }
  .toggle:focus-visible {
    outline: 3px solid color-mix(in srgb, var(--accent) 65%, white);
    outline-offset: 2px;
  }
  @media (prefers-reduced-motion: reduce) {
    .toggle,
    .thumb {
      transition-duration: 0.001ms;
    }
  }
</style>
