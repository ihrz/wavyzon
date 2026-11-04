//! The logged-in Deezer gateway: ARL sessions, track tokens and media URLs,
//! port of Cadence's `internal/source/deezer/media.go`.
//!
//! Flow (as in Cadence): `deezer.getUserData` exchanges the `arl` cookie for
//! an API token and a license token, `song.getListData` turns a track id into
//! a stream token, and `media/v1/get_url` exchanges those for a CDN URL. All
//! of it rotates over the [account pool](super::accounts) on refusals.

use std::future::Future;
use std::sync::Arc;
use std::time::{Duration, Instant};

use reqwest::Method;
use reqwest::header::{COOKIE, SET_COOKIE};
use serde::Deserialize;
use serde_json::{Value, json};
use wavyzon_plugin::{Failure, Severity};

use super::accounts::Account;
use super::{Deezer, LANG, NAME};
use crate::httpx::{MAX_BODY, Req, read_limited, safe_url};
use crate::metrics::inc;
use crate::util::bad_json;

/// The `VALID_TOKEN_REQUIRED` gw error that means "session expired".
pub const GW_INVALID_TOKEN: &str = "VALID_TOKEN_REQUIRED";
/// The [`Failure::message`] of a rejected ARL (matched by
/// [`is_rejected`]).
pub const REJECTED_MESSAGE: &str = "arl rejected";
/// Why no ARL means no full-length playback.
pub const NO_ARL: &str = "DEEZER_ARL is not set, so full-length streams are unavailable";
/// Why a rejected ARL is a `fault` (Cadence's `errARLRejected`).
const ARL_REJECTED: &str = "the ARL is expired or the account was logged out; replace it in DEEZER_ARL";
/// No [`super::stripe::StripeDecryptor`] was configured.
pub const NO_DECRYPTOR: &str = "no deezer decryptor is registered";

const SESSION_TTL: Duration = Duration::from_secs(3600);
/// How long a media URL is cached (also the prefetch head TTL in Cadence).
pub const MEDIA_URL_TTL: Duration = Duration::from_secs(10 * 60);
/// The only cipher the CDN offers us.
pub(crate) const STREAM_CIPHER: &str = "BF_CBC_STRIPE";
/// 128 kbit/s MP3 (with `DEEZER_HIGH_QUALITY`).
pub const FORMAT_MP3_HIGH: &str = "MP3_128";
/// 64 kbit/s MP3 (always tried last, or alone).
pub const FORMAT_MP3_LOW: &str = "MP3_64";

/// A logged-in gateway session (Cadence's `session`). Debug output never
/// shows the tokens.
pub struct Session {
    pub api_token: String,
    pub license_token: String,
    pub sid: String,
    expires: Instant,
}

impl std::fmt::Debug for Session {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Session")
            .field("sid", &self.sid)
            .finish_non_exhaustive()
    }
}

/// A CDN stream URL and the format it serves (Cadence's `mediaLink`).
#[derive(Debug, Clone)]
pub struct MediaLink {
    pub url: String,
    pub format: String,
}

/// A `(cipher, format)` pair the media endpoint may serve (Cadence's
/// `mediaFormat`).
#[derive(Debug, Clone)]
pub struct MediaFormat {
    pub cipher: String,
    pub format: String,
}

#[derive(Debug, Deserialize)]
struct GwResponse {
    #[serde(default)]
    error: Value,
    #[serde(default)]
    results: Value,
}

#[derive(Debug, Default, Deserialize)]
struct UserData {
    #[serde(default, rename = "checkForm")]
    check_form: String,
    #[serde(default, rename = "USER")]
    user: UserNode,
}

#[derive(Debug, Default, Deserialize)]
struct UserNode {
    #[serde(default, rename = "USER_ID")]
    id: i64,
    #[serde(default, rename = "OPTIONS")]
    options: OptionsNode,
}

#[derive(Debug, Default, Deserialize)]
struct OptionsNode {
    #[serde(default, rename = "license_token")]
    license_token: String,
}

#[derive(Debug, Default, Deserialize)]
struct SongData {
    #[serde(default)]
    data: Vec<SongEntry>,
}

#[derive(Debug, Default, Deserialize)]
struct SongEntry {
    #[serde(default, rename = "TRACK_TOKEN")]
    track_token: String,
}

