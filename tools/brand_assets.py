#!/usr/bin/env python3
"""Generate the Luminode mark, wordmark lockups and application icons.

Geometry is a direct port of the mark-tuning tool used to lock Direction 01
(6 rays, terminals, halo). Every number below is a dial value from that tool;
change a value here and re-run to regenerate every asset at once:

    python3 tools/brand_assets.py

Outputs land in app/static/brand (source SVGs plus raster marks), app/static
(favicon) and app/src-tauri/icons (bundled application icons). Raster output
needs Chromium; pass --svg-only to skip it.
"""

import argparse
import math
import pathlib
import shutil
import struct
import subprocess
import tempfile
import zlib

ROOT = pathlib.Path(__file__).resolve().parents[1]
BRAND = ROOT / "app" / "static" / "brand"
STATIC = ROOT / "app" / "static"
ICONS = ROOT / "app" / "src-tauri" / "icons"

# --- locked dial values -------------------------------------------------
LOCKED = dict(core=6.5, inner=12.0, dist=26.5, term=3.1, sw=2.6, halo=100.0,
              alt=False, fill=False, rot=False)

# Raster marks ship at 2x the 96px the README displays them at.
MARK_PNG_PX = 192

# --- palettes -----------------------------------------------------------
LIGHT = dict(ink="#7B1D2E", lit="#E8A33D", bg="#F7F9FA")   # on the light panel
INVERSE = dict(ink="#EDE7E2", lit="#F0B054", bg="#171C1F")  # on the dark ground


def num(v, places):
    """Match the tool's toFixed() output so exports stay byte-comparable."""
    return f"{v:.{places}f}"


def mark(palette, uid="a", mono=False, **overrides):
    """Return the mark's SVG body (no <svg> wrapper) in a 64-unit box."""
    o = dict(LOCKED)
    o.update(overrides)
    core, inner, dist = o["core"], o["inner"], o["dist"]
    tr, sw, halo = o["term"], o["sw"], o["halo"] / 100.0

    ink, lit, bg = palette["ink"], palette["lit"], palette["bg"]
    if mono:
        lit = ink

    parts = []
    if halo > 0 and not mono:
        parts.append(
            f'<defs><radialGradient id="g{uid}">'
            f'<stop offset="0%" stop-color="{lit}" stop-opacity="{num(0.42 * halo, 3)}"/>'
            f'<stop offset="58%" stop-color="{lit}" stop-opacity="{num(0.10 * halo, 3)}"/>'
            f'<stop offset="100%" stop-color="{lit}" stop-opacity="0"/>'
            f'</radialGradient></defs>'
        )
        parts.append(f'<circle cx="32" cy="32" r="{num(core + 11, 2)}" fill="url(#g{uid})"/>')
        parts.append(
            f'<circle cx="32" cy="32" r="{num(core + 3.7, 2)}" fill="none" stroke="{lit}"'
            f' stroke-width="1.3" opacity="{num(0.75 * halo, 2)}"/>'
        )

    start = -60.0 if o["rot"] else -90.0
    for i in range(6):
        d = dist - 6 if (o["alt"] and i % 2 == 1) else dist
        a = math.radians(start + i * 60)
        cx, cy = math.cos(a), math.sin(a)
        ray_end = d - tr - 1.6
        if ray_end > inner + 0.5:
            parts.append(
                f'<line x1="{num(32 + cx * inner, 2)}" y1="{num(32 + cy * inner, 2)}"'
                f' x2="{num(32 + cx * ray_end, 2)}" y2="{num(32 + cy * ray_end, 2)}"'
                f' stroke="{ink}" stroke-width="{sw:g}" stroke-linecap="round"/>'
            )
        parts.append(
            f'<circle cx="{num(32 + cx * d, 2)}" cy="{num(32 + cy * d, 2)}" r="{tr:g}"'
            f' fill="{ink if o["fill"] else bg}" stroke="{ink}" stroke-width="{num(sw * 0.8, 2)}"/>'
        )

    parts.append(f'<circle cx="32" cy="32" r="{core:g}" fill="{ink}"/>')
    if halo > 0 and not mono:
        parts.append(
            f'<circle cx="32" cy="32" r="{num(core * 0.37, 2)}" fill="{lit}"'
            f' opacity="{num(halo, 2)}"/>'
        )
    return "\n  ".join(parts)


