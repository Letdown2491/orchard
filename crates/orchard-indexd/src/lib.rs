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
//!   artifacts. Kept from the first spike: this is *not* what flatpak's
//!   `oci+` remotes read (validated in the M0 spike: those fetch an **ostree
//!   summary** from the URL base, ref entries mapping to manifest digests,
//!   with `xa.oci-registry-uri` pointing at the blob registry; see
//!   `crates/orchard-flatpak/src/lib.rs`). The native-remote summary
//!   endpoint lands in a later milestone; until then MVP installs go direct
//!   via `flatpak install oci:/…` / `docker://…` with client-side digest
//!   verification.
//! - `GET /` - service description.

use std::sync::{Arc, Mutex};

use axum::extract::{Path as AxumPath, State};
use axum::routing::{get, post};
use axum::{Json, Router};

use orchard_core::CatalogFile;

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
fn spawn_relay_supervisor(
    settings: orchard_core::SettingsHandle,
    feed: Arc<Mutex<orchard_nostr::Feed>>,
    reset: Arc<std::sync::atomic::AtomicBool>,
) {
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
                    let spawned = std::thread::Builder::new()
                        .name(format!("relay-{relay_url}"))
                        .spawn(move || {
                            orchard_nostr::relay::run_forever(
                                &relay_url,
                                feed,
                                Arc::new(|| {}),
                                thread_stop,
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
}

/// Build the axum router for a state.
pub fn router(state: ServerState) -> Router {
    Router::new()
        .route("/", get(root))
        .route("/index/{arch}", get(index))
        .route("/v1/catalog", get(catalog))
        .route("/v1/reviews", get(reviews))
        .route("/v1/profiles", get(profiles))
        .route("/v1/graph", get(graph))
        .route("/v1/cache", post(clear_cache))
        .with_state(state)
}

async fn root() -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "service": "orchard-indexd",
        "description": "Orchard's local catalog service",
        "endpoints": ["/v1/catalog", "/v1/reviews", "/v1/profiles", "/v1/graph", "/index/{arch}"]
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
    state: ServerState,
    settings: orchard_core::SettingsHandle,
    cache: Option<std::path::PathBuf>,
) -> anyhow::Result<()> {
    let feed = Arc::new(Mutex::new(orchard_nostr::Feed::new()));
    load_cache(&cache, &feed);
    // the clear-cache action empties the feed AND asks for a refetch:
    // the supervisor cycles the relay threads, whose fresh connections
    // re-subscribe and re-deliver
    let relays_reset = Arc::new(std::sync::atomic::AtomicBool::new(false));
    spawn_relay_supervisor(settings.clone(), Arc::clone(&feed), Arc::clone(&relays_reset));
    spawn_cache_writer(cache, Arc::clone(&feed));
    let graph = Arc::new(Mutex::new(GraphStore::default()));
    spawn_graph_crawler(settings.clone(), Arc::clone(&feed), Arc::clone(&graph));
    spawn_profile_crawler(settings, Arc::clone(&feed));
    let state = ServerState {
        document: Arc::new(NostrCatalog {
            base: state.document.document(),
            feed,
            graph,
        }),
        relays_reset: Some(relays_reset),
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
                platforms: Vec::new(),
                flatpak_ref: "app/com.orchard.Hello/x86_64/stable".into(),
                location: "oci:/tmp/demo".into(),
                digest: Some("sha256:00".into()),
                oci_assets: None,
                metadata: None,
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
}