#[derive(Debug, Default, Deserialize)]
struct MediaResponse {
    #[serde(default)]
    data: Vec<MediaEntry>,
}

#[derive(Debug, Default, Deserialize)]
struct MediaEntry {
    #[serde(default)]
    media: Vec<MediaKind>,
    #[serde(default)]
    errors: Vec<MediaError>,
}

#[derive(Debug, Default, Deserialize)]
struct MediaKind {
    #[serde(default)]
    format: String,
    #[serde(default)]
    sources: Vec<MediaSource>,
}

#[derive(Debug, Default, Deserialize)]
struct MediaSource {
    #[serde(default)]
    url: String,
}

#[derive(Debug, Default, Deserialize)]
struct MediaError {
    #[serde(default)]
    code: i64,
    #[serde(default)]
    message: String,
}

/// Whether `err` is a rejected ARL (Cadence's `isRejected`).
pub fn is_rejected(err: &Failure) -> bool {
    err.message == REJECTED_MESSAGE
}

/// Whether another account could plausibly fix `err` (Cadence's
/// `isSuspicious`).
pub fn is_suspicious(err: &Failure) -> bool {
    err.severity == Severity::Suspicious
}

/// The gw `error` field counts as failed unless it is empty (`""`, `[]`,
/// `{}`, `null` — as in Cadence).
fn gw_failed(raw: &Value) -> Option<String> {
    let s = match raw {
        Value::Null => String::new(),
        other => other.to_string(),
    };
    let t = s.trim();
    if t.is_empty() || t == "[]" || t == "{}" || t == "null" {
        None
    } else {
        Some(t.to_string())
    }
}

/// The `sid` cookie of a `getUserData` answer.
fn sid_cookie(res: &reqwest::Response) -> Option<String> {
    for v in res.headers().get_all(SET_COOKIE) {
        let Ok(v) = v.to_str() else { continue };
        if let Some(rest) = v.strip_prefix("sid=") {
            return Some(rest.split(';').next().unwrap_or_default().to_string());
        }
    }
    None
}

impl Deezer {
    /// One `gw-light.php` call. Returns the `results` JSON and, for
    /// `getUserData`, the `sid` cookie (Cadence's `gw`).
    pub(crate) async fn gw(
        &self,
        account: &Account,
        session: Option<&Session>,
        method: &str,
        body: &Value,
    ) -> Result<(Value, Option<String>), Failure> {
        let api_token = session.map(|s| s.api_token.as_str()).unwrap_or_default();
        let url = format!(
            "{}?{}",
            self.endpoints.gw,
            crate::util::query(&[
                ("method", method),
                ("input", "3"),
                ("api_version", "1.0"),
                ("api_token", api_token),
            ])
        );
        let mut cookie = format!("arl={}", account.arl());
        if let Some(s) = session.filter(|s| !s.sid.is_empty()) {
            cookie.push_str(&format!("; sid={}", s.sid));
        }
        let req = Req::new(Method::POST, &url)
            .map_err(|e| e.to_failure(NAME))?
            .header("Content-Type", "application/json")
            .header(LANG.0, LANG.1)
            .header(COOKIE.as_str(), &cookie)
            .body(body.to_string());
        let res = self
            .http
            .execute(&req, Some(self.http.options().request_timeout))
            .await
            .map_err(|e| e.to_failure(NAME))?;
        let sid = session.is_none().then(|| sid_cookie(&res)).flatten();
        let body = read_limited(res, MAX_BODY, &safe_url(&req.url))
            .await
            .map_err(|e| e.to_failure(NAME))?;
        let gr: GwResponse = serde_json::from_slice(&body).map_err(|e| bad_json(NAME, e))?;
        if let Some(msg) = gw_failed(&gr.error) {
            if session.is_some() && msg.contains(GW_INVALID_TOKEN) {
                account.invalidate().await;
            }
            return Err(Failure::suspicious(NAME, format!("{method} refused"), Some(msg)));
        }
        Ok((gr.results, sid))
    }

