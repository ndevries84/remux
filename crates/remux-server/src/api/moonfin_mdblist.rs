//! Moonfin/Moonbase-compatible MDBList ratings proxy
//! (`GET /moonfin/mdblist/ratings`).
//!
//! Wire behaviour follows Moonbase's `MdbListController.GetRatings` as called
//! by the Moonfin client (`mdblist_repository.dart`): the client resolves the
//! TMDb id itself and sends `type` (`movie` | `show`), `tmdbId` and its device
//! `profile`. The reply is always `200 {success, error, ratings[]}` for
//! anything the client should treat as "no ratings" (missing key, upstream
//! failure); only malformed requests get a `400`. A `404` would make the
//! client believe the route is absent and stop asking for five minutes.
//!
//! The API key stays server-side: the user's own `mdblistApiKey` (from their
//! synced settings) wins, then the server-wide `MDBLIST_API_KEY`. Upstream
//! ratings are cached unfiltered in SQLite (`mdblist_ratings_cache`, so a
//! restart doesn't re-spend MDBList's 1,000 requests/day free quota) and
//! filtered per request by the user's selected rating sources.
//!
//! MDBList API: <https://api.mdblist.com/> (title lookup
//! `GET /tmdb/{movie|show}/{id}?apikey=...`; a `429` carries `Retry-After`).

use std::{
    hash::{Hash, Hasher},
    sync::Arc,
    time::{Duration, Instant},
};

use axum::{
    Json,
    body::Body,
    extract::{Query, State},
    http::Request,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use dashmap::DashMap;
use remux_macros::{get, post};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::SqlitePool;
use strum_macros::{Display, EnumString};
use tracing::warn;

use crate::{
    AppState, Config,
    api::moonfin_settings::resolved_profile,
    db::{self, auth::AuthSession},
};

/// Rating sources returned when the user has not chosen any.
const DEFAULT_SOURCES: [&str; 4] = ["imdb", "tmdb", "tomatoes", "metacritic"];
/// How long a failed lookup (bad key, outage) is remembered.
const NEGATIVE_TTL: Duration = Duration::from_secs(10 * 60);
/// Longest `Retry-After` worth sleeping through for an in-request retry.
const MAX_INLINE_RETRY: Duration = Duration::from_secs(2);
/// Longest a 429 can keep us off the upstream API.
const MAX_RATE_LIMIT_GATE: Duration = Duration::from_secs(10 * 60);

/// Most keys tracked in the rate-limit gate at once.
const MAX_RATE_LIMITED_KEYS: usize = 1024;
const NOT_IN_LIBRARY_MESSAGE: &str = "No MDBList ratings available for this title.";

const RATE_LIMIT_MESSAGE: &str = "MDBList rate limit reached. Try again later.";
const NO_KEY_MESSAGE: &str = "No MDBList API key configured. Add your key in Moonfin Settings, or ask your server admin to set a server-wide key.";

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// MDBList media type; the lowercase form is also the upstream path segment.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Display, EnumString)]
#[strum(serialize_all = "lowercase", ascii_case_insensitive)]
enum MediaType {
    Movie,
    Show,
}

/// Opaque, process-local identity of an API key: a hash, never the key.
/// Scopes failures to the credential that caused them.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct KeyId(u64);

impl KeyId {
    fn of(api_key: &str) -> Self {
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        api_key.hash(&mut hasher);
        Self(hasher.finish())
    }
}

/// One rating as MDBList reports it and as Moonbase relays it.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct Rating {
    source: Option<String>,
    value: Option<f64>,
    score: Option<f64>,
    votes: Option<i64>,
    url: Option<String>,
}

#[derive(Serialize)]
struct RatingsResponse {
    success: bool,
    error: Option<String>,
    ratings: Vec<Rating>,
}

impl RatingsResponse {
    fn ok(ratings: Vec<Rating>) -> Self {
        Self {
            success: true,
            error: None,
            ratings,
        }
    }

    fn failed(error: impl Into<String>) -> Self {
        Self {
            success: false,
            error: Some(error.into()),
            ratings: Vec::new(),
        }
    }
}

#[derive(Deserialize)]
pub struct RatingsQuery {
    #[serde(rename = "type", alias = "Type")]
    kind: Option<String>,
    #[serde(rename = "tmdbId", alias = "tmdbid", alias = "TmdbId")]
    tmdb_id: Option<String>,
    #[serde(alias = "Profile")]
    profile: Option<String>,
}

// ---------------------------------------------------------------------------
// Pure helpers
// ---------------------------------------------------------------------------

fn bad_request(message: &str) -> Response {
    (StatusCode::BAD_REQUEST, Json(json!({ "Error": message }))).into_response()
}

/// Device profile name; anything unknown reads the global settings.
fn normalize_profile(raw: Option<&str>) -> &'static str {
    match raw
        .map(|p| {
            p.trim()
                .to_ascii_lowercase()
        })
        .as_deref()
    {
        Some("desktop") => "desktop",
        Some("mobile") => "mobile",
        Some("tv") => "tv",
        _ => "global",
    }
}

