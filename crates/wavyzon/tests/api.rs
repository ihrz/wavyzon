//! Port of Cadence's `internal/api/api_test.go` and `lavalinkV4_test.go`,
//! plus Wavyzon's plugin routes, filters and error shapes.
// They use the example plugin (bitcrush filter, routes, event listener).
#![cfg(feature = "plugin-example")]

mod common;

use std::time::Duration;

use common::*;
use reqwest::Method;
use serde_json::json;
use wavyzon_plugin::{EventType, Track};
use wavyzon_testkit::TestClient;

#[tokio::test]
async fn auth_is_required_everywhere() {
    let n = start(&[]).await;
    for path in [
        "/v1/info",
        "/v4/info",
        "/version",
        "/metrics",
        "/v1/plugins",
        "/v1/plugins/example/",
        "/v1/cache/index",
    ] {
        let (status, body) = call_with(Method::GET, &n.url(path), None, None).await;
        assert_eq!(status, 401, "{path}");
        assert_eq!(body["message"], "missing or wrong Authorization header");
        assert_eq!(body["path"], path.trim_end_matches('/'));
        let (status, _) = call_with(Method::GET, &n.url(path), None, Some("wrong")).await;
        assert_eq!(status, 401, "{path}");
    }
    let (status, _) = call(Method::GET, &n.url("/v1/info"), None).await;
    assert_eq!(status, 200);
    let err = TestClient::builder(&n.base)
        .password("nope")
        .user_id("1")
        .api("v1")
        .connect()
        .await
        .err()
        .unwrap();
    assert!(
        matches!(err, wavyzon_testkit::TestClientError::Status { status: 401, .. }),
        "{err}"
    );
    n.stop().await;
}

#[tokio::test]
async fn cache_routes_accept_the_cache_secret_only_there() {
    let n = start(&[("WAVYZON_CACHE_SECRET", "peer-secret")]).await;
    let (status, body) = call_with(Method::GET, &n.url("/v1/cache/index"), None, Some("peer-secret")).await;
    assert_eq!((status, body), (200, json!([])));
    let (status, _) = call_with(Method::GET, &n.url("/v1/cache/file?key=abc"), None, Some("peer-secret")).await;
    assert_eq!(status, 404);
    let (status, _) = call_with(Method::GET, &n.url("/v1/info"), None, Some("peer-secret")).await;
    assert_eq!(status, 401);
    n.stop().await;
}

#[tokio::test]
async fn websocket_needs_user_id() {
    let n = start(&[]).await;
    let err = TestClient::builder(&n.base)
        .password(PASSWORD)
        .user_id("")
        .api("v1")
        .connect()
        .await
        .err()
        .unwrap();
    assert!(
        matches!(err, wavyzon_testkit::TestClientError::Status { status: 400, .. }),
        "{err}"
    );
    n.stop().await;
}

#[tokio::test]
async fn info_lists_plugins_sources_and_filters() {
    let n = start(&[]).await;
    let (_, info) = call(Method::GET, &n.url("/v1/info"), None).await;
    assert_eq!(info["version"], env!("CARGO_PKG_VERSION"));
    assert_eq!(info["wireClock"], true);
    let sources: Vec<&str> = info["sources"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s.as_str().unwrap())
        .collect();
    assert!(
        sources.contains(&"testtone") && sources.contains(&"tone") && sources.contains(&"http"),
        "{sources:?}"
    );
    assert!(info["pluginFilters"].as_array().unwrap().contains(&json!("bitcrush")));
    let names: Vec<&str> = info["plugins"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["name"].as_str().unwrap())
        .collect();
    let mut want = vec!["testkit", "example"];
    if cfg!(feature = "plugin-apo") {
        want.push("apo");
    }
    if cfg!(feature = "plugin-flowery") {
        want.push("flowery");
    }
    if cfg!(feature = "plugin-genius") {
        want.push("genius");
    }
    assert_eq!(names, want);
    let (status, plugins) = call(Method::GET, &n.url("/v1/plugins"), None).await;
    assert_eq!(status, 200);
    assert_eq!(plugins, info["plugins"]);
    assert_eq!(plugins[1]["apiVersion"], 1);
    n.stop().await;
}

#[tokio::test]
async fn plugin_routes_are_mounted_behind_auth() {
    let n = start(&[]).await;
    for path in ["/v1/plugins/example/", "/v1/plugins/example"] {
        let (status, body) = call(Method::GET, &n.url(path), None).await;
        assert_eq!(status, 200, "{path}: {body}");
        assert_eq!(body["plugin"], "example");
    }
    let (status, body) = call(Method::POST, &n.url("/v1/plugins/example/reset"), None).await;
    assert_eq!((status, body["reset"].clone()), (200, json!(true)));
    let (status, _) = call(Method::GET, &n.url("/v1/plugins/nope/"), None).await;
    assert_eq!(status, 404);
    n.stop().await;
}

