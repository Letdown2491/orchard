//! NIP-82 software events for Orchard.
//!
//! Orchard consumes the NIP-82 draft event kinds: the same ones Zapstore's
//! `zsp` publishes. Installation selects supported Flatpak OCI manifests:
//!
//! | kind  | role              | `d` tag              |
//! |-------|-------------------|----------------------|
//! | 0     | publisher profile | (none)               |
//! | 32267 | app listing       | `<app-id>`           |
//! | 30063 | release           | `<app-id>@<version>` |
//! | 3063  | asset             | (none)               |
//!
//! Assets use `f` for platform and the `T=flatpak-oci`
//! convention for the software target. Standard `url`, `m`, `size` and `x` describe
//! the manifest file. Permissions and runtime requirements live in the
//! verified OCI content, not in unsigned catalog claims.
//!
//! Everything here is offline: events arrive as signed NIP-01 JSON, are
//! verified (id hash + Schnorr signature), accumulated, and projected into
//! Orchard's [`CatalogFile`]. The relay transport lives elsewhere.

use std::collections::BTreeMap;

use anyhow::Result;
use nostr::prelude::*;
use orchard_core::{CatalogApp, CatalogFile, Publisher};

pub mod connect;
pub mod events;
pub mod relay;

pub const KIND_PROFILE: u32 = 0;
pub const KIND_APP: u32 = 32267;
pub const KIND_RELEASE: u32 = 30063;
pub const KIND_ASSET: u32 = 3063;
pub const KIND_REVIEW: u32 = 30065;
/// Publisher migration (Orchard-draft, same caveat as the review
/// kind): signed by the OLD key, `d` the app id, `p` the successor
/// key. No `p` revokes a previous designation.
pub const KIND_MIGRATION: u32 = 30066;

/// The event fields Orchard reads, parsed once from the canonical JSON so
/// tag handling does not depend on the `Tag` API surface.
struct Parts {
    id: String,
    pubkey: String,
    created_at: u64,
    kind: u32,
    content: String,
    /// Tag rows as-is: `["d", "com.example.App"]`, …
    tags: Vec<Vec<String>>,
}