/// Leniently reads MDBList's `ratings` array (numbers may arrive as strings,
/// `url` may be `false`).
fn parse_upstream(body: &Value) -> Vec<Rating> {
    let number = |v: Option<&Value>| match v? {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s
            .trim()
            .parse()
            .ok(),
        _ => None,
    };
    body.get("ratings")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| {
                    let item = item.as_object()?;
                    Some(Rating {
                        source: item
                            .get("source")
                            .and_then(Value::as_str)
                            .map(str::to_string),
                        value: number(item.get("value")),
                        score: number(item.get("score")),
                        votes: number(item.get("votes")).map(|v| v as i64),
                        url: item
                            .get("url")
                            .and_then(Value::as_str)
                            .map(str::to_string),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Picks and orders ratings by the user's selected sources (Moonbase's
/// `FilterAndOrderRatings`). Ratings keep the name the user asked for.
fn filter_and_order(all: &[Rating], selected: Option<&[String]>) -> Vec<Rating> {
    let defaults: Vec<String> = DEFAULT_SOURCES
        .iter()
        .map(|s| s.to_string())
        .collect();
    let sources = match selected {
        Some(s) if !s.is_empty() => s,
        _ => &defaults,
    };
    let mut out = Vec::new();
    for source in sources {
        // RT audience scores arrive from MDBList as "popcorn".
        let lookup = if source.eq_ignore_ascii_case("rtAudience")
            || source.eq_ignore_ascii_case("tomatoes_audience")
        {
            "popcorn"
        } else {
            source.as_str()
        };
        // Later duplicates win, as in Moonbase's dictionary build.
        let Some(found) = all
            .iter()
            .rev()
            .find(|r| {
                r.source
                    .as_deref()
                    .is_some_and(|s| !s.is_empty() && s.eq_ignore_ascii_case(lookup))
            })
        else {
            continue;
        };
        let mut rating = found.clone();
        rating.source = Some(source.clone());
        // Letterboxd's value is on an ambiguous 0-10 scale; native is 0-5.
        if source.eq_ignore_ascii_case("letterboxd") {
            if let Some(score) = rating.score {
                rating.value = Some((score / 20.0 * 10.0).round() / 10.0);
            } else if let Some(v) = rating.value {
                rating.value = Some(if v > 5.0 {
                    (v / 2.0 * 10.0).round() / 10.0
                } else {
                    v
                });
            }
        }
        out.push(rating);
    }
    out
}

fn retry_after(resp: &reqwest::Response) -> Option<Duration> {
    resp.headers()
        .get(reqwest::header::RETRY_AFTER)?
        .to_str()
        .ok()?
        .trim()
        .parse()
        .ok()
        .map(Duration::from_secs)
}

// ---------------------------------------------------------------------------
// Service
// ---------------------------------------------------------------------------

/// Process-wide proxy state held in [`crate::AppContext`]. Cheap to clone.
#[derive(Clone)]
pub struct MdbListService {
    inner: Arc<Inner>,
}

struct Inner {
    client: reqwest::Client,
    /// One lock per cache key so concurrent lookups share a single fetch.
    in_flight: DashMap<String, Arc<tokio::sync::Mutex<()>>>,
    /// Title cache key -> error from an upstream outage (5xx, network, parse).
    /// Independent of any key, so shared by everyone.
    title_failures: moka::sync::Cache<String, String>,
    /// (key, title cache key) -> error for 401/403: only that key is bad.
    key_failures: moka::sync::Cache<(KeyId, String), String>,
    /// Key -> when upstream 429s stop applying. Memory only, bounded.
    rate_limited: DashMap<KeyId, Instant>,
}

impl Default for MdbListService {
    fn default() -> Self {
        // No redirects: the API key travels in the query string and must
        // never follow a redirect to another host.
        let client = reqwest::Client::builder()
            .user_agent(concat!("Remux/", env!("CARGO_PKG_VERSION")))
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(15))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .expect("failed to build MDBList client");
        Self {
            inner: Arc::new(Inner {
                client,
                in_flight: DashMap::new(),
                title_failures: moka::sync::Cache::builder()
                    .max_capacity(10_000)
                    .time_to_live(NEGATIVE_TTL)
                    .build(),
                key_failures: moka::sync::Cache::builder()
                    .max_capacity(10_000)
                    .time_to_live(NEGATIVE_TTL)
                    .build(),
                rate_limited: DashMap::new(),
            }),
        }
    }
}

/// What a failed fetch says about who is to blame, and so who remembers it.
enum Blame {
    /// Upstream trouble (5xx, 404, network, parse): remembered per title.
    Title,
    /// 401/403: this key is bad; remembered per (key, title).
    Key,
    /// 429: gates only this key for the given time; not a title failure.
    RateLimit(Duration),
}

enum Fetch {
    Ratings(Vec<Rating>),
    Failed { message: String, blame: Blame },
}

impl MdbListService {
    /// Unfiltered ratings for a title: cache, then upstream. `Err` carries
    /// the user-facing message (never containing the key).
    async fn ratings(
        &self,
        db: &SqlitePool,
        config: &Config,
        api_key: &str,
        media: MediaType,
        tmdb_id: u64,
    ) -> Result<Vec<Rating>, String> {
        let cache_key = format!("{media}:{tmdb_id}");
        let key_id = KeyId::of(api_key);
        let ttl = Duration::from_secs(
            config
                .mdblist_cache_ttl_hours
                .saturating_mul(3600),
        );
        // Cache hits are served whichever key asks: the ratings are public
        // MDBList data and a hit costs no quota.
        if let Some(hit) = cache_get(db, &cache_key, ttl).await {
            return Ok(hit);
        }
        if let Some(err) = self.known_failure(&cache_key, key_id) {
            return Err(err);
        }

        let lock = self
            .inner
            .in_flight
            .entry(cache_key.clone())
            .or_default()
            .clone();
        let result = {
            let _guard = lock
                .lock()
                .await;
            // A concurrent lookup may have finished while we waited.
            if let Some(hit) = cache_get(db, &cache_key, ttl).await {
                Ok(hit)
            } else if let Some(err) = self.known_failure(&cache_key, key_id) {
                Err(err)
            } else {
                match self
                    .fetch(config, api_key, media, tmdb_id)
                    .await
                {
                    Fetch::Ratings(ratings) => {
                        cache_put(db, &cache_key, &ratings, ttl).await;
                        Ok(ratings)
                    }
                    Fetch::Failed { message, blame } => {
                        match blame {
                            Blame::Title => self
                                .inner
                                .title_failures
                                .insert(cache_key.clone(), message.clone()),
                            Blame::Key => self
                                .inner
                                .key_failures
                                .insert((key_id, cache_key.clone()), message.clone()),
                            Blame::RateLimit(wait) => self.gate(key_id, wait),
                        }
                        Err(message)
                    }
                }
            }
        };
        drop(lock);
        self.inner
            .in_flight
            .remove_if(&cache_key, |_, l| Arc::strong_count(l) == 1);
        result
    }

    /// Starts (or extends) the 429 gate for one key, keeping the map bounded.
    fn gate(&self, key_id: KeyId, wait: Duration) {
        let now = Instant::now();
        let map = &self
            .inner
            .rate_limited;
        map.retain(|_, until| *until > now);
        if map.len() >= MAX_RATE_LIMITED_KEYS
            && !map.contains_key(&key_id)
            && let Some(oldest) = map
                .iter()
                .min_by_key(|e| *e.value())
                .map(|e| *e.key())
        {
            map.remove(&oldest);
        }
        map.insert(key_id, now + wait);
    }

    fn known_failure(&self, cache_key: &str, key_id: KeyId) -> Option<String> {
        if let Some(err) = self
            .inner
            .title_failures
            .get(cache_key)
        {
            return Some(err);
        }
        if let Some(err) = self
            .inner
            .key_failures
            .get(&(key_id, cache_key.to_string()))
        {
            return Some(err);
        }
        let gated = self
            .inner
            .rate_limited
            .get(&key_id)
            .is_some_and(|until| Instant::now() < *until);
        gated.then(|| RATE_LIMIT_MESSAGE.to_string())
    }

    async fn fetch(
        &self,
        config: &Config,
        api_key: &str,
        media: MediaType,
        tmdb_id: u64,
    ) -> Fetch {
        let Ok(mut url) = reqwest::Url::parse(&format!(
            "{}/tmdb/{media}/{tmdb_id}",
            config
                .mdblist_base_url
                .trim_end_matches('/')
        )) else {
            return Fetch::Failed {
                message: "MDBList is not configured correctly".into(),
                blame: Blame::Title,
            };
        };
        url.query_pairs_mut()
            .append_pair("apikey", api_key);
        for attempt in 0..2 {
            let resp = match self
                .inner
                .client
                .get(url.clone())
                .send()
                .await
            {
                Ok(r) => r,
                Err(e) => {
                    // `without_url`: the URL carries the API key.
                    warn!(err = %e.without_url(), "MDBList request failed");
                    return Fetch::Failed {
                        message: "Failed to fetch from MDBList".into(),
                        blame: Blame::Title,
                    };
                }
            };
            let status = resp.status();
            if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
                let wait = retry_after(&resp);
                if attempt == 0
                    && let Some(wait) = wait.filter(|w| *w <= MAX_INLINE_RETRY)
                {
                    tokio::time::sleep(wait).await;
                    continue;
                }
                let gate = wait
                    .unwrap_or(Duration::from_secs(60))
                    .clamp(Duration::from_secs(1), MAX_RATE_LIMIT_GATE);
                warn!(?gate, "MDBList rate limit reached");
                return Fetch::Failed {
                    message: RATE_LIMIT_MESSAGE.into(),
                    blame: Blame::RateLimit(gate),
                };
            }
            if !status.is_success() {
                warn!(status = status.as_u16(), "MDBList returned an error status");
                let blame = if matches!(
                    status,
                    reqwest::StatusCode::UNAUTHORIZED | reqwest::StatusCode::FORBIDDEN
                ) {
                    Blame::Key
                } else {
                    Blame::Title
                };
                return Fetch::Failed {
                    message: format!("MDBList returned status {}", status.as_u16()),
                    blame,
                };
            }
            return match resp
                .json::<Value>()
                .await
            {
                Ok(body) => Fetch::Ratings(parse_upstream(&body)),
                Err(e) => {
                    warn!(err = %e.without_url(), "MDBList returned an unreadable body");
                    Fetch::Failed {
                        message: "Failed to fetch from MDBList".into(),
                        blame: Blame::Title,
                    }
                }
            };
        }
        unreachable!("the retry loop always returns on its second pass")
    }
}

// ---------------------------------------------------------------------------
// Cache (SQLite)
// ---------------------------------------------------------------------------

async fn cache_get(db: &SqlitePool, key: &str, ttl: Duration) -> Option<Vec<Rating>> {
    let min_fetched = chrono::Utc::now().timestamp() - ttl.as_secs() as i64;
    let row: Option<(String,)> = sqlx::query_as(
        "SELECT ratings FROM mdblist_ratings_cache \
         WHERE cache_key = ?1 AND fetched_at > ?2",
    )
    .bind(key)
    .bind(min_fetched)
    .fetch_optional(db)
    .await
    .inspect_err(|e| warn!(err = %e, "MDBList cache read failed"))
    .ok()?;
    serde_json::from_str(&row?.0).ok()
}

async fn cache_put(db: &SqlitePool, key: &str, ratings: &[Rating], ttl: Duration) {
    let Ok(json) = serde_json::to_string(ratings) else {
        return;
    };
    let now = chrono::Utc::now().timestamp();
    let result = sqlx::query(
        "INSERT INTO mdblist_ratings_cache (cache_key, ratings, fetched_at) \
         VALUES (?1, ?2, ?3) \
         ON CONFLICT(cache_key) DO UPDATE SET ratings = ?2, fetched_at = ?3",
    )
    .bind(key)
    .bind(json)
    .bind(now)
    .execute(db)
    .await;
    // Entries past twice the TTL can never be served again.
    let pruned = sqlx::query("DELETE FROM mdblist_ratings_cache WHERE fetched_at < ?1")
        .bind(now - (ttl.as_secs() as i64).saturating_mul(2))
        .execute(db)
        .await;
    for err in [result.err(), pruned.err()]
        .into_iter()
        .flatten()
    {
        warn!(err = %err, "MDBList cache write failed");
    }
}

// ---------------------------------------------------------------------------
// Handler
// ---------------------------------------------------------------------------

/// Whether the library holds a movie/series with this TMDb id (uses the
/// `json_extract(external_ids, '$.tmdb')` expression index).
async fn title_in_library(db: &SqlitePool, media: MediaType, tmdb_id: u64) -> bool {
    let Ok(tmdb_id) = i64::try_from(tmdb_id) else {
        return false;
    };
    let kind = match media {
        MediaType::Movie => db::MediaKind::Movie,
        MediaType::Show => db::MediaKind::Series,
    };
    sqlx::query_scalar::<_, i64>(
        "SELECT 1 FROM media WHERE kind = ?1 \
         AND json_extract(external_ids, '$.tmdb') = ?2 LIMIT 1",
    )
    .bind(kind)
    .bind(tmdb_id)
    .fetch_optional(db)
    .await
    .inspect_err(|e| warn!(err = %e, "MDBList library lookup failed"))
    .ok()
    .flatten()
    .is_some()
}

#[get("/moonfin/mdblist/ratings")]
pub async fn moonfin_mdblist_ratings(
    State(state): State<AppState>,
    session: AuthSession,
    Query(q): Query<RatingsQuery>,
) -> Response {
    let (Some(kind), Some(tmdb_id)) = (
        q.kind
            .as_deref()
            .filter(|s| {
                !s.trim()
                    .is_empty()
            }),
        q.tmdb_id
            .as_deref()
            .filter(|s| {
                !s.trim()
                    .is_empty()
            }),
    ) else {
        return bad_request("Missing required parameters: type, tmdbId");
    };
    let Ok(media) = kind
        .trim()
        .parse::<MediaType>()
    else {
        return bad_request("Invalid type. Expected: movie or show");
    };
    let Ok(tmdb_id) = tmdb_id
        .trim()
        .parse::<u64>()
    else {
        return bad_request("Invalid tmdbId. Expected a number");
    };

    // The caller's profile supplies their own key and chosen sources.
    let profile = match resolved_profile(
        &state,
        session
            .user
            .id,
        normalize_profile(
            q.profile
                .as_deref(),
        ),
    )
    .await
    {
        Ok(p) => p,
        Err(e) => {
            warn!(err = %e, "MDBList could not load user settings");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "Error": "Internal error" })),
            )
                .into_response();
        }
    };
    let config = &state
        .ctx
        .config;
    let own_key = profile
        .get("mdblistApiKey")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|k| !k.is_empty());
    let server_key = super::moonfin_server_settings::effective(&state).await;
    let Some(api_key) = own_key.or_else(|| server_key.mdblist_key()) else {
        return Json(RatingsResponse::failed(NO_KEY_MESSAGE)).into_response();
    };
    // The shared server key's quota is only spent on titles in this library;
    // a user's own key is their own quota and is unrestricted.
    if own_key.is_none()
        && !title_in_library(
            &state
                .ctx
                .db,
            media,
            tmdb_id,
        )
        .await
    {
        return Json(RatingsResponse::failed(NOT_IN_LIBRARY_MESSAGE)).into_response();
    }

    match state
        .ctx
        .mdblist
        .ratings(
            &state
                .ctx
                .db,
            config,
            api_key,
            media,
            tmdb_id,
        )
        .await
    {
        Ok(all) => {
            let sources: Option<Vec<String>> = profile
                .get("mdblistRatingSources")
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(|s| {
                            s.as_str()
                                .map(str::to_string)
                        })
                        .collect()
                });
            Json(RatingsResponse::ok(filter_and_order(
                &all,
                sources.as_deref(),
            )))
            .into_response()
        }
        Err(message) => Json(RatingsResponse::failed(message)).into_response(),
    }
}

