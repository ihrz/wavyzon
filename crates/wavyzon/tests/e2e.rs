//! End to end: a real node, a `TestClient`, plugin sources, and a mock Discord
//! voice gateway receiving the RTP.
// They use the example plugin (bitcrush filter, routes, event listener).
#![cfg(feature = "plugin-example")]

mod common;

use std::sync::Arc;
use std::time::Duration;

use common::*;
use reqwest::Method;
use serde_json::json;
use wavyzon_plugin::EventType;
use wavyzon_testkit::client::VoiceState;
use wavyzon_testkit::{MockVoiceGateway, TestClient};

fn voice(gw: &MockVoiceGateway, session: &str) -> VoiceState {
    VoiceState {
        token: "token".into(),
        endpoint: gw.endpoint(),
        session_id: session.into(),
        channel_id: "456".into(),
    }
}

async fn plays_into_the_gateway(api: &str) {
    let n = start(&[]).await;
    let gw = MockVoiceGateway::start().await.unwrap();
    let c = n.client(api).await;
    if api == "v4" {
        c.update_session(json!({ "wavyzonEvents": true })).await.unwrap();
    }
    let res = c.load_tracks("testtonesearch:440:1500").await.unwrap();
    let encoded = res["data"][0]["encoded"].as_str().unwrap().to_string();
    c.voice_update("42", &voice(&gw, "vs1")).await.unwrap();
    c.play("42", &encoded).await.unwrap();

    let start = c
        .wait_for_guild_event("42", EventType::TrackStartEvent, WAIT)
        .await
        .unwrap();
    let audible = c
        .wait_for_guild_event("42", EventType::TrackAudibleEvent, WAIT)
        .await
        .unwrap();
    assert!(audible["sentAt"].as_i64().unwrap() > 0, "{audible}");
    assert_eq!(audible["instance"], start["instance"]);
    let packets = gw.wait_for_packets(25, WAIT).await.unwrap();
    assert!(packets.iter().all(|p| p.ssrc == gw.ssrc()));
    assert!(packets.iter().any(|p| !p.is_opus_silence()), "real audio was sent");
    let id = gw.identify_payload().unwrap();
    assert_eq!(
        (id["server_id"].as_str(), id["user_id"].as_str()),
        (Some("42"), Some("123"))
    );

    let p = c.get_player("42").await.unwrap();
    assert_eq!(p["state"]["connected"], true, "{p}");

    let end = c
        .wait_for_guild_event("42", EventType::TrackEndEvent, WAIT)
        .await
        .unwrap();
    assert_eq!(end["reason"], "finished");
    assert!(
        n.tone
            .opens()
            .iter()
            .any(|(t, at)| t.identifier == "440:1500" && *at == 0)
    );
    // the example plugin's listener saw the events
    let (_, stats) = call(Method::GET, &n.url("/v1/plugins/example/"), None).await;
    assert!(stats["totalEvents"].as_u64().unwrap() >= 3, "{stats}");
    n.stop().await;
}

#[tokio::test]
async fn v1_plays_into_the_voice_gateway() {
    plays_into_the_gateway("v1").await;
}

#[tokio::test]
async fn v4_plays_into_the_voice_gateway() {
    plays_into_the_gateway("v4").await;
}

#[tokio::test]
async fn track_before_voice_waits_without_its_clock_moving() {
    let n = start(&[]).await;
    let gw = MockVoiceGateway::start().await.unwrap();
    let c = n.client("v1").await;
    c.play("42", &tone_encoded(440, 10_000)).await.unwrap();
    c.wait_for_guild_event("42", EventType::TrackStartEvent, WAIT)
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(600)).await;
    let p = c.get_player("42").await.unwrap();
    assert_eq!(p["state"]["player"]["positionMs"], 0, "{p}");
    assert_eq!(p["state"]["player"]["sounding"], false);
    assert!(c.events().iter().all(|e| e["type"] != "TrackAudibleEvent"));
    c.voice_update("42", &voice(&gw, "vs1")).await.unwrap();
    let audible = c
        .wait_for_guild_event("42", EventType::TrackAudibleEvent, WAIT)
        .await
        .unwrap();
    assert!(audible.get("positionMs").is_none(), "audible from the start: {audible}");
    gw.wait_for_packets(10, WAIT).await.unwrap();
    n.stop().await;
}

#[tokio::test]
async fn voice_updates_same_server_is_a_no_op_new_one_reconnects() {
    let n = start(&[]).await;
    let gw = MockVoiceGateway::start().await.unwrap();
    let c = n.client("v1").await;
    c.voice_update("42", &voice(&gw, "vs1")).await.unwrap();
    gw.wait_for_sessions(1, WAIT).await.unwrap();
    c.voice_update("42", &voice(&gw, "vs1")).await.unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(gw.connection_count(), 1, "the same voice server again is ignored");
    c.voice_update("42", &voice(&gw, "vs2")).await.unwrap();
    gw.wait_for_sessions(2, WAIT).await.unwrap();
    assert_eq!(gw.connection_count(), 2);
    assert!(
        gw.closes().iter().any(|r| r.code == Some(1000) || !r.by_server),
        "old connection closed: {:?}",
        gw.closes()
    );
    let id = gw.identify_payload().unwrap();
    assert_eq!(id["session_id"], "vs2");
    n.stop().await;
}

