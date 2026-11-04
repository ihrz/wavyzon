# wavyzon-testkit

Test doubles and assertions for Wavyzon. The crate is not published
(`publish = false`). Add it as a dev-dependency:

```toml
[dev-dependencies]
wavyzon-testkit = { path = "../wavyzon-testkit" }
```

Everything runs in-process on `127.0.0.1` with random ports. Nothing needs the
network, Discord, or audio files.

| Mock | Module | Replaces |
| --- | --- | --- |
| `MockVoiceGateway` | `gateway` | Discord voice server (WebSocket v8 + UDP) |
| `PacketSealer` / `PacketOpener`, RTP and discovery helpers | `gateway::wire` | the voice transport bytes |
| `ToneSource`, `FailingSource`, `SlowSource`, `ScriptedSource` | `sources` | real audio sources |
| `MockFrameSource` | `frames` | the player, as seen by the voice sender |
| WAV builder, signals, RMS / peak / Goertzel, asserts | `audio` | audio files, by-ear checks |
| `TestClient` | `client` | a Lavalink client library (WebSocket + REST) |

---

## `MockVoiceGateway`

This is a fake Discord voice server. It plays the normal v8 handshake:

1. It sends **Hello** `{heartbeat_interval}`.
2. It waits for **Identify** and answers **Ready** `{ssrc, ip, port, modes}`.
3. It answers **UDP IP discovery** on its UDP socket.
4. It answers **SelectProtocol** with **SessionDescription**
   `{mode, secret_key: [32 numbers], dave_protocol_version: 0}`.
5. It answers each **Heartbeat** `{t, seq_ack}` with a **HeartbeatACK** `{t}`.
6. It answers **Resume** with **Resumed**.

Every server JSON message except Hello and ACK carries a `seq` field.

On UDP, it parses RTP and decrypts with the negotiated `*_rtpsize` AEAD mode
(`aead_aes256_gcm_rtpsize` or `aead_xchacha20_poly1305_rtpsize`). In that
layout the header is the AAD and a 4-byte big-endian nonce counter is
appended. It then records the Opus payloads with their arrival time.

```rust
use std::time::Duration;
use wavyzon_testkit::gateway::{MockVoiceGateway, close_code, op};

#[tokio::test]
async fn sends_audio() {
    let gw = MockVoiceGateway::start().await.unwrap();

    // Give the voice connection gw.endpoint() == "ws://127.0.0.1:PORT".
    // The connection dials gw.ws_url() == "ws://127.0.0.1:PORT/?v=8".
    let conn = my_voice_dial(gw.endpoint(), "token", "session", source.clone());

    let id = gw.wait_for_op(op::IDENTIFY, Duration::from_secs(2)).await.unwrap();
    assert_eq!(id.d["token"], "token");
    let packets = gw.wait_for_packets(50, Duration::from_secs(3)).await.unwrap();
    assert!(packets.iter().all(|p| p.ssrc == gw.ssrc()));
    assert_eq!(gw.speaking_events()[0].speaking, 1);

    // Terminal close: the connection must report WebSocketClosedEvent{4014, terminal}.
    gw.close(close_code::DISCONNECTED, "kicked").await;
}
```

### Inspecting what the client did

| Method | Returns |
| --- | --- |
| `identify_payload()`, `resume_payload()`, `select_protocol_payload()` | the last `d` of each message |
| `negotiated_mode()` | the mode the client chose |
| `received_packets()` | decrypted `ReceivedPacket { at, from, sequence, timestamp, ssrc, payload_type, nonce, opus }` |
| `packet_count()` | the number of decrypted packets |
| `dropped_packets()` | packets thrown away by a fault |
| `undecryptable_packets()` | packets that failed to decrypt |
| `speaking_events()` | `SpeakingEvent { at, speaking, delay, ssrc }` |
| `heartbeats()` | `HeartbeatEvent { at, nonce, seq_ack }` |
| `client_messages()`, `messages_with_op(op)` | every message received, text or binary |
| `connection_count()`, `connect_uris()`, `is_connected()` | connection history |
| `closes()` | `CloseRecord { code, reason, by_server, … }` |
| `sessions_established()`, `resumes()`, `discovery_requests()`, `last_seq()` | counters |