// ---------------------------------------------------------------------------
// Admin tools: key test and cache clear
// ---------------------------------------------------------------------------

/// Largest accepted admin test body (it carries at most one key).
const MAX_ADMIN_BODY_BYTES: usize = 16 * 1024;

/// Outcome of asking MDBList about a key.
#[derive(Debug, PartialEq)]
pub(crate) enum KeyCheck {
    Ok { limits: Value },
    Failed(String),
}

fn header_i64(resp: &reqwest::Response, name: &str) -> Option<i64> {
    resp.headers()
        .get(name)?
        .to_str()
        .ok()?
        .trim()
        .parse()
        .ok()
}

impl MdbListService {
    /// One cheap call, `GET /user`, which reports the plan, its limits and the
    /// request counters, and carries `X-RateLimit-*` headers. It bypasses the
    /// ratings failure/rate-limit state on purpose: an admin testing a key
    /// wants a live answer. The key is only sent upstream, never returned.
    pub(crate) async fn check_key(&self, config: &Config, api_key: &str) -> KeyCheck {
        let Ok(mut url) = reqwest::Url::parse(&format!(
            "{}/user",
            config
                .mdblist_base_url
                .trim_end_matches('/')
        )) else {
            return KeyCheck::Failed("MDBList is not configured correctly".into());
        };
        url.query_pairs_mut()
            .append_pair("apikey", api_key);
        let resp = match self
            .inner
            .client
            .get(url)
            .send()
            .await
        {
            Ok(r) => r,
            Err(e) => {
                warn!(err = %e.without_url(), "MDBList key test failed");
                return KeyCheck::Failed("Could not reach MDBList".into());
            }
        };
        let status = resp.status();
        if status == reqwest::StatusCode::UNAUTHORIZED
            || status == reqwest::StatusCode::FORBIDDEN
        {
            return KeyCheck::Failed("MDBList rejected the API key".into());
        }
        if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
            return KeyCheck::Failed(RATE_LIMIT_MESSAGE.into());
        }
        if !status.is_success() {
            return KeyCheck::Failed(format!(
                "MDBList returned status {}",
                status.as_u16()
            ));
        }
        let header_limits = json!({
            "limit": header_i64(&resp, "x-ratelimit-limit"),
            "remaining": header_i64(&resp, "x-ratelimit-remaining"),
            "reset": header_i64(&resp, "x-ratelimit-reset"),
        });
        let body = resp
            .json::<Value>()
            .await
            .unwrap_or(Value::Null);
        let mut limits = serde_json::Map::new();
        limits.insert("rateLimit".into(), header_limits);
        // Plan details, when the response has them; never anything else.
        for (src, dst) in [
            ("limits", "plan"),
            ("api_requests", "apiRequests"),
            ("api_requests_count", "apiRequestsCount"),
        ] {
            if let Some(v) = body.get(src) {
                limits.insert(dst.into(), v.clone());
            }
        }
        KeyCheck::Ok {
            limits: Value::Object(limits),
        }
    }

    /// Forgets every remembered failure and rate-limit gate; returns how many
    /// (title failures, key failures, rate-limit gates) were dropped.
    pub(crate) fn clear_memory(&self) -> (usize, usize, usize) {
        let counts = (
            self.inner
                .title_failures
                .iter()
                .count(),
            self.inner
                .key_failures
                .iter()
                .count(),
            self.inner
                .rate_limited
                .len(),
        );
        self.inner
            .title_failures
            .invalidate_all();
        self.inner
            .key_failures
            .invalidate_all();
        self.inner
            .rate_limited
            .clear();
        counts
    }
}