#[tokio::test]
async fn terminal_voice_close_is_reported() {
    let n = start(&[]).await;
    let gw = MockVoiceGateway::start().await.unwrap();
    let c1 = n.client("v1").await;
    let c4 = n.client("v4").await;
    c1.voice_update("42", &voice(&gw, "a")).await.unwrap();
    gw.wait_for_sessions(1, WAIT).await.unwrap();
    gw.close(4014, "kicked").await;
    let e = c1
        .wait_for_guild_event("42", EventType::WebSocketClosedEvent, WAIT)
        .await
        .unwrap();
    assert_eq!(
        (e["code"].as_u64(), e["terminal"].as_bool()),
        (Some(4014), Some(true)),
        "{e}"
    );
    assert_eq!(e["closeReason"], "kicked");
    let p = c1.get_player("42").await.unwrap();
    assert_eq!(p["state"]["connected"], false);

    let gw4 = MockVoiceGateway::start().await.unwrap();
    c4.voice_update("43", &voice(&gw4, "b")).await.unwrap();
    gw4.wait_for_sessions(1, WAIT).await.unwrap();
    gw4.close(4014, "kicked").await;
    let e = c4
        .wait_for_guild_event("43", EventType::WebSocketClosedEvent, WAIT)
        .await
        .unwrap();
    assert_eq!(
        (e["code"].as_u64(), e["reason"].as_str(), e["byRemote"].as_bool()),
        (Some(4014), Some("kicked"), Some(true))
    );
    n.stop().await;
}

#[tokio::test]
async fn plugin_filter_applies_while_playing() {
    let n = start(&[]).await;
    let gw = MockVoiceGateway::start().await.unwrap();
    let c = n.client("v4").await;
    c.voice_update("42", &voice(&gw, "vs")).await.unwrap();
    c.play("42", &tone_encoded(440, 3_000)).await.unwrap();
    c.wait_for_guild_event("42", EventType::TrackStartEvent, WAIT)
        .await
        .unwrap();
    let p = c
        .update_player(
            "42",
            json!({ "filters": { "pluginFilters": { "bitcrush": { "bits": 4, "downsample": 4 } } } }),
            false,
        )
        .await
        .unwrap();
    assert_eq!(p["filters"]["pluginFilters"]["bitcrush"]["bits"], 4);
    let err = c
        .update_player(
            "42",
            json!({ "filters": { "pluginFilters": { "unknown": {} } } }),
            false,
        )
        .await
        .unwrap_err();
    assert!(matches!(
        err,
        wavyzon_testkit::TestClientError::Status { status: 400, .. }
    ));
    gw.wait_for_packets(20, WAIT).await.unwrap();
    n.stop().await;
}

#[tokio::test]
async fn state_is_saved_on_shutdown_and_restored_on_start() {
    let dir = Arc::new(TempDir::new());
    let state = dir.0.join("state").join("sessions.json");
    let state_s = state.display().to_string();
    let env = [("WAVYZON_STATE_FILE", state_s.as_str())];
    let n = start_in(dir.clone(), &env).await;
    let c = n.client("v4").await;
    let sid = c.session_id().to_string();
    c.update_session(json!({ "resuming": true, "timeout": 120, "wavyzonEvents": true }))
        .await
        .unwrap();
    // never-resuming sessions are not saved
    let _other = n.client("v1").await;
    c.update_player(
        "42",
        json!({ "track": { "encoded": tone_encoded(440, 600_000), "userData": { "requester": "9" } }, "position": 61_000, "volume": 77, "bitrate": 128000 }),
        false,
    )
    .await
    .unwrap();
    c.wait_for_guild_event("42", EventType::TrackStartEvent, WAIT)
        .await
        .unwrap();
    n.stop().await;
    assert!(state.exists(), "state file written");

    let n2 = start_in(dir.clone(), &env).await;
    assert!(!state.exists(), "state file consumed");
    assert_eq!(n2.sessions.sessions().len(), 1);
    let c2 = TestClient::builder(&n2.base)
        .password(PASSWORD)
        .user_id("123")
        .api("v4")
        .resume(&sid)
        .connect()
        .await
        .unwrap();
    assert!(c2.resumed());
    let sess = n2.sessions.get(&sid).unwrap();
    assert!(sess.extra_events());
    let p = c2.get_player("42").await.unwrap();
    assert_eq!(p["volume"], 77);
    assert_eq!(p["track"]["info"]["identifier"], "440:600000");
    assert_eq!(p["track"]["userData"]["requester"], "9");
    let pos = p["state"]["position"].as_i64().unwrap();
    assert!((61_000..62_000).contains(&pos), "resumed at its position: {pos}");
    assert_eq!(sess.guild("42", false).unwrap().preferred_bitrate(), 128000);
    assert!(
        n2.tone.opens().iter().any(|o| o.1 == 61_000),
        "opened at the saved position: {:?}",
        n2.tone.opens()
    );
    n2.stop().await;
}