def at_size(px, **overrides):
    """The tool's size strip: thicken strokes and drop the halo as it shrinks.

    The <=16 step extends the tool's strip. Below 24px the outlined terminals
    average out into grey mush, so they go solid and the whole mark grows into
    the padding — six countable dots beat a faithful but illegible ring.
    """
    o = dict(LOCKED)
    o.update(overrides)
    if px <= 16:
        o.update(halo=0, core=11.0, sw=6.4, term=5.4, dist=23.0, inner=13.5, fill=True)
    elif px <= 24:
        o.update(halo=0, sw=max(o["sw"], 4.2), core=max(o["core"], 9.0),
                 term=max(o["term"], 4.2), dist=min(o["dist"], 24.0))
    elif px <= 48:
        o.update(sw=max(o["sw"], 3.1), term=max(o["term"], 3.6), halo=min(o["halo"], 70.0))
    return o


def svg_doc(body, title="Luminode"):
    return (
        '<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 64 64" role="img"'
        f' aria-label="{title}">\n  {body}\n</svg>\n'
    )


def icon_doc(px_hint):
    """App icon: the inverse mark on the dark ground, rounded to a squircle."""
    body = mark(INVERSE, uid="i", **at_size(px_hint))
    # Tiny icons trade icon padding for mark size; large ones keep the margin.
    scale = 0.92 if px_hint <= 16 else 0.78
    return (
        '<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 64 64" role="img"'
        ' aria-label="Luminode">\n'
        f'  <rect width="64" height="64" rx="14.32" fill="{INVERSE["bg"]}"/>\n'
        f'  <g transform="translate(32 32) scale({scale}) translate(-32 -32)">\n  '
        + body
        + "\n  </g>\n</svg>\n"
    )


# --- in-app component ---------------------------------------------------

TOKENS = dict(ink="var(--mark-ink)", lit="var(--mark-lit)", bg="var(--mark-bg)")

SVELTE_TEMPLATE = """<!--
  Luminode mark. Generated by tools/brand_assets.py — edit the dial values
  there and re-run rather than tweaking the geometry below.

  Colours come from --mark-ink / --mark-lit / --mark-bg so the mark follows the
  theme; --mark-bg fills the ray terminals and must match whatever surface the
  mark sits on. Geometry steps down with `size`: below 49px the strokes thicken
  and the halo fades, below 25px the halo goes entirely.
-->
<script lang="ts">
  // An empty title marks the instance decorative — use it wherever the
  // wordmark or a heading already names Luminode next to the mark.
  let { size = 28, title = "Luminode" }: { size?: number; title?: string } = $props();
</script>

<svg
  width={size}
  height={size}
  viewBox="0 0 64 64"
  role={title ? "img" : "presentation"}
  aria-label={title || undefined}
  aria-hidden={title ? undefined : "true"}
  class="mark"
>
  {#if size <= 24}
@@SMALL@@
  {:else if size <= 48}
@@MEDIUM@@
  {:else}
@@FULL@@
  {/if}
</svg>

<style>
  .mark { display: block; flex-shrink: 0; overflow: visible; }
</style>
"""


def svelte_component():
    def body(uid, **overrides):
        markup = mark(TOKENS, uid=uid, **overrides)
        return "\n".join("    " + line.strip() for line in markup.split("\n"))

    out = SVELTE_TEMPLATE
    for token, markup in (("@@SMALL@@", body("s", **at_size(24))),
                          ("@@MEDIUM@@", body("m", **at_size(48))),
                          ("@@FULL@@", body("f"))):
        out = out.replace(token, markup)
    return out


# --- raster -------------------------------------------------------------

def find_chromium():
    for name in ("chromium", "chromium-browser", "google-chrome", "chrome"):
        found = shutil.which(name)
        if found:
            return found
    for path in sorted(pathlib.Path("/opt/pw-browsers").glob("chromium*/chrome-linux/chrome")):
        return str(path)
    return None


