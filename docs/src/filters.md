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

## Volume

`volume` is separate from filters: `0`–`1000`, `100` = unity. Values above
100 amplify and can clip; prefer the equalizer or normalisation
(`WAVYZON_NORMALIZE`) for loudness.