/// Tests an MDBList key without echoing it: the candidate `{"apiKey": "..."}`
/// in the body, else the effective server key. Always `200 {ok, error?,
/// limits?}`; `401`/`403` for non-admins.
#[post("/moonfin/admin/mdblist/test")]
pub async fn moonfin_admin_mdblist_test(
    State(state): State<AppState>,
    session: AuthSession,
    req: Request<Body>,
) -> Response {
    use super::moonfin_admin::{json_error, require_admin};
    if let Some(denied) = require_admin(&session) {
        return denied;
    }
    let bytes = match axum::body::to_bytes(req.into_body(), MAX_ADMIN_BODY_BYTES).await
    {
        Ok(b) => b,
        Err(_) => {
            return json_error(StatusCode::PAYLOAD_TOO_LARGE, "Request body too large");
        }
    };
    let candidate = if bytes
        .iter()
        .all(u8::is_ascii_whitespace)
    {
        None
    } else {
        match serde_json::from_slice::<Value>(&bytes) {
            Ok(Value::Object(body)) => match body.get("apiKey") {
                None | Some(Value::Null) => None,
                Some(Value::String(k)) => Some(
                    k.trim()
                        .to_string(),
                )
                .filter(|k| !k.is_empty()),
                Some(_) => {
                    return json_error(
                        StatusCode::BAD_REQUEST,
                        "apiKey must be a string",
                    );
                }
            },
            _ => {
                return json_error(
                    StatusCode::BAD_REQUEST,
                    "Body must be a JSON object",
                );
            }
        }
    };
    let key = match candidate {
        Some(k) => k,
        None => match super::moonfin_server_settings::effective(&state)
            .await
            .mdblist_key
        {
            Some((k, _)) => k,
            None => {
                return Json(json!({
                    "ok": false,
                    "error": "No MDBList API key configured",
                }))
                .into_response();
            }
        },
    };
    let body = match state
        .ctx
        .mdblist
        .check_key(
            &state
                .ctx
                .config,
            &key,
        )
        .await
    {
        KeyCheck::Ok { limits } => json!({ "ok": true, "limits": limits }),
        KeyCheck::Failed(error) => json!({ "ok": false, "error": error }),
    };
    Json(body).into_response()
}

