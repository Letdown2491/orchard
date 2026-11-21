//! # orchard-indexd
//!
//! The heart of Orchard's distribution model: a small HTTP service on
//! `127.0.0.1` that the client runs in-process. It is the future home of
//! every protocol surface Orchard needs between "the catalog" and "flatpak",
//! so that the user's flatpak can one day talk to it as an ordinary remote
//! (`oci+http://127.0.0.1:7780`) and GNOME Software, Discover, and
//! `flatpak update` all keep working.
//!
//! ## Endpoints
//!
//! - `GET /v1/catalog` - the full catalog document ([`CatalogFile`]). The
//!   client reads this over HTTP rather than opening catalog files directly,
//!   because indexd is the seam a remote catalog (Nostr, M3) plugs into
//!   without touching client code.
//! - `GET /index/<arch>` - an OCI image index enumerating the catalog's
//!   artifacts. Kept from the first spike, with no production consumer
//!   left: flatpak's `oci+` remotes read `/index/static` and the `/v2`
//!   proxy below (validated against flatpak 1.18.4), and the client's
//!   direct path installs from its own OCI layouts. Kept for
//!   inspection and debugging of the catalog's OCI view.
//! - `GET /` - service description.

use std::sync::{Arc, Mutex};

pub mod web;

use axum::extract::{Path as AxumPath, Query as AxumQuery, State};
use axum::http::header;
use axum::response::{Html, IntoResponse};
use axum::routing::{get, post};
use axum::{Json, Router};

use orchard_core::model::{CatalogApp, CatalogFile};

/// Annotation key Flatpak uses to map an OCI manifest to a flatpak ref.
pub const ANNOTATION_FLATPAK_REF: &str = "org.flatpak.ref";

const INDEX_MEDIA_TYPE: &str = "application/vnd.oci.image.index.v1+json";
const MANIFEST_MEDIA_TYPE: &str = "application/vnd.oci.image.manifest.v1+json";

/// The document served at `GET /index/<arch>`: an OCI image index whose
/// entries are the Flatpak apps this catalog serves.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct OciIndex {
    #[serde(rename = "schemaVersion")]
    pub schema_version: u32,
    #[serde(rename = "mediaType")]
    pub media_type: String,
    pub manifests: Vec<IndexEntry>,
}

/// One entry in the index: a pointer to an image manifest plus Flatpak annotations.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct IndexEntry {
    #[serde(rename = "mediaType")]
    pub media_type: String,
    pub digest: String,
    pub size: u64,
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub annotations: std::collections::BTreeMap<String, String>,
}

impl IndexEntry {
    /// Build an index entry for a flatpak ref, e.g.
    /// `app/com.example.Hello/x86_64/stable`.
    pub fn for_ref(flatpak_ref: &str, digest: String, size: u64) -> Self {
        let mut annotations = std::collections::BTreeMap::new();
        annotations.insert(
            ANNOTATION_FLATPAK_REF.to_string(),
            flatpak_ref.to_string(),
        );
        Self {
            media_type: MANIFEST_MEDIA_TYPE.to_string(),
            digest,
            size,
            annotations,
        }
    }

    pub fn flatpak_ref(&self) -> Option<&str> {
        self.annotations.get(ANNOTATION_FLATPAK_REF).map(String::as_str)
    }
}

/// Where the served catalog document comes from. In the MVP this is the
/// static catalog file; the Nostr feed wraps it in [`NostrCatalog`].
pub trait CatalogDocument: Send + Sync + 'static {
    /// The full catalog document to serve.
    fn document(&self) -> CatalogFile;

    /// The reviews document to serve. Static files carry none.
    fn reviews(&self) -> orchard_core::ReviewsFile {
        orchard_core::ReviewsFile {
            version: 1,
            reviews: Vec::new(),
            first_seen: Default::default(),
        }
    }

    /// The raw kind 0 metadata per pubkey. Static files carry none.
    fn profiles(&self) -> orchard_core::ProfilesFile {
        orchard_core::ProfilesFile {
            version: 1,
            profiles: std::collections::BTreeMap::new(),
        }
    }

    /// The reviewer contact-list graph. Static files carry none.
    fn graph(&self) -> orchard_core::GraphFile {
        orchard_core::GraphFile {
            version: 1,
            graph: Default::default(),
        }
    }

    /// Drop absorbed events: the client's "clear cached data" action.
    /// Static documents have nothing to clear.
    fn clear(&self) {}
}

impl CatalogDocument for CatalogFile {
    fn document(&self) -> CatalogFile {
        self.clone()
    }
}

/// The base document plus a live Nostr feed. Feed rows win on app-id
/// collisions: an app a publisher actually signed is the authoritative row,
/// the file entry is the bootstrap fallback.
pub struct NostrCatalog {
    pub base: CatalogFile,
    pub feed: Arc<Mutex<orchard_nostr::Feed>>,
    /// The crawled reviewer contact-list graph, refreshed by the
    /// crawler thread.
    pub graph: Arc<Mutex<GraphStore>>,
}

impl CatalogDocument for NostrCatalog {
    fn document(&self) -> CatalogFile {
        let mut apps = self
            .feed
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .catalog()
            .apps;
        for base_app in &self.base.apps {
            if !apps.iter().any(|app| app.app_id == base_app.app_id) {
                apps.push(base_app.clone());
            }
        }
        apps.sort_by(|a, b| {
            a.app_id
                .cmp(&b.app_id)
                .then_with(|| a.publisher.pubkey.cmp(&b.publisher.pubkey))
        });
        CatalogFile { version: 1, apps }
    }

    fn reviews(&self) -> orchard_core::ReviewsFile {
        self.feed
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .reviews()
    }

    fn profiles(&self) -> orchard_core::ProfilesFile {
        orchard_core::ProfilesFile {
            version: 1,
            profiles: self
                .feed
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .profile_raws(),
        }
    }

    fn graph(&self) -> orchard_core::GraphFile {
        self.graph
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .file()
    }

    fn clear(&self) {
        self.feed
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clear();
    }
}

/// Shared server state: the catalog document (and the OCI index derived
/// from it per request).
#[derive(Clone)]
pub struct ServerState {
    pub document: Arc<dyn CatalogDocument>,
    /// Shared with the relay supervisor: the clear-cache endpoint sets
    /// it, the supervisor answers by cycling every relay thread so
    /// each re-subscribes and re-delivers its events into the emptied
    /// feed. None for a static document (no relays to nudge).
    pub relays_reset: Option<Arc<std::sync::atomic::AtomicBool>>,
    /// Per-relay health, written by the relay threads and served by
    /// /v1/relays (issue #12: silent relay failures become visible
    /// ones). None for a static document.
    pub relays_health: Option<orchard_nostr::relay::RelayHealthMap>,
    /// The externally-visible base URL of this service: what the
    /// /index/static response names as its Registry, so flatpak's
    /// manifest and blob fetches come back here (the proxy). Defaults
    /// to the bind address; a hosted deployment sets the public URL
    /// (--public-url / ORCHARD_PUBLIC_URL) when TLS terminates at a
    /// reverse proxy.
    pub public_url: Option<String>,
}

/// Relay and cache configuration for [`serve`].
#[derive(Debug, Clone, Default)]
pub struct RelayConfig {
    /// NIP-01 relay URLs to subscribe to, e.g. `ws://127.0.0.1:8008`.
    pub urls: Vec<String>,
    /// Social relay URLs for the profile crawler: kind 0 lives on the
    /// social relays, not here. Empty: no profile crawling.
    pub social: Vec<String>,
    /// Where to persist verified events across restarts. Absent: no cache.
    pub cache: Option<std::path::PathBuf>,
}