impl Parts {
    fn parse(event: &Event) -> Result<Self> {
        let value: serde_json::Value = serde_json::from_str(&event.as_json())?;
        let id = value["id"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("event without id"))?
            .to_string();
        let pubkey = value["pubkey"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("event without pubkey"))?
            .to_string();
        let created_at = value["created_at"].as_u64().unwrap_or(0);
        let kind = value["kind"].as_u64().unwrap_or(0) as u32;
        let content = value["content"].as_str().unwrap_or_default().to_string();
        let tags = value["tags"]
            .as_array()
            .map(|rows| {
                rows.iter()
                    .filter_map(|row| {
                        row.as_array().map(|items| {
                            items
                                .iter()
                                .filter_map(|item| item.as_str().map(str::to_string))
                                .collect::<Vec<_>>()
                        })
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        Ok(Self {
            id,
            pubkey,
            created_at,
            kind,
            content,
            tags,
        })
    }

    /// First value of the named tag across all rows.
    fn tag(&self, name: &str) -> Option<String> {
        self.tags
            .iter()
            .find(|row| row.first().is_some_and(|key| key == name))
            .and_then(|row| row.get(1).cloned())
            .filter(|value| !value.is_empty())
    }

    /// The value of each named tag. Later elements are hints, not values.
    fn tag_values(&self, name: &str) -> Vec<String> {
        self.tags
            .iter()
            .filter(|row| row.first().is_some_and(|key| key == name))
            .filter_map(|row| row.get(1).cloned())
            .filter(|value| !value.is_empty())
            .collect()
    }
}

/// One app listing (kind 32267). `d` is the app id (also the map key).
struct Listing {
    event: Event,
    created_at: u64,
    name: String,
    summary: String,
    icon_url: Option<String>,
    categories: Vec<String>,
    platforms: Vec<String>,
    repository: Option<String>,
    homepage: Option<String>,
}

/// A publisher migration (kind 30066, Orchard-draft): signed by the
/// OLD publisher key, `d` the app id, optional `p` the successor key.
/// Replaceable per (app id, old key): the newest statement from a key
/// wins, and an empty successor revokes the previous designation.
struct Migration {
    event: Event,
    created_at: u64,
    new_key: Option<String>,
}

/// One release (kind 30063). `d` is `<app-id>@<version>`; `e` refs asset
/// events. Newest first in the feed's per-app vector.
struct Release {
    event: Event,
    created_at: u64,
    /// The signer's hex key, kept at absorb: the trim and the
    /// projection compare it against the listing's author on every
    /// visit, and re-formatting the pubkey per visit is the
    /// projection loop's allocation churn.
    publisher: String,
    version: String,
    /// The platform this release was built for, from the `f` tag.
    /// None: a legacy release from before the tag existed; it stays
    /// installable on any host.
    platform: Option<String>,
    channel: Option<String>,
    asset_refs: Vec<String>,
}

/// One asset (kind 3063). Standard Flatpak manifest assets use `url` and
/// pin manifest bytes with `x`; existing OCI references pin their manifest.
/// Assets are unique by event id (no `d` tag).
struct Asset {
    event: Event,
    created_at: u64,
    /// The asset signer, cached for catalog projection.
    publisher: String,
    /// Supported platforms, one per f tag.
    platforms: Vec<String>,
    digest: Option<String>,
    location: Option<String>,
    blob: Option<orchard_core::oci_asset::BlobAsset>,
    // the runtime requirement and asset version surface on the app page
    #[allow(dead_code)]
    runtime: Option<String>,
    #[allow(dead_code)]
    version: Option<String>,
    /// What the artifact is: Zapstore-shaped events tag it `m`
    /// (a content type); Orchard's own events carry no `m`.
    #[allow(dead_code)]
    content_type: Option<String>,
    /// The file name the publisher gave it, `filename` tag.
    #[allow(dead_code)]
    filename: Option<String>,
}

impl Asset {
    /// Whether the artifact is one this flatpak store can install:
    /// an image location (Orchard's own events), or a Zapstore-shaped
    /// asset that names itself a flatpak. A release references every
    /// format the publisher ships (apk, exe, AppImage), all sharing
    /// the linux platform tags, so the kind, not just the platform,
    /// decides installability.
    fn installable_kind(&self) -> bool {
        if let Some(blob) = &self.blob {
            return blob.mime == orchard_core::oci_asset::MIME;
        }
        match self.location.as_deref() {
            Some(loc) if loc.starts_with("oci:") || loc.starts_with("docker://") => true,
            _ => {
                self.content_type.as_deref() == Some("application/vnd.flatpak")
                    || self
                        .filename
                        .as_deref()
                        .is_some_and(|name| name.ends_with(".flatpak"))
            }
        }
    }
}

/// A publisher profile (kind 0): content is standard NIP-01 metadata.
struct Profile {
    event: Event,
    created_at: u64,
    nip05: Option<String>,
}

/// A user review (kind 30065): `d` is the app id, content the text,
/// `rating` an optional 0.0 to 5.0 tag. Replaceable per (pubkey, app id):
/// editing is republishing, so one identity holds one review per app.
struct Review {
    event: Event,
    created_at: u64,
    rating: Option<f32>,
}

/// Unix millis, for the writer's debounce clocks.
pub fn unix_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or_default()
}

/// The accumulated set of verified NIP-82 events, ready to project into a
/// catalog. Replaceable semantics: the newest event per `d` wins.
/// Listings are keyed (app id, publisher): the permissionless case is
/// two publishers of one app id, and both rows survive.
#[derive(Default)]
pub struct Feed {
    /// Set on every retained event (and on clear) and taken by the
    /// cache writer: without a change, a writer tick costs one flag
    /// check, not a full-feed serialization under the lock.
    dirty: std::sync::atomic::AtomicBool,
    /// Unix millis of the newest retained event: the cache writer
    /// waits for a quiet gap after the last change, so a backfill
    /// storm costs one serialization, not one per tick.
    last_change: std::sync::atomic::AtomicU64,
    /// Unix millis of when the current dirty stretch began, 0 when
    /// clean: the writer's staleness bound writes a feed that never
    /// quiets instead of holding the cache back forever.
    dirty_since: std::sync::atomic::AtomicU64,
    /// Every verified event id ever absorbed. Relays re-deliver the
    /// since-overlap window on every reconnect; a seen id
    /// short-circuits before the schnorr check, because the id is the
    /// canonical hash of the signed bytes and has nothing to teach
    /// the feed a second time.
    seen: std::collections::HashSet<EventId>,
    listings: BTreeMap<(String, String), Listing>,
    /// (app id, old key) -> the newest migration statement that key
    /// signed for the id.
    migrations: BTreeMap<(String, String), Migration>,
    releases: BTreeMap<String, Vec<Release>>,
    assets: BTreeMap<String, Asset>,
    profiles: BTreeMap<String, Profile>,
    /// Keyed (pubkey, app id): one review per identity per app.
    reviews: BTreeMap<(String, String), Review>,
    /// The earliest event per author key, unix seconds: the client's
    /// new-key signal for review trust. Deliberately naive: a relay
    /// that only recently saw a key makes the key look new.
    first_seen: BTreeMap<String, u64>,
}

impl Feed {
    pub fn new() -> Self {
        Self::default()
    }

    /// Drop every absorbed event: the "clear cached data" action.
    /// Relays refill the feed on their next wake; the disk cache
    /// follows on the writer's next tick.
    pub fn clear(&mut self) {
        *self = Self::default();
        // the writer must rewrite the (now empty) file, or the disk
        // cache the next load reads restores everything just dropped
        self.dirty.store(true, std::sync::atomic::Ordering::Relaxed);
    }

    /// Swap out the change flag: true when an event was retained (or
    /// the feed was cleared) since the last take.
    pub fn take_dirty(&self) -> bool {
        let was = self.dirty.swap(false, std::sync::atomic::Ordering::Relaxed);
        if was {
            self.dirty_since
                .store(0, std::sync::atomic::Ordering::Relaxed);
        }
        was
    }

    /// Re-arm the change flag after a failed persistence tick, so the
    /// writer retries instead of assuming the loss was written.
    pub fn mark_dirty(&self) {
        self.dirty.store(true, std::sync::atomic::Ordering::Relaxed);
        // restore the stretch's start so the staleness bound and the
        // quiet gap keep measuring the same dirty period
        self.dirty_since
            .compare_exchange(0, unix_millis(), std::sync::atomic::Ordering::Relaxed, std::sync::atomic::Ordering::Relaxed)
            .ok();
    }

    /// (last change, dirty-since) in unix millis, dirty-since 0 meaning
    /// clean. Two atomic loads: the writer peeks without the lock and
    /// takes the lock only when a write is actually due.
    pub fn dirty_state(&self) -> (u64, u64) {
        (
            self.last_change.load(std::sync::atomic::Ordering::Relaxed),
            self.dirty_since.load(std::sync::atomic::Ordering::Relaxed),
        )
    }

    /// The newest event time the feed holds, across every kind: the
    /// reconnect `since` anchor.
    pub fn newest_created_at(&self) -> Option<u64> {
        [
            self.listings.values().map(|l| l.created_at).max(),
            self.migrations.values().map(|m| m.created_at).max(),
            self.releases
                .values()
                .flatten()
                .map(|r| r.created_at)
                .max(),
            self.assets.values().map(|a| a.created_at).max(),
            self.profiles.values().map(|p| p.created_at).max(),
            self.reviews.values().map(|r| r.created_at).max(),
        ]
        .into_iter()
        .flatten()
        .max()
    }

    /// Bound the feed to what the catalog can show. Relays refill on
    /// reconnect, so anything the projection ignores is pure cost:
    /// foreign releases no catalog can pick, versions past the newest
    /// few, assets nothing references, reviews of apps nobody lists,
    /// profiles nobody reads. Without this, memory and the disk cache
    /// grow with everything a relay ever saw, not with Orchard.
    pub fn trim(&mut self) {
        // a release is projectable only under a listing signed by the
        // same author; a short tail of each app's versions keeps the
        // newest-release-has-no-asset-yet walk working across a
        // publisher's recent history
        const VERSION_TAIL: usize = 10;
        let listed: std::collections::HashSet<(String, String)> =
            self.listings.keys().cloned().collect();
        for (app_id, releases) in self.releases.iter_mut() {
            releases.retain(|release| {
                listed.contains(&(app_id.clone(), release.publisher.clone()))
            });
            // stored newest-first; the tail is the newest
            releases.truncate(VERSION_TAIL);
        }
        self.releases.retain(|_, releases| !releases.is_empty());
        // assets survive only while a kept release points at them,
        // which also drops any asset whose author never listed
        let referenced: std::collections::HashSet<String> = self
            .releases
            .values()
            .flat_map(|releases| {
                releases
                    .iter()
                    .flat_map(|release| release.asset_refs.iter().cloned())
            })
            .collect();
        self.assets.retain(|id, _| referenced.contains(id));
        // reviews of unlisted apps are never rendered
        let listed_ids: std::collections::HashSet<String> =
            self.listings.keys().map(|(id, _)| id.clone()).collect();
        self.reviews
            .retain(|(_, app_id), _| listed_ids.contains(app_id));
        // profiles and first-seen live for the keys the survivors
        // name: publishers, reviewers, and a hand-over's two ends
        let mut wanted: std::collections::HashSet<String> = self
            .listings
            .keys()
            .map(|(_, key)| key.clone())
            .chain(self.reviews.keys().map(|(key, _)| key.clone()))
            .collect();
        for (app_id, migration) in self.migrations.iter() {
            if listed_ids.contains(&app_id.0) {
                wanted.insert(app_id.1.clone());
                if let Some(new_key) = &migration.new_key {
                    wanted.insert(new_key.clone());
                }
            }
        }
        self.profiles.retain(|key, _| wanted.contains(key));
        self.first_seen.retain(|key, _| wanted.contains(key));
        // a migration for an unlisted app id is inert: no old row can
        // step aside for a successor when the id is not listed at all
        self.migrations
            .retain(|(app_id, _), _| listed_ids.contains(app_id));
    }

    /// Verify and absorb one event. `Ok(false)` means the event is valid
    /// but not useful (stale replaceable, wrong kind, no linux platform,
    /// duplicate asset); `Err` means malformed or rejected.
    pub fn absorb(&mut self, event: Event) -> Result<bool> {
        // a seen id short-circuits before verification: the id is the
        // canonical hash of the signed bytes, so the same id carries
        // nothing new, and relays re-deliver the since-overlap window
        // on every reconnect
        if self.seen.contains(&event.id) {
            return Ok(false);
        }
        // NIP-01 integrity: the id is the event's canonical hash and the
        // signature must verify against it. A bad signature is a hard
        // error, not a skip: relays that pass garbage are lying.
        event
            .verify()
            .map_err(|err| anyhow::anyhow!("signature verification failed: {err}"))?;
        self.seen.insert(event.id);

        let parts = Parts::parse(&event)?;
        // first-seen bookkeeping: keep the earliest event per author
        // key across every kind the feed retains
        let author = event.pubkey.to_string();
        match self.first_seen.get(&author) {
            Some(seen) if *seen <= parts.created_at => {}
            _ => {
                self.first_seen.insert(author, parts.created_at);
            }
        }
        let retained = match parts.kind {
            KIND_APP => self.absorb_app(event, parts),
            KIND_RELEASE => self.absorb_release(event, parts),
            KIND_ASSET => self.absorb_asset(event, parts),
            KIND_PROFILE => self.absorb_profile(event, parts),
            KIND_REVIEW => self.absorb_review(event, parts),
            KIND_MIGRATION => self.absorb_migration(event, parts),
            _ => Ok(false),
        };
        if matches!(retained, Ok(true)) {
            let now = unix_millis();
            self.last_change
                .store(now, std::sync::atomic::Ordering::Relaxed);
            // the stretch's start is taken only from clean: a burst of
            // retained events shares one dirty_since
            if self.dirty_since.load(std::sync::atomic::Ordering::Relaxed) == 0 {
                self.dirty_since
                    .store(now, std::sync::atomic::Ordering::Relaxed);
            }
            self.dirty.store(true, std::sync::atomic::Ordering::Relaxed);
        }
        retained
    }

    fn absorb_app(&mut self, event: Event, parts: Parts) -> Result<bool> {
        let Some(app_id) = parts.tag("d") else {
            return Ok(false);
        };
        // Only flatpak platforms are catalogable: Zapstore android events
        // must not become rows in a flatpak store. A listing with no `f`
        // tag at all is treated as platform-agnostic.
        let platforms = parts.tag_values("f");
        if !platforms.is_empty() && !platforms.iter().any(|p| p.starts_with("linux-")) {
            return Ok(false);
        }
        // a publisher replaces their own listing; another publisher's
        // listing of the same id is a second row, not a challenge
        let key = (app_id.clone(), parts.pubkey.clone());
        let newer = self
            .listings
            .get(&key)
            .is_none_or(|old| old.created_at <= parts.created_at);
        if !newer {
            return Ok(false);
        }
        self.listings.insert(
            key,
            Listing {
                created_at: parts.created_at,
                name: parts
                    .tag("name")
                    .or_else(|| parts.tag("d"))
                    .unwrap_or_default(),
                summary: parts.tag("summary").unwrap_or_default(),
                icon_url: parts.tag("icon"),
                categories: parts.tag_values("t"),
                platforms,
                repository: parts.tag("repository"),
                homepage: parts.tag("url").or_else(|| parts.tag_values("h").into_iter()
                    .find(|value| value.starts_with("https://") || value.starts_with("http://"))),
                event,
            },
        );
        Ok(true)
    }

    fn absorb_migration(&mut self, event: Event, parts: Parts) -> Result<bool> {
        let Some(app_id) = parts.tag("d") else {
            return Ok(false);
        };
        let key = (app_id, parts.pubkey.clone());
        let newer = self
            .migrations
            .get(&key)
            .is_none_or(|old| old.created_at <= parts.created_at);
        if !newer {
            return Ok(false);
        }
        self.migrations.insert(
            key,
            Migration {
                event,
                created_at: parts.created_at,
                new_key: parts.tag("p"),
            },
        );
        Ok(true)
    }

    fn absorb_release(&mut self, event: Event, parts: Parts) -> Result<bool> {
        let Some(d) = parts.tag("d") else {
            return Ok(false);
        };
        // d names the release: <app-id>@<version> for legacy events,
        // <app-id>@<version>@<platform> once releases grew a platform.
        // Per-arch identity keeps one arch's republish from replacing
        // the other's on a relay, which keys replaceability on d.
        // Anything else (Zapstore runs its own d convention) is not an
        // Orchard release: a skip, not an error. The sync must never
        // wedge on content: a validly signed event whose shape we do
        // not speak arrives on the public relays the moment any
        // third-party publisher shows up, and one bail in a session
        // would retry forever, freezing the whole catalog.
        let pieces: Vec<&str> = d.split('@').collect();
        let (app_id, version, platform) = match pieces.as_slice() {
            [app_id, version] => (app_id.to_string(), version.to_string(), None),
            [app_id, version, platform] => (
                app_id.to_string(),
                version.to_string(),
                Some(platform.to_string()),
            ),
            _ => return Ok(false),
        };
        if app_id.is_empty() || version.is_empty() || platform.as_deref() == Some("") {
            return Ok(false);
        }
        // the i tag must agree with the d tag's app id when present
        if let Some(i) = parts.tag("i")
            && i != app_id
        {
            return Ok(false);
        }
        let releases = self.releases.entry(app_id.to_string()).or_default();
        if releases.iter().any(|old| old.event.id == event.id) {
            return Ok(false);
        }
        // A release's asset list is authoritative. An edited app@version
        // must not leave superseded assets selectable from the old event.
        if let Some(index) = releases.iter().position(|old| {
            old.event.pubkey == event.pubkey
                && Parts::parse(&old.event).ok().and_then(|p| p.tag("d")).as_deref() == Some(d.as_str())
        }) {
            let old = &releases[index];
            if old.created_at > parts.created_at
                || (old.created_at == parts.created_at && old.event.id.to_string() <= parts.id)
            {
                return Ok(false);
            }
            releases.remove(index);
        }
        // Newest versions first; replacement above follows NIP-01's
        // lowest-event-ID tie break for the same address and timestamp.
        let position = releases
            .iter()
            .position(|old| old.created_at < parts.created_at)
            .unwrap_or(releases.len());
        releases.insert(
            position,
            Release {
                created_at: parts.created_at,
                publisher: parts.pubkey.clone(),
                version,
                platform,
                channel: parts.tag("c"),
                asset_refs: parts.tag_values("e"),
                event,
            },
        );
        Ok(true)
    }

    fn absorb_asset(&mut self, event: Event, parts: Parts) -> Result<bool> {
        let digest = parts.tag("x").map(normalize_digest);
        let location = parts.tag("url").or_else(|| parts.tag("oci"));
        if !digest_agrees(digest.as_deref(), location.as_deref()) {
            return Ok(false);
        }
        let blob = if parts.tag_values("T").iter().any(|t| t == orchard_core::oci_asset::PACKAGE) {
            let (Some(hash), Some(url), Some(mime), Some(size)) =
                (parts.tag("x"), location.clone(), parts.tag("m"), parts.tag("size").and_then(|s| s.parse().ok())) else {
                    return Ok(false);
                };
            if hash.len() != 64 || orchard_core::oci_asset::hash_hex(&hash).is_err()
                || !(url.starts_with("https://") || url.starts_with("http://")) {
                return Ok(false);
            }
            Some(orchard_core::oci_asset::BlobAsset { sha256: hash, url, mime, size })
        } else { None };
        if self.assets.contains_key(&parts.id) {
            return Ok(false);
        }
        self.assets.insert(
            parts.id.clone(),
            Asset {
                created_at: parts.created_at,
                publisher: parts.pubkey.clone(),
                platforms: parts.tag_values("f"),
                blob,
                digest,
                location,
                runtime: parts.tag("runtime"),
                version: parts.tag("version"),
                content_type: parts.tag("m"),
                filename: parts.tag("filename"),
                event,
            },
        );
        Ok(true)
    }

    fn absorb_profile(&mut self, event: Event, parts: Parts) -> Result<bool> {
        let newer = self
            .profiles
            .get(&parts.pubkey)
            .is_none_or(|old| old.created_at <= parts.created_at);
        if !newer {
            return Ok(false);
        }
        let metadata: serde_json::Value = serde_json::from_str(&parts.content).unwrap_or_default();
        self.profiles.insert(
            parts.pubkey.clone(),
            Profile {
                created_at: parts.created_at,
                nip05: metadata["nip05"].as_str().map(str::to_string),
                event,
            },
        );
        Ok(true)
    }

    fn absorb_review(&mut self, event: Event, parts: Parts) -> Result<bool> {
        let Some(app_id) = parts.tag("d") else {
            return Ok(false);
        };
        // a rating tag, when present, must be a sane 0.0 to 5.0: a client
        // that publishes another format is broken, not lying. Skip, not
        // error: the sync must never wedge on content.
        let rating = match parts.tag("rating") {
            Some(raw) => {
                let value: f32 = match raw.parse() {
                    Ok(value) => value,
                    Err(_) => return Ok(false),
                };
                if !(0.0..=5.0).contains(&value) {
                    return Ok(false);
                }
                Some(value)
            }
            None => None,
        };
        let key = (parts.pubkey.clone(), app_id);
        let newer = self
            .reviews
            .get(&key)
            .is_none_or(|old| old.created_at <= parts.created_at);
        if !newer {
            return Ok(false);
        }
        self.reviews.insert(
            key,
            Review {
                created_at: parts.created_at,
                rating,
                event,
            },
        );
        Ok(true)
    }

    /// The feed's reviews, app id ascending, newest first within each
    /// app. Author name fields resolve from the author's kind 0 profile
    /// when one has been seen.
    pub fn reviews(&self) -> orchard_core::ReviewsFile {
        let mut reviews: Vec<orchard_core::Review> = self
            .reviews
            .iter()
            .map(|((pubkey, app_id), review)| orchard_core::Review {
                app_id: app_id.clone(),
                pubkey: pubkey.clone(),
                name: self
                    .profiles
                    .get(pubkey)
                    .and_then(|p| {
                        let event: serde_json::Value =
                            serde_json::from_str(&p.event.content).unwrap_or_default();
                        event["name"].as_str().map(str::to_string)
                    }),
                nip05: self.profiles.get(pubkey).and_then(|p| p.nip05.clone()),
                rating: review.rating,
                content: review.event.content.clone(),
                created_at: review.created_at as i64,
            })
            .collect();
        reviews.sort_by(|a, b| a.app_id.cmp(&b.app_id).then(b.created_at.cmp(&a.created_at)));
        // first_seen rides along, filtered to the keys that actually
        // review: the catalog's authors are not the client's business
        let mut first_seen: BTreeMap<String, u64> = BTreeMap::new();
        for (pubkey, _) in self.reviews.keys() {
            if let Some(seen) = self.first_seen.get(pubkey) {
                first_seen.insert(pubkey.clone(), *seen);
            }
        }
        orchard_core::ReviewsFile {
            version: 1,
            reviews,
            first_seen,
        }
    }

    /// Project the feed into a catalog: only apps with a listing, a
    /// release, and an asset, all signed by the same publisher key,
    /// become rows. `metadata` is deliberately `None`: the client reads
    /// the real sandbox from the artifact itself.
    pub fn catalog(&self) -> CatalogFile {
        self.catalog_for(&orchard_core::host_platform())
    }

    /// The catalog projected for one platform: releases built for
    /// other machines do not win, and a listing whose chain exists
    /// only for other platforms still projects (empty version, the
    /// claimed platforms attached) so the client can say so instead
    /// of offering an install that cannot work.
    pub fn catalog_for(&self, platform: &str) -> CatalogFile {
        let mut apps: Vec<CatalogApp> = Vec::new();
        // one row per (app id, publisher): the same app id signed by
        // two keys projects twice, each against its own release chain
        for ((app_id, author), listing) in &self.listings {
            // a migration statement signed by the listed key hands the
            // app id to a successor: once the successor's own listing
            // exists, the old row steps aside. A revoked or successor-
            // less designation keeps the row.
            let superseded = self
                .migrations
                .get(&(app_id.clone(), author.clone()))
                .and_then(|migration| migration.new_key.as_ref())
                .is_some_and(|successor| {
                    successor != author
                        && self
                            .listings
                            .contains_key(&(app_id.clone(), successor.clone()))
                });
            if superseded {
                continue;
            }
            // an installable asset of one release: same publisher,
            // pinned somewhere, built for this platform, and a flatpak
            // artifact (not the apk or AppImage riding the same
            // release's platform tags). An asset from before the
            // platform tag existed stays installable anywhere; it is
            // the fallback after an exact match.
            let same_publisher = |reference: &String| {
                self.assets
                    .get(reference)
                    .filter(|asset| asset.publisher == *author)
                    .filter(|asset| asset.location.is_some())
                    .filter(|asset| asset.installable_kind())
            };
            let installable = |release: &Release| {
                release
                    .asset_refs
                    .iter()
                    .filter_map(same_publisher)
                    .find(|asset| asset.platforms.iter().any(|p| p == platform))
                    .or_else(|| {
                        release
                            .asset_refs
                            .iter()
                            .filter_map(same_publisher)
                            .find(|asset| asset.platforms.is_empty())
                    })
            };
            // walk releases newest-first; first one with an
            // installable asset for this machine wins
            fn stable<'a>(
                releases: &'a [Release],
                author: &'a str,
            ) -> impl Iterator<Item = &'a Release> + 'a {
                releases
                    .iter()
                    .filter(move |release| release.publisher == author)
                    // flatpak installs from the stable branch; "main" is
                    // Zapstore's default channel name for the same thing
                    .filter(|release| {
                        matches!(release.channel.as_deref(), None | Some("stable") | Some("main"))
                    })
            }
            let picked = self.releases.get(app_id).and_then(|releases| {
                stable(releases, author)
                    .filter(|release| {
                        release.platform.as_deref().is_none_or(|built| built == platform)
                    })
                    .find_map(|release| Some((release, Some(installable(release)?))))
            });
            // the chain exists but builds nothing for this machine:
            // project the row with no install, so the page can say
            // what was built. A chain with no releases at all still
            // stays invisible, as before.
            let picked = picked.or_else(|| {
                self.releases.get(app_id).and_then(|releases| {
                    stable(releases, author).find(|release| {
                            release
                                .asset_refs
                                .iter()
                                .filter_map(same_publisher)
                                .next()
                                .is_some()
                        })
                        .map(|release| (release, None))
                })
            });
            let Some((release, asset)) = picked else {
                continue;
            };
            let (version, digest, location) = match asset {
                Some(asset) => (
                    release.version.clone(),
                    asset.digest.clone(),
                    asset.location.clone().unwrap_or_default(),
                ),
                // nothing for this machine: no version, no pin, no
                // install. The claimed platforms ride the row.
                None => (String::new(), None, String::new()),
            };
            let ref_arch = platform.strip_prefix("linux-").unwrap_or(platform);
            apps.push(CatalogApp {
                app_id: app_id.clone(),
                name: listing.name.clone(),
                summary: listing.summary.clone(),
                description: (!listing.event.content.is_empty())
                    .then(|| listing.event.content.clone()),
                icon_url: listing.icon_url.clone(),
                categories: listing.categories.clone(),
                publisher: Publisher {
                    pubkey: author.clone(),
                    nip05: self.profiles.get(author).and_then(|p| p.nip05.clone()),
                },
                version,
                platforms: listing.platforms.clone(),
                flatpak_ref: format!("app/{app_id}/{ref_arch}/stable"),
                location,
                digest,
                oci_assets: asset.filter(|a| a.blob.is_some()).map(|_| {
                    release.asset_refs.iter().filter_map(|id| self.assets.get(id))
                        .filter(|a| a.event.pubkey.to_string() == *author)
                        .filter(|a| a.platforms.iter().any(|p| p == platform))
                        .filter_map(|a| a.blob.clone())
                        .filter(|b| b.mime != orchard_core::oci_asset::MIME).collect()
                }),
                metadata: None,
                repository: listing.repository.clone(),
                homepage: listing.homepage.clone(),
                notes: (!release.event.content.is_empty())
                    .then(|| release.event.content.clone()),
                published_at: release.created_at as i64,
                // the newest statement naming this publisher as
                // successor, if any: the row's handover is authorized
                successor_to: self
                    .migrations
                    .iter()
                    .filter(|((_, _), migration)| {
                        migration.new_key.as_deref() == Some(author.as_str())
                    })
                    .max_by_key(|((_, _), migration)| migration.created_at)
                    .map(|((_, old_key), _)| old_key.clone()),
            });
        }
        apps.sort_by(|a, b| a.app_id.cmp(&b.app_id));
        CatalogFile { version: 1, apps }
    }

    /// Canonical JSON for every absorbed event, in stable order: the
    /// on-disk cache. Loading re-verifies each event, so a corrupted or
    /// tampered cache is harmless.
    pub fn event_jsons(&self) -> Vec<String> {
        let mut events: Vec<(u64, String)> = Vec::new();
        events.extend(self.listings.values().map(|l| (l.created_at, l.event.as_json())));
        events.extend(
            self.migrations
                .values()
                .map(|m| (m.created_at, m.event.as_json())),
        );
        events.extend(
            self.releases
                .values()
                .flatten()
                .map(|r| (r.created_at, r.event.as_json())),
        );
        events.extend(self.assets.values().map(|a| (a.created_at, a.event.as_json())));
        events.extend(
            self.reviews
                .values()
                .map(|r| (r.created_at, r.event.as_json())),
        );
        events.extend(
            self.profiles
                .values()
                .map(|p| (p.created_at, p.event.as_json())),
        );
        events.sort_by_key(|a| a.0);
        events.into_iter().map(|(_, json)| json).collect()
    }

    /// The latest kind 0 metadata per pubkey, as the raw JSON the
    /// author signed. Clients merge against this before republishing a
    /// profile: kind 0 is replaceable, a subset would drop fields.
    pub fn profile_raws(&self) -> std::collections::BTreeMap<String, String> {
        self.profiles
            .iter()
            .map(|(pubkey, profile)| (pubkey.clone(), profile.event.content.clone()))
            .collect()
    }

    /// The keys whose profiles matter to this store: every listing
    /// author and every review author. Profiles ride the social
    /// relays rather than the app relays, so the profile crawler
    /// asks the social set for these keys' kind 0 events.
    pub fn crawl_keys(&self) -> Vec<String> {
        let mut keys: std::collections::BTreeSet<String> = self
            .listings
            .keys()
            .map(|(_, author)| author.clone())
            .collect();
        for (pubkey, _) in self.reviews.keys() {
            keys.insert(pubkey.clone());
        }
        keys.into_iter().collect()
    }
}

/// When the location embeds its own digest (`docker://…@sha256:…`), it must
/// agree with the `x` pin; path-style locations (`oci:/dir`) carry no
/// digest and are checked at install time instead.
/// Whether an asset's location agrees with its digest pin. An oci
/// ref pins the digest in its `@sha256:` suffix; a content-addressed
/// https path (blossom and friends name the file by its own sha256)
/// must agree with the digest too when it carries one. Silent on
/// everything else: no pin is no claim.
fn digest_agrees(digest: Option<&str>, location: Option<&str>) -> bool {
    let (Some(digest), Some(location)) = (digest, location) else {
        return true;
    };
    let Some(at) = location.rfind('@') else {
        // not an oci pin
        if location.starts_with("http") {
            if let Some(segment) = location.rsplit('/').next() {
                let hex = segment.trim_end_matches(".flatpak");
                let is_hex64 = hex.len() == 64 && hex.chars().all(|c| c.is_ascii_hexdigit());
                if is_hex64 && !format!("sha256:{hex}").eq_ignore_ascii_case(digest) {
                    return false;
                }
            }
        }
        return true;
    };
    let pinned = &location[at + 1..];
    !(pinned.starts_with("sha256:") && !pinned.eq_ignore_ascii_case(digest))
}

/// Accept bare 64-hex or already-prefixed digests; the catalog's pin
/// format is `sha256:<hex>`.
fn normalize_digest(x: String) -> String {
    if x.starts_with("sha256:") {
        x
    } else {
        format!("sha256:{x}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Sign one event; the builder + finalize path is the canonical
    /// construction (id + sig computed by the crate).
    fn signed(keys: &Keys, kind: u32, content: &str, tags: Vec<(&str, &str)>) -> Event {
        let builder = EventBuilder::new(Kind::Custom(kind as u16), content).tags(tags.into_iter().map(
            |(name, value)| Tag::custom(name, [value]),
        ));
        builder.finalize(keys).unwrap()
    }

    /// A realistic test digest (64 hex chars).
    const DIGEST: &str = "f127a079f4a50c3d960e6707aeb7373ee01403cc7b079b9b6fc3743c21a6e7a7";

    #[test]
    fn homepage_prefers_url_and_accepts_legacy_http_h() {
        let keys = demo_keys();
        for (tags, expected) in [
            (vec![("h", "https://legacy.example")], Some("https://legacy.example")),
            (vec![("h", DIGEST)], None),
            (vec![("h", "https://legacy.example"), ("url", "https://current.example")], Some("https://current.example")),
        ] {
            let mut tags = tags;
            tags.push(("d", "com.example.Home"));
            let event = signed(&keys, KIND_APP, "", tags);
            let mut feed = Feed::new();
            assert!(feed.absorb(event).unwrap());
            assert_eq!(feed.listings.values().next().unwrap().homepage.as_deref(), expected);
        }
    }

    #[test]
    fn edited_release_replaces_its_authoritative_asset_list() {
        let keys = demo_keys();
        let make = |time, id: &str| EventBuilder::new(Kind::Custom(KIND_RELEASE as u16), "")
            .tags([Tag::custom("d", ["com.example.App@1"]), Tag::custom("i", ["com.example.App"]),
                Tag::custom("version", ["1"]), Tag::custom("e", [id])])
            .custom_created_at(Timestamp::from_secs(time)).finalize(&keys).unwrap();
        let old = make(10, &"a".repeat(64));
        let new = make(20, &"b".repeat(64));
        let mut feed = Feed::new();
        assert!(feed.absorb(old.clone()).unwrap());
        assert!(feed.absorb(new.clone()).unwrap());
        assert!(!feed.absorb(old).unwrap());
        let releases = &feed.releases["com.example.App"];
        assert_eq!(releases.len(), 1);
        assert_eq!(releases[0].event.id, new.id);
        assert_eq!(releases[0].asset_refs, ["b".repeat(64)]);
    }

    #[test]
    fn standard_release_selects_flatpak_manifest_among_other_formats() {
        let keys = demo_keys();
        let listing = signed(&keys, KIND_APP, "", vec![("d", "com.example.App"), ("name", "App"), ("f", "linux-x86_64"), ("f", "linux-aarch64")]);
        let deb = signed(&keys, KIND_ASSET, "", vec![("i", "com.example.App"), ("version", "1"), ("f", "linux-x86_64"), ("m", "application/vnd.debian.binary-package"), ("x", DIGEST), ("url", "https://example.com/app.deb")]);
        let container = signed(&keys, KIND_ASSET, "", vec![
            ("i", "com.example.App"), ("version", "1"), ("f", "linux-x86_64"),
            ("m", "application/vnd.oci.image.manifest.v1+json"), ("T", "oci"), ("t", "flatpak-oci"),
            ("x", DIGEST), ("url", "https://example.com/container.json")]);
        let make_manifest = |platform, url| signed(&keys, KIND_ASSET, "", vec![
            ("i", "com.example.App"), ("version", "1"), ("f", platform),
            ("m", "application/vnd.oci.image.manifest.v1+json"), ("T", "flatpak-oci"), ("size", "123"), ("x", DIGEST), ("url", url)]);
        let x86 = make_manifest("linux-x86_64", "https://example.com/x86.json");
        let arm = make_manifest("linux-aarch64", "https://example.com/arm.json");
        let config = signed(&keys, KIND_ASSET, "", vec![("i", "com.example.App"),
            ("f", "linux-x86_64"), ("T", "flatpak-oci"), ("x", &"b".repeat(64)),
            ("size", "42"), ("m", "application/vnd.oci.image.config.v1+json"), ("url", "https://elsewhere.example/config")]);
        let foreign = signed(&Keys::generate(), KIND_ASSET, "", vec![("f", "linux-x86_64"),
            ("T", "flatpak-oci"), ("x", &"c".repeat(64)), ("size", "42"),
            ("m", "application/vnd.oci.image.config.v1+json"), ("url", "https://untrusted.example/config")]);
        let release = signed(&keys, KIND_RELEASE, "", vec![
            ("d", "com.example.App@1"), ("i", "com.example.App"), ("version", "1"), ("c", "main"),
            ("e", &config.id.to_string()), ("e", &foreign.id.to_string()), ("e", &deb.id.to_string()), ("e", &container.id.to_string()), ("e", &x86.id.to_string()), ("e", &arm.id.to_string())]);
        let mut feed = Feed::new();
        absorb_all(&mut feed, &[listing, deb, container, config, foreign, x86, arm, release]);
        let catalog = feed.catalog_for("linux-x86_64");
        let blobs = catalog.apps[0].oci_assets.as_ref().unwrap();
        assert_eq!(blobs.len(), 1);
        assert_eq!(blobs[0].url, "https://elsewhere.example/config");
        assert!(feed.catalog_for("linux-aarch64").apps[0].oci_assets.as_ref().unwrap().is_empty());
        assert_eq!(feed.catalog_for("linux-x86_64").apps[0].location, "https://example.com/x86.json");
        assert_eq!(feed.catalog_for("linux-aarch64").apps[0].location, "https://example.com/arm.json");
    }

    #[test]
    fn repeated_tags_do_not_treat_relay_hints_as_values() {
        let event = EventBuilder::new(Kind::Custom(KIND_RELEASE as u16), "")
            .tag(Tag::custom("e", [DIGEST, "wss://relay.example.com"]))
            .finalize(&demo_keys()).unwrap();
        assert_eq!(Parts::parse(&event).unwrap().tag_values("e"), [DIGEST]);
    }

    fn demo_keys() -> Keys {
        Keys::parse("5122b2537caa7e084e8aec9278e222b1a9b53a1cd2dbfb7d76f0b1bd41c8d6ea").unwrap()
    }

    fn absorb_all(feed: &mut Feed, events: &[Event]) {
        for event in events {
            feed.absorb(event.clone()).unwrap();
        }
    }

    #[test]
    fn projects_listing_release_asset_into_catalog() {
        let keys = demo_keys();
        let app = signed(
            &keys,
            KIND_APP,
            "Long description of the demo app.",
            vec![
                ("d", "com.example.Hello"),
                ("name", "Hello"),
                ("summary", "A demo app"),
                ("icon", "https://example.com/icon.png"),
                ("t", "Utilities"),
                ("f", "linux-x86_64"),
                ("repository", "https://github.com/example/hello"),
                ("url", "https://hello.example.com"),
            ],
        );
        let asset = signed(
            &keys,
            KIND_ASSET,
            "",
            vec![
                ("i", "com.example.Hello"),
                ("x", DIGEST),
                ("version", "1.0.0"),
                ("oci", "oci:/srv/registry/hello"),
                ("runtime", "org.freedesktop.Platform/x86_64/25.08"),
            ],
        );
        let asset_id = asset.id.to_string();
        let release = signed(
            &keys,
            KIND_RELEASE,
            "First release.",
            vec![
                ("d", "com.example.Hello@1.0.0"),
                ("i", "com.example.Hello"),
                ("version", "1.0.0"),
                ("c", "stable"),
                ("e", &asset_id),
            ],
        );
        let mut feed = Feed::new();
        absorb_all(&mut feed, &[app, asset, release]);
        let catalog = feed.catalog();
        assert_eq!(catalog.apps.len(), 1);
        let entry = &catalog.apps[0];
        assert_eq!(entry.app_id, "com.example.Hello");
        assert_eq!(entry.name, "Hello");
        assert_eq!(entry.summary, "A demo app");
        assert_eq!(
            entry.repository.as_deref(),
            Some("https://github.com/example/hello")
        );
        assert_eq!(entry.homepage.as_deref(), Some("https://hello.example.com"));
        assert_eq!(
            entry.description.as_deref(),
            Some("Long description of the demo app.")
        );
        assert_eq!(entry.version, "1.0.0");
        assert_eq!(entry.flatpak_ref, "app/com.example.Hello/x86_64/stable");
        assert_eq!(entry.location, "oci:/srv/registry/hello");
        assert_eq!(entry.digest.as_deref(), Some(format!("sha256:{DIGEST}").as_str()));
        assert_eq!(entry.metadata, None);
        assert_eq!(entry.notes.as_deref(), Some("First release."));
        assert_eq!(entry.publisher.pubkey, demo_keys().get_public_key().unwrap().to_string());
        assert_eq!(entry.publisher.nip05, None);
        assert!(entry.published_at > 0);
    }

    /// Two arches of one version are two releases (per-arch `d`, so a
    /// relay keeps both) and each host picks its own chain.
    #[test]
    fn catalog_picks_the_release_built_for_the_host() {
        let keys = demo_keys();
        let chain = |platform: &str, version: &str| {
            let asset = signed(
                &keys,
                KIND_ASSET,
                "",
                vec![
                    ("i", "com.example.Hello"),
                    ("x", DIGEST),
                    ("version", version),
                    ("f", platform),
                    ("oci", "oci:/srv/registry/hello"),
                ],
            );
            let asset_id = asset.id.to_string();
            let d = format!("com.example.Hello@{version}@{platform}");
            let release = signed(
                &keys,
                KIND_RELEASE,
                "",
                vec![
                    ("d", &d),
                    ("i", "com.example.Hello"),
                    ("version", version),
                    ("f", platform),
                    ("c", "stable"),
                    ("e", &asset_id),
                ],
            );
            vec![asset, release]
        };
        let listing = signed(
            &keys,
            KIND_APP,
            "",
            vec![
                ("d", "com.example.Hello"),
                ("name", "Hello"),
                ("summary", "A demo app"),
                ("f", "linux-x86_64"),
                ("f", "linux-aarch64"),
            ],
        );
        let mut events = vec![listing];
        events.extend(chain("linux-x86_64", "1.0.0"));
        events.extend(chain("linux-aarch64", "1.0.0"));
        let mut feed = Feed::new();
        absorb_all(&mut feed, &events);
        let x86 = feed.catalog_for("linux-x86_64");
        assert_eq!(x86.apps.len(), 1);
        assert_eq!(x86.apps[0].version, "1.0.0");
        assert_eq!(x86.apps[0].flatpak_ref, "app/com.example.Hello/x86_64/stable");
        let arm = feed.catalog_for("linux-aarch64");
        assert_eq!(arm.apps.len(), 1);
        assert_eq!(arm.apps[0].version, "1.0.0");
        assert_eq!(arm.apps[0].flatpak_ref, "app/com.example.Hello/aarch64/stable");
        // the listing's coverage rides both rows
        assert_eq!(arm.apps[0].platforms, vec!["linux-x86_64", "linux-aarch64"]);
    }

    /// A chain that exists only for another platform still projects:
    /// no version, no pin, the claimed platforms attached, so the
    /// page can say what was built instead of offering an install
    /// that cannot work.
    #[test]
    fn foreign_arch_projects_without_an_install() {
        let keys = demo_keys();
        let listing = signed(
            &keys,
            KIND_APP,
            "",
            vec![
                ("d", "com.example.Hello"),
                ("name", "Hello"),
                ("summary", "A demo app"),
                ("f", "linux-aarch64"),
            ],
        );
        let asset = signed(
            &keys,
            KIND_ASSET,
            "",
            vec![
                ("i", "com.example.Hello"),
                ("x", DIGEST),
                ("version", "1.0.0"),
                ("f", "linux-aarch64"),
                ("oci", "oci:/srv/registry/hello"),
            ],
        );
        let asset_id = asset.id.to_string();
        let release = signed(
            &keys,
            KIND_RELEASE,
            "",
            vec![
                ("d", "com.example.Hello@1.0.0@linux-aarch64"),
                ("i", "com.example.Hello"),
                ("version", "1.0.0"),
                ("f", "linux-aarch64"),
                ("c", "stable"),
                ("e", &asset_id),
            ],
        );
        let mut feed = Feed::new();
        absorb_all(&mut feed, &[listing, asset, release]);
        let x86 = feed.catalog_for("linux-x86_64");
        assert_eq!(x86.apps.len(), 1);
        assert_eq!(x86.apps[0].version, "");
        assert_eq!(x86.apps[0].location, "");
        assert_eq!(x86.apps[0].digest, None);
        assert_eq!(x86.apps[0].platforms, vec!["linux-aarch64"]);
        // the machine it was built for installs normally
        let arm = feed.catalog_for("linux-aarch64");
        assert_eq!(arm.apps[0].version, "1.0.0");
        assert_eq!(
            arm.apps[0].digest.as_deref(),
            Some(format!("sha256:{DIGEST}").as_str())
        );
    }

    /// The permissionless case: a second publisher signs the same app
    /// id and both listings project, each against its own release
    /// chain. Neither replaces the other.
    #[test]
    fn two_publishers_of_one_app_id_project_twice() {
        let a = demo_keys();
        let b = Keys::parse(
            "a462984d90b5badd1ddef88f19ee6ad74bfcb06f431afc9b7b6ce2f6dbf6af70",
        )
        .unwrap();
        let listing = |keys: &Keys, summary: &str| {
            signed(
                keys,
                KIND_APP,
                "",
                vec![
                    ("d", "com.example.Hello"),
                    ("name", "Hello"),
                    ("summary", summary),
                    ("f", "linux-x86_64"),
                ],
            )
        };
        let asset = |keys: &Keys| {
            let asset = signed(
                keys,
                KIND_ASSET,
                "",
                vec![
                    ("i", "com.example.Hello"),
                    ("x", DIGEST),
                    ("version", "1.0.0"),
                    ("oci", "oci:/srv/registry/hello"),
                    ("runtime", "org.freedesktop.Platform/x86_64/25.08"),
                ],
            );
            let release = signed(
                keys,
                KIND_RELEASE,
                "",
                vec![
                    ("d", "com.example.Hello@1.0.0"),
                    ("i", "com.example.Hello"),
                    ("version", "1.0.0"),
                    ("c", "stable"),
                    ("e", asset.id.to_string().as_str()),
                ],
            );
            (asset, release)
        };
        let (asset_a, release_a) = asset(&a);
        let (asset_b, release_b) = asset(&b);
        let mut feed = Feed::new();
        absorb_all(
            &mut feed,
            &[
                listing(&a, "first publisher"),
                asset_a,
                release_a,
                // the second publisher's listing arrives LAST, and must
                // not win the merge: it is a second row
                listing(&b, "second publisher"),
                asset_b,
                release_b,
            ],
        );
        let catalog = feed.catalog();
        assert_eq!(catalog.apps.len(), 2);
        let keys: Vec<&str> = catalog
            .apps
            .iter()
            .map(|app| app.publisher.pubkey.as_str())
            .collect();
        assert!(keys.contains(&a.get_public_key().unwrap().to_string().as_str()));
        assert!(keys.contains(&b.get_public_key().unwrap().to_string().as_str()));
        // each publisher's own summary rode along on their row
        assert!(catalog.apps.iter().any(|app| app.summary == "first publisher"));
        assert!(catalog
            .apps
            .iter()
            .any(|app| app.summary == "second publisher"));
    }

    /// The migration case: the old key's designation hands the app id
    /// to the successor, whose listing then stands alone instead of
    /// doubling the row.

    /// The live migration event from the dev relay, absorbed cold:
    /// signature, tags, and map placement all hold for real data.
    #[test]
    fn live_migration_event_absorbs() {
        let raw = r#"{"id":"aae5473ea4a30738e0595cfb23bdd741a6faae540e2372e445a8edc9a60128b5","pubkey":"ec2c39e7fb46ddea4e916c240661c4e0ba5128fe6cf8ccf10fa074b95b63379d","created_at":1791706308,"kind":30066,"tags":[["d","com.orchard.Orchard"],["p","14cad7029d2b32e46eb47172d47988d180b17ac97de07a57fdb16a5a97d50365"]],"content":"","sig":"a787e0aefa18be9547faa5d6fb4dd250111b62643d999f0afb641b2b3af0ab85615371544c1824fb8731d34db9f2154cff2dce09f57d5973b4f8b080d32c3306"}"#;
        let event = Event::from_json(raw).unwrap();
        let mut feed = Feed::new();
        let absorbed = feed.absorb(event).unwrap();
        assert!(absorbed, "the migration must absorb");
        assert_eq!(feed.event_jsons().len(), 1);
    }

    #[test]
    fn migrated_listing_steps_aside() {
        let a = demo_keys();
        let b = Keys::parse(
            "a462984d90b5badd1ddef88f19ee6ad74bfcb06f431afc9b7b6ce2f6dbf6af70",
        )
        .unwrap();
        let b_pubkey = b.get_public_key().unwrap().to_string();
        // a complete chain per key: listing, asset, release
        let chain = |keys: &Keys, summary: &str| {
            let listing = signed(
                keys,
                KIND_APP,
                "",
                vec![
                    ("d", "com.example.Hello"),
                    ("name", "Hello"),
                    ("summary", summary),
                    ("f", "linux-x86_64"),
                ],
            );
            let asset = signed(
                keys,
                KIND_ASSET,
                "",
                vec![
                    ("i", "com.example.Hello"),
                    ("x", DIGEST),
                    ("version", "1.0.0"),
                    ("oci", "oci:/srv/registry/hello"),
                    ("runtime", "org.freedesktop.Platform/x86_64/25.08"),
                ],
            );
            let release = signed(
                keys,
                KIND_RELEASE,
                "",
                vec![
                    ("d", "com.example.Hello@1.0.0"),
                    ("i", "com.example.Hello"),
                    ("version", "1.0.0"),
                    ("c", "stable"),
                    ("e", asset.id.to_string().as_str()),
                ],
            );
            [listing, asset, release]
        };
        let mut feed = Feed::new();
        // both keys publish the app: two rows
        absorb_all(&mut feed, &chain(&a, "old key"));
        absorb_all(&mut feed, &chain(&b, "new key"));
        assert_eq!(feed.catalog().apps.len(), 2);
        // the old key designates the successor: its own row steps
        // aside and the successor's listing stands alone
        let migration = signed(
            &a,
            KIND_MIGRATION,
            "rotated to a new device",
            vec![("d", "com.example.Hello"), ("p", &b_pubkey)],
        );
        absorb_all(&mut feed, &[migration]);
        let catalog = feed.catalog();
        assert_eq!(catalog.apps.len(), 1);
        assert_eq!(catalog.apps[0].publisher.pubkey, b_pubkey);
        assert_eq!(catalog.apps[0].summary, "new key");
        // the surviving row carries the handover: the client can tell
        // an authorized rotation from an unexplained new key
        assert_eq!(
            catalog.apps[0].successor_to.as_deref(),
            Some(a.get_public_key().unwrap().to_string().as_str())
        );

        // revocation: a newer statement from the old key with no `p`
        // restores its row
        let revoke = EventBuilder::new(Kind::Custom(KIND_MIGRATION as u16), "")
            .custom_created_at(Timestamp::now())
            .tag(Tag::custom("d", ["com.example.Hello"]))
            .finalize(&a)
            .unwrap();
        absorb_all(&mut feed, &[revoke]);
        assert_eq!(feed.catalog().apps.len(), 2);
        // and the handover annotation is gone with the designation
        assert!(feed
            .catalog()
            .apps
            .iter()
            .all(|app| app.successor_to.is_none()));
    }

    /// A designation whose successor never lists suppresses nothing:
    /// the app must not lose its only row over an undelivered handover.
    #[test]
    fn successor_less_migration_keeps_the_row() {
        let a = demo_keys();
        let b = Keys::parse(
            "a462984d90b5badd1ddef88f19ee6ad74bfcb06f431afc9b7b6ce2f6dbf6af70",
        )
        .unwrap();
        let stranger = b.get_public_key().unwrap().to_string();
        let listing = signed(
            &a,
            KIND_APP,
            "",
            vec![
                ("d", "com.example.Hello"),
                ("name", "Hello"),
                ("f", "linux-x86_64"),
            ],
        );
        let asset = signed(
            &a,
            KIND_ASSET,
            "",
            vec![
                ("i", "com.example.Hello"),
                ("x", DIGEST),
                ("version", "1.0.0"),
                ("oci", "oci:/srv/registry/hello"),
                ("runtime", "org.freedesktop.Platform/x86_64/25.08"),
            ],
        );
        let release = signed(
            &a,
            KIND_RELEASE,
            "",
            vec![
                ("d", "com.example.Hello@1.0.0"),
                ("i", "com.example.Hello"),
                ("version", "1.0.0"),
                ("c", "stable"),
                ("e", asset.id.to_string().as_str()),
            ],
        );
        let migration = signed(
            &a,
            KIND_MIGRATION,
            "",
            vec![("d", "com.example.Hello"), ("p", &stranger)],
        );
        let mut feed = Feed::new();
        absorb_all(&mut feed, &[listing, asset, release, migration]);
        let catalog = feed.catalog();
        assert_eq!(catalog.apps.len(), 1);
        assert_eq!(
            catalog.apps[0].publisher.pubkey,
            a.get_public_key().unwrap().to_string()
        );
    }

    #[test]
    fn rejects_tampered_signatures() {
        let keys = demo_keys();
        let app = signed(
            &keys,
            KIND_APP,
            "Original.",
            vec![("d", "com.example.Hello"), ("f", "linux-x86_64")],
        );
        // flip one content byte: id and signature no longer match
        let raw = app.as_json().replace("Original.", "Tampered.");
        let tampered = Event::from_json(raw).unwrap();
        let mut feed = Feed::new();
        assert!(feed.absorb(tampered).is_err(), "tampered event must not absorb");
        assert!(feed.catalog().apps.is_empty());
    }

    #[test]
    fn cross_key_release_is_not_projected() {
        let publisher = demo_keys();
        let attacker = Keys::generate();
        let app = signed(
            &publisher,
            KIND_APP,
            "App.",
            vec![("d", "com.example.Hello"), ("f", "linux-x86_64")],
        );
        let asset = signed(
            &publisher,
            KIND_ASSET,
            "",
            vec![("x", DIGEST), ("oci", &format!("docker://ghcr.io/x@sha256:{DIGEST}"))],
        );
        let asset_id = asset.id.to_string();
        let release = signed(
            &attacker,
            KIND_RELEASE,
            "Hijacked release.",
            vec![
                ("d", "com.example.Hello@9.9.9"),
                ("i", "com.example.Hello"),
                ("version", "9.9.9"),
                ("e", &asset_id),
            ],
        );
        let mut feed = Feed::new();
        absorb_all(&mut feed, &[app, asset, release]);
        assert!(feed.catalog().apps.is_empty());
    }

    #[test]
    fn mismatched_pin_and_ref_is_skipped() {
        let keys = demo_keys();
        let other = "aa".repeat(32);
        let asset = signed(
            &keys,
            KIND_ASSET,
            "",
            vec![("x", DIGEST), ("oci", &format!("docker://ghcr.io/x@sha256:{other}"))],
        );
        let mut feed = Feed::new();
        assert!(
            !feed.absorb(asset).unwrap(),
            "pin/ref disagreement must skip: not stored, never installable"
        );
        assert!(feed.assets.is_empty());
    }

    /// A Zapstore-shaped asset: the location is a plain https URL (a
    /// blossom, content-addressed by the sha256 the event pins) and
    /// the kind rides the `m`/`filename` tags instead of an image
    /// scheme. It projects installable with the URL as its location.
    #[test]
    fn zapstore_url_assets_project_as_installable() {
        let keys = demo_keys();
        let app = signed(
            &keys,
            KIND_APP,
            "An app.",
            vec![("d", "com.example.Zap"), ("f", "linux-x86_64")],
        );
        let url = format!("https://blossom.example/{DIGEST}");
        let asset = signed(
            &keys,
            KIND_ASSET,
            "",
            vec![
                ("i", "com.example.Zap"),
                ("x", DIGEST),
                ("version", "0.64.14"),
                ("f", "linux-x86_64"),
                ("m", "application/vnd.flatpak"),
                ("filename", "Armada.flatpak"),
                ("url", &url),
            ],
        );
        let asset_id = asset.id.to_string();
        let release = signed(
            &keys,
            KIND_RELEASE,
            "",
            vec![
                ("d", "com.example.Zap@0.64.14"),
                ("i", "com.example.Zap"),
                ("version", "0.64.14"),
                ("c", "main"),
                ("e", &asset_id),
            ],
        );
        let mut feed = Feed::new();
        absorb_all(&mut feed, &[app, asset, release]);
        let catalog = feed.catalog();
        assert_eq!(catalog.apps.len(), 1);
        let entry = &catalog.apps[0];
        assert_eq!(entry.version, "0.64.14");
        assert_eq!(entry.location, url);
        assert_eq!(entry.digest.as_deref(), Some(format!("sha256:{DIGEST}").as_str()));
    }

    /// A release's non-flatpak artifacts (the AppImage and apk riding
    /// the same platform tags as the bundle) never win the pick: the
    /// kind decides, so a mixed release installs the flatpak.
    #[test]
    fn non_flatpak_assets_on_the_host_platform_are_not_installable() {
        let keys = demo_keys();
        let app = signed(
            &keys,
            KIND_APP,
            "An app.",
            vec![("d", "com.example.Mixed"), ("f", "linux-x86_64")],
        );
        // first ref in the release is an AppImage, second the bundle
        let appimage = signed(
            &keys,
            KIND_ASSET,
            "",
            vec![
                ("i", "com.example.Mixed"),
                ("x", DIGEST),
                ("version", "1.0"),
                ("f", "linux-x86_64"),
                ("m", "application/vnd.appimage"),
                ("filename", "Mixed.AppImage"),
                ("url", "https://blossom.example/somebytes"),
            ],
        );
        let bundle = signed(
            &keys,
            KIND_ASSET,
            "",
            vec![
                ("i", "com.example.Mixed"),
                ("x", DIGEST),
                ("version", "1.0"),
                ("f", "linux-x86_64"),
                ("m", "application/vnd.flatpak"),
                ("filename", "Mixed.flatpak"),
                ("url", &format!("https://blossom.example/{DIGEST}")),
            ],
        );
        let (appimage_id, bundle_id) = (appimage.id.to_string(), bundle.id.to_string());
        let release = signed(
            &keys,
            KIND_RELEASE,
            "",
            vec![
                ("d", "com.example.Mixed@1.0"),
                ("i", "com.example.Mixed"),
                ("version", "1.0"),
                ("c", "main"),
                ("e", &appimage_id),
                ("e", &bundle_id),
            ],
        );
        let mut feed = Feed::new();
        absorb_all(&mut feed, &[app, appimage, bundle, release]);
        let catalog = feed.catalog();
        assert_eq!(catalog.apps.len(), 1);
        let entry = &catalog.apps[0];
        assert!(entry.location.ends_with(DIGEST), "the bundle asset wins, got {}", entry.location);
    }

    /// A content-addressed URL whose path names a different sha256
    /// than the event's digest pin never stores: the asset is absent,
    /// so nothing can ever install it, and the sync keeps running.
    #[test]
    fn mismatched_url_and_digest_is_skipped() {
        let keys = demo_keys();
        let other = "bb".repeat(32);
        let asset = signed(
            &keys,
            KIND_ASSET,
            "",
            vec![
                ("i", "com.example.Zap"),
                ("x", DIGEST),
                ("url", &format!("https://blossom.example/{other}")),
            ],
        );
        let mut feed = Feed::new();
        assert!(!feed.absorb(asset).unwrap(), "url/digest disagreement must skip");
        assert!(feed.assets.is_empty());
    }

    /// A release whose d tag follows another convention (Zapstore
    /// shapes its own) is a skip, not an error: one foreign event on
    /// a public relay must not wedge the catalog sync in a retry
    /// loop.
    #[test]
    fn foreign_release_shapes_are_skipped() {
        let keys = demo_keys();
        let asset = signed(
            &keys,
            KIND_RELEASE,
            "",
            vec![("d", "stampede-android-apk")],
        );
        let mut feed = Feed::new();
        assert!(!feed.absorb(asset.clone()).unwrap(), "foreign d shape must skip");
        assert!(feed.releases.is_empty());
        // and the relay frame path survives it: no session error
        let frame = serde_json::json!(["EVENT", "sub", serde_json::to_value(&asset).unwrap()]);
        let (absorbed, eose) =
            crate::relay::handle_frame(
                &frame.to_string(),
                &std::sync::Arc::new(std::sync::Mutex::new(feed)),
            )
            .unwrap();
        assert!(!absorbed && eose.is_none());
    }

    #[test]
    fn android_listings_are_ignored() {
        let keys = demo_keys();
        let app = signed(
            &keys,
            KIND_APP,
            "An android app.",
            vec![("d", "com.example.Android"), ("f", "android-arm64-v8a")],
        );
        let mut feed = Feed::new();
        absorb_all(&mut feed, &[app]);
        assert!(feed.catalog().apps.is_empty());
    }

    #[test]
    fn replaceable_events_keep_the_newest() {
        let keys = demo_keys();
        let old = signed(
            &keys,
            KIND_APP,
            "Old description.",
            vec![("d", "com.example.Hello"), ("f", "linux-x86_64")],
        );
        // same d tag, newer created_at
        let newer = EventBuilder::new(Kind::Custom(KIND_APP as u16), "New description.")
            .tag(Tag::custom("d", ["com.example.Hello"]))
            .tag(Tag::custom("f", ["linux-x86_64"]))
            .custom_created_at(Timestamp::from(old.created_at + 60))
            .finalize(&keys)
            .unwrap();
        // a full chain beneath, so the projection has something to show
        let asset = signed(
            &keys,
            KIND_ASSET,
            "",
            vec![("x", DIGEST), ("oci", "oci:/srv/hello")],
        );
        let asset_id = asset.id.to_string();
        let release = signed(
            &keys,
            KIND_RELEASE,
            "Release.",
            vec![
                ("d", "com.example.Hello@1.0.0"),
                ("version", "1.0.0"),
                ("e", &asset_id),
            ],
        );
        let mut feed = Feed::new();
        absorb_all(&mut feed, &[old, newer, asset, release]);
        let catalog = feed.catalog();
        assert_eq!(catalog.apps.len(), 1);
        assert_eq!(
            catalog.apps[0].description.as_deref(),
            Some("New description.")
        );
    }

    #[test]
    fn newest_release_wins_and_beta_is_skipped() {
        let keys = demo_keys();
        let app = signed(
            &keys,
            KIND_APP,
            "App.",
            vec![("d", "com.example.Hello"), ("f", "linux-x86_64")],
        );
        let asset = signed(
            &keys,
            KIND_ASSET,
            "",
            vec![("x", DIGEST), ("oci", "oci:/srv/hello")],
        );
        let asset_id = asset.id.to_string();
        let stable = signed(
            &keys,
            KIND_RELEASE,
            "Stable.",
            vec![
                ("d", "com.example.Hello@1.0.0"),
                ("version", "1.0.0"),
                ("c", "stable"),
                ("e", &asset_id),
            ],
        );
        let beta = EventBuilder::new(Kind::Custom(KIND_RELEASE as u16), "Beta.")
            .tag(Tag::custom("d", ["com.example.Hello@2.0.0"]))
            .tag(Tag::custom("version", ["2.0.0"]))
            .tag(Tag::custom("c", ["beta"]))
            .tag(Tag::custom("e", [asset_id.as_str()]))
            .custom_created_at(Timestamp::from(stable.created_at + 60))
            .finalize(&keys)
            .unwrap();
        let mut feed = Feed::new();
        absorb_all(&mut feed, &[app, asset, stable, beta]);
        let catalog = feed.catalog();
        assert_eq!(catalog.apps.len(), 1);
        assert_eq!(catalog.apps[0].version, "1.0.0");
    }

    /// first_seen keeps the earliest event per key no matter the order
    /// the feed learned about them, and rides on the reviews document
    /// filtered to reviewing keys.
    #[test]
    fn first_seen_tracks_earliest_event() {
        let keys = demo_keys();
        let pubkey = keys.get_public_key().unwrap().to_string();
        let older_secs: u64 = 1_700_000_000;
        let newer_secs: u64 = older_secs + 100;
        let later = EventBuilder::new(Kind::Custom(KIND_REVIEW as u16), "A review.")
            .tag(Tag::custom("d", ["com.example.Hello"]))
            .tag(Tag::custom("rating", ["4.0"]))
            .custom_created_at(Timestamp::from_secs(newer_secs))
            .finalize(&keys)
            .unwrap();
        let earlier = EventBuilder::new(Kind::Custom(KIND_APP as u16), "Older listing.")
            .tag(Tag::custom("d", ["com.example.Old"]))
            .custom_created_at(Timestamp::from_secs(older_secs))
            .finalize(&keys)
            .unwrap();
        let mut feed = Feed::new();
        // the feed learns the review first, the older listing second:
        // first_seen must still be the older event
        absorb_all(&mut feed, &[later, earlier]);
        assert_eq!(feed.first_seen.get(&pubkey).copied(), Some(older_secs));

        let file = feed.reviews();
        assert_eq!(file.reviews.len(), 1);
        assert_eq!(file.first_seen.get(&pubkey).copied(), Some(older_secs));

        // a key that never reviewed is not the client's business
        let quiet = Keys::generate();
        let listing = signed(
            &quiet,
            KIND_APP,
            "Just a listing.",
            vec![("d", "com.example.Quiet"), ("f", "linux-x86_64")],
        );
        let mut feed = Feed::new();
        absorb_all(&mut feed, &[listing]);
        assert!(feed.reviews().first_seen.is_empty());
    }

    /// The trim bounds the feed to what the catalog can show: foreign
    /// releases and their assets drop, reviews of unlisted apps drop,
    /// and profiles nobody reads drop. Without it, memory and the
    /// disk cache grow with everything a relay ever saw.
    #[test]
    fn trim_keeps_only_what_the_catalog_can_show() {
        let publisher = demo_keys();
        let outsider = Keys::generate();
        let reviewer = Keys::generate();

        let listing = signed(
            &publisher,
            KIND_APP,
            "A listing.",
            vec![("d", "com.example.Hello"), ("f", "linux-x86_64")],
        );
        let asset = signed(
            &publisher,
            KIND_ASSET,
            "",
            vec![
                ("i", "com.example.Hello"),
                ("x", DIGEST),
                ("oci", "oci:/srv/registry/hello"),
            ],
        );
        let asset_id = asset.id.to_string();
        let release = signed(
            &publisher,
            KIND_RELEASE,
            "",
            vec![
                ("d", "com.example.Hello@1.0.0"),
                ("c", "stable"),
                ("e", asset_id.as_str()),
            ],
        );
        // an outsider's release for the same app id: no catalog can
        // project it (the publisher check fails), so it is pure cost
        let foreign_asset = signed(
            &outsider,
            KIND_ASSET,
            "",
            vec![
                ("i", "com.example.Hello"),
                ("x", DIGEST),
                ("oci", "oci:/srv/registry/evil"),
            ],
        );
        let foreign_id = foreign_asset.id.to_string();
        let foreign_release = signed(
            &outsider,
            KIND_RELEASE,
            "",
            vec![
                ("d", "com.example.Hello@9.9.9"),
                ("c", "stable"),
                ("e", foreign_id.as_str()),
            ],
        );
        // a review of the listed app, and one of an app nobody lists
        let kept_review = signed(
            &reviewer,
            KIND_REVIEW,
            "Good app.",
            vec![("d", "com.example.Hello"), ("rating", "4.0")],
        );
        let stray_review = signed(
            &reviewer,
            KIND_REVIEW,
            "For an unlisted app.",
            vec![("d", "com.example.Ghost"), ("rating", "1.0")],
        );
        // the reviewer's profile is read (review names); the
        // outsider's is not referenced by anything kept
        let reviewer_profile = signed(&reviewer, KIND_PROFILE, r#"{"name":"Reviewer"}"#, vec![]);
        let stranger_profile = signed(&outsider, KIND_PROFILE, r#"{"name":"Stranger"}"#, vec![]);

        let mut feed = Feed::new();
        absorb_all(
            &mut feed,
            &[
                listing, asset, release, foreign_asset, foreign_release,
                kept_review, stray_review, reviewer_profile, stranger_profile,
            ],
        );
        feed.trim();

        assert_eq!(feed.catalog().apps.len(), 1);
        let reviews = feed.reviews();
        assert_eq!(reviews.reviews.len(), 1);
        assert_eq!(reviews.reviews[0].content, "Good app.");
        assert!(feed
            .profiles
            .contains_key(&reviewer.get_public_key().unwrap().to_string()));
        assert!(!feed
            .profiles
            .contains_key(&outsider.get_public_key().unwrap().to_string()));
    }

    /// The change flag: a retained event arms it, taking disarms it,
    /// and clear arms it (the writer must rewrite the emptied file,
    /// or the next load restores what the user just dropped).
    #[test]
    fn dirty_flag_tracks_retained_events_and_clear() {
        let keys = demo_keys();
        let mut feed = Feed::new();
        assert!(!feed.take_dirty());
        let listing = signed(
            &keys,
            KIND_APP,
            "A listing.",
            vec![("d", "com.example.Hello"), ("f", "linux-x86_64")],
        );
        absorb_all(&mut feed, &[listing]);
        assert!(feed.take_dirty());
        assert!(!feed.take_dirty());
        feed.clear();
        assert!(feed.take_dirty());
    }
}