#[tokio::test]
async fn expired_saved_sessions_are_not_restored() {
    let dir = Arc::new(TempDir::new());
    let state = dir.0.join("sessions.json");
    let saved_at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
        - 3_600_000;
    let old = json!({ "version": 1, "savedAt": saved_at, "sessions": [
        { "id": "old", "userId": "123", "clientName": "x", "protocol": 1, "timeoutMs": 60_000, "cadenceEvents": true, "players": [] }
    ]});
    std::fs::write(&state, old.to_string()).unwrap();
    let state_s = state.display().to_string();
    let n = start_in(dir.clone(), &[("WAVYZON_STATE_FILE", state_s.as_str())]).await;
    assert!(n.sessions.get("old").is_none(), "expired session restored");
    assert!(!state.exists(), "state file consumed even when nothing was restored");
    n.stop().await;

    let fresh = json!({ "version": 1, "savedAt": saved_at + 3_600_000, "sessions": [
        { "id": "fresh", "userId": "123", "clientName": "x", "protocol": 0, "timeoutMs": 60_000, "cadenceEvents": true, "players": [
            { "guildId": "1", "positionMs": 0, "paused": true, "volume": 30, "filters": {} }
        ] }
    ]});
    std::fs::write(&state, fresh.to_string()).unwrap();
    let n = start_in(dir.clone(), &[("WAVYZON_STATE_FILE", state_s.as_str())]).await;
    let s = n.sessions.get("fresh").expect("fresh session restored");
    assert!(s.extra_events(), "Cadence's cadenceEvents name is read");
    assert_eq!(s.guild("1", false).unwrap().player.state().volume, 30);
    let wrong_user = TestClient::builder(&n.base)
        .password(PASSWORD)
        .user_id("9")
        .api("v1")
        .resume("fresh")
        .connect()
        .await
        .unwrap();
    assert!(!wrong_user.resumed());
    let c = TestClient::builder(&n.base)
        .password(PASSWORD)
        .user_id("123")
        .api("v1")
        .resume("fresh")
        .connect()
        .await
        .unwrap();
    assert!(c.resumed());
    n.stop().await;
}

/// What lavalink-client does after an automatic crossfade: on `finished` it
/// advances its queue with `play({ noReplace: true })`, i.e. the next track
/// with `position: 0`. That must neither restart nor seek the track already
/// fading in (it used to seek it back to 0, so its first seconds were heard
/// twice), and it acknowledges that track at once.
#[tokio::test]
async fn crossfaded_next_track_survives_the_clients_no_replace_play() {
    let n = start(&[("WAVYZON_CROSSFADE_MS", "300")]).await;
    let gw = MockVoiceGateway::start().await.unwrap();
    let c = n.client("v4").await;
    let (a, b) = (tone_encoded(440, 1500), tone_encoded(660, 4000));
    c.voice_update("42", &voice(&gw, "vs1")).await.unwrap();
    c.play("42", &a).await.unwrap();
    c.wait_for_guild_event("42", EventType::TrackStartEvent, WAIT)
        .await
        .unwrap();
    c.update_player("42", json!({ "nextTrack": { "encoded": b } }), false)
        .await
        .unwrap();
    let end = c
        .wait_for_guild_event("42", EventType::TrackEndEvent, WAIT)
        .await
        .unwrap();
    assert_eq!(end["reason"], "finished", "{end}");

    let asked = std::time::Instant::now();
    let body = json!({ "track": { "encoded": b, "userData": {} }, "volume": 100, "position": 0 });
    c.update_player("42", body, true).await.unwrap();
    let started = c
        .wait_for_guild_event("42", EventType::TrackStartEvent, WAIT)
        .await
        .unwrap();
    assert_eq!(started["track"]["encoded"], b.as_str(), "{started}");
    assert!(
        asked.elapsed() < Duration::from_secs(2),
        "the start waited for the adoption window: {:?}",
        asked.elapsed()
    );
    tokio::time::sleep(Duration::from_millis(300)).await;
    let opens: Vec<_> = n
        .tone
        .opens()
        .into_iter()
        .filter(|(t, _)| t.identifier == "660:4000")
        .collect();
    assert_eq!(opens.len(), 1, "the next track was reopened: {opens:?}");
    n.stop().await;
}