    /// The cached gw session of `account`, logging in with the ARL when it
    /// expired (Cadence's `session`).
    pub(crate) async fn session(&self, account: &Account) -> Result<Arc<Session>, Failure> {
        let mut slot = account.session().await;
        if let Some(s) = slot.as_ref() {
            if Instant::now() < s.expires {
                return Ok(s.clone());
            }
        }
        let (results, sid) = self.gw(account, None, "deezer.getUserData", &json!({})).await?;
        let u: UserData = serde_json::from_value(results).map_err(|e| bad_json(NAME, e))?;
        if u.user.id == 0 || u.user.options.license_token.is_empty() {
            return Err(Failure::fault(NAME, REJECTED_MESSAGE, Some(ARL_REJECTED.to_string())));
        }
        let s = Arc::new(Session {
            api_token: u.check_form,
            license_token: u.user.options.license_token,
            sid: sid.unwrap_or_default(),
            expires: Instant::now() + SESSION_TTL,
        });
        *slot = Some(s.clone());
        Ok(s)
    }

    /// The CDN URL of `track_id`, cached for [`MEDIA_URL_TTL`] (Cadence's
    /// `mediaURL`).
    pub(crate) async fn media_url(&self, track_id: &str) -> Result<MediaLink, Failure> {
        if let Some(u) = self.urls.get(&track_id.to_string()) {
            return Ok(u);
        }
        if self.accounts.is_empty() {
            return Err(Failure::fault(NAME, "stream unavailable", Some(NO_ARL.to_string())));
        }
        let u = self
            .first_account(|a| async move { self.media_url_from(&a, track_id).await })
            .await?;
        self.urls.set(track_id.to_string(), u.clone());
        Ok(u)
    }

    /// [`Deezer::fetch_media_url`], retrying once on a fresh session when the
    /// account was refused (Cadence's `mediaURLFrom`).
    async fn media_url_from(&self, account: &Arc<Account>, track_id: &str) -> Result<MediaLink, Failure> {
        match self.fetch_media_url(account, track_id).await {
            Ok(u) => Ok(u),
            Err(e) if is_suspicious(&e) => {
                account.invalidate().await;
                self.fetch_media_url(account, track_id).await
            }
            Err(e) => Err(e),
        }
    }

    /// `song.getListData` + `media/v1/get_url` for one account (Cadence's
    /// `fetchMediaURL`).
    async fn fetch_media_url(&self, account: &Arc<Account>, track_id: &str) -> Result<MediaLink, Failure> {
        let s = self.session(account).await?;
        let (results, _) = self
            .gw(account, Some(&s), "song.getListData", &json!({ "sng_ids": [track_id] }))
            .await?;
        let songs: SongData = serde_json::from_value(results).map_err(|e| bad_json(NAME, e))?;
        let Some(token) = songs
            .data
            .first()
            .map(|d| d.track_token.as_str())
            .filter(|t| !t.is_empty())
        else {
            return Err(Failure::common(NAME, "track has no stream token", None));
        };
        let formats: Vec<Value> = self
            .formats
            .iter()
            .map(|MediaFormat { cipher, format }| json!({ "cipher": cipher, "format": format }))
            .collect();
        let payload = json!({
            "license_token": s.license_token,
            "media": [{ "type": "FULL", "formats": formats }],
            "track_tokens": [token],
        });
        let req = Req::new(Method::POST, &self.endpoints.media)
            .map_err(|e| e.to_failure(NAME))?
            .header("Content-Type", "application/json")
            .header(LANG.0, LANG.1)
            .body(payload.to_string());
        let body = self.http.fetch(&req).await.map_err(|e| e.to_failure(NAME))?;
        let mr: MediaResponse = serde_json::from_slice(&body).map_err(|e| bad_json(NAME, e))?;
        let Some(d) = mr.data.first() else {
            return Err(Failure::common(NAME, "no media returned", None));
        };
        if let Some(e) = d.errors.first() {
            return Err(Failure::suspicious(
                NAME,
                "media refused",
                Some(format!("{}: {}", e.code, e.message)),
            ));
        }
        for m in &d.media {
            for src in &m.sources {
                if !src.url.is_empty() {
                    return Ok(MediaLink {
                        url: src.url.clone(),
                        format: m.format.clone(),
                    });
                }
            }
        }
        Err(Failure::common(NAME, "no stream url returned", None))
    }