def png_decode(data):
    """Minimal 8-bit RGB/RGBA non-interlaced PNG reader -> (w, h, rgba)."""
    pos, idat = 8, b""
    while pos < len(data):
        length, = struct.unpack(">I", data[pos:pos + 4])
        kind, chunk = data[pos + 4:pos + 8], data[pos + 8:pos + 8 + length]
        if kind == b"IHDR":
            w, h, depth, ctype, _, _, interlace = struct.unpack(">IIBBBBB", chunk)
            if depth != 8 or ctype not in (2, 6) or interlace:
                raise ValueError(f"unsupported PNG: depth={depth} ctype={ctype}")
        elif kind == b"IDAT":
            idat += chunk
        pos += 12 + length

    bpp = 3 if ctype == 2 else 4
    raw, stride = zlib.decompress(idat), w * bpp
    rows, prev, off = [], bytearray(stride), 0
    for _ in range(h):
        filt, off = raw[off], off + 1
        line, off = bytearray(raw[off:off + stride]), off + stride
        for i in range(stride):
            a = line[i - bpp] if i >= bpp else 0
            b = prev[i]
            c = prev[i - bpp] if i >= bpp else 0
            if filt == 1:
                line[i] = (line[i] + a) & 255
            elif filt == 2:
                line[i] = (line[i] + b) & 255
            elif filt == 3:
                line[i] = (line[i] + ((a + b) >> 1)) & 255
            elif filt == 4:
                p = a + b - c
                pa, pb, pc = abs(p - a), abs(p - b), abs(p - c)
                pred = a if (pa <= pb and pa <= pc) else (b if pb <= pc else c)
                line[i] = (line[i] + pred) & 255
        rows.append(bytes(line))
        prev = line

    if bpp == 4:
        return w, h, b"".join(rows)
    out = bytearray()
    for row in rows:
        for x in range(w):
            out += row[x * 3:x * 3 + 3] + b"\xff"
    return w, h, bytes(out)


def png_encode(w, h, rgba):
    """Write 8-bit RGBA PNG with no per-line filtering."""
    raw = b"".join(b"\x00" + rgba[y * w * 4:(y + 1) * w * 4] for y in range(h))

    def chunk(kind, payload):
        return (struct.pack(">I", len(payload)) + kind + payload
                + struct.pack(">I", zlib.crc32(kind + payload) & 0xFFFFFFFF))

    return (b"\x89PNG\r\n\x1a\n"
            + chunk(b"IHDR", struct.pack(">IIBBBBB", w, h, 8, 6, 0, 0, 0))
            + chunk(b"IDAT", zlib.compress(raw, 9))
            + chunk(b"IEND", b""))


def png_crop(data, size):
    """Crop to the top-left size x size square."""
    w, h, rgba = png_decode(data)
    if (w, h) == (size, size):
        return data
    if w < size or h < size:
        raise ValueError(f"render is {w}x{h}, smaller than the requested {size}")
    out = bytearray()
    for y in range(size):
        start = (y * w) * 4
        out += rgba[start:start + size * 4]
    return png_encode(size, size, bytes(out))


def render_png(chromium, svg_text, size):
    """Screenshot the SVG at `size` square on a transparent canvas.

    Headless Chromium hands back a viewport shorter than --window-size, so the
    canvas is oversized and cropped back to an exact square.
    """
    with tempfile.TemporaryDirectory() as tmp:
        page = pathlib.Path(tmp) / "page.html"
        page.write_text(
            "<!doctype html><meta charset=utf-8>"
            "<style>html,body{margin:0;background:transparent}"
            f"svg{{display:block;width:{size}px;height:{size}px}}</style>" + svg_text
        )
        shot = pathlib.Path(tmp) / "shot.png"
        subprocess.run(
            [chromium, "--headless", "--disable-gpu", "--no-sandbox",
             "--hide-scrollbars", "--default-background-color=00000000",
             f"--screenshot={shot}", f"--window-size={size},{size + 240}",
             "--force-device-scale-factor=1", page.as_uri()],
            check=True, capture_output=True,
        )
        return png_crop(shot.read_bytes(), size)


# --- .ico / .icns containers --------------------------------------------

