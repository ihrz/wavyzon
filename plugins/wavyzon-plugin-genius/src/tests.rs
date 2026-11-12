//! Against a fake Genius on a local port: API search under `/api`, the site
//! (web search and song pages) under `/web`.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use wavyzon_plugin::axum::Router;
use wavyzon_plugin::axum::extract::{Query, State};
use wavyzon_plugin::axum::http::{HeaderMap, StatusCode};
use wavyzon_plugin::axum::response::IntoResponse;
use wavyzon_plugin::axum::routing::{get, post};
use wavyzon_plugin::serde_json::json;

use super::*;

#[derive(Default)]
struct Calls {
    token: AtomicU32,
    api: AtomicU32,
    web: AtomicU32,
    page: AtomicU32,
}

fn n(c: &AtomicU32) -> u32 {
    c.load(Ordering::SeqCst)
}

const PAGE: &str = r#"<html><body><div data-lyrics-container="true" class="Lyrics__Container"><div data-exclude-from-selection="true"><span>Under Pressure Lyrics</span></div>[Intro]<br/>Pressure<br/><a href="/1"><span>Pushing down on me</span></a></div></body></html>"#;

fn hits(q: &str) -> Vec<wavyzon_plugin::serde_json::Value> {
    let song = |title: &str, artist: &str, path: &str| json!({"title": title, "primary_artist": {"name": artist}, "artist_names": artist, "path": path, "lyrics_state": "complete"});
    if q.contains("Under Pressure") {
        vec![song(
            "Under Pressure",
            "Queen & David Bowie",
            "/Queen-under-pressure-lyrics",
        )]
    } else if q.contains("Blocked") {
        vec![song("Blocked", "Nobody", "/blocked-lyrics")]
    } else {
        vec![]
    }
}