    /// Ask the accounts in order until one answers (Cadence's
    /// `firstAccount`): a rejected ARL is benched, a suspicious refusal moves
    /// on without benching, anything else stops the walk.
    pub(crate) async fn first_account<T, F, Fut>(&self, fetch: F) -> Result<T, Failure>
    where
        F: Fn(Arc<Account>) -> Fut,
        Fut: Future<Output = Result<T, Failure>>,
    {
        let mut last: Option<Failure> = None;
        for (i, a) in self.accounts.order().into_iter().enumerate() {
            match fetch(a.clone()).await {
                Ok(v) => {
                    if i > 0 {
                        inc(&self.metrics.deezer_accounts.switched);
                    }
                    return Ok(v);
                }
                Err(e) => {
                    if is_rejected(&e) {
                        inc(&self.metrics.deezer_accounts.rejected);
                        if self.accounts.bench(&a) {
                            tracing::error!(
                                account = a.number(),
                                accounts = self.accounts.len(),
                                for = ?super::accounts::ACCOUNT_BENCH,
                                "deezer account rejected, benched"
                            );
                        }
                        last = Some(e);
                        continue;
                    }
                    if !is_suspicious(&e) {
                        return Err(e);
                    }
                    last = Some(e);
                }
            }
        }
        Err(last.unwrap_or_else(|| Failure::common(NAME, "no deezer account available", None)))
    }
}

