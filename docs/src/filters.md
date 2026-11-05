# Filters

Filters are set with the `filters` field of a player update. The object
**replaces** the whole filter set: send every filter you want to keep, and
`{}` to clear them all.

```json
PATCH /v1/sessions/{session}/players/{guild}
{
  "filters": {
    "equalizer": [ { "band": 0, "gain": 0.2 }, { "band": 1, "gain": 0.15 } ],
    "timescale": { "speed": 1.15, "pitch": 1.1, "rate": 1.0 },
    "reverb": { "mix": 0.25, "room": 0.8, "damping": 0.5 },
    "pluginFilters": { "bitcrush": { "bits": 8 } }
  }
}
```

## Changes glide

A new value never switches on the next frame. Equalizer gains, pitch and
speed **glide** to their targets over about 0.5 s; effects that turn on or
off **crossfade** in and out, keeping their last settings while they fade.
Nightcore sweeps up instead of snapping, and toggling reverb never clicks.

## Built-in filters

The shapes are Lavalink's, so existing filter presets work unchanged.

### `equalizer`

15 bands, `gain` from `-0.25` (mute) to `1.0` (+ about 12 dB), `0` is flat.

| Band | 0 | 1 | 2 | 3 | 4 | 5 | 6 | 7 | 8 | 9 | 10 | 11 | 12 | 13 | 14 |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| Hz | 25 | 40 | 63 | 100 | 160 | 250 | 400 | 630 | 1k | 1.6k | 2.5k | 4k | 6.3k | 10k | 16k |

```json
"equalizer": [ { "band": 0, "gain": 0.25 }, { "band": 1, "gain": 0.2 }, { "band": 2, "gain": 0.1 } ]
```

### `timescale`

| Field | Default | Effect |
| --- | --- | --- |
| `speed` | 1.0 | Tempo **without** changing pitch (time-stretch) |
| `pitch` | 1.0 | Pitch without changing tempo |
| `rate` | 1.0 | Both, like a turntable |

Values ≤ 0 mean 1.0. The [wire clock](wire-clock.md) accounts for
`speed × rate`, so positions stay exact.

```json
"timescale": { "speed": 1.2, "pitch": 1.2, "rate": 1.0 }    // nightcore
"timescale": { "speed": 0.85, "pitch": 0.85, "rate": 1.0 }  // vaporwave / slowed
```

### `karaoke`

Removes centre-panned content (usually vocals).

| Field | Meaning |
| --- | --- |
| `level` | 0–1, how much of the centre to remove |
| `monoLevel` | 0–1, how much of the mono signal to keep |

### `tremolo` / `vibrato`

Amplitude (tremolo) or pitch (vibrato) oscillation.

| Field | Meaning |
| --- | --- |
| `frequency` | Hz, > 0 (vibrato ≤ 14) |
| `depth` | 0–1 |

### `rotation`

Rotates the sound around the stereo field ("8D audio").

```json
"rotation": { "rotationHz": 0.2 }
```

### `lowPass`

Smooths high frequencies. `smoothing` > 1 enables it; higher is darker.

```json
"lowPass": { "smoothing": 20 }
```

### `reverb` (Wavyzon)

A Freeverb (8 comb and 4 all-pass filters per channel, tuned for 48 kHz).
Every value is 0–1.

| Field | Meaning |
| --- | --- |
| `mix` | Dry/wet |
| `room` | Room size (decay) |
| `damping` | High-frequency absorption |

```json
"reverb": { "mix": 0.3, "room": 0.85, "damping": 0.4 }
```

`reverb` is listed in `/v4/info` `filters`, so Lavalink clients pass it
through unchanged.

## Processing order

Per deck:

```text
decoded PCM ─► resample / time-stretch (timescale) ─► karaoke ─► equalizer ─► low-pass
           ─► vibrato ─► tremolo ─► rotation ─► reverb ─► plugin filters (name order)
```

then the two decks are mixed (during a crossfade) and the player volume is
applied.

> Note: provisional. Where exactly the volume and the crossfade sit relative
> to plugin filters is the intended design, not yet a guarantee.

## Plugin filters

