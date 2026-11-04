# Wavyzon brand assets

| File | Use |
| --- | --- |
| `logo.svg` | The mark: a rounded tile with the wave "W". Use it for avatars, favicons, docs. Works on light and dark backgrounds because it carries its own background. |
| `logo.png` | 512×512 PNG render of `logo.svg`, for places that do not accept SVG (GitLab/GitHub project avatar, Discord). |
| `banner.svg` | 1280×320 banner with the mark, the name and the tagline, for the top of the README and social previews. Self-contained dark background. |
| `icon-mono.svg` | Single-colour mark without the tile. Draws in `currentColor` (black when used as an `<img>`), for monochrome contexts, masks and print. |

## The mark

The "W" is drawn as a sound wave split in two halves: the left half at full
strength and the right half fainter, overlapping at the centre peak. It stands
for Wavyzon's two decks crossfading into each other.

## Palette

| Name | Hex | Use |
| --- | --- | --- |
| Wavy Violet | `#7C5CFF` | Primary. Gradient start, links, accents. |
| Signal Cyan | `#22D3EE` | Secondary. Gradient end, highlights. |
| Night Ink | `#0D0B1A` | Dark backgrounds (banner). |
| Paper | `#F6F5FF` | The wave stroke, text on dark backgrounds. |
| Mist | `#A8A3C7` | Secondary text on dark backgrounds (banner tagline). |

The brand gradient runs from Wavy Violet (top left) to Signal Cyan (bottom
right) at 45°.

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