#[tokio::test]
async fn errors_use_the_lavalink_shape() {
    let n = start(&[]).await;
    let (status, body) = call(Method::GET, &n.url("/v1/sessions/nope/players"), None).await;
    assert_eq!(status, 404);
    assert_eq!(body["status"], 404);
    assert_eq!(body["error"], "Not Found");
    assert_eq!(body["message"], "unknown session");
    assert_eq!(body["path"], "/v1/sessions/nope/players");
    assert!(body["timestamp"].as_i64().unwrap() > 0);
    let (status, body) = call(Method::GET, &n.url("/v1/loadtracks"), None).await;
    assert_eq!(
        (status, body["message"].clone()),
        (400, json!("identifier is required"))
    );
    let (status, body) = call(Method::POST, &n.url("/v1/info"), None).await;
    assert_eq!((status, body["status"].clone()), (405, json!(405)));
    let (status, _) = call(Method::GET, &n.url("/v1/decodetrack?encodedTrack=%%%"), None).await;
    assert_eq!(status, 400);
    n.stop().await;
}

#[tokio::test]
async fn load_and_decode() {
    let n = start(&[]).await;
    let (_, out) = call(
        Method::GET,
        &n.url("/v1/loadtracks?identifier=testtonesearch:440:60000"),
        None,
    )
    .await;
    assert_eq!(out["loadType"], "search", "{out}");
    let enc = out["data"][0]["encoded"].as_str().unwrap().to_string();
    assert_eq!(out["data"][0]["info"]["durationMs"], 60000);
    let (_, dec) = call(
        Method::GET,
        &n.url(&format!("/v1/decodetrack?encodedTrack={enc}")),
        None,
    )
    .await;
    assert_eq!(dec["info"]["source"], "testtone");
    assert_eq!(dec["info"]["identifier"], "440:60000");
    let (status, many) = call(Method::POST, &n.url("/v1/decodetracks"), Some(json!([enc, enc]))).await;
    assert_eq!(status, 200);
    assert_eq!(many.as_array().unwrap().len(), 2);
    let (status, _) = call(Method::POST, &n.url("/v1/decodetracks"), Some(json!([enc, "bad"]))).await;
    assert_eq!(status, 400);
    let (status, q) = call(Method::POST, &n.url("/v1/prefetch"), Some(json!({ "tracks": [enc] }))).await;
    assert_eq!((status, q["queued"].clone()), (202, json!(1)));
    let (status, _) = call(Method::POST, &n.url("/v1/prefetch"), Some(json!({ "tracks": [] }))).await;
    assert_eq!(status, 400);
    let (status, a) = call(
        Method::POST,
        &n.url("/v1/mix/analyze"),
        Some(json!({ "encodedTrack": enc })),
    )
    .await;
    assert_eq!((status, a["queued"].clone()), (202, json!(true)));
    let (status, _) = call(Method::GET, &n.url("/v1/mix/analysis/testtone/440:60000"), None).await;
    assert_eq!(status, 404);
    n.stop().await;
}

#[tokio::test]
async fn player_lifecycle_and_mix_conflicts() {
    let n = start(&[]).await;
    let c = n.client("v1").await;
    let base = n.url(&format!("/v1/sessions/{}/players/555", c.session_id()));
    let enc = tone_encoded(440, 60_000);
    let (status, state) = call(
        Method::PATCH,
        &base,
        Some(json!({ "track": { "encoded": enc }, "volume": 80 })),
    )
    .await;
    assert_eq!(status, 200, "{state}");
    assert_eq!(state["op"], "playerUpdate");
    assert_eq!(state["state"]["player"]["volume"], 80);
    assert_eq!(state["state"]["connected"], false);
    let (status, _) = call(
        Method::POST,
        &format!("{base}/mix/crossfade"),
        Some(json!({ "durationMs": 1000 })),
    )
    .await;
    assert_eq!(status, 409);
    let (status, _) = call(
        Method::POST,
        &format!("{base}/mix/crossfade"),
        Some(json!({ "durationMs": 0 })),
    )
    .await;
    assert!(status == 409 || status == 400, "{status}");
    let (status, _) = call(
        Method::POST,
        &format!("{base}/mix/host"),
        Some(json!({ "audio": "bm90IGFuIG1wMw==" })),
    )
    .await;
    assert_eq!(status, 400, "dj voice with a clip that is not mp3");
    let (status, _) = call(
        Method::POST,
        &format!("{base}/mix/host"),
        Some(json!({ "audio": "%%%" })),
    )
    .await;
    assert_eq!(status, 400, "dj voice with a clip that is not base64");
    let (status, deck) = call(
        Method::POST,
        &format!("{base}/mix/deckb"),
        Some(json!({ "encodedTrack": tone_encoded(660, 30_000), "position": 1000 })),
    )
    .await;
    assert_eq!(status, 200, "{deck}");
    assert!(deck["instance"].as_u64().unwrap() > 0);
    let (status, _) = call(Method::DELETE, &format!("{base}/mix"), None).await;
    assert_eq!(status, 204);
    let (status, _) = call(Method::POST, &format!("{base}/mix/analyze"), None).await;
    assert_eq!(status, 204);
    let (status, list) = call(
        Method::GET,
        &n.url(&format!("/v1/sessions/{}/players", c.session_id())),
        None,
    )
    .await;
    assert_eq!((status, list.as_array().unwrap().len()), (200, 1));
    let (status, _) = call(Method::DELETE, &base, None).await;
    assert_eq!(status, 204);
    let (status, _) = call(Method::GET, &base, None).await;
    assert_eq!(status, 404, "deleted player still present");
    let (status, _) = call(
        Method::POST,
        &format!("{base}/mix/crossfade"),
        Some(json!({ "durationMs": 1 })),
    )
    .await;
    assert_eq!(status, 404);
    n.stop().await;
}