/// Exposed for tests: build the suspicious refusal the fake gw returns.
#[cfg(test)]
pub(crate) fn media_refused(code: i64, message: &str) -> Failure {
    Failure::suspicious(NAME, "media refused", Some(format!("{code}: {message}")))
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU32, Ordering};

    use super::*;
    use crate::deezer::accounts::Rotation;
    use crate::deezer::tests_support::*;

    #[test]
    fn gw_failed_matches_cadence() {
        assert_eq!(gw_failed(&Value::Null), None);
        assert_eq!(gw_failed(&json!([])), None);
        assert_eq!(gw_failed(&json!({})), None);
        assert!(
            gw_failed(&json!({"error": "VALID_TOKEN_REQUIRED"}))
                .unwrap()
                .contains("VALID_TOKEN")
        );
    }

    #[tokio::test]
    async fn login_exchanges_the_arl_for_tokens_and_a_sid() {
        let fake = fake_deezer().await;
        let dz = fake.deezer();
        let a = dz.accounts.order().remove(0);
        let s = dz.session(&a).await.unwrap();
        assert_eq!(s.api_token, "check-form");
        assert_eq!(s.license_token, "license-1");
        assert_eq!(s.sid, "sid-1");
        let calls = fake.gw_calls();
        assert!(calls[0].cookie.contains("arl=arl-one"), "cookie: {}", calls[0].cookie);
        assert!(!calls[0].cookie.contains("sid="), "no sid on the first call");
        // The second lookup reuses the session, with the sid attached.
        dz.fetch_media_url(&a, "3135553").await.unwrap();
        let calls = fake.gw_calls();
        assert_eq!(
            calls.iter().filter(|c| c.method == "deezer.getUserData").count(),
            1,
            "the session is cached"
        );
        let list = calls.iter().find(|c| c.method == "song.getListData").unwrap();
        assert!(list.cookie.contains("sid=sid-1"), "cookie: {}", list.cookie);
    }

    #[tokio::test]
    async fn a_rejected_arl_becomes_a_rejection() {
        let fake = fake_deezer().await;
        fake.reject_accounts(true);
        let dz = fake.deezer();
        let a = dz.accounts.order().remove(0);
        let err = dz.session(&a).await.unwrap_err();
        assert!(is_rejected(&err), "{err}");
        assert_eq!(err.severity, Severity::Fault);
    }

    #[tokio::test]
    async fn a_refused_token_relogs_the_account_once() {
        let fake = fake_deezer().await;
        fake.expire_once();
        let dz = fake.deezer();
        let a = dz.accounts.order().remove(0);
        let u = dz.media_url_from(&a, "3135553").await.unwrap();
        assert!(u.url.contains("/cdn/"), "{}", u.url);
        assert_eq!(
            fake.gw_calls()
                .iter()
                .filter(|c| c.method == "deezer.getUserData")
                .count(),
            2,
            "the refused session is replaced by a fresh login"
        );
    }

    #[tokio::test]
    async fn media_urls_are_cached() {
        let fake = fake_deezer().await;
        let dz = fake.deezer();
        let u = dz.media_url("3135553").await.unwrap();
        assert_eq!(u.format, "MP3_128");
        assert!(u.url.contains("/cdn/"), "{}", u.url);
        assert!(dz.media_url("3135553").await.is_ok());
        assert_eq!(fake.media_calls(), 1, "the second lookup is served from the cache");
    }

    #[tokio::test]
    async fn a_track_without_a_stream_token_is_common() {
        let fake = fake_deezer().await;
        fake.drop_track_token(true);
        let dz = fake.deezer();
        let err = dz.media_url("1").await.unwrap_err();
        assert_eq!(err.severity, Severity::Common);
        assert!(err.message.contains("no stream token"), "{err}");
    }

    #[tokio::test]
    async fn media_refusals_are_suspicious() {
        let fake = fake_deezer().await;
        fake.refuse_media(true);
        let dz = fake.deezer();
        let err = dz.media_url("3135553").await.unwrap_err();
        assert_eq!(err.severity, Severity::Suspicious);
        assert_eq!(err.message, "media refused");
    }

    #[tokio::test]
    async fn first_account_skips_a_rejected_account_and_benches_it() {
        let dz = bare_deezer(&["dead", "live"], Rotation::Failover);
        let tried = Arc::new(std::sync::Mutex::new(Vec::new()));
        let u = dz
            .first_account(|a| {
                tried.lock().unwrap().push(a.number());
                async move {
                    if a.arl() == "dead" {
                        Err(Failure::fault(NAME, REJECTED_MESSAGE, None))
                    } else {
                        Ok(MediaLink {
                            url: "ok".into(),
                            format: "MP3_128".into(),
                        })
                    }
                }
            })
            .await
            .unwrap();
        assert_eq!(u.url, "ok");
        assert_eq!(*tried.lock().unwrap(), vec![1, 2]);
        let order: Vec<usize> = dz.accounts.order().iter().map(|a| a.number()).collect();
        assert_eq!(order, vec![2], "the dead account is benched");
    }

    #[tokio::test]
    async fn first_account_tries_another_account_when_a_track_is_refused() {
        let dz = bare_deezer(&["free", "premium"], Rotation::Failover);
        let u = dz
            .first_account(|a| async move {
                if a.arl() == "free" {
                    Err(media_refused(2002, "FALLBACK"))
                } else {
                    Ok(MediaLink {
                        url: "ok".into(),
                        format: "MP3_128".into(),
                    })
                }
            })
            .await
            .unwrap();
        assert_eq!(u.url, "ok");
        let order: Vec<usize> = dz.accounts.order().iter().map(|a| a.number()).collect();
        assert_eq!(order, vec![1, 2], "a refused track must not bench the account");
    }

    #[tokio::test]
    async fn first_account_stops_on_an_error_another_account_cannot_fix() {
        let dz = bare_deezer(&["a", "b"], Rotation::Failover);
        let calls = Arc::new(AtomicU32::new(0));
        let err = dz
            .first_account(|_| {
                calls.fetch_add(1, Ordering::SeqCst);
                async { Err::<(), _>(Failure::common(NAME, "track has no stream token", None)) }
            })
            .await
            .unwrap_err();
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(err.message, "track has no stream token");
    }

    #[tokio::test]
    async fn first_account_returns_the_rejection_when_every_account_fails() {
        let dz = bare_deezer(&["a", "b"], Rotation::Failover);
        let err = dz
            .first_account(|_| async { Err::<(), _>(Failure::fault(NAME, REJECTED_MESSAGE, None)) })
            .await
            .unwrap_err();
        assert!(is_rejected(&err));
    }

    #[tokio::test]
    async fn the_last_working_account_is_never_benched() {
        let single = bare_deezer(&["only"], Rotation::Failover);
        for _ in 0..2 {
            let err = single
                .first_account(|_| async { Err::<(), _>(Failure::fault(NAME, REJECTED_MESSAGE, None)) })
                .await
                .unwrap_err();
            assert!(is_rejected(&err));
        }
        let order: Vec<usize> = single.accounts.order().iter().map(|a| a.number()).collect();
        assert_eq!(order, vec![1], "a lone account stays in rotation");

        let pair = bare_deezer(&["a", "b"], Rotation::Failover);
        let _ = pair
            .first_account(|_| async { Err::<(), _>(Failure::fault(NAME, REJECTED_MESSAGE, None)) })
            .await;
        let order: Vec<usize> = pair.accounts.order().iter().map(|a| a.number()).collect();
        assert_eq!(order, vec![2], "the first benched, the last kept");
    }
}