def _dib_entry(size, png):
    """A 32-bit bottom-up DIB plus its (unused) AND mask.

    Windows reads PNG-in-ICO only reliably at 256px; every smaller entry is
    stored as a raw DIB so older shell code paths still find an icon.
    """
    w, h, rgba = png_decode(png)
    pixels = bytearray()
    for y in range(h - 1, -1, -1):                     # DIB rows run bottom-up
        for x in range(w):
            r, g, b, a = rgba[(y * w + x) * 4:(y * w + x) * 4 + 4]
            pixels += bytes((b, g, r, a))
    mask_stride = ((w + 31) // 32) * 4                 # 1bpp, padded to 4 bytes
    header = struct.pack("<IiiHHIIiiII", 40, w, h * 2, 1, 32, 0,
                         len(pixels) + mask_stride * h, 0, 0, 0, 0)
    return header + bytes(pixels) + b"\x00" * (mask_stride * h)


def write_ico(png_by_size, out_path):
    sizes = sorted(png_by_size)
    payloads = {s: (png_by_size[s] if s >= 256 else _dib_entry(s, png_by_size[s]))
                for s in sizes}
    header = struct.pack("<HHH", 0, 1, len(sizes))
    offset = len(header) + 16 * len(sizes)
    entries, blobs = b"", b""
    for size in sizes:
        data = payloads[size]
        dim = 0 if size >= 256 else size               # 0 means 256 in an ICO
        entries += struct.pack("<BBBBHHII", dim, dim, 0, 0, 1, 32, len(data), offset)
        blobs += data
        offset += len(data)
    out_path.write_bytes(header + entries + blobs)


# What iconutil emits for a full .iconset, as (OSType, source size).
ICNS_TYPES = [
    (b"icp4", 16), (b"ic11", 32),      # 16pt @1x, @2x
    (b"icp5", 32), (b"ic12", 64),      # 32pt @1x, @2x
    (b"ic07", 128), (b"ic13", 256),    # 128pt @1x, @2x
    (b"ic08", 256), (b"ic14", 512),    # 256pt @1x, @2x
    (b"ic09", 512), (b"ic10", 1024),   # 512pt @1x, @2x
]


def write_icns(png_by_size, out_path):
    body = b""
    for ostype, size in ICNS_TYPES:
        data = png_by_size.get(size)
        if data:
            body += ostype + struct.pack(">I", len(data) + 8) + data
    out_path.write_bytes(b"icns" + struct.pack(">I", len(body) + 8) + body)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--svg-only", action="store_true", help="skip raster output")
    args = ap.parse_args()

    BRAND.mkdir(parents=True, exist_ok=True)
    ICONS.mkdir(parents=True, exist_ok=True)

    svgs = {
        "luminode-mark.svg": svg_doc(mark(LIGHT, uid="l")),
        "luminode-mark-inverse.svg": svg_doc(mark(INVERSE, uid="d")),
        "luminode-mark-mono.svg": svg_doc(mark(LIGHT, uid="m", mono=True)),
        "luminode-mark-small.svg": svg_doc(mark(LIGHT, uid="s", **at_size(24))),
        "luminode-icon.svg": icon_doc(512),
    }
    for name, text in svgs.items():
        (BRAND / name).write_text(text)
        print("wrote", (BRAND / name).relative_to(ROOT))

    component = ROOT / "app" / "src" / "lib" / "Logo.svelte"
    component.write_text(svelte_component())
    print("wrote", component.relative_to(ROOT))

    if args.svg_only:
        return

    chromium = find_chromium()
    if not chromium:
        raise SystemExit("no chromium found; re-run with --svg-only")

    # Small icons use the reduced geometry so the rays survive the downscale.
    sizes = [16, 32, 48, 64, 128, 256, 512, 1024]
    rendered = {}
    for size in sizes:
        rendered[size] = render_png(chromium, icon_doc(size), size)
        print("rendered", size)

    named = {
        "32x32.png": 32, "128x128.png": 128, "128x128@2x.png": 256,
        "icon.png": 512, "Square30x30Logo.png": 32, "Square44x44Logo.png": 48,
        "Square71x71Logo.png": 64, "Square89x89Logo.png": 128,
        "Square107x107Logo.png": 128, "Square142x142Logo.png": 256,
        "Square150x150Logo.png": 256, "Square284x284Logo.png": 512,
        "Square310x310Logo.png": 512, "StoreLogo.png": 64,
    }
    for name, size in named.items():
        (ICONS / name).write_bytes(rendered[size])
    write_ico({s: rendered[s] for s in (16, 32, 48, 64, 128, 256)}, ICONS / "icon.ico")
    write_icns(rendered, ICONS / "icon.icns")
    (STATIC / "favicon.png").write_bytes(rendered[128])
    print("wrote application icons")

    # Raster marks for readers that do not render SVG (README on some mirrors,
    # release notes, chat previews). 2x the 96px the README displays them at.
    # The icon carries its own dark ground, so it is the one that reads on a
    # light and a dark page alike -- what the README header points at, since
    # renderers without <picture> get no light/dark swap.
    for name, text in (("luminode-mark.png", svgs["luminode-mark.svg"]),
                       ("luminode-mark-inverse.png", svgs["luminode-mark-inverse.svg"]),
                       ("luminode-icon.png", svgs["luminode-icon.svg"])):
        (BRAND / name).write_bytes(render_png(chromium, text, MARK_PNG_PX))
        print("wrote", (BRAND / name).relative_to(ROOT))


if __name__ == "__main__":
    main()