#[cfg(feature = "plugin-apo")]
#[tokio::test]
async fn apo_presets_routes_and_player_filter() {
    let dir = std::sync::Arc::new(TempDir::new());
    let presets = dir.0.join("data/plugins/apo");
    std::fs::create_dir_all(&presets).unwrap();
    std::fs::write(
        presets.join("bass.txt"),
        "Preamp: -4 dB\nFilter 1: ON LSC Fc 105 Hz Gain 4.0 dB Q 0.70\nReverb: Mix 20 %\n",
    )
    .unwrap();
    std::fs::write(presets.join("broken.txt"), "Include: missing.txt").unwrap();
    let n = start_in(dir, &[]).await;

    let (status, list) = call(Method::GET, &n.url("/v1/plugins/apo/"), None).await;
    assert_eq!(status, 200, "{list}");
    assert_eq!(list["presets"][0]["name"], "bass");
    assert_eq!(list["presets"][0]["stages"], json!(["preamp", "filter", "reverb"]));
    assert!(list["presets"][1]["error"].as_str().unwrap().contains("missing.txt"));
    let (status, v) = call(
        Method::POST,
        &n.url("/v1/plugins/apo/check"),
        Some(json!({ "config": "GraphicEQ: 25 6; 16000 3\nStage: pre-mix" })),
    )
    .await;
    assert_eq!(status, 200, "{v}");
    assert_eq!(v["stages"], json!(["convolution"]));
    assert_eq!(v["warnings"].as_array().unwrap().len(), 1);

    let c = n.client("v1").await;
    let base = n.url(&format!("/v1/sessions/{}/players/1", c.session_id()));
    for filter in [
        json!({ "preset": "bass" }),
        json!({ "config": "Channel: L\nFilter: ON PK Fc 1000 Hz Gain -3 dB Q 1.41" }),
    ] {
        let (status, p) = call(
            Method::PATCH,
            &base,
            Some(json!({ "filters": { "pluginFilters": { "apo": filter } } })),
        )
        .await;
        assert_eq!(status, 200, "{p}");
        assert_eq!(p["state"]["player"]["filters"]["pluginFilters"]["apo"], filter);
    }
    for (filter, why) in [
        (json!({ "preset": "broken" }), "missing.txt"),
        (json!({ "preset": "nope" }), "nope"),
        (json!({ "config": "Delay: 9000 ms" }), "delay"),
    ] {
        let (status, e) = call(
            Method::PATCH,
            &base,
            Some(json!({ "filters": { "pluginFilters": { "apo": filter } } })),
        )
        .await;
        assert_eq!(status, 400, "{e}");
        assert!(e.to_string().contains(why), "{e}");
    }
    n.stop().await;
}

#[tokio::test]
async fn filters_including_plugin_filters() {
    let n = start(&[]).await;
    let c = n.client("v1").await;
    let base = n.url(&format!("/v1/sessions/{}/players/1", c.session_id()));
    let body = json!({ "filters": {
        "timescale": { "speed": 1.1, "pitch": 1.0, "rate": 1.0 },
        "reverb": { "mix": 0.25 },
        "pluginFilters": { "bitcrush": { "bits": 6 } }
    }});
    let (status, p) = call(Method::PATCH, &base, Some(body)).await;
    assert_eq!(status, 200, "{p}");
    assert_eq!(p["state"]["player"]["filters"]["pluginFilters"]["bitcrush"]["bits"], 6);
    let (status, e) = call(
        Method::PATCH,
        &base,
        Some(json!({ "filters": { "pluginFilters": { "nope": {} } } })),
    )
    .await;
    assert_eq!(status, 400);
    assert!(e["message"].as_str().unwrap().contains("nope"), "{e}");
    let (status, _) = call(
        Method::PATCH,
        &base,
        Some(json!({ "filters": { "pluginFilters": { "bitcrush": { "bits": 99 } } } })),
    )
    .await;
    assert_eq!(status, 400, "bitcrush refuses its configuration");
    let (status, _) = call(
        Method::PATCH,
        &base,
        Some(json!({ "filters": { "timescale": "fast" } })),
    )
    .await;
    assert_eq!(status, 400, "malformed built-in filter");
    let (_, p) = call(Method::GET, &base, None).await;
    assert_eq!(
        p["state"]["player"]["filters"]["pluginFilters"]["bitcrush"]["bits"], 6,
        "a refused update changes nothing"
    );
    n.stop().await;
}

