# Mixing & crossfades

Every Wavyzon player has **two decks**. Deck A plays; deck B can be loaded,
pre-buffered and faded in, DJ-style, on an exact frame.

```text
deck A ─► bass / sweep DSP ─┐
                            ├─► crossfade (equal-power or linear) ─► filters ─► volume ─► out
deck B ─► bass / sweep DSP ─┘        (armed at a cue on deck A)
```

The node makes **no musical decisions**. Your bot chooses what to mix, where
and how (typically from the [track analysis](api/rest.md#get-v1mixanalysissourceidentifier));
Wavyzon executes it to the frame.

## Three ways to chain tracks

| Technique | API | Overlap | Use for |
| --- | --- | --- | --- |
| Normal | `TrackEndEvent{finished}` → play next | none, small gap | simple queues |
| Gapless | `"nextTrack": {"encoded": …}` on a player update | none, **no gap** | albums, live sets, DJ mixes |
| Crossfade | `mix/deckb` + `mix/crossfade` | yes | automix, radio-style transitions |

### Gapless

```json
PATCH /v1/sessions/{session}/players/{guild}
{ "nextTrack": { "encoded": "eyJzb3…" } }
```

The next track is resolved and pre-buffered in the background. When the
current track ends, it starts on the very next frame, and you get
`TrackEndEvent{finished}` followed by `TrackStartEvent` and
`TrackAudibleEvent`. `{ "encoded": null }` clears it.

## Crossfades step by step

### 1. Load deck B

```json
POST /v1/sessions/{session}/players/{guild}/mix/deckb
{ "encodedTrack": "eyJzb3…", "position": 32000 }
```

`position` lets deck B start anywhere, for example past its intro. The
response (`{"instance": 8}`) is the instance deck B will have once promoted.
Loading resolves and opens the track synchronously, so a `200` means it can
play; `422` means it can't.

### 2. Start or arm the transition

```json
POST /v1/sessions/{session}/players/{guild}/mix/crossfade
{
  "durationMs": 8000,
  "curve": "equal_power",
  "atPositionMs": 201500,
  "tempoRatio": 1.02,
  "gainRatio": 0.93,
  "profile": { "bassSwapAt": 0.5, "sweepOutToHz": 400 }
}
```

| Field | Default | Meaning |
| --- | --- | --- |
| `durationMs` | | Length of the fade |
| `curve` | `equal_power` | `equal_power` (constant loudness, best for music) or `linear`. Case and `-`/`_` don't matter |
| `atPositionMs` | now | **Arm** the fade: it starts on the exact frame where deck A crosses this position. Emits `MixArmedEvent` |
| `tempoRatio` | `1` | Play deck B at this speed to beat-match deck A. After promotion it glides back to its natural tempo over 11 s |
| `gainRatio` | `1` | Gain applied to deck B to match loudness |
| `profile` | none | Transition shape, below |

`profile`, where progress runs from `0` (start of fade) to `1` (end):

| Field | Meaning |
| --- | --- |
| `bassSwapAt` | Progress at which the low end swaps from A to B. Before it, B plays without bass; after it, A loses its bass. Omit for no bass swap |
| `bassSwapRamp` | Length of the swap, in progress units (default `0.12`) |
| `sweepOutFromHz` → `sweepOutToHz` | High-pass sweep on the outgoing deck (e.g. 20 → 400 Hz thins it out) |
| `sweepInFromHz` → `sweepInToHz` | High-pass sweep on the incoming deck (e.g. 300 → 20 Hz brings its body in) |

### 3. Follow the events

```text
MixArmedEvent        (if atPositionMs was set)
MixStartedEvent      fade begins: into, intoPositionMs, durationMs
   … playerUpdate: mixing=true, mixProgress 0→1 …
MixPromotedEvent     fade complete
TrackEndEvent        reason=replaced (old track)
TrackStartEvent      deck B is now deck A
```

If anything goes wrong (cancel, deck B fails, deck A ends before the cue)
you get `MixDroppedEvent` instead and deck A carries on (or ends normally).

### Cancel

```http
DELETE /v1/sessions/{session}/players/{guild}/mix
```

## DJ voice-overs

Like a radio host talking over the outro, `mix/host` plays a short clip
(MP3, base64, ≤ 512 KiB, ≤ 25 s) while ducking the music, and can drive the
transition into deck B:

```json
POST /v1/sessions/{session}/players/{guild}/mix/host
{ "audio": "SUQzBAAAAAAAI1RTU0UAAAAPAAADTGF2ZjU4Lj…", "atPositionMs": 199000, "outFadeMs": 2500, "leadMs": 1800, "rampMs": 1200, "duckDb": 9 }
```

| Field | Default | Max | Meaning |
| --- | --- | --- | --- |
| `atPositionMs` | now | | Start at this deck A position |
| `outFadeMs` | 2500 | 8000 | Fade-out of deck A |
| `leadMs` | 1800 | 8000 | How long the voice leads before deck B comes in |
| `rampMs` | 1200 | 8000 | Duck ramp time |
| `duckDb` | 9 | 30 | How much the music is lowered under the voice |

The clip is levelled automatically. Events: `HostStartedEvent`,
`HostEndedEvent`.

## Planning transitions with analysis

With `"analyze": true` on a player (or `POST /v1/mix/analyze` for offline
analysis), Wavyzon computes a fingerprint per track: BPM and beat grid,
downbeats and phrases, Camelot key, sections, vocal regions, intro and outro
regions, and EBU R128 loudness. A typical automix planner:

1. Fetch both fingerprints from `GET /v1/mix/analysis/{source}/{identifier}`.
2. Pick the cue: a downbeat at the start of A's outro phrase
   (`mixRegions.outroStartMs`, snapped to `rhythm.downbeatsMs`).
3. Pick B's entry: B's intro start, snapped to a downbeat.
4. `tempoRatio = bpmA / bpmB` if close (within ~6 %), else `1`.
5. `gainRatio` from `10^((lufsA − lufsB)/20)`.
6. Bass swap at the phrase boundary, a fade length of 4 to 16 bars.
7. `mix/deckb` with B's entry, then `mix/crossfade` with `atPositionMs` = the cue.