/// The two live relay sets, resolved from settings: environment beats
/// the file beats the rendezvous default.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RelaySets {
    pub events: Vec<String>,
    pub social: Vec<String>,
}

/// Resolve one relay set: a non-blank environment value wins (the dev
/// loop's escape hatch, left unpinned on purpose), then the settings
/// file's list with the required relay pinned in (empty means default,
/// which already leads with it).
pub fn resolve_set(file: &[String], env: Option<&str>, default: &[String]) -> Vec<String> {
    if let Some(value) = env
        && !value.trim().is_empty()
    {
        return orchard_nostr::relay::parse_urls(value);
    }
    if !file.is_empty() {
        orchard_nostr::relay::with_required_relay(file.to_vec())
    } else {
        default.to_vec()
    }
}

/// The effective relay sets right now: env overrides, then settings,
/// then the rendezvous defaults.
pub fn relay_sets(settings: &orchard_core::SettingsHandle) -> RelaySets {
    let settings = settings.get();
    RelaySets {
        events: resolve_set(
            &settings.event_relays,
            std::env::var("ORCHARD_RELAYS").ok().as_deref(),
            &orchard_nostr::relay::relay_urls(None),
        ),
        social: resolve_set(
            &settings.social_relays,
            std::env::var("ORCHARD_SOCIAL_RELAYS").ok().as_deref(),
            &orchard_nostr::relay::relay_urls(None),
        ),
    }
}

/// The crawled reviewer graph plus per-reviewer crawl times. The
/// crawler thread refreshes entries on a fixed cadence; the endpoint
/// serves whatever is here.
#[derive(Default)]
pub struct GraphStore {
    graph: std::collections::BTreeMap<String, Vec<String>>,
    crawled: std::collections::BTreeMap<String, std::time::Instant>,
}

/// How often the crawler wakes to look for new reviewers.
const GRAPH_TICK: std::time::Duration = std::time::Duration::from_secs(60);
/// How stale a crawled contact list may get before it is refetched.
const GRAPH_REFRESH: std::time::Duration = std::time::Duration::from_secs(600);
/// The one-shot REQ deadline per relay.
const GRAPH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// How often the profile crawler wakes to look for new keys.
const PROFILE_TICK: std::time::Duration = std::time::Duration::from_secs(60);
/// How stale a crawled profile may get before it is refetched.
const PROFILE_REFRESH: std::time::Duration = std::time::Duration::from_secs(600);
/// The one-shot REQ deadline per relay.
const PROFILE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

impl GraphStore {
    /// Reviewers to crawl now: in the reviewer set and either never
    /// crawled or last crawled longer ago than the refresh cadence.
    fn pending(&self, reviewers: &[String], cadence: std::time::Duration) -> Vec<String> {
        reviewers
            .iter()
            .filter(|pubkey| {
                self.crawled
                    .get(*pubkey)
                    .is_none_or(|at| at.elapsed() >= cadence)
            })
            .cloned()
            .collect()
    }

    /// Absorb one relay's answer for a batch. Requested authors with
    /// no event on the relay stamp anyway: absence is an answer (no
    /// contact list there), so the crawl does not re-ask every tick.
    fn absorb(&mut self, found: Vec<(String, Vec<String>)>, requested: &[String]) {
        let now = std::time::Instant::now();
        for (pubkey, follows) in found {
            self.graph.insert(pubkey.clone(), follows);
            self.crawled.insert(pubkey, now);
        }
        for pubkey in requested {
            self.crawled.entry(pubkey.clone()).or_insert(now);
        }
    }

    /// The served document: a copy of the graph.
    fn file(&self) -> orchard_core::GraphFile {
        orchard_core::GraphFile {
            version: 1,
            graph: self.graph.clone(),
        }
    }
}

/// Crawl the contact lists of known reviewers: the reviewers come
/// from the feed's review authors, the kind 3s from one batched
/// read-only REQ per relay. Blocking loop; one thread. The relay set
/// re-resolves every tick, so relay settings apply live.
fn spawn_graph_crawler(
    settings: orchard_core::SettingsHandle,
    feed: Arc<Mutex<orchard_nostr::Feed>>,
    graph: Arc<Mutex<GraphStore>>,
) {
    std::thread::Builder::new()
        .name("orchard-graph".into())
        .spawn(move || loop {
            std::thread::sleep(GRAPH_TICK);
            guarded("graph", || {
                let urls = relay_sets(&settings).events;
                let reviewers: Vec<String> = {
                    let feed =
                        feed.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
                    let mut seen: Vec<String> = Vec::new();
                    for review in feed.reviews().reviews {
                        if !seen.contains(&review.pubkey) {
                            seen.push(review.pubkey);
                        }
                    }
                    seen
                };
                if reviewers.is_empty() {
                    return;
                }
                let pending = graph
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .pending(&reviewers, GRAPH_REFRESH);
                if pending.is_empty() {
                    return;
                }
                for url in &urls {
                    match orchard_nostr::relay::fetch_contact_lists(url, &pending, GRAPH_TIMEOUT) {
                        Ok(found) => graph
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner())
                            .absorb(found, &pending),
                        Err(err) => eprintln!("graph: {url}: {err:#}"),
                    }
                }
            })
        })
        .expect("spawning the graph crawler");
}

/// The profile crawler: kind 0 profiles ride the social relays, not
/// the app relays, so a store fed only from its app relay (the dev
/// loop, or a private relay set) never sees reviewers' names without
/// this. Every tick the crawler asks each social relay, one batched
/// REQ per relay, for the keys the store cares about and absorbs the
/// events through the feed's verify path; the feed keeps the newest
/// per key. Keys are marked before the fetch, so a dead relay backs
/// off to the refresh window instead of retrying every tick. The
/// relay set re-resolves every tick, so relay settings apply live.
/// Anonymous and read-only, like every fetch here.
fn spawn_profile_crawler(settings: orchard_core::SettingsHandle, feed: Arc<Mutex<orchard_nostr::Feed>>) {
    std::thread::Builder::new()
        .name("orchard-profiles".into())
        .spawn(move || {
            let mut crawled: std::collections::BTreeMap<String, std::time::Instant> =
                std::collections::BTreeMap::new();
            loop {
                let now = std::time::Instant::now();
                guarded("profiles", || {
                    let urls = relay_sets(&settings).social;
                    let pending: Vec<String> = {
                        let feed = feed
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner());
                        feed.crawl_keys()
                    }
                    .into_iter()
                    .filter(|key| {
                        crawled
                            .get(key)
                            .is_none_or(|at| now.duration_since(*at) >= PROFILE_REFRESH)
                    })
                    .collect();
                    if !pending.is_empty() {
                        for key in &pending {
                            crawled.insert(key.clone(), now);
                        }
                        for url in &urls {
                            match orchard_nostr::relay::fetch_profiles(url, &pending, PROFILE_TIMEOUT)
                            {
                                Ok(events) => {
                                    let mut feed = feed
                                        .lock()
                                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                                    for event in events {
                                        let _ = feed.absorb(event);
                                    }
                                }
                                Err(err) => eprintln!("profiles: {url}: {err:#}"),
                            }
                        }
                    }
                });
                std::thread::sleep(PROFILE_TICK);
            }
        })
        .expect("spawning the profile crawler");
}

