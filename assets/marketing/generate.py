"""Wavyzon marketing pack: every visual as an HTML page, rendered to PNG by
headless Chromium. Standard library only.

    python3 assets/marketing/generate.py <output dir>

Needs `chromium` on PATH and the Fira Sans family installed (the brand
typeface, SIL OFL). The recurring motif is "the wire": deck A's waveform,
deck B's echo, the white playhead of the wire clock on an exact frame, and a
20 ms frame ruler — what Wavyzon does, drawn.
"""
import math
import pathlib
import shutil
import subprocess
import sys

ASSETS = pathlib.Path(__file__).resolve().parent.parent
OUT = pathlib.Path(sys.argv[1]).resolve()
WORK = OUT / ".html"

INK, TILE, VIOLET, CYAN, PAPER, MIST = "#0D0B1A", "#1B1640", "#7C5CFF", "#22D3EE", "#F6F5FF", "#A8A3C7"
LOGO = (ASSETS / "logo.svg").as_uri()
MONO = (ASSETS / "icon-mono.svg").read_text()


# --------------------------------------------------------------------------- the wire

def wave_points(x0, x1, mid, amp, seed, step=4.0):
    """An audio-like waveform: a few incommensurate partials under a slow
    envelope, so it reads as music rather than a sine."""
    pts = []
    n = int((x1 - x0) / step) + 1
    for i in range(n):
        x = x0 + i * step
        t = (x - x0) / max(1.0, x1 - x0)
        env = 0.35 + 0.65 * (0.5 + 0.5 * math.sin(2 * math.pi * (t * 1.3 + seed * 0.17)))
        y = (math.sin(x / 31.0 + seed) * 0.62
             + math.sin(x / 13.3 + seed * 2.1) * 0.28
             + math.sin(x / 6.1 + seed * 0.7) * 0.10)
        pts.append((x, mid + amp * env * y))
    return " ".join(f"{x:.1f},{y:.1f}" for x, y in pts)


def wire(width, height, mid, amp, playhead_x, stamp, seed=1.0, ruler=True, scale=1.0):
    """The signature: deck B echo, deck A wave, playhead, frame ruler."""
    sw = 4.5 * scale
    tick_gap = 14 * scale
    ticks = []
    if ruler:
        base = mid + amp * 1.35 + 28 * scale
        i = 0
        x = 0.0
        while x <= width:
            h = (16 if i % 5 == 0 else 8) * scale
            ticks.append(f'<line x1="{x:.1f}" y1="{base:.1f}" x2="{x:.1f}" y2="{base + h:.1f}"/>')
            x += tick_gap
            i += 1
    label = ""
    if stamp:
        label = (f'<text x="{playhead_x + 14 * scale:.1f}" y="{mid - amp * 1.35 - 6 * scale:.1f}" '
                 f'font-family="Fira Sans Condensed" font-size="{17 * scale:.1f}" fill="{PAPER}" '
                 f'style="font-variant-numeric: tabular-nums">{stamp}</text>')
    return f"""
<svg class="wire" viewBox="0 0 {width} {height}" width="{width}" height="{height}" xmlns="http://www.w3.org/2000/svg" aria-hidden="true">
  <defs>
    <linearGradient id="a" x1="0" x2="1"><stop offset="0" stop-color="#9D85FF"/><stop offset=".5" stop-color="#C9C0FF"/><stop offset="1" stop-color="#5EE6F7"/></linearGradient>
    <linearGradient id="b" x1="0" x2="1"><stop offset="0" stop-color="{VIOLET}" stop-opacity="0"/><stop offset=".55" stop-color="#38BDF8" stop-opacity=".4"/><stop offset="1" stop-color="{CYAN}" stop-opacity=".55"/></linearGradient>
    <filter id="g" x="-5%" y="-50%" width="110%" height="200%"><feGaussianBlur stdDeviation="{6 * scale:.1f}"/></filter>
  </defs>
  <polyline points="{wave_points(0, width, mid + 14 * scale, amp * 0.9, seed + 1.9)}" fill="none" stroke="url(#b)" stroke-width="{sw * 0.8:.1f}" stroke-linejoin="round" stroke-linecap="round"/>
  <polyline points="{wave_points(0, width, mid, amp, seed)}" fill="none" stroke="{VIOLET}" stroke-opacity=".55" stroke-width="{sw * 2.4:.1f}" filter="url(#g)"/>
  <polyline points="{wave_points(0, width, mid, amp, seed)}" fill="none" stroke="url(#a)" stroke-width="{sw:.1f}" stroke-linejoin="round" stroke-linecap="round"/>
  <g stroke="{MIST}" stroke-opacity=".35" stroke-width="{1.2 * scale:.1f}">{''.join(ticks)}</g>
  <line x1="{playhead_x}" y1="{mid - amp * 1.35:.1f}" x2="{playhead_x}" y2="{mid + amp * 1.35 + 28 * scale + 18 * scale:.1f}" stroke="{PAPER}" stroke-width="{2.4 * scale:.1f}"/>
  <circle cx="{playhead_x}" cy="{mid - amp * 1.35:.1f}" r="{5 * scale:.1f}" fill="{PAPER}"/>
  {label}
</svg>"""