Each `wait_*` helper polls and returns `Err(WaitTimeout)` when time runs out:

- `wait_for_packets(n, t)`
- `wait_for_op(op, t)`
- `wait_for_op_count(op, nth, t)` (useful for "Identify #2 after a reconnect")
- `wait_for_sessions(n, t)`
- `wait_for_speaking(n, t)`
- `wait_until(t, what, |gw| …)`

### Faults and scripting

```rust
gw.update_faults(|f| {
    f.withhold_heartbeat_acks = true;      // zombie connection, so the client must reconnect
    f.reply_delay = Duration::from_millis(500);
    f.ignore_discovery = 2;                // the client must resend discovery
    f.drop_every_nth_rtp = 10;             // or f.drop_all_rtp = true
    f.reject_resume = true;                // Resume is closed with 4006
    f.stall_after_identify = true;         // Ready never comes
    f.stall_after_select_protocol = true;  // SessionDescription never comes
});

gw.close(4015, "crashed").await;        // resumable close
gw.abort_connection().await;            // TCP drop with no close frame
gw.send_op(op::CLIENTS_CONNECT, json!({"user_ids": ["1"]})).await;
gw.send_binary_op(op::DAVE_EXTERNAL_SENDER, bytes).await;   // [seq u16][op][payload]
gw.send(GatewayMessage::Text(raw_json)).await;

// Reply to client messages, for example to script a DAVE exchange later on.
gw.set_hook(|m| match m.op {
    op::DAVE_KEY_PACKAGE => vec![GatewayMessage::Binary { op: op::DAVE_ANNOUNCE_COMMIT, payload: vec![0, 1] }],
    _ => vec![],
});
```

`GatewayConfig` sets these values:

- `ssrc`
- `heartbeat_interval_ms`
- `modes`, the offered mode strings, which can include unknown ones
- `secret_key`
- `expected_token`: a wrong token gets close code 4004
- `dave_protocol_version`, which defaults to `0` (DAVE is not implemented)
- `ready_ip`
- `discovery_ip`

The mock closes with these codes when the client breaks the protocol:

| Code | Cause |
| --- | --- |
| 4002 | bad JSON or missing Identify fields |
| 4003 | a message before Identify |
| 4004 | wrong token |
| 4005 | a second Identify |
| 4006 | a Resume it rejects |
| 4012 | a protocol other than `udp` |
| 4016 | an unknown or unoffered encryption mode |

### What `wavyzon-voice` must accept

- **The endpoint scheme.** Discord sends `host:port` with no scheme, and Cadence
  always prepends `wss://`. For tests, the voice crate must keep an endpoint
  that already starts with `ws://` (or `wss://`) and only append `/?v=8`.
  `gw.endpoint()` returns `ws://127.0.0.1:PORT`. `gw.host_port()` returns the
  bare form.
- **The Ready address.** Ready sends `ip: "127.0.0.1"` and the UDP port. IP
  discovery answers with the sender's own address.
- **The secret key format.** `secret_key` is a JSON array of 32 numbers, as on
  Discord.

### `gateway::wire`

These are the same primitives the mock uses, made public so a test can build
or check packets itself:

- `rtp_header(seq, ts, ssrc)` and `RtpHeader::parse`. The parser handles CSRCs
  and the `X` extension; under rtpsize the extension body is encrypted and is
  stripped from the result.
- `PacketSealer::new(mode, &key).seal(&header, &opus)`
- `PacketOpener::new(mode, &key).open(&packet)`
- `discovery_request` / `discovery_response` / `parse_discovery`
- `encode_client_binary` / `encode_server_binary`
- `TransportMode::{Aes256Gcm, XChaCha20Poly1305}`, with `as_str()` and
  `parse()`