/// One tick of a background loop, guarded: a panic in any of the
/// crawler, writer, or supervisor bodies costs a log line, not the
/// thread. Background threads have no UI to surface a death, and a
/// thread gone missing is silent breakage; the panic hook still
/// prints, and the loop's next tick carries on.
fn guarded<F: FnOnce()>(name: &str, body: F) {
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(body));
    if let Err(panic) = result {
        let message = panic
            .downcast_ref::<&str>()
            .map(|s| s.to_string())
            .or_else(|| panic.downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "unknown panic".to_string());
        eprintln!("{name}: tick panicked: {message}; the loop continues");
    }
}

/// Keep one relay thread running per URL in the settings' event set,
/// spawning and stopping as the set changes. One thread, polling the
/// settings once a second: a removed relay's thread is signalled to
/// stop (its read wakes on a timeout) and a new relay's thread starts
/// on the next pass. The feed deduplicates, so nothing is re-read
/// that a kept relay already delivered.
///
/// Returns the per-URL health map: each relay thread stamps its
/// entry, the /v1/relays endpoint serves it (issue #12).
fn spawn_relay_supervisor(
    settings: orchard_core::SettingsHandle,
    feed: Arc<Mutex<orchard_nostr::Feed>>,
    reset: Arc<std::sync::atomic::AtomicBool>,
) -> orchard_nostr::relay::RelayHealthMap {
    let health: orchard_nostr::relay::RelayHealthMap = Default::default();
    let health_threads = Arc::clone(&health);
    std::thread::Builder::new()
        .name("orchard-relays".into())
        .spawn(move || {
            let mut running: std::collections::BTreeMap<
                String,
                Arc<std::sync::atomic::AtomicBool>,
            > = Default::default();
            loop {
                guarded("relays", || {
                    // a clear cycle: every thread goes, fresh ones
                    // re-subscribe and re-deliver into the emptied
                    // feed. Logged: the cycle while a relay is down
                    // was the suspected actor in one unexplained
                    // client death, and a recurrence must be visible.
                    if reset.swap(false, std::sync::atomic::Ordering::Relaxed) {
                        eprintln!(
                            "relays: cache clear, cycling {} threads",
                            running.len()
                        );
                        for stop in running.values() {
                            stop.store(true, std::sync::atomic::Ordering::Relaxed);
                        }
                        running.clear();
                    }
                    let urls = relay_sets(&settings).events;
                running.retain(|url, stop| {
                    if urls.contains(url) {
                        true
                    } else {
                        stop.store(true, std::sync::atomic::Ordering::Relaxed);
                        // the thread is gone; its health row goes with
                        // it, so the endpoint never serves a dead URL
                        health_threads.lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner())
                            .remove(url);
                        false
                    }
                });
                for url in urls {
                    if running.contains_key(&url) {
                        continue;
                    }
                    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
                    let relay_url = url.clone();
                    let feed = Arc::clone(&feed);
                    let thread_stop = Arc::clone(&stop);
                    let relay_health = health_threads
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .entry(url.clone())
                        .or_default()
                        .clone();
                    let spawned = std::thread::Builder::new()
                        .name(format!("relay-{relay_url}"))
                        .spawn(move || {
                            orchard_nostr::relay::run_forever(
                                &relay_url,
                                feed,
                                Arc::new(|| {}),
                                thread_stop,
                                relay_health,
                            )
                        });
                    match spawned {
                        Ok(_) => {
                            running.insert(url, stop);
                        }
                        Err(_) => eprintln!("failed to spawn relay thread for {url}"),
                    }
                }
                });
                std::thread::sleep(std::time::Duration::from_secs(1));
            }
        })
        .expect("spawning the relay supervisor");
    health
}

/// Build the router for a state.
pub fn router(state: ServerState) -> Router {
    Router::new()
        .route("/", get(root))
        .route("/index/{arch}", get(index))
        .route("/v1/catalog", get(catalog))
        .route("/v1/reviews", get(reviews))
        .route("/v1/profiles", get(profiles))
        .route("/v1/graph", get(graph))
        .route("/v1/relays", get(relays))
        .route("/index/static", get(index_static))
        .route("/v2/", get(v2_ping))
        .route("/v2/{*rest}", get(v2_proxy))
        .route("/apps", get(web_list))
        .route("/apps/{app_id}", get(web_detail))
        .route("/v1/cache", post(clear_cache))
        .with_state(state)
}

async fn root() -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "service": "orchard-indexd",
        "description": "Orchard's local catalog service",
        "endpoints": ["/v1/catalog", "/v1/reviews", "/v1/profiles", "/v1/graph", "/v1/relays", "/index/static", "/index/{arch}", "/v2/", "/apps"]
    }))
}

async fn index(
    State(state): State<ServerState>,
    AxumPath(arch): AxumPath<String>,
) -> Json<OciIndex> {
    // derive the entries from the live document so relay-published apps
    // show up without restarting
    let needle = format!("/{arch}/");
    let manifests = state
        .document
        .document()
        .apps
        .iter()
        .filter(|app| app.flatpak_ref.contains(&needle))
        .map(|app| {
            IndexEntry::for_ref(
                &app.flatpak_ref,
                app.digest.clone().unwrap_or_else(|| "sha256:unknown".into()),
                0,
            )
        })
        .collect();
    Json(OciIndex {
        schema_version: 2,
        media_type: INDEX_MEDIA_TYPE.to_string(),
        manifests,
    })
}

async fn catalog(State(state): State<ServerState>) -> Json<CatalogFile> {
    Json(state.document.document())
}

/// The web page response: ETag revalidation on top of `no-cache`, so a
/// visitor never renders a stale page and an unchanged page costs the
/// server a 304.
fn web_response(
    if_none_match: Option<&axum::http::HeaderValue>,
    body: String,
) -> axum::response::Response {
    let tag = web::etag(&body);
    let matches = if_none_match
        .and_then(|value| value.to_str().ok())
        .map(|value| {
            value
                .split(',')
                .map(|candidate| candidate.trim().trim_start_matches("W/"))
                .any(|candidate| candidate == tag || candidate == "*")
        })
        .unwrap_or(false);
    if matches {
        return (
            axum::http::StatusCode::NOT_MODIFIED,
            [
                (header::CACHE_CONTROL, web::cache_header()),
                (header::ETAG, tag.as_str()),
            ],
        )
            .into_response();
    }
    (
        [
            (header::CACHE_CONTROL, web::cache_header()),
            (header::ETAG, tag.as_str()),
        ],
        Html(body),
    )
        .into_response()
}

/// The web-browsable catalog (issue #13): the list at /apps. The sort
/// comes from ?sort= (updated by default, name as the alternative),
/// rendered server-side with a toggle link.
async fn web_list(
    State(state): State<ServerState>,
    AxumQuery(query): AxumQuery<std::collections::HashMap<String, String>>,
    headers: axum::http::HeaderMap,
) -> impl IntoResponse {
    let sort = web::Sort::from_query(query.get("sort").map(String::as_str));
    let page = query
        .get("page")
        .and_then(|value| value.parse::<usize>().ok())
        .map(|value| value.max(1))
        .unwrap_or(1);
    let document = state.document.document();
    let body = web::list_page(&document.apps, sort, page);
    web_response(headers.get(header::IF_NONE_MATCH), body)
}