/// Empties the ratings cache and forgets in-memory failures and rate-limit
/// gates, so the next lookups go upstream again.
#[post("/moonfin/admin/mdblist/clear-cache")]
pub async fn moonfin_admin_mdblist_clear_cache(
    State(state): State<AppState>,
    session: AuthSession,
) -> Response {
    if let Some(denied) = super::moonfin_admin::require_admin(&session) {
        return denied;
    }
    let cleared = match sqlx::query("DELETE FROM mdblist_ratings_cache")
        .execute(
            &state
                .ctx
                .db,
        )
        .await
    {
        Ok(done) => done.rows_affected(),
        Err(e) => return super::moonfin_settings::internal_error(e),
    };
    let (titles, keys, gates) = state
        .ctx
        .mdblist
        .clear_memory();
    Json(json!({
        "clearedRatings": cleared,
        "clearedFailures": titles + keys,
        "clearedRateLimits": gates,
    }))
    .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        Secret, init_app_with_ctx,
        integration_test::{AUTH_HEADER, TestGuard, auth_header_with_token},
        rewrite_request_uri,
    };
    use axum::Router;
    use axum_test::{TestResponse, TestServer};
    use http::header;
    use httpmock::prelude::*;
    use tower::{Layer, util::MapRequestLayer};

    /// Fake key; never a real credential.
    const KEY: &str = "fake-server-key-123";

    struct Harness {
        server: TestServer,
        _guard: TestGuard,
        token: String,
        _data: tempfile::TempDir,
    }

    impl Harness {
        async fn get(&self, path: &str) -> TestResponse {
            self.get_as(&self.token, path)
                .await
        }

        async fn get_as(&self, token: &str, path: &str) -> TestResponse {
            self.server
                .get(path)
                .add_header(
                    header::AUTHORIZATION,
                    http::HeaderValue::from_str(&auth_header_with_token(token))
                        .unwrap(),
                )
                .await
        }

        /// Creates a regular second user and returns its token.
        async fn second_user(&self) -> String {
            self.server
                .post("/Users/New")
                .add_header(
                    header::AUTHORIZATION,
                    http::HeaderValue::from_str(&auth_header_with_token(&self.token))
                        .unwrap(),
                )
                .json(&json!({ "Name": "second", "Password": "pw" }))
                .await;
            self.server
                .post("/Users/AuthenticateByName")
                .add_header(
                    header::AUTHORIZATION,
                    http::HeaderValue::from_static(AUTH_HEADER),
                )
                .json(&json!({ "Username": "second", "Pw": "pw" }))
                .await
                .json::<Value>()["AccessToken"]
                .as_str()
                .unwrap()
                .to_string()
        }

        async fn push_as(&self, token: &str, settings: Value) {
            self.server
                .post("/Moonfin/Settings/Profile/global")
                .add_header(
                    header::AUTHORIZATION,
                    http::HeaderValue::from_str(&auth_header_with_token(token))
                        .unwrap(),
                )
                .json(&json!({ "profile": settings, "clientId": "test" }))
                .await
                .assert_status_ok();
        }

        /// Puts a movie/series with this TMDb id in the library.
        async fn seed(&self, kind: db::MediaKind, tmdb: i64) {
            let external_ids = db::ExternalIds {
                tmdb: Some(tmdb),
                ..Default::default()
            };
            let mut media = db::Media {
                id: uuid::Uuid::from(&db::MediaIdRaw {
                    kind: kind.clone(),
                    external_ids: external_ids.clone(),
                    season: None,
                    episode: None,
                }),
                title: format!("title {tmdb}"),
                kind,
                external_ids,
                ..Default::default()
            };
            media
                .save(
                    &self
                        ._guard
                        .0
                        .db,
                )
                .await
                .unwrap();
        }

        async fn seed_movies(&self, ids: &[i64]) {
            for id in ids {
                self.seed(db::MediaKind::Movie, *id)
                    .await;
            }
        }

        async fn json(&self, path: &str) -> Value {
            let resp = self
                .get(path)
                .await;
            resp.assert_status_ok();
            resp.json::<Value>()
        }

        async fn push(&self, settings: Value) {
            self.push_as(&self.token, settings)
                .await;
        }
    }

    async fn harness(key: Option<&str>, base_url: String) -> Harness {
        let data = tempfile::tempdir().unwrap();
        let config = Config {
            database_url: Some("sqlite::memory:".into()),
            torrent_http_port: None,
            torrent_peer_port: None,
            disable_dht: true,
            data_dir: data
                .path()
                .to_path_buf(),
            mdblist_api_key: key.map(Secret::new),
            mdblist_base_url: base_url,
            ..Default::default()
        };
        let (router, ctx) = init_app_with_ctx(config)
            .await
            .unwrap();
        let app = Router::new()
            .fallback_service(MapRequestLayer::new(rewrite_request_uri).layer(router));
        let server = TestServer::builder()
            .mock_transport()
            .build(app)
            .unwrap();
        server
            .post("/Startup/User")
            .json(&json!({ "Name": "test", "Password": "test" }))
            .await;
        server
            .post("/Startup/Complete")
            .await;
        let token = server
            .post("/Users/AuthenticateByName")
            .add_header(
                header::AUTHORIZATION,
                http::HeaderValue::from_static(AUTH_HEADER),
            )
            .json(&json!({ "Username": "test", "Pw": "test" }))
            .await
            .json::<Value>()["AccessToken"]
            .as_str()
            .unwrap()
            .to_string();
        Harness {
            server,
            _guard: TestGuard(ctx),
            token,
            _data: data,
        }
    }

    fn upstream_body() -> Value {
        json!({
            "title": "The Matrix",
            "type": "movie",
            "ratings": [
                { "source": "imdb", "value": 8.7, "score": 87, "votes": 2100000,
                  "url": "https://www.imdb.com/title/tt0133093" },
                { "source": "tmdb", "value": 82, "score": 82, "votes": 25000, "url": false },
                { "source": "tomatoes", "value": 83, "score": 83, "votes": null, "url": null },
                { "source": "popcorn", "value": 85, "score": 85, "votes": 250000 },
                { "source": "letterboxd", "value": 8.2, "score": 82, "votes": 1500000 },
                { "source": "metacritic", "value": 73, "score": 73, "votes": 35 },
            ]
        })
    }

    const URL_603: &str = "/Moonfin/MdbList/Ratings?type=movie&tmdbId=603&profile=tv";

    #[tokio::test]
    async fn returns_default_sources_in_moonbase_shape_and_hides_key() {
        let mock = MockServer::start();
        let m = mock.mock(|when, then| {
            when.method(GET)
                .path("/tmdb/movie/603")
                .query_param("apikey", KEY);
            then.status(200)
                .json_body(upstream_body());
        });
        let h = harness(Some(KEY), mock.base_url()).await;
        h.seed_movies(&[603])
            .await;

        let resp = h
            .get(URL_603)
            .await;
        resp.assert_status_ok();
        let text = resp.text();
        assert!(!text.contains(KEY));
        let body: Value = serde_json::from_str(&text).unwrap();

        assert_eq!(body["success"], true);
        assert_eq!(body["error"], Value::Null);
        let sources: Vec<&str> = body["ratings"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| {
                r["source"]
                    .as_str()
                    .unwrap()
            })
            .collect();
        assert_eq!(sources, ["imdb", "tmdb", "tomatoes", "metacritic"]);
        assert_eq!(
            body["ratings"][0],
            json!({
                "source": "imdb", "value": 8.7, "score": 87.0,
                "votes": 2100000, "url": "https://www.imdb.com/title/tt0133093"
            })
        );
        // `url: false` upstream is relayed as null.
        assert_eq!(body["ratings"][1]["url"], Value::Null);
        m.assert_hits(1);
    }

    #[tokio::test]
    async fn second_lookup_is_served_from_cache_for_any_profile_and_sources() {
        let mock = MockServer::start();
        let m = mock.mock(|when, then| {
            when.method(GET)
                .path("/tmdb/movie/603");
            then.status(200)
                .json_body(upstream_body());
        });
        let h = harness(Some(KEY), mock.base_url()).await;
        h.seed_movies(&[603])
            .await;

        h.json(URL_603)
            .await;
        h.push(json!({ "mdblistRatingSources": ["rtAudience", "letterboxd"] }))
            .await;
        let body = h
            .json("/Moonfin/MdbList/Ratings?type=Movie&tmdbId=603")
            .await;

        m.assert_hits(1);
        assert_eq!(body["success"], true);
        let ratings = body["ratings"]
            .as_array()
            .unwrap();
        assert_eq!(ratings[0]["source"], "tomatoes_audience");
        assert_eq!(ratings[0]["value"], 85.0);
        assert_eq!(ratings[1]["source"], "letterboxd");
        // Letterboxd is rescaled from the 0-100 score to its native 0-5.
        assert_eq!(ratings[1]["value"], 4.1);
    }

    #[tokio::test]
    async fn fetched_ratings_are_persisted_in_sqlite() {
        // Persisted so a restart does not re-spend the daily request quota.
        let mock = MockServer::start();
        let m = mock.mock(|when, then| {
            when.method(GET)
                .path("/tmdb/show/1396");
            then.status(200)
                .json_body(upstream_body());
        });
        let h = harness(Some(KEY), mock.base_url()).await;
        h.seed(db::MediaKind::Series, 1396)
            .await;
        let url = "/Moonfin/MdbList/Ratings?type=show&tmdbId=1396";
        h.json(url)
            .await;
        let row: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM mdblist_ratings_cache")
            .fetch_one(
                &h._guard
                    .0
                    .db,
            )
            .await
            .unwrap();
        assert_eq!(row.0, 1);
        m.assert_hits(1);
    }

    #[tokio::test]
    async fn missing_key_answers_failure_without_calling_upstream() {
        let mock = MockServer::start();
        let m = mock.mock(|when, then| {
            when.method(GET);
            then.status(200)
                .json_body(upstream_body());
        });
        let h = harness(None, mock.base_url()).await;

        let body = h
            .json(URL_603)
            .await;
        assert_eq!(body["success"], false);
        assert!(
            body["error"]
                .as_str()
                .unwrap()
                .contains("No MDBList API key")
        );
        assert_eq!(body["ratings"], json!([]));
        m.assert_hits(0);
    }

    #[tokio::test]
    async fn users_own_key_wins_over_server_key() {
        let mock = MockServer::start();
        let own = mock.mock(|when, then| {
            when.method(GET)
                .path("/tmdb/movie/603")
                .query_param("apikey", "fake-user-key");
            then.status(200)
                .json_body(upstream_body());
        });
        let h = harness(Some(KEY), mock.base_url()).await;
        h.push(json!({ "mdblistApiKey": "fake-user-key" }))
            .await;

        let resp = h
            .get(URL_603)
            .await;
        let text = resp.text();
        assert!(!text.contains("fake-user-key") && !text.contains(KEY));
        own.assert_hits(1);
    }

    #[tokio::test]
    async fn user_key_alone_works_without_a_server_key() {
        let mock = MockServer::start();
        let m = mock.mock(|when, then| {
            when.method(GET)
                .path("/tmdb/movie/603")
                .query_param("apikey", "fake-user-key");
            then.status(200)
                .json_body(upstream_body());
        });
        let h = harness(None, mock.base_url()).await;
        h.push(json!({ "mdblistApiKey": "fake-user-key" }))
            .await;
        assert_eq!(
            h.json(URL_603)
                .await["success"],
            true
        );
        m.assert_hits(1);
    }

    #[tokio::test]
    async fn rejects_bad_requests_and_anonymous_callers() {
        let mock = MockServer::start();
        let m = mock.mock(|when, then| {
            when.method(GET);
            then.status(200)
                .json_body(upstream_body());
        });
        let h = harness(Some(KEY), mock.base_url()).await;

        for path in [
            "/Moonfin/MdbList/Ratings",
            "/Moonfin/MdbList/Ratings?type=movie",
            "/Moonfin/MdbList/Ratings?tmdbId=603",
            "/Moonfin/MdbList/Ratings?type=episode&tmdbId=603",
            "/Moonfin/MdbList/Ratings?type=movie&tmdbId=tt0133093",
            "/Moonfin/MdbList/Ratings?type=movie&tmdbId=../user",
        ] {
            let resp = h
                .get(path)
                .await;
            resp.assert_status(StatusCode::BAD_REQUEST);
            assert!(resp.json::<Value>()["Error"].is_string(), "{path}");
        }
        h.server
            .get(URL_603)
            .await
            .assert_status(StatusCode::UNAUTHORIZED);
        m.assert_hits(0);
    }

    #[tokio::test]
    async fn unknown_title_and_server_errors_fail_softly_and_are_remembered() {
        let mock = MockServer::start();
        let not_found = mock.mock(|when, then| {
            when.method(GET)
                .path("/tmdb/movie/1");
            then.status(404);
        });
        let broken = mock.mock(|when, then| {
            when.method(GET)
                .path("/tmdb/movie/2");
            then.status(500)
                .body(format!("upstream exploded for apikey={KEY}"));
        });
        let h = harness(Some(KEY), mock.base_url()).await;
        h.seed_movies(&[1, 2])
            .await;

        for (id, status, mock_hits) in [(1, 404, &not_found), (2, 500, &broken)] {
            let url = format!("/Moonfin/MdbList/Ratings?type=movie&tmdbId={id}");
            for _ in 0..2 {
                let resp = h
                    .get(&url)
                    .await;
                resp.assert_status_ok();
                let text = resp.text();
                assert!(!text.contains(KEY));
                let body: Value = serde_json::from_str(&text).unwrap();
                assert_eq!(body["success"], false);
                assert_eq!(body["error"], format!("MDBList returned status {status}"));
            }
            // The failure is remembered, so the repeat never reached upstream.
            mock_hits.assert_hits(1);
        }
    }

    #[tokio::test]
    async fn rate_limit_backs_off_all_lookups_with_that_key() {
        let mock = MockServer::start();
        let limited = mock.mock(|when, then| {
            when.method(GET)
                .path("/tmdb/movie/1");
            then.status(429)
                .header("Retry-After", "120");
        });
        let other = mock.mock(|when, then| {
            when.method(GET)
                .path("/tmdb/movie/2");
            then.status(200)
                .json_body(upstream_body());
        });
        let h = harness(Some(KEY), mock.base_url()).await;
        h.seed_movies(&[1, 2])
            .await;

        let body = h
            .json("/Moonfin/MdbList/Ratings?type=movie&tmdbId=1")
            .await;
        assert_eq!(body["success"], false);
        assert_eq!(body["error"], RATE_LIMIT_MESSAGE);
        // A different title is not even attempted while the key is gated.
        let body = h
            .json("/Moonfin/MdbList/Ratings?type=movie&tmdbId=2")
            .await;
        assert_eq!(body["error"], RATE_LIMIT_MESSAGE);
        limited.assert_hits(1);
        other.assert_hits(0);
    }

    #[tokio::test]
    async fn short_retry_after_is_retried_once_inline() {
        let mock = MockServer::start();
        let limited = mock.mock(|when, then| {
            when.method(GET)
                .path("/tmdb/movie/1");
            then.status(429)
                .header("Retry-After", "0");
        });
        let h = harness(Some(KEY), mock.base_url()).await;
        h.seed_movies(&[1])
            .await;
        let body = h
            .json("/Moonfin/MdbList/Ratings?type=movie&tmdbId=1")
            .await;
        assert_eq!(body["error"], RATE_LIMIT_MESSAGE);
        limited.assert_hits(2);
    }

    #[tokio::test]
    async fn unreachable_upstream_never_leaks_the_key() {
        let closed = {
            let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            format!(
                "http://{}",
                l.local_addr()
                    .unwrap()
            )
        };
        let h = harness(Some(KEY), closed).await;
        h.seed_movies(&[603])
            .await;
        let resp = h
            .get(URL_603)
            .await;
        resp.assert_status_ok();
        let text = resp.text();
        assert!(!text.contains(KEY), "{text}");
        let body: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(body["success"], false);
        assert_eq!(body["error"], "Failed to fetch from MDBList");
    }

    #[tokio::test]
    async fn concurrent_lookups_share_one_upstream_call() {
        let mock = MockServer::start();
        let m = mock.mock(|when, then| {
            when.method(GET)
                .path("/tmdb/movie/603");
            then.status(200)
                .delay(Duration::from_millis(300))
                .json_body(upstream_body());
        });
        let h = harness(Some(KEY), mock.base_url()).await;
        h.seed_movies(&[603])
            .await;
        let (a, b, c) = tokio::join!(h.get(URL_603), h.get(URL_603), h.get(URL_603));
        for r in [a, b, c] {
            assert_eq!(r.json::<Value>()["success"], true);
        }
        m.assert_hits(1);
    }

    #[tokio::test]
    async fn ping_reports_mdblist_only_when_a_server_key_is_configured() {
        let with = harness(Some(KEY), "http://127.0.0.1:1".into()).await;
        assert_eq!(
            with.json("/Moonfin/Ping")
                .await["mdblistAvailable"],
            true
        );
        let blank = harness(Some("   "), "http://127.0.0.1:1".into()).await;
        assert_eq!(
            blank
                .json("/Moonfin/Ping")
                .await["mdblistAvailable"],
            false
        );
    }

    #[tokio::test]
    async fn proxy_uses_the_key_an_admin_set_at_runtime() {
        let mock = MockServer::start();
        let m = mock_ok(&mock, "/tmdb/movie/603", KEY);
        // No environment key: the proxy only works once the admin sets one.
        let h = harness(None, mock.base_url()).await;
        h.seed_movies(&[603])
            .await;
        assert_eq!(
            h.json(URL_603)
                .await["success"],
            false
        );
        h.server
            .put("/moonfin/admin/server-settings")
            .add_header(
                header::AUTHORIZATION,
                http::HeaderValue::from_str(&auth_header_with_token(&h.token)).unwrap(),
            )
            .json(&json!({ "mdblistApiKey": KEY }))
            .await
            .assert_status_ok();
        let body = h
            .json(URL_603)
            .await;
        assert_eq!(body["success"], true);
        m.assert_hits(1);
    }

    fn admin_post(h: &Harness, token: &str, path: &str) -> axum_test::TestRequest {
        h.server
            .post(path)
            .add_header(
                header::AUTHORIZATION,
                http::HeaderValue::from_str(&auth_header_with_token(token)).unwrap(),
            )
    }

    #[tokio::test]
    async fn key_test_reports_limits_without_echoing_the_key() {
        let mock = MockServer::start();
        let m = mock.mock(|when, then| {
            when.method(GET)
                .path("/user")
                .query_param("apikey", KEY);
            then.status(200)
                .header("X-RateLimit-Limit", "1000")
                .header("X-RateLimit-Remaining", "990")
                .header("X-RateLimit-Reset", "1790000000")
                .json_body(json!({ "api_requests": 1000, "api_requests_count": 10, "email": "x@y.z" }));
        });
        let h = harness(Some(KEY), mock.base_url()).await;
        // Effective key, empty body.
        let resp = admin_post(&h, &h.token, "/moonfin/admin/mdblist/test").await;
        resp.assert_status_ok();
        assert!(
            !resp
                .text()
                .contains(KEY)
        );
        assert!(
            !resp
                .text()
                .contains("x@y.z")
        );
        let body = resp.json::<Value>();
        assert_eq!(body["ok"], true);
        assert_eq!(body["limits"]["rateLimit"]["limit"], 1000);
        assert_eq!(body["limits"]["rateLimit"]["remaining"], 990);
        assert_eq!(body["limits"]["apiRequests"], 1000);
        m.assert_hits(1);
    }

    #[tokio::test]
    async fn key_test_uses_a_candidate_key_and_reports_rejection() {
        let mock = MockServer::start();
        let rejected = mock.mock(|when, then| {
            when.method(GET)
                .path("/user")
                .query_param("apikey", BAD);
            then.status(403);
        });
        let h = harness(Some(KEY), mock.base_url()).await;
        let resp = admin_post(&h, &h.token, "/moonfin/admin/mdblist/test")
            .json(&json!({ "apiKey": BAD }))
            .await;
        let text = resp.text();
        assert!(!text.contains(BAD));
        let body: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(body["ok"], false);
        assert_eq!(body["error"], "MDBList rejected the API key");
        rejected.assert_hits(1);

        // Malformed bodies are a client error.
        let bad = admin_post(&h, &h.token, "/moonfin/admin/mdblist/test")
            .json(&json!({ "apiKey": 5 }))
            .await;
        assert_eq!(bad.status_code(), 400);
    }

    #[tokio::test]
    async fn key_test_without_any_key_says_so() {
        let h = harness(None, "http://127.0.0.1:1".into()).await;
        let body = admin_post(&h, &h.token, "/moonfin/admin/mdblist/test")
            .await
            .json::<Value>();
        assert_eq!(body["ok"], false);
        assert_eq!(body["error"], "No MDBList API key configured");
    }

    #[tokio::test]
    async fn clear_cache_empties_the_cache_and_forgets_failures() {
        let mock = MockServer::start();
        let ok = mock.mock(|when, then| {
            when.method(GET)
                .path("/tmdb/movie/603");
            then.status(200)
                .json_body(upstream_body());
        });
        let h = harness(Some(KEY), mock.base_url()).await;
        h.seed_movies(&[603, 604])
            .await;
        h.json(URL_603)
            .await;
        ok.assert_hits(1);
        // A failure for another title is remembered ...
        let fail = mock.mock(|when, then| {
            when.method(GET)
                .path("/tmdb/movie/604");
            then.status(500);
        });
        let first = h
            .json("/Moonfin/MdbList/Ratings?type=movie&tmdbId=604")
            .await;
        assert_eq!(first["success"], false);
        h.json("/Moonfin/MdbList/Ratings?type=movie&tmdbId=604")
            .await;
        fail.assert_hits(1);

        let resp = admin_post(&h, &h.token, "/moonfin/admin/mdblist/clear-cache").await;
        resp.assert_status_ok();
        let body = resp.json::<Value>();
        assert_eq!(body["clearedRatings"], 1);
        assert_eq!(body["clearedFailures"], 1);
        assert_eq!(body["clearedRateLimits"], 0);

        // ... and goes upstream again once cleared.
        h.json(URL_603)
            .await;
        h.json("/Moonfin/MdbList/Ratings?type=movie&tmdbId=604")
            .await;
        ok.assert_hits(2);
        fail.assert_hits(2);
    }

    #[tokio::test]
    async fn mdblist_admin_tools_need_an_admin() {
        let h = harness(Some(KEY), "http://127.0.0.1:1".into()).await;
        let other = h
            .second_user()
            .await;
        for path in [
            "/moonfin/admin/mdblist/test",
            "/moonfin/admin/mdblist/clear-cache",
        ] {
            assert_eq!(
                h.server
                    .post(path)
                    .await
                    .status_code(),
                401
            );
            assert_eq!(
                admin_post(&h, &other, path)
                    .await
                    .status_code(),
                403
            );
        }
    }

    fn mock_ok<'a>(mock: &'a MockServer, path: &str, key: &str) -> httpmock::Mock<'a> {
        mock.mock(|when, then| {
            when.method(GET)
                .path(path)
                .query_param("apikey", key);
            then.status(200)
                .json_body(upstream_body());
        })
    }

    const BAD: &str = "fake-bad-user-key";
    const OTHER: &str = "fake-other-user-key";
    const URL_1: &str = "/Moonfin/MdbList/Ratings?type=movie&tmdbId=1";

    #[tokio::test]
    async fn bad_user_key_does_not_poison_the_title_for_other_users() {
        let mock = MockServer::start();
        let bad = mock.mock(|when, then| {
            when.method(GET)
                .path("/tmdb/movie/1")
                .query_param("apikey", BAD);
            then.status(401);
        });
        let good = mock_ok(&mock, "/tmdb/movie/1", KEY);
        let h = harness(Some(KEY), mock.base_url()).await;
        h.seed_movies(&[1])
            .await;
        let second = h
            .second_user()
            .await;
        h.push_as(&second, json!({ "mdblistApiKey": BAD }))
            .await;

        // User A (bad key) fails, and is remembered failed for that key.
        for _ in 0..2 {
            let resp = h
                .get_as(&second, URL_1)
                .await;
            resp.assert_status_ok();
            let body = resp.json::<Value>();
            assert_eq!(body["success"], false);
            assert_eq!(body["error"], "MDBList returned status 401");
        }
        bad.assert_hits(1);
        // User B (server key) still gets the title.
        let body = h
            .json(URL_1)
            .await;
        assert_eq!(body["success"], true);
        good.assert_hits(1);
    }

    #[tokio::test]
    async fn rate_limit_gates_only_the_key_that_hit_it() {
        let mock = MockServer::start();
        let limited = mock.mock(|when, then| {
            when.method(GET)
                .path("/tmdb/movie/1")
                .query_param("apikey", OTHER);
            then.status(429)
                .header("Retry-After", "30");
        });
        let ok_other = mock_ok(&mock, "/tmdb/movie/2", OTHER);
        let ok_server = mock_ok(&mock, "/tmdb/movie/1", KEY);
        let h = harness(Some(KEY), mock.base_url()).await;
        h.seed_movies(&[1, 2])
            .await;
        let second = h
            .second_user()
            .await;
        h.push_as(&second, json!({ "mdblistApiKey": OTHER }))
            .await;

        let body = h
            .get_as(&second, URL_1)
            .await
            .json::<Value>();
        assert_eq!(body["error"], RATE_LIMIT_MESSAGE);
        // The gated key is held off for other titles too, with no new call.
        let body = h
            .get_as(&second, "/Moonfin/MdbList/Ratings?type=movie&tmdbId=2")
            .await
            .json::<Value>();
        assert_eq!(body["error"], RATE_LIMIT_MESSAGE);
        limited.assert_hits(1);
        ok_other.assert_hits(0);
        // The server-key user is unaffected, even for the same title.
        assert_eq!(
            h.json(URL_1)
                .await["success"],
            true
        );
        ok_server.assert_hits(1);
        // A 429 is a key gate, never a remembered title failure.
        let svc = &h
            ._guard
            .0
            .mdblist
            .inner;
        assert_eq!(
            svc.title_failures
                .iter()
                .count(),
            0
        );
        assert_eq!(
            svc.key_failures
                .iter()
                .count(),
            0
        );
        let gate = svc
            .rate_limited
            .get(&KeyId::of(OTHER))
            .map(|g| *g)
            .unwrap();
        let left = gate - Instant::now();
        assert!(left <= Duration::from_secs(30), "{left:?}");
    }

    #[tokio::test]
    async fn short_retry_after_gate_expires_instead_of_lasting_ten_minutes() {
        let mock = MockServer::start();
        let limited = mock.mock(|when, then| {
            when.method(GET)
                .path("/tmdb/movie/1");
            then.status(429)
                .header("Retry-After", "3");
        });
        let h = harness(Some(KEY), mock.base_url()).await;
        h.seed_movies(&[1])
            .await;
        assert_eq!(
            h.json(URL_1)
                .await["error"],
            RATE_LIMIT_MESSAGE
        );
        limited.assert_hits(1);
        let svc = &h
            ._guard
            .0
            .mdblist
            .inner;
        assert!(
            svc.rate_limited
                .get(&KeyId::of(KEY))
                .map(|g| *g - Instant::now())
                .unwrap()
                <= Duration::from_secs(3)
        );
        // Nothing title-scoped lingers once the short gate passes.
        assert_eq!(
            svc.title_failures
                .iter()
                .count(),
            0
        );
    }

    #[tokio::test]
    async fn upstream_5xx_stays_remembered_for_everyone() {
        let mock = MockServer::start();
        let broken = mock.mock(|when, then| {
            when.method(GET)
                .path("/tmdb/movie/1");
            then.status(503);
        });
        let h = harness(Some(KEY), mock.base_url()).await;
        h.seed_movies(&[1])
            .await;
        let second = h
            .second_user()
            .await;
        h.push_as(&second, json!({ "mdblistApiKey": OTHER }))
            .await;
        assert_eq!(
            h.json(URL_1)
                .await["success"],
            false
        );
        // A different user with a different key is served the memory.
        let body = h
            .get_as(&second, URL_1)
            .await
            .json::<Value>();
        assert_eq!(body["error"], "MDBList returned status 503");
        broken.assert_hits(1);
    }

    #[tokio::test]
    async fn server_key_skips_titles_outside_the_library() {
        let mock = MockServer::start();
        let m = mock.mock(|when, then| {
            when.method(GET);
            then.status(200)
                .json_body(upstream_body());
        });
        let h = harness(Some(KEY), mock.base_url()).await;
        h.seed_movies(&[603])
            .await;
        h.seed(db::MediaKind::Series, 1396)
            .await;

        for url in [
            "/Moonfin/MdbList/Ratings?type=movie&tmdbId=999999",
            // Right id, wrong type.
            "/Moonfin/MdbList/Ratings?type=show&tmdbId=603",
            "/Moonfin/MdbList/Ratings?type=movie&tmdbId=1396",
            "/Moonfin/MdbList/Ratings?type=movie&tmdbId=99999999999999999999",
        ] {
            let resp = h
                .get(url)
                .await;
            if url.ends_with("99999999999999999999") {
                resp.assert_status(StatusCode::BAD_REQUEST);
                continue;
            }
            let body = resp.json::<Value>();
            assert_eq!(body["success"], false, "{url}");
            assert_eq!(body["ratings"], json!([]), "{url}");
        }
        m.assert_hits(0);
        // In-library ids of the matching type proceed.
        assert_eq!(
            h.json(URL_603)
                .await["success"],
            true
        );
        assert_eq!(
            h.json("/Moonfin/MdbList/Ratings?type=show&tmdbId=1396")
                .await["success"],
            true
        );
        m.assert_hits(2);
    }

    #[tokio::test]
    async fn own_key_lookup_is_not_limited_to_the_library() {
        let mock = MockServer::start();
        let m = mock_ok(&mock, "/tmdb/movie/424242", "fake-user-key");
        let h = harness(Some(KEY), mock.base_url()).await;
        h.push(json!({ "mdblistApiKey": "fake-user-key" }))
            .await;
        let body = h
            .json("/Moonfin/MdbList/Ratings?type=movie&tmdbId=424242")
            .await;
        assert_eq!(body["success"], true);
        m.assert_hits(1);
    }

    #[tokio::test]
    async fn api_keys_never_appear_in_internal_state() {
        let mock = MockServer::start();
        let _m = mock.mock(|when, then| {
            when.method(GET)
                .path("/tmdb/movie/1");
            then.status(401);
        });
        let h = harness(Some(KEY), mock.base_url()).await;
        h.seed_movies(&[1])
            .await;
        let second = h
            .second_user()
            .await;
        h.push_as(&second, json!({ "mdblistApiKey": OTHER }))
            .await;
        for token in [
            h.token
                .clone(),
            second,
        ] {
            let text = h
                .get_as(&token, URL_1)
                .await
                .text();
            assert!(!text.contains(KEY) && !text.contains(OTHER));
        }
        let svc = &h
            ._guard
            .0
            .mdblist
            .inner;
        let mut dump = format!("{:?}", KeyId::of(KEY));
        for (k, v) in svc
            .key_failures
            .iter()
        {
            dump += &format!("{k:?} {v}");
        }
        for (k, v) in svc
            .title_failures
            .iter()
        {
            dump += &format!("{k:?} {v}");
        }
        for e in svc
            .rate_limited
            .iter()
        {
            dump += &format!("{:?}", e.key());
        }
        assert!(!dump.contains(KEY) && !dump.contains(OTHER), "{dump}");
        assert!(
            svc.key_failures
                .iter()
                .count()
                > 0
        );
    }

    #[test]
    fn rate_limit_gate_map_is_bounded_and_pruned() {
        let svc = MdbListService::default();
        for i in 0..(MAX_RATE_LIMITED_KEYS as u64 + 50) {
            svc.gate(KeyId(i), Duration::from_secs(60 + i));
        }
        assert!(
            svc.inner
                .rate_limited
                .len()
                <= MAX_RATE_LIMITED_KEYS
        );
        svc.gate(KeyId(u64::MAX), Duration::from_millis(1));
        std::thread::sleep(Duration::from_millis(5));
        svc.gate(KeyId(u64::MAX - 1), Duration::from_secs(60));
        assert!(
            !svc.inner
                .rate_limited
                .contains_key(&KeyId(u64::MAX))
        );
    }

    #[test]
    fn config_never_prints_the_key() {
        let config = Config {
            mdblist_api_key: Some(Secret::new(KEY)),
            ..Default::default()
        };
        assert!(
            !serde_json::to_string_pretty(&config)
                .unwrap()
                .contains(KEY)
        );
        assert!(!format!("{config:?}").contains(KEY));
    }

    #[test]
    fn filter_falls_back_to_defaults_and_skips_missing_sources() {
        let all = parse_upstream(&upstream_body());
        let out = filter_and_order(&all, Some(&["trakt".into(), "metacritic".into()]));
        assert_eq!(out.len(), 1);
        assert_eq!(
            out[0]
                .source
                .as_deref(),
            Some("metacritic")
        );
        assert_eq!(filter_and_order(&all, Some(&[])).len(), 4);
    }
}