---

## Mock sources

Every mock implements `wavyzon_plugin::Source` and `Playable`. Each streams
`Format::Pcm48` sine tones generated as it is read, so long tracks cost no
memory. A tone identifier has the form `"<freq_hz>:<duration_ms>"`, and a
duration of `0` means an endless live stream.

```rust
use std::{sync::Arc, time::Duration};
use wavyzon_plugin::{Severity, Source};
use wavyzon_testkit::*;

let tone = Arc::new(ToneSource::new());                 // "tone", prefix "tonesearch", links tone://440:3000
let track = tone.tone_track(440.0, 3_000);
let stream = tone.playable().unwrap().open(&track, 1_500).await?;
assert_eq!(stream.start_sample, 72_000);                // start_ms * 48, phase-continuous
assert_eq!(tone.opens()[0].1, 1_500);

let failing = FailingSource::new(Severity::Suspicious)  // "failing", prefix "failsearch"
    .at(FailAt::MidStream { after_ms: 2_000 })          // or FailAt::Resolve / FailAt::Open (default)
    .with_message("refused by source");

let slow = SlowSource::new()                            // "slow", prefix "slowsearch"
    .resolve_delay(Duration::from_secs(1))
    .open_delay(Duration::from_secs(2))
    .stall_after(1_000, None);                          // 1 s of audio, then a stall that never ends
assert!(!slow.stalled());

let scripted = ScriptedSource::new("scripted")
    .with_search_prefix("scsearch")
    .matching_urls("https://scripted.test/")
    .on_search("hello", Ok(vec![sources::test_track("scripted", "a", 10_000)]))
    .on_url("https://scripted.test/a", Err(Failure::common("scripted", "gone", None)))
    .on_track("a", Ok(sources::test_track("scripted", "a", 10_000)));
// scripted.calls() == [Search("hello"), Open { identifier, start_ms }, Warm(..), …]
```

| Mock | What it is for |
| --- | --- |
| `ToneSource` | Searches and links for `"<freq>:<ms>"` identifiers. It is seekable and honours `start_ms` exactly. |
| `FailingSource` | The configured `Failure` at the chosen stage. Resolve gives `loadType: error`, Open gives an exception before any audio, and MidStream gives an exception after `after_ms` of audio. |
| `SlowSource` | Load timeouts, a slow first byte, and `TrackStuckEvent`, using `stall_after(ms, Some(d))` or `stall_after(ms, None)` for a stall that never ends. |
| `ScriptedSource` | Canned search, URL and track results. It records every call and plays a 440 Hz tone for `track.duration_ms`. Use `not_playable()` to remove playback. |

`sources::ToneReader` is the `AsyncRead` tone generator on its own.

---

## `MockFrameSource`

This implements `wavyzon_audio::FrameSource` for voice sender tests.

```rust
let src = Arc::new(MockFrameSource::tone(440.0, 0.5).with_limit(250));  // 5 s, then end of track
// or MockFrameSource::silence() / ::from_frames(vec![...])
let conn = my_voice_dial(gw.endpoint(), ..., src.clone());
src.wait_for_sent(50, Duration::from_secs(3)).await?;
let gaps = src.send_intervals();                 // each gap should be about 20 ms
src.end_track();                                 // next_frame now returns None, so the sender sends silence and stops speaking
assert!(src.stalls().is_empty());
```

It has these controls:

- `end_track()`
- `start_instance(n)`, which resets the position
- `set_paused(bool)`
- `push_frames(..)`
- `set_content(..)`

It records these values:

- `pulled()`
- `next_frame_calls()`
- `pull_instants()`
- `sent_frames()`, as `SentFrame { info, at }`
- `sent_count()`
- `send_intervals()`
- `stalls()`
- `ended()`

