# Brand

Luminode's mark is a six-ray node: a lit core, six rays out to terminals, and
an amber halo. It reads as one point of light feeding several outputs, which is
what the app does — one process, many sources, one strip.

<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="../app/static/brand/luminode-mark-inverse.svg">
    <img src="../app/static/brand/luminode-mark.svg" alt="" width="120" height="120">
  </picture>
</p>

## Assets

Everything below is generated. Do not hand-edit the SVGs, the icons, or
`app/src/lib/Logo.svelte` — change the dial values in `tools/brand_assets.py`
and re-run it, so every size and format moves together:

```sh
python3 tools/brand_assets.py            # SVGs, Logo.svelte, icons (needs Chromium)
python3 tools/brand_assets.py --svg-only # SVGs and Logo.svelte only
```

| Asset | Where | Use |
| --- | --- | --- |
| `luminode-mark.svg` | `app/static/brand/` | The mark on light surfaces |
| `luminode-mark-inverse.svg` | `app/static/brand/` | The mark on dark surfaces |
| `luminode-mark-mono.svg` | `app/static/brand/` | One colour: engraving, stamping, fax-grade print |
| `luminode-mark-small.svg` | `app/static/brand/` | Pre-reduced geometry for 24px and below |
| `luminode-icon.svg` | `app/static/brand/` | App icon artwork: mark on the dark squircle |
| `Logo.svelte` | `app/src/lib/` | The mark inside the app, themed and size-aware |
| `icon.png`, `icon.ico`, `icon.icns`, `Square*.png` | `app/src-tauri/icons/` | Bundled application and tray icons |

## Palette

| Token | Light surface | Dark surface | Role |
| --- | --- | --- | --- |
| ink | `#7B1D2E` | `#EDE7E2` | Rays, terminal outlines, core |
| lit | `#E8A33D` | `#F0B054` | Halo, core highlight, UI accent |
| bg | `#F7F9FA` | `#171C1F` | Ground, and the fill inside each hollow terminal |

`#E8A33D` is also the app's default UI accent. It is only a default: the accent
picker in Settings overrides it and the choice persists, so the brand colour
never traps anyone in it.

In the app the mark reads three CSS variables — `--mark-ink`, `--mark-lit` and
`--mark-bg` — which `+page.svelte` sets per theme. The terminals are hollow, so
**`--mark-bg` must match whatever surface the mark sits on**; a mark moved onto
a new background needs that variable set alongside it or the terminals will
show the wrong fill.

## Geometry

The mark is drawn in a 64-unit box. The locked values, straight off the tuning
tool's dials:

| Dial | Value | Notes |
| --- | --- | --- |
| Core radius | 6.5 | |
| Ray start | 12 | Where a ray leaves the core |
| Terminal distance | 26.5 | Capped at 28 — beyond that the dots clip the box |
| Terminal radius | 3.1 | |
| Stroke weight | 2.6 | |
| Halo strength | 100% | Has a floor near 40%, not a preference — below it the wine red takes over and the mark reads as a hazard glyph |

Rays sit at 60° intervals starting at 12 o'clock.

## Size steps

The mark loses detail deliberately as it shrinks, rather than being scaled flat:

- **49px and up** — full geometry, full halo.
- **25–48px** — strokes to 3.1, terminals to 3.6, halo down to 70%.
- **17–24px** — halo off, strokes to 4.2, core to 9, terminals to 4.2, distance
  in to 24.
- **16px and below** — terminals go solid and the mark grows into its padding.
  Outlined terminals average into grey mush at that size; six countable dots
  beat a faithful but illegible ring. This step extends the tuning tool, which
  stopped at the 24px rule.

## Wordmark

`lumi` at weight 800, `node` at weight 200, in Plus Jakarta Sans, tracked
-0.045em, always lowercase. The weight split is the whole idea, and it stops
working below about 15px — use the mark alone there instead of shrinking the
lockup.

The font is vendored at `app/static/brand/fonts/` (SIL Open Font License 1.1)
rather than linked, because the Tauri webview's content security policy only
allows same-origin assets and the app has to render correctly offline. It is
scoped to the wordmark; UI text stays on the system font stack.

## Application icon

The icon is the inverse mark on the `#171C1F` ground, in a squircle with a
22.4% corner radius, the mark at 78% scale (92% at 16px, where padding matters
less than legibility). The dark ground was chosen over the light panel because
it holds up in both a light and a dark dock or taskbar.