async fn fake() -> (String, Arc<Calls>) {
    let calls = Arc::new(Calls::default());
    let token = |State(c): State<Arc<Calls>>, body: String| async move {
        c.token.fetch_add(1, Ordering::SeqCst);
        if body.contains("client_id=good") && body.contains("grant_type=client_credentials") {
            (StatusCode::OK, r#"{"access_token":"exchanged","token_type":"bearer"}"#).into_response()
        } else {
            (StatusCode::UNAUTHORIZED, r#"{"error":"invalid_client"}"#).into_response()
        }
    };
    let api = |State(c): State<Arc<Calls>>, h: HeaderMap, Query(q): Query<HashMap<String, String>>| async move {
        c.api.fetch_add(1, Ordering::SeqCst);
        let auth = h.get("authorization").and_then(|v| v.to_str().ok()).unwrap_or("");
        if auth != "Bearer tok" && auth != "Bearer exchanged" {
            return (StatusCode::UNAUTHORIZED, r#"{"meta":{"status":401}}"#).into_response();
        }
        let hits: Vec<_> = hits(&q["q"])
            .into_iter()
            .map(|r| json!({"type": "song", "result": r}))
            .collect();
        wavyzon_plugin::axum::Json(json!({"meta": {"status": 200}, "response": {"hits": hits}})).into_response()
    };
    let web = |State(c): State<Arc<Calls>>, Query(q): Query<HashMap<String, String>>| async move {
        c.web.fetch_add(1, Ordering::SeqCst);
        let hits: Vec<_> = hits(&q["q"]).into_iter().map(|r| json!({"result": r})).collect();
        wavyzon_plugin::axum::Json(json!({"response": {"sections": [{"type": "song", "hits": hits}]}}))
    };
    let page = |State(c): State<Arc<Calls>>| async move {
        c.page.fetch_add(1, Ordering::SeqCst);
        (StatusCode::OK, [("content-type", "text/html")], PAGE).into_response()
    };
    let blocked = |State(c): State<Arc<Calls>>| async move {
        c.page.fetch_add(1, Ordering::SeqCst);
        (StatusCode::OK, "<html>Just a moment...</html>").into_response()
    };
    let app: Router = Router::new()
        .route("/api/oauth/token", post(token))
        .route("/api/search", get(api))
        .route("/web/api/search/song", get(web))
        .route("/web/Queen-under-pressure-lyrics", get(page))
        .route("/web/blocked-lyrics", get(blocked))
        .with_state(calls.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { wavyzon_plugin::axum::serve(listener, app).await.unwrap() });
    (base, calls)
}

fn settings(base: &str, keys: &[(&str, &str)]) -> Settings {
    let mut env: HashMap<String, String> = [
        ("api_url".to_string(), format!("{base}/api")),
        ("web_url".to_string(), format!("{base}/web")),
    ]
    .into();
    for (k, v) in keys {
        env.insert(k.to_string(), v.to_string());
    }
    Settings::from_lookup(|k| env.get(k).cloned()).unwrap()
}

fn track(author: &str, title: &str) -> Track {
    Track {
        source: "deezer".into(),
        identifier: "1".into(),
        author: author.into(),
        title: title.into(),
        ..Default::default()
    }
}

const WANT: &str = "[Intro]\nPressure\nPushing down on me";

#[tokio::test]
async fn access_token_searches_the_api_and_caches() {
    let (base, calls) = fake().await;
    let g = GeniusClient::new(settings(&base, &[("access_token", "tok")])).unwrap();
    let got = g
        .lyrics(&track("Queen, David Bowie", "Under Pressure - Remastered 2011"))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(&*got.text, WANT);
    assert_eq!(got.provider, PROVIDER);
    assert_eq!((n(&calls.api), n(&calls.web), n(&calls.page)), (1, 0, 1));
    // Same song, other spelling: served from the cache.
    assert!(g.lyrics(&track("Queen", "under pressure")).await.unwrap().is_some());
    assert_eq!((n(&calls.api), n(&calls.page)), (1, 1));
}

#[tokio::test]
async fn no_key_uses_the_public_search() {
    let (base, calls) = fake().await;
    let g = GeniusClient::new(settings(&base, &[])).unwrap();
    let got = g.lyrics(&track("Queen", "Under Pressure")).await.unwrap().unwrap();
    assert_eq!(&*got.text, WANT);
    assert_eq!((n(&calls.api), n(&calls.web), n(&calls.token)), (0, 1, 0));
}

#[tokio::test]
async fn client_credentials_are_exchanged_once() {
    let (base, calls) = fake().await;
    let g = GeniusClient::new(settings(&base, &[("client_id", "good"), ("client_secret", "s")])).unwrap();
    assert!(g.lyrics(&track("Queen", "Under Pressure")).await.unwrap().is_some());
    assert_eq!(g.lyrics(&track("Nobody", "Nothing")).await.unwrap(), None);
    assert_eq!((n(&calls.token), n(&calls.api), n(&calls.web)), (1, 2, 0));
}

#[tokio::test]
async fn refused_credentials_and_bad_tokens_fall_back_to_the_public_search() {
    let (base, calls) = fake().await;
    let g = GeniusClient::new(settings(&base, &[("client_id", "bad"), ("client_secret", "s")])).unwrap();
    assert!(g.lyrics(&track("Queen", "Under Pressure")).await.unwrap().is_some());
    assert_eq!(g.lyrics(&track("Nobody", "Nothing")).await.unwrap(), None);
    // The refused exchange is not retried right away.
    assert_eq!((n(&calls.token), n(&calls.api), n(&calls.web)), (1, 0, 2));

    let (base, calls) = fake().await;
    let g = GeniusClient::new(settings(&base, &[("access_token", "wrong")])).unwrap();
    assert!(g.lyrics(&track("Queen", "Under Pressure")).await.unwrap().is_some());
    assert_eq!((n(&calls.api), n(&calls.web)), (1, 1));
}

#[tokio::test]
async fn a_miss_is_cached_and_a_broken_page_is_an_uncached_error() {
    let (base, calls) = fake().await;
    let g = GeniusClient::new(settings(&base, &[])).unwrap();
    assert_eq!(g.lyrics(&track("Nobody", "Nothing")).await.unwrap(), None);
    assert_eq!(g.lyrics(&track("Nobody", "Nothing")).await.unwrap(), None);
    assert_eq!(n(&calls.web), 1, "the miss was searched twice");

    assert!(g.lyrics(&track("Nobody", "Blocked")).await.is_err());
    assert!(g.lyrics(&track("Nobody", "Blocked")).await.is_err());
    assert_eq!(n(&calls.page), 2, "a failure was cached");
}

#[tokio::test]
async fn an_unreachable_genius_is_an_error() {
    // Port 9 (discard) is closed on test machines: connection refused.
    let g = GeniusClient::new(settings("http://127.0.0.1:9", &[("timeout_ms", "2000")])).unwrap();
    assert!(g.lyrics(&track("Queen", "Under Pressure")).await.is_err());
}

#[test]
fn settings_from_the_environment() {
    let s = Settings::from_lookup(|_| None).unwrap();
    assert!(s.enabled);
    assert_eq!(s.auth_mode(), "public search (no key)");
    assert_eq!(s.api_url.as_str(), "https://api.genius.com/");
    let off = Settings::from_lookup(|k| (k == "enabled").then(|| "off".to_string())).unwrap();
    assert!(!off.enabled);
    let only_id = Settings::from_lookup(|k| (k == "client_id").then(|| "x".to_string()));
    assert!(matches!(only_id, Err(PluginError::Config(m)) if m.contains("CLIENT_SECRET")));
    let bad = Settings::from_lookup(|k| (k == "web_url").then(|| "ftp://x".to_string()));
    assert!(bad.is_err());
    let s = Settings::from_lookup(|k| (k == "access_token").then(|| "secret-value".to_string())).unwrap();
    assert_eq!(s.auth_mode(), "access token");
    assert!(!format!("{s:?}").contains("secret-value"), "Debug leaks the token");
}