#[tokio::test]
async fn resume_keeps_players_and_replays_missed_messages() {
    let n = start(&[]).await;
    let c = n.client("v1").await;
    let sid = c.session_id().to_string();
    let r = c
        .update_session(json!({ "resuming": true, "timeout": 60 }))
        .await
        .unwrap();
    assert_eq!(r, json!({ "resuming": true, "timeout": 60 }));
    c.update_player("777", json!({ "volume": 50 }), false).await.unwrap();
    c.close().await;
    drop(c);
    tokio::time::sleep(Duration::from_millis(200)).await;
    // played while away: its events are replayed on resume
    let (status, _) = call(
        Method::PATCH,
        &n.url(&format!("/v1/sessions/{sid}/players/777")),
        Some(json!({ "track": { "encoded": tone_encoded(440, 5_000) } })),
    )
    .await;
    assert_eq!(status, 200);
    tokio::time::sleep(Duration::from_millis(200)).await;
    let c2 = TestClient::builder(&n.base)
        .password(PASSWORD)
        .user_id("123")
        .api("v1")
        .resume(&sid)
        .connect()
        .await
        .unwrap();
    assert!(c2.resumed());
    assert_eq!(c2.session_id(), sid);
    c2.wait_for_guild_event("777", EventType::TrackLoadingEvent, WAIT)
        .await
        .unwrap();
    let p = c2.get_player("777").await.unwrap();
    assert_eq!(p["state"]["player"]["volume"], 50);
    n.stop().await;
}

#[tokio::test]
async fn non_resuming_session_is_destroyed() {
    let n = start(&[]).await;
    let c = n.client("v1").await;
    let sid = c.session_id().to_string();
    c.close().await;
    drop(c);
    wavyzon_testkit::poll_until(Duration::from_secs(2), "session removed", || {
        n.sessions.get(&sid).is_none()
    })
    .await
    .unwrap();
    n.stop().await;
}

#[tokio::test]
async fn resume_with_another_user_creates_a_new_session() {
    let n = start(&[]).await;
    let c = n.client("v1").await;
    let sid = c.session_id().to_string();
    c.update_session(json!({ "resuming": true })).await.unwrap();
    c.close().await;
    drop(c);
    tokio::time::sleep(Duration::from_millis(100)).await;
    let other = TestClient::builder(&n.base)
        .password(PASSWORD)
        .user_id("999")
        .api("v1")
        .resume(&sid)
        .connect()
        .await
        .unwrap();
    assert!(!other.resumed());
    assert_ne!(other.session_id(), sid);
    n.stop().await;
}

// ----- Lavalink v4 -----------------------------------------------------------

/// What lavalink-client checks before an `ftts` search, then the load
/// itself (no network: loading builds the track, and prefetch is off).
#[cfg(feature = "plugin-flowery")]
#[tokio::test]
async fn v4_flowery_tts_is_advertised_and_loads() {
    let n = start(&[("WAVYZON_PREFETCH_ON_LOAD", "false")]).await;
    let (_, info) = call(Method::GET, &n.url("/v4/info"), None).await;
    let managers = info["sourceManagers"].as_array().unwrap();
    assert!(
        managers.contains(&json!("ftts")) && managers.contains(&json!("flowery-tts")),
        "{managers:?}"
    );
    assert!(
        info["plugins"]
            .as_array()
            .unwrap()
            .iter()
            .any(|p| p["name"] == "flowery")
    );

    // lavalink-client: `ftts://` + encodeURIComponent(text) + `?voice=…`
    let id = "ftts://Hello%20world%21?voice=fa3ea565-121f-5efd-b4e9-59895c77df23";
    let url = reqwest::Url::parse_with_params(&n.url("/v4/loadtracks"), [("identifier", id)]).unwrap();
    let (status, body) = call(Method::GET, url.as_str(), None).await;
    assert_eq!(status, 200);
    assert_eq!(body["loadType"], "track", "{body}");
    let info = &body["data"]["info"];
    assert_eq!(info["sourceName"], "flowery-tts");
    assert_eq!(info["title"], "Hello world!");
    assert_eq!(info["author"], "Flowery TTS");
    assert_eq!(info["isStream"], false);
    assert_eq!(info["identifier"], id);
    assert!(
        info["uri"]
            .as_str()
            .unwrap()
            .starts_with("https://api.flowery.pw/v1/tts?text=Hello+world%21&voice=fa3ea565"),
        "{info}"
    );
    let encoded = body["data"]["encoded"].as_str().unwrap();
    let (_, decoded) = call(
        Method::GET,
        &n.url(&format!("/v4/decodetrack?encodedTrack={encoded}")),
        None,
    )
    .await;
    assert_eq!(decoded["info"]["identifier"], id);

    for (bad, kind) in [("ftts://x?speed=fast", "error"), ("ftts://", "empty")] {
        let url = reqwest::Url::parse_with_params(&n.url("/v4/loadtracks"), [("identifier", bad)]).unwrap();
        let (_, body) = call(Method::GET, url.as_str(), None).await;
        assert_eq!(body["loadType"], kind, "{bad}: {body}");
    }
    n.stop().await;

    // disabled the usual way: a WAVYZON_PLUGINS list without it
    let n = start(&[("WAVYZON_PLUGINS", "testkit,example")]).await;
    let (_, info) = call(Method::GET, &n.url("/v4/info"), None).await;
    let managers = info["sourceManagers"].as_array().unwrap();
    assert!(
        !managers.contains(&json!("ftts")) && !managers.contains(&json!("flowery-tts")),
        "{managers:?}"
    );
    assert!(
        !info["plugins"]
            .as_array()
            .unwrap()
            .iter()
            .any(|p| p["name"] == "flowery")
    );
    n.stop().await;
}

