# Wavyzon brand assets

| File | Use |
| --- | --- |
| `logo.svg` | The mark: a Night Ink tile with the glowing wave "W". Use it for avatars, favicons, docs. Works on light and dark backgrounds because it carries its own background, and stays legible down to 16 px. |
| `logo.png` | 512×512 PNG render of `logo.svg`, for places that do not accept SVG (GitLab/GitHub project avatar, Discord). |
| `banner.svg` | 1280×320 banner with the mark, the name and the tagline, for the top of the README and social previews. Self-contained dark background; the mark is inlined (a README `<img>` cannot load other files). |
| `icon-mono.svg` | Single-colour mark without the tile. Draws in `currentColor` (black when used as an `<img>`), for monochrome contexts, masks and print. |

## The mark

Each element is a piece of what Wavyzon does:

- **The "W"** is one continuous sound wave, violet to cyan, with a soft glow:
  deck A, the track that is playing.
- **The echo** behind it, offset down and to the right, fades in from the
  left: deck B, the next track coming in. Together they are the two-deck
  crossfade.
- **The white playhead** on the centre peak is the wire clock: the exact
  frame Discord received, where the crossfade happens.
- **The faint bars** along the bottom of the tile are the stream itself.

`logo.svg` uses `wz-` prefixed ids and wraps the drawing in `<g id="wz-mark">`
so it can be inlined in other SVGs (as `banner.svg` does) without id clashes.

To re-render `logo.png` after editing the SVG (headless Chromium):

```bash
chromium --headless=new --hide-scrollbars --window-size=512,512 \
  --default-background-color=00000000 \
  --screenshot=assets/logo.png "file://$PWD/assets/logo.svg"
```

## Palette

| Name | Hex | Use |
| --- | --- | --- |
| Wavy Violet | `#7C5CFF` | Primary. Halo, glow, links, accents. |
| Signal Cyan | `#22D3EE` | Secondary. Deck B echo, rim, highlights. |
| Night Ink | `#0D0B1A` | Dark backgrounds (banner). The tile runs `#1B1640` → `#0B0918`. |
| Paper | `#F6F5FF` | Text on dark backgrounds. |
| Mist | `#A8A3C7` | Secondary text on dark backgrounds (banner tagline). |

The wave stroke runs left to right through `#9D85FF` (light violet),
`#C9C0FF` (lavender, at the playhead) and `#5EE6F7` (light cyan). The
brand gradient elsewhere runs from Wavy Violet (top left) to Signal Cyan
(bottom right) at 45°.

## Type

The wordmark and tagline in `banner.svg` are set in
[Fira Sans](https://github.com/mozilla/Fira) Bold and Regular (SIL Open Font
License 1.1) and converted to outlines, so the banner renders the same
everywhere without the font installed.

## Rules

- Keep clear space around the mark of at least a quarter of its width.
- Do not recolour the tile outside the palette, stretch it, or add effects.
- On busy or mid-tone backgrounds, use `logo.svg` (with its tile) rather than `icon-mono.svg`.

The assets are part of Wavyzon and are covered by the repository's
CC BY-NC-SA 4.0 license.
