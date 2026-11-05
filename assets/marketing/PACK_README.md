# Wavyzon marketing pack

Everything here is rendered from the sources in the Wavyzon repository
(`assets/logo.svg`, `assets/banner.svg`, `assets/marketing/generate.py`).
Regenerate with `python3 assets/marketing/generate.py <folder>`.

## What's inside

| Folder | Files | Use |
| --- | --- | --- |
| `logo/` | `wavyzon-logo-{16…2048}.png` | The mark on its tile, transparent corners: avatars, app icons, favicons |
| | `wavyzon-mark-{white,black,violet}-{512,1024}.png` | The mark alone, one colour: dark photos, print, watermarks |
| | `wavyzon-lockup-{dark,light,transparent}.png` | Mark and name side by side: headers, slides, partner pages |
| | `wavyzon-logo.svg`, `wavyzon-mark.svg` | Vector sources |
| `social/` | `og-card-1200x630.png` | Link previews (Open Graph, Discord embeds) |
| | `repo-social-preview-1280x640.png` | GitLab / GitHub repository social preview |
| | `x-header-1500x500.png`, `linkedin-banner-1584x396.png` | Profile headers |
| | `discord-banner-960x540.png` | Discord server or profile banner |
| | `readme-banner-2560x640.png` | The README banner at 2× |
| `discord/` | `bot-avatar-1024.png`, `server-icon-512.png`, `emoji-wavyzon-128.png` | Bot avatar, server icon, custom emoji |
| `posts/` | `post-*-1080.png` | Square posts, one idea each: the wire clock, Lavalink compatibility, Equalizer APO, the Pelican egg, crossfades |
| `brand/` | `wavyzon-brand-sheet-1920x1200.png` | Mark, colours and type on one page |

## The wire

Every visual shares one motif: deck A's waveform (violet to cyan), deck B's
echo behind it, the white playhead of the wire clock on an exact frame, and a
ruler of 20 ms frames. It is what Wavyzon does, drawn. Keep it at the bottom
edge, running off both sides; don't put text over it.

## Colours

| Name | Hex | Use |
| --- | --- | --- |
| Night Ink | `#0D0B1A` | Backgrounds |
| Tile | `#1B1640` | Depth, the logo tile |
| Wavy Violet | `#7C5CFF` | Primary accent |
| Signal Cyan | `#22D3EE` | Secondary accent |
| Paper | `#F6F5FF` | Text on dark |
| Mist | `#A8A3C7` | Secondary text |

The violet-to-cyan gradient belongs to the wave and the mark. Don't use it as
a background wash.

## Type

Fira Sans (SIL Open Font License): Heavy and tight for headlines, Regular for
sentences, Condensed with tabular figures for timings. Headlines in sentence
case, short, ending with a full stop.

## Rules

- Keep clear space around the mark of at least a quarter of its width.
- Don't recolour, stretch, rotate or outline the mark; on busy backgrounds use
  the tile version.
- One message per visual.

The assets are part of Wavyzon, © 2026 Anaïs Saraiva and the iHorizon team,
under CC BY-NC-SA 4.0.