/// The detail page at /apps/{app_id}; 404 when the id is not in the
/// catalog.
async fn web_detail(
    State(state): State<ServerState>,
    AxumPath(app_id): AxumPath<String>,
    headers: axum::http::HeaderMap,
) -> axum::response::Response {
    let document = state.document.document();
    // one row per release; the newest is the page, the rest are the
    // versions list
    let mut rows: Vec<CatalogApp> = document
        .apps
        .iter()
        .filter(|app| app.app_id == app_id)
        .cloned()
        .collect();
    let Some(_newest) = rows.first() else {
        return axum::http::StatusCode::NOT_FOUND.into_response();
    };
    rows.sort_by(|a, b| b.published_at.cmp(&a.published_at));
    // reviews for this app, newest first (the page renders the newest
    // few in full and counts the rest)
    let mut reviews: Vec<orchard_core::model::Review> = state
        .document
        .reviews()
        .reviews
        .iter()
        .filter(|review| review.app_id == app_id)
        .cloned()
        .collect();
    reviews.sort_by(|a, b| b.created_at.cmp(&a.created_at));
    let base = state
        .public_url
        .clone()
        .unwrap_or_else(|| "http://127.0.0.1:7780".to_string());
    let body = web::detail_page(&base, &rows[0], &rows[1..], &reviews);
    web_response(headers.get(header::IF_NONE_MATCH), body)
}

/// The reviews document, straight from the live feed.
async fn reviews(State(state): State<ServerState>) -> Json<orchard_core::ReviewsFile> {
    Json(state.document.reviews())
}

/// The raw kind 0 metadata per pubkey, straight from the live feed.
async fn profiles(State(state): State<ServerState>) -> Json<orchard_core::ProfilesFile> {
    Json(state.document.profiles())
}

/// The reviewer contact-list graph, straight from the crawler.
async fn graph(State(state): State<ServerState>) -> Json<orchard_core::GraphFile> {
    Json(state.document.graph())
}

/// Per-relay health (issue #12): what the relay threads stamp. The
/// frame age is computed here, at read time, so the client needs no
/// clock of its own and a poll every few seconds is the whole story.
/// Rows appear and disappear with the supervisor's relay set.
async fn relays(State(state): State<ServerState>) -> Json<serde_json::Value> {
    let now = orchard_nostr::unix_millis();
    let rows: Vec<serde_json::Value> = match &state.relays_health {
        Some(map) => map
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .iter()
            .map(|(url, health)| {
                let health = health
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                serde_json::json!({
                    "url": url,
                    "connected": health.connected,
                    "last_frame_at": health.last_frame_at,
                    "last_frame_age_secs": health
                        .last_frame_at
                        .map(|at| now.saturating_sub(at) / 1000),
                    "last_error": health.last_error,
                    "failures": health.failures,
                })
            })
            .collect(),
        None => vec![],
    };
    Json(serde_json::json!({ "relays": rows }))
}

// ---------------------------------------------------------------------------
// The flatpak remote surface (issue #7): `oci+http(s)` remotes fetch an
// OCI index at `{base}/index/static` and synthesize the ostree summary
// client-side, then fetch manifests and blobs over the OCI distribution
// API at the index's Registry URI. Ground truth: flatpak 1.18.4
// `common/flatpak-oci-registry.c` (flatpak_oci_index_make_summary,
// flatpak_oci_index_ensure_cached) and `common/flatpak-json-oci.c` (the
// JSON member names are the literal CamelCase below).
//
// indexd is the blob path: flatpak's OCI fetch never authenticates (the
// M5 lesson: not even a token attempt on a 401), and publisher
// registries like ghcr require the anonymous bearer dance. So Registry
// points at indexd itself, and indexd proxies /v2/ by digest against
// the URLs the verified catalog already carries. A proxy cannot lie:
// flatpak checks the manifest digest against the summary ref entry and
// every blob against the manifest's own digests.
// ---------------------------------------------------------------------------

/// The `{base}/index/static` response. Member names are flatpak's
/// (ground truth: `flatpak-json-oci.c` demarshals these literals), not
/// the OCI spec's.
#[derive(Debug, serde::Serialize)]
pub struct FlatpakOciIndexResponse {
    /// The registry that serves manifests and blobs: resolved relative
    /// to the index URI, so an absolute URL wins (the operator's
    /// public base; indexd's own proxy endpoints live there).
    #[serde(rename = "Registry")]
    registry: String,
    #[serde(rename = "Results")]
    results: Vec<FlatpakOciIndexRepository>,
}

#[derive(Debug, serde::Serialize)]
pub struct FlatpakOciIndexRepository {
    #[serde(rename = "Name")]
    name: String,
    #[serde(rename = "Images")]
    images: Vec<FlatpakOciIndexImage>,
}

#[derive(Debug, serde::Serialize)]
pub struct FlatpakOciIndexImage {
    #[serde(rename = "Digest")]
    digest: String,
    #[serde(rename = "MediaType")]
    mediatype: String,
    #[serde(rename = "OS")]
    os: String,
    #[serde(rename = "Architecture")]
    architecture: String,
    #[serde(rename = "Annotations")]
    annotations: std::collections::BTreeMap<String, String>,
    #[serde(rename = "Labels", skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    labels: std::collections::BTreeMap<String, String>,
    #[serde(rename = "Tags")]
    tags: Vec<String>,
}

/// flatpak's `flatpak_arch_to_oci_arch`, backwards: the catalog's
/// platform tags use flatpak arches, the index/static query filters by
/// OCI arch.
fn oci_arch_to_flatpak_arch(oci: &str) -> Option<&'static str> {
    match oci {
        "amd64" => Some("x86_64"),
        "arm64" => Some("aarch64"),
        "x86_64" => Some("x86_64"),
        "aarch64" => Some("aarch64"),
        _ => None,
    }
}

/// The registry host and repository path of an artifact `location`
/// (`http(s)://host/v2/<name>/manifests/<digest>`). None for the other
/// location shapes (http manifest URLs, local layouts), which have no
/// registry to route through.
fn parse_registry_location(location: &str) -> Option<(String, String)> {
    let rest = location
        .strip_prefix("http://")
        .or_else(|| location.strip_prefix("https://"))?;
    // authority up to the first slash
    let (host, path) = rest.split_once('/')?;
    let name = path.strip_prefix("v2/")?.split("/manifests/").next()?.to_string();
    if name.is_empty() {
        return None;
    }
    Some((host.to_string(), name))
}