#[tokio::test]
async fn v4_info_and_version_match_lavalink() {
    let n = start(&[]).await;
    let (_, info) = call(Method::GET, &n.url("/v4/info"), None).await;
    assert_eq!(info["version"]["semver"], env!("CARGO_PKG_VERSION"));
    assert_eq!(info["wireClock"], true);
    assert_eq!(info["wavyzon"], env!("CARGO_PKG_VERSION"));
    let managers = info["sourceManagers"].as_array().unwrap();
    assert!(managers.contains(&json!("youtube")) && managers.contains(&json!("testtone")));
    assert_eq!(
        info["plugins"][1],
        json!({ "name": "example", "version": env!("CARGO_PKG_VERSION") })
    );
    assert!(info["jvm"].as_str().unwrap().starts_with("rustc"));
    let res = reqwest::Client::new()
        .get(n.url("/version"))
        .header("Authorization", PASSWORD)
        .send()
        .await
        .unwrap();
    assert!(
        res.headers()["content-type"]
            .to_str()
            .unwrap()
            .starts_with("text/plain")
    );
    assert_eq!(res.text().await.unwrap(), env!("CARGO_PKG_VERSION"));
    let (status, stats) = call(Method::GET, &n.url("/v4/stats"), None).await;
    assert_eq!(status, 200);
    assert!(stats["memory"].is_object() && stats["cpu"]["cores"].as_u64().unwrap() >= 1);
    n.stop().await;
}

#[tokio::test]
async fn v4_loadtracks_uses_lavalink_track_shape() {
    let n = start(&[]).await;
    let c = n.client("v4").await;
    let out = c.load_tracks("testtonesearch:440:60000").await.unwrap();
    assert_eq!(out["loadType"], "search");
    let tr = &out["data"][0];
    let info = &tr["info"];
    assert_eq!(info["length"], 60000);
    assert_eq!(info["sourceName"], "testtone");
    assert_eq!(info["isSeekable"], true);
    assert_eq!(info["identifier"], "440:60000");
    assert!(tr["pluginInfo"].is_object());
    let dec = c.decode_track(tr["encoded"].as_str().unwrap()).await.unwrap();
    assert_eq!(dec["info"]["length"], 60000);
    n.stop().await;
}

#[tokio::test]
async fn v4_session_sends_stats_and_lavalink_players() {
    let n = start(&[]).await;
    let c = n.client("v4").await;
    let stats = c.wait_for_op("stats", WAIT).await.unwrap();
    assert!(stats["memory"].is_object() && stats["cpu"].is_object(), "{stats}");
    let failing = Track {
        source: "failing".into(),
        identifier: "x".into(),
        title: "x".into(),
        duration_ms: 1000,
        ..Default::default()
    };
    let p = c
        .update_player(
            "555",
            json!({ "track": { "encoded": failing.encode(), "userData": { "requester": "42" } }, "volume": 80 }),
            false,
        )
        .await
        .unwrap();
    assert_eq!(p["guildId"], "555");
    assert_eq!(p["volume"], 80);
    assert_eq!(p["state"]["connected"], false);
    assert_eq!(p["state"]["ping"], -1);
    assert_eq!(p["track"]["userData"]["requester"], "42");
    let end = c
        .wait_for_guild_event("555", EventType::TrackEndEvent, WAIT)
        .await
        .unwrap();
    assert_eq!(end["reason"], "loadFailed");
    assert_eq!(end["track"]["info"]["sourceName"], "failing");
    assert_eq!(end["track"]["userData"]["requester"], "42");
    assert!(
        !c.events().iter().any(|e| e["type"] == "TrackLoadingEvent"),
        "a Wavyzon-only event was sent to a client that did not opt in"
    );
    let exc = c
        .events()
        .into_iter()
        .find(|e| e["type"] == "TrackExceptionEvent")
        .unwrap();
    assert_eq!(exc["exception"]["causeStackTrace"], "");
    let list = c.players().await.unwrap();
    assert_eq!(list.as_array().unwrap().len(), 1);
    assert_eq!(list[0]["guildId"], "555");
    n.stop().await;
}

