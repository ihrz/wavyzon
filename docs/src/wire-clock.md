# The wire clock

Wavyzon's central promise: **"position" means what listeners hear**, not where
the decoder happens to be.

## Why

In Lavalink, a player's position is where the *decoder* is, typically a few
hundred milliseconds ahead of the network; `TrackStartEvent` fires before
the stream is even fetched; updates arrive every 5 s; and the position ignores
speed filters. Live lyrics drift by an amount that changes every song.

Wavyzon measures time at the **last possible moment**, when a packet is
written to the UDP socket:

```text
decode ─► resample ─► time-stretch ─► effects ─► mix ─► opus ─► DAVE ─► AEAD ─► UDP
                                                                                 │
                                                       position + sentAt stamped here
```

- The sender pulls **one** 20 ms frame per tick and writes it straight to the
  socket. There is no queue after it to drift.
- Each frame carries its exact **source position** through resampling and
  time-stretching, so positions stay correct under nightcore, vaporwave or
  an automix tempo ramp.
- The send loop is paced on a monotonic clock. When it falls behind, it says
  so with a [`SendStallEvent`](api/websocket.md#sendstallevent).

## The anchor

Every `playerUpdate` (on `/v1`) carries an **anchor**: one recent frame with
its position and send time.

| Field | Meaning |
| --- | --- |
| `anchorMs` | Track position of the anchor frame (ms, fractional) |
| `anchorSentAt` | Node time (Unix ms) that frame was written to the socket |
| `anchorAudibleAt` | `anchorSentAt + WAVYZON_PLAYOUT_DELAY_MS` |
| `rate` | Track milliseconds per wall-clock millisecond: `1.0` normally, `1.25` with `timescale.speed = 1.25`, varying during tempo ramps |
| `sounding` | `false` while paused, buffering, waiting for voice or for DAVE. When `false`, the position is frozen at `anchorMs` |

### The position formula

What is being sent right now:

```text
position = anchorMs + (serverNow − anchorSentAt) × rate
```

What listeners hear right now (use this for lyrics):

```text
heard = anchorMs + (serverNow − anchorAudibleAt) × rate
```

with `serverNow = clientNow − offset` (see [clock sync](#clock-sync)), and:

```text
if !sounding:        position = anchorMs
position = clamp(position, 0, durationMs)    // durationMs > 0 only
```

In JavaScript:

```js
function livePosition(player, offset, { heard = true } = {}) {
  if (!player.sounding) return player.anchorMs;
  const serverNow = Date.now() - offset;
  const at = heard ? player.anchorAudibleAt : player.anchorSentAt;
  return Math.max(0, player.anchorMs + (serverNow - at) * player.rate);
}
```

Recompute on every animation frame; replace the anchor whenever a new
`playerUpdate` arrives. Because the formula already accounts for `rate`, the
new anchor and the extrapolated old one agree to within a frame, so the
display doesn't jump.

### Event anchors

`TrackAudibleEvent` is itself an anchor: `positionMs` was sent at `sentAt`
and is heard at `audibleAt`. `PositionEvent` (after a seek) is a hint to
re-anchor at the next `playerUpdate`, which follows immediately.

## Clock sync

`sentAt` and `audibleAt` are **node** clock values. Your bot's clock (or a
browser's) can be off by seconds. Measure the offset with an NTP-style
exchange against `GET /v1/clock`:

```js
async function measureOffset(base, password, samples = 5) {
  let best = { rtt: Infinity, offset: 0 };
  for (let i = 0; i < samples; i++) {
    const t0 = Date.now();
    const { now } = await fetch(`${base}/v1/clock`, { headers: { Authorization: password } }).then((r) => r.json());
    const t1 = Date.now();
    const rtt = t1 - t0;
    // assume the server read its clock halfway through the round trip
    const offset = (t0 + t1) / 2 - now; // clientNow − serverNow
    if (rtt < best.rtt) best = { rtt, offset };
  }
  return best.offset;
}
```

- Keep the sample with the **lowest round trip**: it has the least
  asymmetric delay.
- Re-measure every few minutes; clocks drift.
- If you relay positions to browsers, sync the browser against your bot the
  same way and add the offsets.

## Playout delay

Between the moment Wavyzon sends a packet and the moment a listener hears
it, there is Discord's media server, the listener's network and jitter
buffer. That delay is fairly constant for a region. Measure it once (for
example by recording a Discord client while playing a click track and
comparing with `sentAt`) and set:

```bash
WAVYZON_PLAYOUT_DELAY_MS=180
```

It shifts every `audibleAt` / `anchorAudibleAt` by that amount. One constant
replaces the unpredictable error of decoder-side positions.

## Under filters, mixes and seeks

| Situation | What the clock does |
| --- | --- |
| `timescale.speed` / `rate` | `rate` reflects the change; positions are in **track** time |
| `timescale.pitch` only | No effect on `rate` |
| Filter glide (speed sweeping from 1.0 to 1.25) | `rate` follows the glide; a fresh anchor is sent every update |
| Crossfade | The main track's clock runs until promotion; `mixInto`/`mixIntoPositionMs` give deck B's position; `MixPromotedEvent` hands over |
| Tempo-matched deck B | Deck B's `rate` glides back to 1.0 after promotion (11 s) |
| Seek | `PositionEvent`, then a new anchor |
| Pause with fade | `sounding` turns `false` when the fade-out completes |
| DAVE epoch transition | Audio is held (never sent unencrypted), `sounding` is `false` meanwhile |