[Plugins](plugins.md#3-audio-filters) can add filters. They are enabled under
`filters.pluginFilters.<name>`, exactly like Lavalink's `pluginFilters`:

```json
{ "filters": { "pluginFilters": { "bitcrush": { "bits": 6, "downsample": 4, "mix": 1.0 } } } }
```

- The available names are listed in `/v1/info` `pluginFilters`.
- The value is passed to the plugin as-is. An unknown name, or a value the
  plugin rejects, fails the update with `400` and the plugin's message; the
  previous filters stay active.
- Plugin filters run **after** the built-in chain, in **name order**, on each
  deck separately (so both decks of a crossfade are processed).
- Like built-in effects, a plugin filter that is added **fades in** and one
  that is removed **fades out**.
- When only the configuration changes, the running filter is reconfigured in
  place (no reset, no click) if the plugin supports it; otherwise a new
  instance is built and crossfaded with the old one.
- Plugin filter state is cleared on seek and on track change.
- Removing a key from `pluginFilters` (or sending `"pluginFilters": {}`)
  turns that filter off.

The example plugin's `bitcrush`:

| Field | Default | Range | Meaning |
| --- | --- | --- | --- |
| `bits` | 8 | 1–16 | Bit depth |
| `downsample` | 1 | 1–64 | Keep one sample in N (sample-and-hold) |
| `mix` | 1.0 | 0–1 | Dry/wet |

### Equalizer APO configurations (`apo`)

The `apo` plugin (cargo feature `plugin-apo`, on by default) plays through
[Equalizer APO](https://sourceforge.net/projects/equalizerapo/)
configuration files: AutoEQ headphone corrections, REW exports, hand-written
EQ, convolution reverbs. Use a preset from the presets directory, or send the
text inline:

```json
{ "filters": { "pluginFilters": { "apo": { "preset": "headphones/hd600" } } } }
{ "filters": { "pluginFilters": { "apo": { "config": "Preamp: -6 dB\nFilter: ON PK Fc 100 Hz Gain 4 dB Q 1.41\nReverb: Mix 20 %" } } } }
```

| Command | Support |
| --- | --- |
| `Preamp` | Yes (several preamps add up in dB, as in APO) |
| `Filter` | `PK`/`Modal`/`PEQ`, `LP`, `LPQ`, `HP`, `HPQ`, `BP`, `NO`, `AP`, `LS`/`LSC` and `HS`/`HSC` (with `Q`, `BW Oct` or an `x dB` slope), `LS`/`HS 6dB` and `12dB` (corner frequency), `IIR` (order ≤ 16). `OFF` filters are skipped. |
| `GraphicEQ` | Yes: the curve (linear in log-frequency, flat outside the points) becomes a minimum-phase FIR, so it adds no latency |
| `Channel` | `L`, `R`, `1`, `2`, `all` (other speakers don't exist in a stereo frame and are ignored) |
| `Copy` | Yes, with factors, `dB` factors and constants; assignments on one line are evaluated together (`Copy: L=R R=L` swaps) |
| `Delay` | `ms` or `samples`, up to 1 s |
| `Include` | Files in the presets directory, relative to the including file (`\` or `/`), up to 8 levels |
| `Convolution` | WAV, FLAC or Ogg impulse responses in the presets directory, resampled to 48 kHz if needed, up to 10 s; IR channels go to the selected channels round-robin, as in APO |
| `Reverb` | **Wavyzon extension**: `Reverb: Mix 30 % Room 70 % Damping 50 %` (values in `%` or `0`–`1`, `Reverb: off`). The same Freeverb as the built-in `reverb` filter. APO ignores unknown commands, so files that use it still load in APO. |
| `Device`, `Stage` | Ignored (warning): the filter applies to whatever the player plays |
| `If`/`ElseIf`/`Else`/`EndIf`, `Eval`, inline `` `expressions` `` | Not evaluated: the block or line is skipped (warning) |

**Presets** are the `.txt` files under `WAVYZON_PLUGIN_APO_DIR` (default
`$WAVYZON_DATA_DIR/plugins/apo`), named by their path without `.txt`
(`headphones/hd600`; lookups ignore case, as on Windows). The directory,
its impulse responses and every preset are loaded and compiled at startup
and on `POST /v1/plugins/apo/reload`, never while audio plays. Paths can't
leave the directory and symbolic links are not followed.

| Route | |
| --- | --- |
| `GET /v1/plugins/apo/` | Presets (stages, skipped lines, or why one can't be used), impulse responses, directory warnings |
| `POST /v1/plugins/apo/reload` | Re-read the directory (new and edited files) |
| `POST /v1/plugins/apo/check` | Compile `{"config": "..."}` or `{"preset": "..."}`: `200` with the stages and warnings, or `400` with the error |

- A missing `Include`/`Convolution` file, a delay over 1 s or more than
  512 filters fail the update with `400`. A malformed line of a known command
  is skipped with a warning, as APO skips it.
- Convolution runs in 20 ms partitions, aligned on the player's frames: no
  added latency. All convolutions and GraphicEQs of one configuration are
  limited to about 20 s of impulse response per channel. A heavy preset
  (10 peaking filters, a GraphicEQ, a 3 s stereo IR and the reverb) costs
  about 1 % of one core per playing deck.
- Like other plugin filters, changing the configuration builds a new
  instance and crossfades to it; boosts can clip, so add a `Preamp`.
- Ready-made reverb presets (a convolution reverb with its impulse
  response and its generator, and a file-free `Reverb:` one) are in
  [`plugins/wavyzon-plugin-apo/examples`](https://gitlab.com/ihrz/wavyzon/-/tree/main/plugins/wavyzon-plugin-apo/examples).
  They are not loaded from there: copy them to the presets directory.

## Volume

`volume` is separate from filters: `0`–`1000`, `100` = unity. Values above
100 amplify and can clip; prefer the equalizer or normalisation
(`WAVYZON_NORMALIZE`) for loudness.