/// GET /index/static: the live catalog as flatpak's OCI index, one
/// image per (app, platform). Regenerated per request, so relay events
/// appear without a restart; the ETag is the body's own digest.
async fn index_static(
    State(state): State<ServerState>,
    AxumQuery(query): AxumQuery<std::collections::HashMap<String, String>>,
) -> impl IntoResponse {
    let want_arch = query
        .get("architecture")
        .and_then(|a| oci_arch_to_flatpak_arch(a).map(|s| s.to_string()));
    let document = state.document.document();

    // Group one image per (repository, platform). Rows whose location
    // is not a registry reference have no flatpak-remote story (the
    // client installs them directly); they simply do not appear here.
    let mut images: std::collections::BTreeMap<String, Vec<FlatpakOciIndexImage>> =
        Default::default();
    for app in &document.apps {
        let digest = match &app.digest {
            Some(digest) => digest.clone(),
            None => continue,
        };
        let Some((_host, repository)) = parse_registry_location(&app.location) else {
            continue;
        };
        for platform in &app.platforms {
            // platform tags are `linux-<arch>`; the index/static query
            // asks for a specific OCI arch when flatpak sends one
            let Some(arch) = platform.strip_prefix("linux-") else {
                continue;
            };
            if let Some(want) = &want_arch
                && arch != want
            {
                continue;
            }
            // flatpak reads all per-image metadata from Labels, not
            // Annotations (ground truth: get_image_metadata only
            // consults img->labels): the ref makes the row installable,
            // the metadata ini is the flatpak metadata it deploys, and
            // a row whose labels lack the metadata key is skipped as
            // "not a flatpak" by the summary builder
            let mut labels = std::collections::BTreeMap::new();
            labels.insert(
                crate::ANNOTATION_FLATPAK_REF.to_string(),
                app.flatpak_ref.clone(),
            );
            labels.insert(
                "org.flatpak.metadata".to_string(),
                app.metadata.clone().unwrap_or_default(),
            );
            images.entry(repository.clone()).or_default().push(
                FlatpakOciIndexImage {
                    digest: digest.clone(),
                    mediatype: MANIFEST_MEDIA_TYPE.to_string(),
                    os: "linux".to_string(),
                    architecture: arch.to_string(),
                    annotations: Default::default(),
                    labels,
                    tags: vec![],
                },
            );
        }
    }

    let body = FlatpakOciIndexResponse {
        registry: state
            .public_url
            .as_deref()
            .unwrap_or_default()
            .trim_end_matches('/')
            .to_string(),
        results: images
            .into_iter()
            .map(|(name, mut images)| {
                // the summary builder sorts by ref (the ref lives in
                // Labels; flatpak reads all per-image metadata there)
                images.sort_by(|a, b| {
                    a.labels
                        .get(crate::ANNOTATION_FLATPAK_REF)
                        .cmp(&b.labels.get(crate::ANNOTATION_FLATPAK_REF))
                });
                FlatpakOciIndexRepository { name, images }
            })
            .collect(),
    };
    let json = serde_json::to_vec(&body).expect("serializing the OCI index");
    use sha2::Digest as _;
    let etag = format!("\"{:x}\"", sha2::Sha256::digest(&json));
    (
        [(axum::http::header::ETAG, etag)],
        [(axum::http::header::CONTENT_TYPE, "application/json")],
        json,
    )
}

/// GET /v2/: the distribution API ping some clients make first.
async fn v2_ping() -> &'static str {
    "{}"
}

/// A proxied fetch that failed, already shaped for the client: the
/// status the client should see (the upstream registry's own answer
/// where one exists) and a one-line reason in the body. An empty 502
/// teaches a client nothing; a machine consumer pulling on a schedule
/// needs the difference between "the registry says 404" and "the proxy
/// failed", and a rate-limited one needs the upstream Retry-After.
struct ProxyFailure {
    status: axum::http::StatusCode,
    reason: String,
    retry_after: Option<axum::http::HeaderValue>,
}

impl ProxyFailure {
    fn into_response(self) -> axum::response::Response {
        let mut response = (
            self.status,
            [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
            self.reason,
        )
            .into_response();
        if let Some(retry_after) = self.retry_after {
            response
                .headers_mut()
                .insert(header::RETRY_AFTER, retry_after);
        }
        response
    }
}

/// Relay an upstream registry's own non-success status to the client:
/// a missing ref is the registry's 404, a rate limit its 429. Inventing
/// a 502 here would misattribute the failure to this proxy.
fn relay_upstream_status(
    head_tx: tokio::sync::oneshot::Sender<Result<String, ProxyFailure>>,
    code: u16,
    response: ureq::Response,
) {
    let retry_after = response
        .header("retry-after")
        .and_then(|value| axum::http::HeaderValue::from_str(value).ok());
    let failure = ProxyFailure {
        status: axum::http::StatusCode::from_u16(code)
            .unwrap_or(axum::http::StatusCode::BAD_GATEWAY),
        reason: format!("upstream registry returned {code} for this ref"),
        retry_after,
    };
    let _ = head_tx.send(Err(failure));
}

/// GET /v2/{rest}: proxy the OCI distribution API against the URLs the
/// verified catalog carries. Manifests resolve against a row's
/// `location`, blobs against its `oci_assets` entries. Everything is
/// digest-addressed, so a row matches by exact URL tail and the bytes
/// relayed are the bytes the publisher signed. Failures propagate:
/// the upstream registry's own status reaches the client as itself
/// (with its Retry-After and a reason body), transport and token-dance
/// failures are a 502 that says which.
async fn v2_proxy(
    State(state): State<ServerState>,
    AxumPath(rest): AxumPath<String>,
) -> axum::response::Response {
    let needle = format!("/v2/{rest}");
    let document = state.document.document();
    let mut upstream: Option<String> = None;
    'rows: for app in &document.apps {
        // hardening: only a publisher digest-pinned row is proxied, and
        // only from a URL this process may fetch (https, or http on
        // loopback for the dev registry): a hosted indexd (0.0.0.0,
        // third-party publishers) must not become an intranet pivot
        // for a publisher-nominated URL
        if app.digest.is_none() {
            continue;
        }
        if app.location.ends_with(&needle) {
            if orchard_core::oci::fetchable_url(&app.location) {
                upstream = Some(app.location.clone());
            }
            continue;
        }
        let Some(assets) = &app.oci_assets else {
            continue;
        };
        for asset in assets {
            if asset.url.ends_with(&needle) {
                if orchard_core::oci::fetchable_url(&asset.url) {
                    upstream = Some(asset.url.clone());
                }
                break 'rows;
            }
        }
    }
    let Some(url) = upstream else {
        return axum::http::StatusCode::NOT_FOUND.into_response();
    };
    let repository = rest
        .trim_start_matches("/v2/")
        .split("/manifests/")
        .next()
        .unwrap_or(&rest)
        .to_string();