`FrameInfo.position_ms` is the frame index × 20. `instance` defaults to `1`.

---

## Audio helpers (`audio`)

The helpers fall into these groups:

- **Generators:** `sine`, `silence`, `clicks(bpm, …)`, `noise(seed)`, and
  `sine_sample` for a single sample.
- **WAV:** `WavBuilder::new(rate, ch)`, chained with `.tone()`, `.silence()`,
  `.clicks()`, `.noise()` and `.samples()`, then `.build()`. `wav_bytes`
  writes a WAV directly.
- **Conversions:** `channel`, `mono`, `frames_to_samples`, `pcm_from_bytes`
  and `pcm_to_bytes`.
- **Streams:** `read_body(&mut stream, max)`, `read_pcm48(&mut stream, ms)`, and
  `decode_bytes(&format, bytes)`, which runs the real
  `wavyzon_audio::codec::decode`.
- **Measurements:** `rms`, `peak` and `dbfs`; `zero_crossing_frequency`,
  `goertzel(mono, rate, freq)` (amplitude) and `dominant_frequency`; `onsets`
  and `estimate_bpm`.
- **Assertions:** `assert_within`, `assert_position_ms(actual, expected, tol)`,
  `assert_frequency(mono, rate, hz, tol)`, `assert_silent` and
  `assert_audible`.

```rust
let wav = WavBuilder::new(48_000, 2).tone(440.0, 500, 0.5).clicks(120.0, 2_000, 0.9).build();
let (pcm, rate) = audio::decode_bytes(&Format::Encoded { hint: Some("wav".into()) }, wav)?;
audio::assert_frequency(&audio::channel(&pcm[..500 * 48 * 2], 2, 0), rate, 440.0, 1.0);
```

---

## `TestClient`

This is a Lavalink-style client for end-to-end tests of the server. It sends
these headers on the WebSocket:

- `Authorization`
- `User-Id`
- `Client-Name`
- `Session-Id`, when resuming

It waits for `ready` and records every message.

```rust
let c = TestClient::builder("http://127.0.0.1:2333").password("pw").user_id("123").api("v4").connect().await?;
let res = c.load_tracks("tonesearch:440:3000").await?;
c.voice_update("42", &client::VoiceState { token: "t".into(), endpoint: gw.endpoint(), session_id: "s".into(), channel_id: "9".into() }).await?;
c.play("42", res["data"][0]["encoded"].as_str().unwrap()).await?;
c.wait_for_guild_event("42", EventType::TrackStartEvent, Duration::from_secs(5)).await?;
let update = c.wait_for_player_update("42", Duration::from_secs(5)).await?;
c.destroy_player("42").await?;
```

Waits work from a cursor. Each `wait_for*` scans only the messages after the
previous match, so waits called one after another see events in order.
`skip_seen()` and `clear()` reset the cursor.

**REST methods:**

- `info`
- `stats`
- `load_tracks`
- `decode_track`
- `players`
- `get_player`
- `update_player(guild, body, no_replace)`
- `play`
- `voice_update`
- `destroy_player`
- `update_session`
- `request(Method, path, body)` for raw calls. The path is relative to the
  `/<api>` prefix unless it starts with `/`.

A non-2xx response becomes `TestClientError::Status { status, body }`.

---

## Self-tests

Run them with `cargo test -p wavyzon-testkit`:

- **Gateway:** a scripted voice client inside the tests runs the full
  handshake in both AEAD modes, then exercises heartbeats, forced closes,
  resume, every protocol-error close code, each fault, hooks and binary DAVE
  framing.
- **Sources:** seeking, phase continuity, live streams, each failure stage,
  and stalls (tested under a paused clock).
- **Audio:** the WAV builder and `ToneSource` are decoded by
  `wavyzon_audio::codec::decode` and checked with Goertzel and zero crossings.
- **`TestClient`:** runs against a small axum stand-in for a Lavalink v4
  server.