BASE_CSS = f"""
* {{ box-sizing: border-box; margin: 0; }}
html, body {{ width: 100%; height: 100%; }}
body {{ background: {INK}; color: {PAPER}; font-family: "Fira Sans", sans-serif; overflow: hidden; position: relative;
        -webkit-font-smoothing: antialiased; }}
.glow {{ position: absolute; inset: 0; background: radial-gradient(60% 70% at 18% 30%, rgba(124,92,255,.22), transparent 70%); }}
.wire {{ position: absolute; left: 0; display: block; }}
.lockup {{ display: flex; align-items: center; gap: .32em; font-weight: 700; letter-spacing: -0.02em; }}
.lockup img {{ height: 1.35em; width: 1.35em; }}
h1 {{ font-weight: 900; letter-spacing: -0.025em; line-height: 1.0; }}
p {{ color: {MIST}; line-height: 1.4; }}
"""


def page(w, h, body, extra_css=""):
    return f"""<!doctype html><html><head><meta charset="utf-8"><style>{BASE_CSS}{extra_css}
html, body {{ width: {w}px; height: {h}px; }}</style></head><body>{body}</body></html>"""


def render(name, w, h, html, transparent=False):
    WORK.mkdir(parents=True, exist_ok=True)
    src = WORK / (pathlib.Path(name).stem + ".html")
    src.write_text(html, encoding="utf-8")
    dst = OUT / name
    dst.parent.mkdir(parents=True, exist_ok=True)
    args = ["chromium", "--headless=new", "--hide-scrollbars", "--force-device-scale-factor=1",
            f"--window-size={w},{h}", f"--screenshot={dst}", src.as_uri()]
    if transparent:
        args.insert(3, "--default-background-color=00000000")
    subprocess.run(args, check=True, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    print(f"  {name} ({w}x{h})")


def svg_page(w, h, svg, bg="transparent"):
    return f"""<!doctype html><html><head><meta charset="utf-8"><style>
* {{ margin: 0; }} html, body {{ width: {w}px; height: {h}px; background: {bg}; overflow: hidden; }}
img, svg {{ display: block; width: {w}px; height: {h}px; }}</style></head><body>{svg}</body></html>"""


# --------------------------------------------------------------------------- logo

def logos():
    print("logo/")
    for s in (16, 32, 48, 64, 128, 256, 512, 1024, 2048):
        render(f"logo/wavyzon-logo-{s}.png", s, s, svg_page(s, s, f'<img src="{LOGO}" alt="">'), transparent=True)
    mono = MONO.replace("<svg ", '<svg style="color:{c}" ', 1)
    for c_name, c in (("white", "#FFFFFF"), ("black", "#000000"), ("violet", VIOLET)):
        for s in (512, 1024):
            render(f"logo/wavyzon-mark-{c_name}-{s}.png", s, s, svg_page(s, s, mono.replace("{c}", c)), transparent=True)
    shutil.copy(ASSETS / "logo.svg", OUT / "logo/wavyzon-logo.svg")
    shutil.copy(ASSETS / "icon-mono.svg", OUT / "logo/wavyzon-mark.svg")

    # Horizontal lockups: tile + wordmark, on ink and on paper.
    for theme, bg, fg in (("dark", INK, PAPER), ("light", PAPER, INK)):
        html = page(1200, 480, f"""
<div class="lockup" style="position:absolute; left:90px; top:50%; transform:translateY(-50%); font-size:170px; color:{fg}">
  <img src="{LOGO}" alt=""><span>Wavyzon</span></div>""", f"body {{ background: {bg}; }}")
        render(f"logo/wavyzon-lockup-{theme}.png", 1200, 480, html)
    render("logo/wavyzon-lockup-transparent.png", 1200, 480, page(1200, 480, f"""
<div class="lockup" style="position:absolute; left:90px; top:50%; transform:translateY(-50%); font-size:170px; color:{PAPER}">
  <img src="{LOGO}" alt=""><span>Wavyzon</span></div>""", "body { background: transparent; }"), transparent=True)


# --------------------------------------------------------------------------- social

def headline_card(w, h, title, sub, size, stamp, mid_ratio=0.74, amp_ratio=0.07, play_ratio=0.62, pad=None, sub_size=None,
                  lockup_size=None, seed=1.0):
    pad = pad or int(w * 0.065)
    scale = w / 1200
    mid = h * mid_ratio
    amp = h * amp_ratio
    sub_size = sub_size or int(size * 0.36)
    lockup_size = lockup_size or int(size * 0.42)
    return page(w, h, f"""
<div class="glow"></div>
<div style="position:absolute; left:{pad}px; top:{int(pad * 0.85)}px; right:{pad}px">
  <div class="lockup" style="font-size:{lockup_size}px">
    <img src="{LOGO}" alt=""><span>Wavyzon</span></div>
  <h1 style="font-size:{size}px; margin-top:{int(size * 0.55)}px; max-width:{int(w * 0.78)}px">{title}</h1>
  <p style="font-size:{sub_size}px; margin-top:{int(size * 0.32)}px; max-width:{int(w * 0.62)}px">{sub}</p>
</div>
<div style="position:absolute; left:0; top:0">{wire(w, h, mid, amp, int(w * play_ratio), stamp, seed=seed, scale=scale)}</div>
""")


def social():
    print("social/")
    title = "The audio node that knows when it was heard."
    sub = "Rust, Lavalink v4 compatible. Streams, mixes and filters music for Discord bots, timed to the frame."
    render("social/og-card-1200x630.png", 1200, 630,
           headline_card(1200, 630, title, sub, 66, "audible 21:04:07.360", mid_ratio=0.80, amp_ratio=0.06))
    render("social/repo-social-preview-1280x640.png", 1280, 640,
           headline_card(1280, 640, title, sub, 70, "audible 21:04:07.360", mid_ratio=0.80, amp_ratio=0.06))
    # Wide headers: name and one line, the wire across the bottom.
    for name, w, h in (("social/x-header-1500x500.png", 1500, 500), ("social/discord-banner-960x540.png", 960, 540),
                       ("social/linkedin-banner-1584x396.png", 1584, 396)):
        size = int(h * 0.2)
        html = page(w, h, f"""
<div class="glow"></div>
<div style="position:absolute; left:{int(w * 0.06)}px; top:{int(h * 0.16)}px">
  <div class="lockup" style="font-size:{size}px"><img src="{LOGO}" alt=""><span>Wavyzon</span></div>
  <p style="font-size:{int(size * 0.3)}px; margin-top:{int(size * 0.3)}px">Music for Discord bots, timed to the frame.</p>
</div>
<div style="position:absolute; left:0; top:0">{wire(w, h, h * 0.74, h * 0.07, int(w * 0.7), "", seed=2.3, scale=w / 1200)}</div>""")
        render(name, w, h, html)
    render("social/readme-banner-2560x640.png", 2560, 640,
           svg_page(2560, 640, f'<img src="{(ASSETS / "banner.svg").as_uri()}" alt="">'))


# --------------------------------------------------------------------------- discord

def discord():
    print("discord/")
    for name, s in (("discord/bot-avatar-1024.png", 1024), ("discord/server-icon-512.png", 512),
                    ("discord/emoji-wavyzon-128.png", 128)):
        render(name, s, s, svg_page(s, s, f'<img src="{LOGO}" alt="">'), transparent=True)


# --------------------------------------------------------------------------- posts

POSTS = [
    ("post-wire-clock", "Hear it when they hear it.",
     "Positions are measured as each packet leaves for Discord, so synced lyrics land on the word.",
     "audible 21:04:07.360", 0.5),
    ("post-lavalink", "Your Lavalink bot already speaks it.",
     "Same v4 protocol. Change the host and the password; lavalink-client, Shoukaku and Wavelink keep working.",
     "", 1.7),
    ("post-apo", "Bring your Equalizer APO presets.",
     "AutoEQ headphone curves, graphic EQ and convolution reverbs, applied to everything the bot plays.",
     "", 2.9),
    ("post-pelican", "Import one egg. It builds itself.",
     "Pelican and Pterodactyl compile Wavyzon from source and run it like any game server.",
     "", 3.6),
    ("post-crossfade", "No silence between tracks.",
     "The next song fades in over the last three seconds, on a second deck, while the queue stays yours.",
     "", 4.4),
]


def posts():
    print("posts/")
    for slug, title, sub, stamp, seed in POSTS:
        w = h = 1080
        html = page(w, h, f"""
<div class="glow"></div>
<div style="position:absolute; left:84px; top:84px; right:84px">
  <div class="lockup" style="font-size:40px"><img src="{LOGO}" alt=""><span>Wavyzon</span></div>
  <h1 style="font-size:104px; margin-top:{150 if len(title) < 30 else 110}px; max-width:880px">{title}</h1>
  <p style="font-size:34px; margin-top:44px; max-width:880px">{sub}</p>
</div>
<div style="position:absolute; left:0; top:0">{wire(w, h, h * 0.84, h * 0.045, int(w * 0.64), stamp, seed=seed, scale=0.9)}</div>""")
        render(f"posts/{slug}-1080.png", w, h, html)


# --------------------------------------------------------------------------- brand sheet

def brand_sheet():
    print("brand/")
    w, h = 1920, 1200
    swatches = [("Night Ink", INK, "Backgrounds", PAPER), ("Tile", TILE, "Depth, the tile", PAPER),
                ("Wavy Violet", VIOLET, "Primary accent", PAPER), ("Signal Cyan", CYAN, "Secondary accent", INK),
                ("Paper", PAPER, "Text on dark", INK), ("Mist", MIST, "Secondary text", INK)]
    sw = "".join(
        f'<div class="sw" style="background:{c}; color:{t}"><b>{n}</b><span>{c}</span><em>{u}</em></div>'
        for n, c, u, t in swatches)
    css = f"""
.grid {{ position:absolute; inset:96px 110px auto 110px; display:grid; grid-template-columns: 520px 1fr; gap: 0 110px; }}
.mark {{ width:420px; height:420px; display:block; }}
.clear {{ position:relative; width:520px; height:520px; border:1px dashed rgba(168,163,199,.35); display:grid; place-items:center; }}
.clear::after {{ content:"clear space: a quarter of the mark"; position:absolute; bottom:-34px; left:0; color:{MIST}; font-size:18px; }}
h2 {{ font-size:22px; font-weight:600; color:{MIST}; margin-bottom:18px; letter-spacing:0; }}
.sws {{ display:grid; grid-template-columns: repeat(3, 1fr); gap:14px; margin-bottom:56px; }}
.sw {{ height:150px; border-radius:16px; padding:18px 20px; display:flex; flex-direction:column; justify-content:flex-end;
       border:1px solid rgba(168,163,199,.18); }}
.sw b {{ font-size:22px; font-weight:700; }} .sw span {{ font-size:17px; opacity:.85; font-variant-numeric: tabular-nums; }}
.sw em {{ font-size:15px; font-style:normal; opacity:.7; }}
.type .big {{ font-size:120px; font-weight:900; letter-spacing:-0.025em; line-height:1; }}
.type .mid {{ font-size:34px; margin-top:14px; color:{PAPER}; }}
.type .small {{ font-family:"Fira Sans Condensed"; font-size:22px; margin-top:12px; color:{MIST}; }}
"""
    html = page(w, h, f"""
<div class="glow"></div>
<div class="grid">
  <div>
    <h2>The mark</h2>
    <div class="clear"><img class="mark" src="{LOGO}" alt=""></div>
    <p style="font-size:20px; margin-top:64px; max-width:520px">The W is one sound wave: deck A playing. The echo is deck B coming in.
      The white playhead is the wire clock, the frame Discord received.</p>
  </div>
  <div>
    <h2>Colour</h2>
    <div class="sws">{sw}</div>
    <h2>Type: Fira Sans</h2>
    <div class="type">
      <div class="big">Heard on time.</div>
      <div class="mid">Regular for sentences. Heavy, tight, for headlines.</div>
      <div class="small">Condensed with tabular figures for timing: audible 21:04:07.360</div>
    </div>
  </div>
</div>
<div style="position:absolute; left:0; top:0">{wire(w, h, h * 0.9, h * 0.03, int(w * 0.58), "", seed=5.1, scale=1.2)}</div>""", css)
    render("brand/wavyzon-brand-sheet-1920x1200.png", w, h, html)


if __name__ == "__main__":
    # Only replace what this script makes; anything else in OUT stays.
    for sub in ("logo", "social", "discord", "posts", "brand", ".html"):
        shutil.rmtree(OUT / sub, ignore_errors=True)
    OUT.mkdir(parents=True, exist_ok=True)
    logos()
    social()
    discord()
    posts()
    brand_sheet()
    shutil.copy(pathlib.Path(__file__).with_name("PACK_README.md"), OUT / "README.md")
    shutil.rmtree(WORK)
    print(f"done: {OUT}")