    // The bearer dance: publisher registries (ghcr included) answer an
    // anonymous pull with 401 + WWW-Authenticate; flatpak would give
    // up here (the M5 lesson: it never even tries a token), indexd
    // does the dance and relays the bytes. The WHOLE fetch runs on the
    // relay thread, not just the body: the GUI runs this service on a
    // current-thread tokio runtime, and a blocking connect or token
    // round-trip here would stall the catalog and health endpoints for
    // its duration. The response head crosses back through a oneshot,
    // the body through a bounded channel.
    let (head_tx, head_rx) = tokio::sync::oneshot::channel::<Result<String, ProxyFailure>>();
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<bytes::Bytes, std::io::Error>>(8);
    std::thread::spawn(move || {
        let agent = upstream_agent();
        let response = match if url.contains("/manifests/") {
            // registries reject a manifest GET whose Accept names no
            // manifest media type (distribution 3.x 404s it)
            agent
                .get(&url)
                .set("Accept", orchard_core::oci::MANIFEST_ACCEPT)
                .call()
        } else {
            agent.get(&url).call()
        } {
            Ok(response) => response,
            Err(ureq::Error::Status(401, denied)) => {
                let Some(challenge) = denied.header("www-authenticate") else {
                    eprintln!("v2 proxy: upstream 401 without a WWW-Authenticate header");
                    let _ = head_tx.send(Err(ProxyFailure {
                        status: axum::http::StatusCode::BAD_GATEWAY,
                        reason: "upstream registry sent 401 without a token challenge".into(),
                        retry_after: None,
                    }));
                    return;
                };
                let token = match orchard_core::oci::token_from_challenge(
                    challenge,
                    &repository,
                    "orchard-indexd",
                ) {
                    Ok(token) => token,
                    Err(err) => {
                        eprintln!("v2 proxy: token fetch failed: {err:#}");
                        let _ = head_tx.send(Err(ProxyFailure {
                            status: axum::http::StatusCode::BAD_GATEWAY,
                            reason: format!("token exchange failed: {err}"),
                            retry_after: None,
                        }));
                        return;
                    }
                };
                match agent
                    .get(&url)
                    // the Accept types ride again: ghcr 404s a manifest
                    // GET that names no manifest media type, and the
                    // initial request's Accept does not survive into
                    // this retry
                    .set("Accept", orchard_core::oci::MANIFEST_ACCEPT)
                    .set("Authorization", &format!("Bearer {token}"))
                    .call()
                {
                    Ok(response) => response,
                    Err(ureq::Error::Status(code, response)) => {
                        // the dance worked and the registry still said
                        // no: that answer is the truth about this ref
                        relay_upstream_status(head_tx, code, response);
                        return;
                    }
                    Err(err) => {
                        eprintln!("v2 proxy: post-token fetch failed: {err:#}");
                        let _ = head_tx.send(Err(ProxyFailure {
                            status: axum::http::StatusCode::BAD_GATEWAY,
                            reason: format!("upstream unreachable after token exchange: {err}"),
                            retry_after: None,
                        }));
                        return;
                    }
                }
            }
            Err(ureq::Error::Status(code, response)) => {
                // a real upstream answer that is not a challenge: hand
                // it to the client as itself, not a made-up 502
                relay_upstream_status(head_tx, code, response);
                return;
            }
            Err(err) => {
                eprintln!("v2 proxy: fetch failed: {err:#}");
                let _ = head_tx.send(Err(ProxyFailure {
                    status: axum::http::StatusCode::BAD_GATEWAY,
                    reason: format!("upstream registry unreachable: {err}"),
                    retry_after: None,
                }));
                return;
            }
        };
        let content_type = response
            .header("content-type")
            .unwrap_or("application/octet-stream")
            .to_string();
        let mut reader = response.into_reader();
        if head_tx.send(Ok(content_type)).is_err() {
            return; // the requester is gone before the head landed
        }
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    if tx
                        .blocking_send(Ok(bytes::Bytes::copy_from_slice(&buf[..n])))
                        .is_err()
                    {
                        break; // the client disconnected mid-stream
                    }
                }
                Err(err) => {
                    let _ = tx.blocking_send(Err(err));
                    break;
                }
            }
        }
    });
    match head_rx.await {
        Ok(Ok(content_type)) => (
            [(axum::http::header::CONTENT_TYPE, content_type)],
            axum::body::Body::from_stream(tokio_stream::wrappers::ReceiverStream::new(rx)),
        )
            .into_response(),
        Ok(Err(failure)) => failure.into_response(),
        // the thread died before reporting (panic, OOM kill): the
        // client still deserves a body that says so
        Err(_) => ProxyFailure {
            status: axum::http::StatusCode::BAD_GATEWAY,
            reason: "the proxy's relay thread exited before reporting".into(),
            retry_after: None,
        }
        .into_response(),
    }
}

/// One shared agent for every upstream registry fetch (the proxy and
/// the token dance): pooled connections survive across flatpak's
/// manifest and blob requests instead of a fresh TLS handshake per
/// request. The connect timeout is tight; the overall timeout is
/// generous because a blob is hundreds of MB and a slow link is not an
/// error (the reader thread exits on client disconnect regardless).
fn upstream_agent() -> ureq::Agent {
    static AGENT: std::sync::OnceLock<ureq::Agent> = std::sync::OnceLock::new();
    AGENT
        .get_or_init(|| {
            ureq::AgentBuilder::new()
                .timeout_connect(std::time::Duration::from_secs(10))
                .timeout(std::time::Duration::from_secs(30 * 60))
                .build()
        })
        .clone()
}

/// Clear absorbed events: the feed refills from the relays on their
/// next pass, and the disk cache empties on the writer's next tick.
async fn clear_cache(State(state): State<ServerState>) -> &'static str {
    state.document.clear();
    // a fresh subscription re-delivers what the feed just dropped
    if let Some(reset) = &state.relays_reset {
        reset.store(true, std::sync::atomic::Ordering::Relaxed);
    }
    "cleared"
}