#[tokio::test]
async fn v4_extra_events_are_opt_in_under_both_names() {
    let n = start(&[]).await;
    for field in ["wavyzonEvents", "cadenceEvents"] {
        let c = n.client("v4").await;
        let r = c.update_session(json!({ field: true })).await.unwrap();
        assert_eq!(r["resuming"], false);
        c.play("9", &tone_encoded(440, 1_000)).await.unwrap();
        let loading = c
            .wait_for_guild_event("9", EventType::TrackLoadingEvent, WAIT)
            .await
            .unwrap();
        assert!(loading["instance"].as_u64().unwrap() > 0, "{loading}");
        assert_eq!(loading["track"]["info"]["sourceName"], "testtone");
    }
    n.stop().await;
}

#[tokio::test]
async fn v4_does_not_resume_a_v1_session() {
    let n = start(&[]).await;
    let c = n.client("v1").await;
    let sid = c.session_id().to_string();
    c.update_session(json!({ "resuming": true, "timeout": 60 }))
        .await
        .unwrap();
    c.close().await;
    drop(c);
    tokio::time::sleep(Duration::from_millis(100)).await;
    let v4 = TestClient::builder(&n.base)
        .password(PASSWORD)
        .user_id("123")
        .api("v4")
        .resume(&sid)
        .connect()
        .await
        .unwrap();
    assert!(!v4.resumed());
    assert_ne!(v4.session_id(), sid);
    n.stop().await;
}

#[tokio::test]
async fn player_bitrate_is_remembered_and_clamped() {
    let n = start(&[]).await;
    let c = n.client("v4").await;
    c.update_player("42", json!({ "bitrate": 128000 }), false)
        .await
        .unwrap();
    let g = n.sessions.get(c.session_id()).unwrap().guild("42", false).unwrap();
    assert_eq!(g.preferred_bitrate(), 128000);
    c.update_player("42", json!({ "bitrate": 9_000_000 }), false)
        .await
        .unwrap();
    assert_eq!(g.preferred_bitrate(), 384000);
    n.stop().await;
}

#[tokio::test]
async fn v4_no_replace_identifier_and_stop() {
    let n = start(&[]).await;
    let c = n.client("v4").await;
    let first = tone_encoded(440, 60_000);
    c.play("7", &first).await.unwrap();
    let p = c
        .update_player("7", json!({ "track": { "encoded": tone_encoded(660, 60_000) } }), true)
        .await
        .unwrap();
    assert_eq!(p["track"]["encoded"], first, "noReplace kept the current track");
    let p = c
        .update_player(
            "7",
            json!({ "track": { "identifier": "testtonesearch:880:60000" } }),
            false,
        )
        .await
        .unwrap();
    assert_eq!(p["track"]["info"]["identifier"], "880:60000");
    let p = c
        .update_player("7", json!({ "track": { "encoded": null } }), false)
        .await
        .unwrap();
    assert!(p["track"].is_null(), "{p}");
    let end = c
        .wait_for("stopped", WAIT, |m| {
            m["type"] == "TrackEndEvent" && m["reason"] == "stopped"
        })
        .await;
    assert!(end.is_ok());
    let (status, _) = c
        .request(
            Method::PATCH,
            &format!("sessions/{}/players/7", c.session_id()),
            Some(json!({ "voice": { "token": "t" } })),
        )
        .await
        .unwrap();
    assert_eq!(status, 400, "incomplete voice state is refused");
    n.stop().await;
}

#[tokio::test]
async fn metrics_are_exposed() {
    let n = start(&[]).await;
    let c = n.client("v1").await;
    c.play("1", &tone_encoded(440, 1_000)).await.unwrap();
    c.wait_for_guild_event("1", EventType::TrackStartEvent, WAIT)
        .await
        .unwrap();
    let res = reqwest::Client::new()
        .get(n.url("/metrics"))
        .header("Authorization", PASSWORD)
        .send()
        .await
        .unwrap();
    assert!(
        res.headers()["content-type"]
            .to_str()
            .unwrap()
            .starts_with("text/plain")
    );
    let text = res.text().await.unwrap();
    // testkit + example, + apo, flowery and genius when compiled in
    let count = 2
        + usize::from(cfg!(feature = "plugin-apo"))
        + usize::from(cfg!(feature = "plugin-flowery"))
        + usize::from(cfg!(feature = "plugin-genius"));
    let plugins = format!("wavyzon_plugins {count}");
    for needle in [
        "# TYPE wavyzon_players gauge",
        "wavyzon_players 1",
        "wavyzon_sessions 1",
        plugins.as_str(),
        "wavyzon_events_total{type=\"TrackStartEvent\"} 1",
        "wavyzon_prefetch_total{outcome=\"hit\"}",
        "wavyzon_audio_cache_bytes",
        "wavyzon_analysis_queued",
        "wavyzon_resident_bytes",
    ] {
        assert!(text.contains(needle), "missing {needle} in\n{text}");
    }
    n.stop().await;
}