/// Bind and serve. The port is reachable from the host even when Orchard runs
/// sandboxed, because flatpak sandboxes share the network namespace by default
/// (asserted in M2).
///
/// Verified NIP-82 events from the relays named in the settings are
/// merged over the state's base document and cached to disk. The
/// settings handle stays live: a changed relay set applies within a
/// second, no restart.
pub async fn serve(
    addr: std::net::SocketAddr,
    base: Arc<dyn CatalogDocument>,
    public_url: Option<String>,
    settings: orchard_core::SettingsHandle,
    cache: Option<std::path::PathBuf>,
) -> anyhow::Result<()> {
    let feed = Arc::new(Mutex::new(orchard_nostr::Feed::new()));
    load_cache(&cache, &feed);
    // the clear-cache action empties the feed AND asks for a refetch:
    // the supervisor cycles the relay threads, whose fresh connections
    // re-subscribe and re-deliver
    let relays_reset = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let relays_health =
        spawn_relay_supervisor(settings.clone(), Arc::clone(&feed), Arc::clone(&relays_reset));
    spawn_cache_writer(cache, Arc::clone(&feed));
    let graph = Arc::new(Mutex::new(GraphStore::default()));
    spawn_graph_crawler(settings.clone(), Arc::clone(&feed), Arc::clone(&graph));
    spawn_profile_crawler(settings, Arc::clone(&feed));
    let state = ServerState {
        document: Arc::new(NostrCatalog {
            base: base.document(),
            feed,
            graph,
        }),
        relays_reset: Some(relays_reset),
        relays_health: Some(relays_health),
        // the Registry name /index/static advertises: the operator's
        // public base when set (a reverse proxy in front), else this
        // process's own bind address
        public_url: Some(public_url.unwrap_or_else(|| format!("http://{addr}"))),
    };
    // A restart races the previous instance's dying listener for the
    // port, and losing that race used to cost the whole session: the
    // client degrades to its file fallback for as long as it runs.
    // Retry while the old socket drains; only a real collision (a
    // foreign process holding the port) surfaces as an error.
    let mut bound = None;
    for attempt in 0..50 {
        match tokio::net::TcpListener::bind(addr).await {
            Ok(listener) => {
                bound = Some(listener);
                break;
            }
            Err(err)
                if err.kind() == std::io::ErrorKind::AddrInUse && attempt < 49 =>
            {
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
            Err(err) => return Err(err.into()),
        }
    }
    let listener = bound.expect("the last retry returned the error instead");
    axum::serve(listener, router(state)).await?;
    Ok(())
}

/// Load the event cache and absorb it into the feed. Every event is
/// re-verified on load; corrupt or tampered entries are skipped with a note.
fn load_cache(path: &Option<std::path::PathBuf>, feed: &Arc<Mutex<orchard_nostr::Feed>>) {
    let Some(path) = path else { return };
    let Ok(bytes) = std::fs::read(path) else {
        return;
    };
    let Ok(jsons) = serde_json::from_slice::<Vec<String>>(&bytes) else {
        eprintln!("nostr cache {}: unparseable, starting empty", path.display());
        return;
    };
    let mut feed = feed.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let mut accepted = 0usize;
    for json in jsons {
        match nostr::event::Event::from_json(json)
            .map_err(|err| anyhow::anyhow!("{err}"))
            .and_then(|event| feed.absorb(event))
        {
            Ok(true) => accepted += 1,
            Ok(false) => {}
            Err(err) => eprintln!("nostr cache: dropping event: {err:#}"),
        }
    }
    eprintln!("nostr cache: {accepted} events loaded");
}

/// Periodically write the feed's events to the cache when they changed.
/// Coalesces bursts by waiting for a quiet gap: a backfill storm costs
/// one serialization (when it has been quiet for two seconds), not one
/// per tick; a feed that never quiets writes at most every 30 seconds.
fn spawn_cache_writer(path: Option<std::path::PathBuf>, feed: Arc<Mutex<orchard_nostr::Feed>>) {
    let Some(path) = path else { return };
    /// How long the feed must have been quiet before a write.
    const QUIET_MS: u64 = 2000;
    /// How long a dirty feed may go unwritten even without a quiet
    /// gap: a crash must not cost more than this much sync.
    const MAX_STALE_MS: u64 = 30_000;
    if std::thread::Builder::new()
        .name("nostr-cache".into())
        .spawn(move || {
            loop {
                std::thread::sleep(std::time::Duration::from_secs(1));
                // the peek is one uncontended lock and two atomic
                // loads: no serialization, until a write is due
                let (last_change, dirty_since) = feed
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .dirty_state();
                if dirty_since == 0 {
                    continue;
                }
                let now = orchard_nostr::unix_millis();
                let quiet = now.saturating_sub(last_change) >= QUIET_MS;
                let stale = now.saturating_sub(dirty_since) >= MAX_STALE_MS;
                if !quiet && !stale {
                    continue;
                }
                guarded("nostr-cache", || {
                    // the feed's change flag is the tick's whole cost
                    // when nothing happened: no serialization, no lock
                    // hold worth naming. When something did, trim to
                    // what the catalog can show before serializing.
                    let jsons = {
                        let mut feed = feed
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner());
                        if !feed.take_dirty() {
                            return;
                        }
                        feed.trim();
                        feed.event_jsons()
                    };
                    if let Some(parent) = path.parent() {
                        let _ = std::fs::create_dir_all(parent);
                    }
                    // atomic swap: a crash mid-write leaves the old
                    // cache intact instead of a truncated file the
                    // next load must discard wholesale
                    let tmp = path.with_extension("json.tmp");
                    let written = serde_json::to_vec(&jsons)
                        .ok()
                        .and_then(|bytes| std::fs::write(&tmp, bytes).ok())
                        .and_then(|_| std::fs::rename(&tmp, &path).ok());
                    if written.is_none() {
                        eprintln!("nostr cache: write failed; the next tick retries");
                        feed.lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner())
                            .mark_dirty();
                    }
                });
            }
        })
        .is_err()
    {
        eprintln!("failed to spawn nostr cache writer");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use orchard_core::CatalogApp;

    /// The shared locks recover from poison: a panic in one holder
    /// must cost that one thread, not cascade into every relay
    /// thread, crawler, and endpoint that shares the feed after it.
    #[test]
    fn the_feed_lock_recovers_from_a_poisoned_holder() {
        let feed = Arc::new(std::sync::Mutex::new(orchard_nostr::Feed::default()));
        let holder = Arc::clone(&feed);
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
            let _guard = holder
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            panic!("panicking while holding the feed lock");
        }));
        // a later holder takes the guard, not the panic
        assert!(
            feed.lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .catalog()
                .apps
                .is_empty()
        );
    }

    /// The crawl selection: never-crawled reviewers go first, freshly
    /// crawled ones wait for the cadence, and absence stamps too (a
    /// reviewer with no contact list is not re-asked every tick).
    #[test]
    fn graph_store_pends_and_absorbs() {
        let mut store = GraphStore::default();
        let reviewers = vec!["aaaa".to_string(), "bbbb".to_string()];
        assert_eq!(store.pending(&reviewers, GRAPH_REFRESH).len(), 2);

        // one reviewer answered, the other absent: both stamp
        store.absorb(
            vec![("aaaa".into(), vec!["cccc".into()])],
            &reviewers,
        );
        assert_eq!(store.pending(&reviewers, GRAPH_REFRESH).len(), 0);
        assert_eq!(store.graph["aaaa"], vec!["cccc".to_string()]);
        assert!(!store.graph.contains_key("bbbb"));

        // after the cadence only the backdated entry is pending again;
        // the freshly crawled one waits out its cadence
        store.crawled.insert(
            "aaaa".into(),
            std::time::Instant::now() - std::time::Duration::from_secs(601),
        );
        assert_eq!(store.pending(&reviewers, GRAPH_REFRESH), vec!["aaaa".to_string()]);
        assert_eq!(store.file().graph.len(), 1);
    }

    #[test]
    fn resolve_set_prefers_env_then_file_then_default() {
        let default = vec!["wss://default".to_string()];
        let file = vec!["wss://file".to_string()];
        assert_eq!(
            resolve_set(&file, Some("ws://env, ws://env2"), &default),
            vec!["ws://env", "ws://env2"]
        );
        // a blank env value does not win: it is unset in spirit
        let pinned = orchard_nostr::relay::with_required_relay(file.clone());
        assert_eq!(resolve_set(&file, Some("  "), &default), pinned);
        assert_eq!(resolve_set(&file, None, &default), pinned);
        // a file list already carrying the required relay is unchanged
        let carried = vec![
            orchard_nostr::relay::REQUIRED_RELAY.to_string(),
            "wss://file".to_string(),
        ];
        assert_eq!(resolve_set(&carried, None, &default), carried);
        // an empty file list means default
        assert_eq!(resolve_set(&[], None, &default), default);
    }

    fn sample_catalog() -> CatalogFile {
        CatalogFile {
            version: 1,
            apps: vec![CatalogApp {
                app_id: "com.orchard.Hello".into(),
                name: "Orchard Hello".into(),
                summary: "Demo app".into(),
                description: None,
                icon_url: None,
                categories: Vec::new(),
                screenshots: Vec::new(),
                publisher: orchard_core::Publisher {
                    pubkey: "demo-pubkey".into(),
                    nip05: None,
                },
                version: "1.0.0".into(),
                platforms: vec!["linux-x86_64".into()],
                flatpak_ref: "app/com.orchard.Hello/x86_64/stable".into(),
                branch: "stable".into(),
                channels: Vec::new(),
                location: "http://registry.example.com/v2/hello/manifests/sha256:00".into(),
                digest: Some("sha256:00".into()),
                oci_assets: None,
                metadata: Some("[Application]\nname=Orchard Hello".into()),
                repository: None,
                homepage: None,
                notes: None,
                published_at: 0,
                successor_to: None,
            }],
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn catalog_endpoint_serves_document() {
        let catalog = sample_catalog();
        let router = router(ServerState {
            document: Arc::new(catalog.clone()),
            relays_reset: None,
            relays_health: None,
            public_url: None,
        });

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });

        // Blocking client on this thread; the multi-thread runtime keeps the
        // server responsive on its own threads.
        let agent = ureq::AgentBuilder::new().build();

        let body: CatalogFile = agent
            .get(&format!("http://{addr}/v1/catalog"))
            .call()
            .unwrap()
            .into_json()
            .unwrap();
        assert_eq!(body.apps[0].app_id, "com.orchard.Hello");

        let index: OciIndex = agent
            .get(&format!("http://{addr}/index/x86_64"))
            .call()
            .unwrap()
            .into_json()
            .unwrap();
        assert_eq!(index.manifests.len(), 1);
        assert_eq!(
            index.manifests[0].flatpak_ref(),
            Some("app/com.orchard.Hello/x86_64/stable")
        );

        // a static catalog carries no profiles: the endpoint exists and
        // serves an empty document rather than a 404
        let profiles: orchard_core::ProfilesFile = agent
            .get(&format!("http://{addr}/v1/profiles"))
            .call()
            .unwrap()
            .into_json()
            .unwrap();
        assert!(profiles.profiles.is_empty());

        server.abort();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn relays_endpoint_serves_health() {
        let router = router(ServerState {
            document: Arc::new(sample_catalog()),
            relays_reset: None,
            relays_health: None,
            public_url: None,
        });

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });

        let agent = ureq::AgentBuilder::new().build();

        // a static document carries no relays: the endpoint exists and
        // serves an empty list rather than a 404 (same rule as profiles)
        let body: serde_json::Value = agent
            .get(&format!("http://{addr}/v1/relays"))
            .call()
            .unwrap()
            .into_json()
            .unwrap();
        assert_eq!(body["relays"].as_array().map(Vec::len), Some(0));

        server.abort();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn relays_endpoint_serves_stamped_rows() {
        use orchard_nostr::relay::{RelayHealth, SharedRelayHealth};
        let map: orchard_nostr::relay::RelayHealthMap = Default::default();
        let live: SharedRelayHealth = Default::default();
        *live.lock().unwrap() = RelayHealth {
            connected: true,
            last_frame_at: Some(
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_millis() as u64
                    - 5_000,
            ),
            last_error: None,
            failures: 0,
        };
        let down: SharedRelayHealth = Default::default();
        *down.lock().unwrap() = RelayHealth {
            connected: false,
            last_frame_at: None,
            last_error: Some("connection refused".into()),
            failures: 3,
        };
        map.lock().unwrap().insert("ws://live".into(), live);
        map.lock().unwrap().insert("ws://down".into(), down);

        let router = router(ServerState {
            document: Arc::new(sample_catalog()),
            relays_reset: None,
            relays_health: Some(map),
            public_url: None,
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });

        let agent = ureq::AgentBuilder::new().build();
        let body: serde_json::Value = agent
            .get(&format!("http://{addr}/v1/relays"))
            .call()
            .unwrap()
            .into_json()
            .unwrap();
        let rows = body["relays"].as_array().unwrap();
        assert_eq!(rows.len(), 2);
        let live = rows.iter().find(|r| r["url"] == "ws://live").unwrap();
        assert_eq!(live["connected"], true);
        // the age is computed at read time: ~5s, not 0, not the stamp
        assert!(live["last_frame_age_secs"].as_u64().unwrap() < 30);
        let down = rows.iter().find(|r| r["url"] == "ws://down").unwrap();
        assert_eq!(down["connected"], false);
        assert_eq!(down["failures"], 3);
        assert_eq!(down["last_error"], "connection refused");

        server.abort();
    }

    #[test]
    fn registry_location_parses() {
        let (host, name) = parse_registry_location(
            "http://127.0.0.1:5000/v2/hello/manifests/sha256:abc",
        )
        .unwrap();
        assert_eq!(host, "127.0.0.1:5000");
        assert_eq!(name, "hello");
        assert!(parse_registry_location("file:///some/layout").is_none());
        assert!(parse_registry_location("https://ghcr.io/v2/a/b/manifests/sha256:x").is_some());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn index_static_serves_flatpaks_names() {
        let router = router(ServerState {
            document: Arc::new(sample_catalog()),
            relays_reset: None,
            relays_health: None,
            public_url: Some("https://index.example.com".into()),
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });

        let agent = ureq::AgentBuilder::new().build();
        let body: serde_json::Value = agent
            .get(&format!("http://{addr}/index/static"))
            .call()
            .unwrap()
            .into_json()
            .unwrap();
        // member names are flatpak's literals, not the OCI spec's
        assert!(body.get("Registry").is_some());
        assert!(body.get("results").is_none());
        let image = &body["Results"][0]["Images"][0];
        assert!(image["Digest"].as_str().unwrap().starts_with("sha256:"));
        assert_eq!(image["Labels"]["org.flatpak.ref"], "app/com.orchard.Hello/x86_64/stable");
        // Registry resolves relative to the index URI: absolute when set
        assert_eq!(body["Registry"], "https://index.example.com");

        server.abort();
    }

    /// An upstream registry serving an error status, for the proxy
    /// propagation tests: the client asks indexd, indexd asks here.
    async fn upstream_registry(status: axum::http::StatusCode, retry_after: &'static str) -> std::net::SocketAddr {
        let handler = move || {
            let mut response = axum::http::Response::builder()
                .status(status)
                .body(axum::body::Body::from("upstream says no\n"))
                .unwrap();
            if !retry_after.is_empty() {
                response.headers_mut().insert(
                    header::RETRY_AFTER,
                    axum::http::HeaderValue::from_static(retry_after),
                );
            }
            async move { response }
        };
        let app = axum::Router::new().fallback(axum::routing::any(handler));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        addr
    }

    /// A proxied ref the upstream registry answers with its own error
    /// status reaches the client as that status, with the upstream's
    /// Retry-After forwarded and a reason in the body: a rate-limited
    /// machine consumer must learn 429, not a made-up bare 502.
    #[tokio::test(flavor = "multi_thread")]
    async fn v2_proxy_relays_the_upstreams_own_status() {
        let upstream = upstream_registry(axum::http::StatusCode::TOO_MANY_REQUESTS, "30").await;
        let mut catalog = sample_catalog();
        catalog.apps[0].location =
            format!("http://{upstream}/v2/hello/manifests/sha256:00");
        let router = router(ServerState {
            document: Arc::new(catalog),
            relays_reset: None,
            relays_health: None,
            public_url: None,
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });

        let agent = ureq::AgentBuilder::new().build();
        let result = agent
            .get(&format!(
                "http://{addr}/v2/hello/manifests/sha256:00"
            ))
            .call();
        // ureq turns the propagated 429 into an Err of its own; the
        // wire status is what the test asserts
        let response = match result {
            Err(ureq::Error::Status(code, response)) => {
                assert_eq!(code, 429);
                response
            }
            other => panic!("expected the upstream's 429, got {other:?}"),
        };
        assert_eq!(response.header("retry-after"), Some("30"));
        let body = response.into_string().unwrap();
        assert!(body.contains("429"), "reason should name the status: {body}");

        server.abort();
    }

    /// An unreachable upstream is a 502, but one that says why: a
    /// client (and the hosted operator reading its journal) sees the
    /// difference between "registry unreachable" and other failures,
    /// instead of an empty body.
    #[tokio::test(flavor = "multi_thread")]
    async fn v2_proxy_transport_failure_carries_a_reason() {
        // a port with no listener: connection refused, instantly
        let dead = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let dead_addr = dead.local_addr().unwrap();
        drop(dead);

        let mut catalog = sample_catalog();
        catalog.apps[0].location =
            format!("http://{dead_addr}/v2/hello/manifests/sha256:00");
        let router = router(ServerState {
            document: Arc::new(catalog),
            relays_reset: None,
            relays_health: None,
            public_url: None,
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });

        let agent = ureq::AgentBuilder::new().build();
        let result = agent
            .get(&format!(
                "http://{addr}/v2/hello/manifests/sha256:00"
            ))
            .call();
        let response = match result {
            Err(ureq::Error::Status(code, response)) => {
                assert_eq!(code, 502);
                response
            }
            other => panic!("expected a 502, got {other:?}"),
        };
        let body = response.into_string().unwrap();
        assert!(
            body.contains("unreachable"),
            "reason should say the upstream was unreachable: {body}"
        );

        server.abort();
    }
}