/// A fake lyrics.ovh on a local port: knows "Queen / Under Pressure".
async fn fake_lyrics() -> String {
    use axum::extract::Path;
    use axum::http::StatusCode;
    use axum::response::IntoResponse;
    let app = axum::Router::new().route(
        "/v1/{artist}/{title}",
        axum::routing::get(|Path((artist, title)): Path<(String, String)>| async move {
            if (artist.as_str(), title.as_str()) == ("Queen", "Under Pressure") {
                (StatusCode::OK, r#"{"lyrics":"Pressure\nPushing down on me"}"#).into_response()
            } else {
                (StatusCode::NOT_FOUND, r#"{"error":"No lyrics found"}"#).into_response()
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}/v1", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    base
}

fn song(author: &str, title: &str) -> String {
    Track {
        source: "testtone".into(),
        identifier: "440:3000".into(),
        author: author.into(),
        title: title.into(),
        duration_ms: 3000,
        ..Default::default()
    }
    .encode()
}

#[tokio::test]
async fn lyrics_follow_the_lavalyrics_api() {
    let api = fake_lyrics().await;
    // Genius first: it fails (closed port) and lyrics.ovh still answers.
    let n = start(&[
        ("LYRICS_ENABLED", "true"),
        ("LYRICS_API_URL", &api),
        ("WAVYZON_LYRICS_ORDER", "genius,lyricsovh"),
    ])
    .await;
    for v in ["v1", "v4"] {
        let url = n.url(&format!("/{v}/lyrics?track={}", song("Queen", "Under Pressure")));
        let (status, body) = call(Method::GET, &url, None).await;
        assert_eq!(status, 200, "{v}: {body}");
        assert_eq!(
            body,
            json!({"sourceName": "testtone", "provider": "lyrics.ovh", "text": "Pressure\nPushing down on me", "lines": [], "plugin": {}})
        );
        let url = n.url(&format!("/{v}/lyrics?track={}", song("Nobody", "Nothing")));
        let (status, _) = call(Method::GET, &url, None).await;
        assert_eq!(status, 204, "{v}: no lyrics");
        let (status, _) = call(Method::GET, &n.url(&format!("/{v}/lyrics")), None).await;
        assert_eq!(status, 400, "{v}: missing track");
        let (status, _) = call(
            Method::GET,
            &n.url(&format!("/{v}/sessions/nope/players/1/track/lyrics")),
            None,
        )
        .await;
        assert_eq!(status, 404, "{v}: unknown session");
    }
    let (_, info) = call(Method::GET, &n.url("/v4/info"), None).await;
    assert!(
        info["plugins"]
            .as_array()
            .unwrap()
            .iter()
            .any(|p| p["name"] == "lavalyrics-plugin"),
        "{info}"
    );
    let (_, info) = call(Method::GET, &n.url("/v1/info"), None).await;
    assert_eq!(info["lyrics"], true);
    let (_, metrics) = call(Method::GET, &n.url("/metrics"), None).await;
    let metrics = metrics.as_str().unwrap().to_string();
    assert!(
        metrics.contains("wavyzon_lyrics_total{outcome=\"cached\"} 1"),
        "{metrics}"
    );
    // Genius (on by default) sits on a closed port here: every lookup failed
    // there first, then lyrics.ovh answered.
    if cfg!(feature = "plugin-genius") {
        assert!(
            !metrics.contains("wavyzon_lyrics_plugin_total{outcome=\"failed\"} 0"),
            "{metrics}"
        );
    }
    n.stop().await;
}

/// A fake Genius: public search under `/web/api/search/song`, one song page.
#[cfg(feature = "plugin-genius")]
async fn fake_genius() -> String {
    use axum::extract::Query;
    use std::collections::HashMap;
    let app = axum::Router::new()
        .route(
            "/web/api/search/song",
            axum::routing::get(|Query(q): Query<HashMap<String, String>>| async move {
                let hits = if q["q"].contains("Under Pressure") {
                    json!([{"result": {"title": "Under Pressure", "primary_artist": {"name": "Queen & David Bowie"},
                        "path": "/Queen-and-david-bowie-under-pressure-lyrics", "lyrics_state": "complete"}}])
                } else {
                    json!([])
                };
                axum::Json(json!({"response": {"sections": [{"type": "song", "hits": hits}]}}))
            }),
        )
        .route(
            "/web/Queen-and-david-bowie-under-pressure-lyrics",
            axum::routing::get(|| async {
                r#"<html><div data-lyrics-container="true">[Intro]<br/>Mm ba ba de<br/><i>Pressure</i></div></html>"#
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}/web", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    base
}

/// Genius put first: Genius, then lyrics.ovh when Genius has nothing; with
/// lyrics.ovh off, Genius alone still serves lyrics.
#[cfg(feature = "plugin-genius")]
#[tokio::test]
async fn lyrics_ask_genius_first() {
    let ovh = fake_lyrics().await;
    let genius = fake_genius().await;
    let n = start(&[
        ("LYRICS_ENABLED", "true"),
        ("LYRICS_API_URL", &ovh),
        ("WAVYZON_PLUGIN_GENIUS_WEB_URL", &genius),
        ("WAVYZON_LYRICS_ORDER", "genius,lyricsovh"),
    ])
    .await;
    let url = n.url(&format!("/v4/lyrics?track={}", song("Queen", "Under Pressure")));
    let (status, body) = call(Method::GET, &url, None).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["provider"], "genius.com");
    assert_eq!(body["text"], "[Intro]\nMm ba ba de\nPressure");
    // Genius misses this one; lyrics.ovh has nothing either.
    let url = n.url(&format!("/v4/lyrics?track={}", song("Nobody", "Nothing")));
    let (status, _) = call(Method::GET, &url, None).await;
    assert_eq!(status, 204);
    let (_, metrics) = call(Method::GET, &n.url("/metrics"), None).await;
    let metrics = metrics.as_str().unwrap().to_string();
    for needle in [
        "wavyzon_lyrics_plugin_total{outcome=\"found\"} 1",
        "wavyzon_lyrics_plugin_total{outcome=\"not_found\"} 1",
        "wavyzon_lyrics_total{outcome=\"not_found\"} 1",
    ] {
        assert!(metrics.contains(needle), "missing {needle} in\n{metrics}");
    }
    n.stop().await;

    let n = start(&[("WAVYZON_PLUGIN_GENIUS_WEB_URL", &genius)]).await;
    let url = n.url(&format!("/v1/lyrics?track={}", song("Queen", "Under Pressure")));
    let (status, body) = call(Method::GET, &url, None).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["provider"], "genius.com");
    let url = n.url(&format!("/v1/lyrics?track={}", song("Nobody", "Nothing")));
    assert_eq!(call(Method::GET, &url, None).await.0, 204);
    n.stop().await;
}

/// A fake LRCLIB: synced lyrics for "Queen / Under Pressure" (any length).
async fn fake_lrclib() -> String {
    use axum::extract::Query;
    use axum::response::IntoResponse;
    use std::collections::HashMap;
    let app = axum::Router::new()
        .route(
            "/api/get",
            axum::routing::get(|Query(q): Query<HashMap<String, String>>| async move {
                if q["artist_name"] == "Queen" && q["track_name"] == "Under Pressure" {
                    axum::Json(
                        json!({"trackName": "Under Pressure", "artistName": "Queen", "duration": 3,
                        "instrumental": false, "plainLyrics": "Pressure\nPushing down on me",
                        "syncedLyrics": "[00:00.50]Pressure\n[00:01.75]Pushing down on me"}),
                    )
                    .into_response()
                } else {
                    (axum::http::StatusCode::NOT_FOUND, r#"{"name":"TrackNotFound"}"#).into_response()
                }
            }),
        )
        .route("/api/search", axum::routing::get(|| async { axum::Json(json!([])) }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}/api", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    base
}

/// LRCLIB's synced lyrics fill LavaLyrics' `lines`, and
/// `WAVYZON_LYRICS_ORDER` picks who answers first.
#[tokio::test]
async fn lyrics_order_and_synced_lines() {
    let ovh = fake_lyrics().await;
    let lrclib = fake_lrclib().await;
    let base = [
        ("LYRICS_ENABLED", "true"),
        ("LYRICS_API_URL", ovh.as_str()),
        ("LRCLIB_ENABLED", "true"),
        ("LRCLIB_API_URL", lrclib.as_str()),
    ];
    let n = start(&base).await;
    let url = n.url(&format!("/v4/lyrics?track={}", song("Queen", "Under Pressure")));
    let (status, body) = call(Method::GET, &url, None).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["provider"], "lrclib.net");
    assert_eq!(body["text"], "Pressure\nPushing down on me");
    assert_eq!(
        body["lines"],
        json!([
            {"timestamp": 500, "duration": 1250, "line": "Pressure", "plugin": {}},
            {"timestamp": 1750, "duration": 1250, "line": "Pushing down on me", "plugin": {}},
        ])
    );
    n.stop().await;

    let mut ordered = base.to_vec();
    ordered.push(("WAVYZON_LYRICS_ORDER", "lyricsovh,lrclib"));
    let n = start(&ordered).await;
    let url = n.url(&format!("/v4/lyrics?track={}", song("Queen", "Under Pressure")));
    let (status, body) = call(Method::GET, &url, None).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["provider"], "lyrics.ovh");
    assert_eq!(body["lines"], json!([]));
    n.stop().await;
}

#[tokio::test]
async fn lyrics_can_be_disabled() {
    // lyrics.ovh is off in tests; Genius is turned off too.
    let n = start(&[("WAVYZON_PLUGIN_GENIUS_ENABLED", "false")]).await;
    let url = n.url(&format!("/v4/lyrics?track={}", song("Queen", "Under Pressure")));
    let (status, body) = call(Method::GET, &url, None).await;
    assert_eq!(status, 404, "{body}");
    let (_, info) = call(Method::GET, &n.url("/v4/info"), None).await;
    assert!(
        !info["plugins"]
            .as_array()
            .unwrap()
            .iter()
            .any(|p| p["name"] == "lavalyrics-plugin"),
        "{info}"
    );
    n.stop().await;
}
