//! Event builders for the publish side: the exact tags each NIP-82 event
//! carries, shared by the test publisher example and `orchard-publish`.
//! The read side ([`crate::Feed`]) accepts these; keeping both ends in one
//! crate means a tag change breaks the build instead of the network.

use anyhow::Result;
use nostr::prelude::*;

/// One app listing (kind 32267). `d` is the app id.
pub struct Listing {
    pub app_id: String,
    pub name: String,
    pub summary: String,
    pub description: Option<String>,
    pub icon: Option<String>,
    pub categories: Vec<String>,
    /// Platform tags the listing claims coverage for, one `f` tag
    /// each, e.g. `linux-x86_64`, `linux-aarch64`.
    pub platforms: Vec<String>,
    pub license: Option<String>,
    pub repository: Option<String>,
    /// The project's homepage, the NIP-82 `url` tag.
    pub homepage: Option<String>,
}

/// One downloadable Flatpak OCI component (kind 3063).
pub struct Asset {
    pub app_id: String,
    pub version: String,
    pub platform: String,
    /// Bare SHA-256 of the component bytes.
    pub digest: String,
    pub url: String,
    pub size: u64,
    pub filename: String,
    pub mime: String,
}

/// One release per application version, referencing every architecture.
pub struct Release {
    pub app_id: String,
    pub version: String,
    pub platforms: Vec<String>,
    pub channel: String,
    pub notes: Option<String>,
    pub asset_ids: Vec<String>,
}

/// One publisher profile (kind 0).
pub struct Profile {
    pub name: Option<String>,
    pub nip05: Option<String>,
}

/// One user review (kind 30065): `d` is the app id, content the text.
/// Replaceable per (pubkey, app id), so editing is republishing.
pub struct Review {
    pub app_id: String,
    /// 0.0 to 5.0, one decimal.
    pub rating: Option<f32>,
    pub content: String,
}

fn tag(name: &str, value: &str) -> Tag {
    Tag::custom(name, [value])
}

fn optional_tag(name: &str, value: &Option<String>) -> Option<Tag> {
    value.as_deref().filter(|v| !v.is_empty()).map(|v| tag(name, v))
}

pub fn sign_listing(keys: &Keys, listing: &Listing) -> Result<Event> {
    let builder = EventBuilder::new(Kind::Custom(crate::KIND_APP as u16), listing.description.as_deref().unwrap_or(""))
        .tag(tag("d", &listing.app_id))
        .tag(tag("name", &listing.name))
        .tag(tag("summary", &listing.summary))
        .tag_maybe(optional_tag("icon", &listing.icon))
        .tag_maybe(optional_tag("license", &listing.license))
        .tag_maybe(optional_tag("repository", &listing.repository))
        .tag_maybe(optional_tag("url", &listing.homepage));
    let builder = listing
        .categories
        .iter()
        .fold(builder, |builder, category| builder.tag(tag("t", category)));
    let builder = listing
        .platforms
        .iter()
        .fold(builder, |builder, platform| builder.tag(tag("f", platform)));
    Ok(builder.finalize(keys)?)
}

pub fn sign_asset(keys: &Keys, asset: &Asset) -> Result<Event> {
    sign_asset_with_legacy_location(keys, asset, None)
}

/// Add a registry location understood by Orchard v0.2.0 to a manifest asset.
/// Both locations must identify the same manifest digest.
pub fn sign_asset_with_legacy_location(keys: &Keys, asset: &Asset, legacy: Option<&str>) -> Result<Event> {
    if let Some(location) = legacy {
        if asset.mime != orchard_core::oci_asset::MIME {
            anyhow::bail!("only manifest assets can carry a legacy OCI location");
        }
        match orchard_core::oci::Location::parse(location)? {
            orchard_core::oci::Location::Registry { digest: Some(digest), .. }
                if orchard_core::oci_asset::hash_hex(&digest)? == asset.digest => {},
            _ => anyhow::bail!("legacy OCI location must pin the same manifest digest"),
        }
    }
    if asset.digest.len() != 64 { anyhow::bail!("asset hash must be bare SHA-256 hex"); }
    orchard_core::oci_asset::hash_hex(&asset.digest)?;
    if !(asset.url.starts_with("https://") || asset.url.starts_with("http://")) {
        anyhow::bail!("asset URL must use HTTP or HTTPS");
    }
    Ok(EventBuilder::new(Kind::Custom(crate::KIND_ASSET as u16), "")
        .tag(tag("i", &asset.app_id))
        .tag(tag("x", &asset.digest))
        .tag(tag("version", &asset.version))
        .tag(tag("f", &asset.platform))
        .tag(tag("m", &asset.mime))
        .tag(tag("T", orchard_core::oci_asset::PACKAGE))
        .tag(tag("url", &asset.url))
        .tag(tag("size", &asset.size.to_string()))
        .tag(tag("filename", &asset.filename))
        .tag_maybe(legacy.map(|location| tag("oci", location)))
        .finalize(keys)?)
}

pub fn sign_release(keys: &Keys, release: &Release) -> Result<Event> {
    if release.asset_ids.is_empty() { anyhow::bail!("release requires assets"); }
    let coordinate = format!("{}:{}:{}", crate::KIND_APP, keys.get_public_key()?, release.app_id);
    let mut builder = EventBuilder::new(
        Kind::Custom(crate::KIND_RELEASE as u16), release.notes.as_deref().unwrap_or(""))
        .tag(tag("d", &format!("{}@{}", release.app_id, release.version)))
        .tag(tag("a", &coordinate))
        .tag(tag("i", &release.app_id))
        .tag(tag("version", &release.version))
        .tag(tag("c", &release.channel));
    for platform in &release.platforms { builder = builder.tag(tag("f", platform)); }
    for id in &release.asset_ids { builder = builder.tag(tag("e", id)); }
    Ok(builder.finalize(keys)?)
}

pub fn sign_profile(keys: &Keys, profile: &Profile) -> Result<Event> {
    let metadata = serde_json::json!({
        "name": profile.name,
        "nip05": profile.nip05,
    });
    Ok(EventBuilder::new(Kind::Metadata, metadata.to_string()).finalize(keys)?)
}

/// Publish a metadata document verbatim. The client merges against the
/// author's latest kind 0 and republishes the whole object: kind 0 is
/// replaceable, so a partial publish would silently drop every field
/// not resent.
pub fn sign_profile_raw(keys: &Keys, raw_metadata: &str) -> Result<Event> {
    Ok(EventBuilder::new(Kind::Metadata, raw_metadata).finalize(keys)?)
}

pub fn sign_review(keys: &Keys, review: &Review) -> Result<Event> {
    let builder = EventBuilder::new(Kind::Custom(crate::KIND_REVIEW as u16), review.content.as_str())
        .tag(tag("d", &review.app_id));
    let builder = match review.rating {
        // one decimal keeps ratings uniform across publishers of reviews
        Some(value) => builder.tag(tag("rating", &format!("{value:.1}"))),
        None => builder,
    };
    Ok(builder.finalize(keys)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transition_tags_only_name_the_same_manifest() {
        let keys = Keys::generate();
        let mut asset = Asset {
            app_id: "com.example.Transition".into(), version: "1".into(),
            platform: "linux-x86_64".into(), digest: "a".repeat(64),
            url: format!("https://ghcr.io/v2/example/app/manifests/sha256:{}", "a".repeat(64)),
            size: 42, filename: "app.manifest.json".into(), mime: orchard_core::oci_asset::MIME.into(),
        };
        let legacy = format!("docker://ghcr.io/example/app@sha256:{}", asset.digest);
        let event = sign_asset_with_legacy_location(&keys, &asset, Some(&legacy)).unwrap();
        let parts = crate::Parts::parse(&event).unwrap();
        assert_eq!(parts.tag("oci").as_deref(), Some(legacy.as_str()));
        assert_eq!(parts.tag("url").as_deref(), Some(asset.url.as_str()));
        assert_eq!(parts.tag("x").as_deref(), Some(asset.digest.as_str()));
        assert!(crate::Parts::parse(&sign_asset(&keys, &asset).unwrap()).unwrap().tag("oci").is_none());
        assert!(sign_asset_with_legacy_location(&keys, &asset,
            Some(&format!("docker://ghcr.io/example/app@sha256:{}", "b".repeat(64)))).is_err());
        asset.mime = "application/vnd.oci.image.config.v1+json".into();
        assert!(sign_asset_with_legacy_location(&keys, &asset, Some(&legacy)).is_err());
    }

    /// The round-trip rule: a builder's event must absorb on the read
    /// side and project the same fields back.
    #[test]
    fn built_events_absorb_and_project() {
        let keys = Keys::generate();
        let listing = sign_listing(
            &keys,
            &Listing {
                app_id: "com.example.Hello".into(),
                name: "Hello".into(),
                summary: "A demo app".into(),
                description: Some("Long description.".into()),
                icon: Some("https://example.com/icon.png".into()),
                categories: vec!["Utilities".into()],
                platforms: vec!["linux-x86_64".into()],
                license: Some("MIT".into()),
                repository: None,
                homepage: None,
            },
        )
        .unwrap();
        let asset = sign_asset(
            &keys,
            &Asset {
                app_id: "com.example.Hello".into(),
                version: "1.0.0".into(),
                platform: "linux-x86_64".into(),
                digest: "ab".repeat(32),
                url: "https://example.com/hello.oci.json".into(),
                size: 123,
                filename: "hello.oci.json".into(),
                mime: orchard_core::oci_asset::MIME.into(),
            },
        )
        .unwrap();
        let release = sign_release(
            &keys,
            &Release {
                app_id: "com.example.Hello".into(),
                version: "1.0.0".into(),
                platforms: vec!["linux-x86_64".into()],
                channel: "stable".into(),
                notes: Some("First.".into()),
                asset_ids: vec![asset.id.to_string()],
            },
        )
        .unwrap();
        let parts = crate::Parts::parse(&release).unwrap();
        assert_eq!(parts.tag("d").as_deref(), Some("com.example.Hello@1.0.0"));
        assert_eq!(parts.tag("a").unwrap(), format!("32267:{}:com.example.Hello", keys.get_public_key().unwrap()));
        let asset_parts = crate::Parts::parse(&asset).unwrap();
        assert_eq!(asset_parts.tag("x").unwrap().len(), 64);
        assert_eq!(asset_parts.tag("T").as_deref(), Some("flatpak-oci"));
        assert!(asset_parts.tag("oci").is_none());
        let mut feed = crate::Feed::new();
        for event in [&listing, &asset, &release] {
            feed.absorb(event.clone()).unwrap();
        }
        let catalog = feed.catalog();
        assert_eq!(catalog.apps.len(), 1);
        let app = &catalog.apps[0];
        assert_eq!(app.version, "1.0.0");
        assert_eq!(app.name, "Hello");
        assert_eq!(app.categories, ["Utilities"]);
        assert_eq!(app.description.as_deref(), Some("Long description."));
    }

    /// Reviews: one per identity per app (replaceable), rating
    /// validated, author names resolved from profiles.
    #[test]
    fn reviews_absorb_replace_and_project() {
        let alice = Keys::generate();
        let bob = Keys::generate();
        let sign = |keys: &Keys, app_id: &str, rating: Option<f32>, text: &str| {
            sign_review(
                keys,
                &Review {
                    app_id: app_id.into(),
                    rating,
                    content: text.into(),
                },
            )
            .unwrap()
        };
        let mut feed = crate::Feed::new();

        // a rating outside 0.0 to 5.0 is a skip: not stored, and the
        // sync keeps running
        let bogus = EventBuilder::new(Kind::Custom(crate::KIND_REVIEW as u16), "bogus")
            .tag(Tag::custom("d", ["com.example.Hello"]))
            .tag(Tag::custom("rating", ["9.9"]))
            .finalize(&alice)
            .unwrap();
        assert!(!feed.absorb(bogus).unwrap());
        assert!(feed.reviews().reviews.is_empty());

        // alice reviews, then edits: the newest wins, one review remains
        feed.absorb(sign(&alice, "com.example.Hello", Some(3.0), "Meh."))
            .unwrap();
        feed.absorb(sign(&alice, "com.example.Hello", Some(5.0), "Great."))
            .unwrap();
        // bob reviews the same app: two identities, two reviews
        feed.absorb(sign(&bob, "com.example.Hello", None, "Text only."))
            .unwrap();
        // a review of another app does not collide
        feed.absorb(sign(&bob, "com.example.Other", Some(4.0), "Other app."))
            .unwrap();

        // replaceability: republish with a NEWER created_at to win
        let newer = EventBuilder::new(Kind::Custom(crate::KIND_REVIEW as u16), "Updated.")
            .tag(Tag::custom("d", ["com.example.Hello"]))
            .tag(Tag::custom("rating", ["5.0"]))
            .custom_created_at(Timestamp::now() + 10)
            .finalize(&alice)
            .unwrap();
        feed.absorb(newer).unwrap();

        let reviews = feed.reviews().reviews;
        let hello: Vec<_> = reviews.iter().filter(|r| r.app_id == "com.example.Hello").collect();
        assert_eq!(hello.len(), 2, "one per identity, edits replace");
        // newest first within the app
        assert_eq!(hello[0].content, "Updated.");
        assert_eq!(hello[0].rating, Some(5.0));
        assert_eq!(hello[1].content, "Text only.");
        assert_eq!(hello[1].rating, None);
        assert_eq!(hello[0].pubkey, alice.get_public_key().unwrap().to_string());
        // the other app's review projects under its own id
        assert_eq!(reviews.last().unwrap().content, "Other app.");
        assert_eq!(reviews.last().unwrap().app_id, "com.example.Other");
    }

    /// Review author names resolve from kind 0 profiles the feed has seen.
    #[test]
    fn reviews_carry_profile_names() {
        let keys = Keys::generate();
        let mut feed = crate::Feed::new();
        feed.absorb(sign_profile(&keys, &Profile { name: Some("Alice".into()), nip05: None }).unwrap())
            .unwrap();
        feed.absorb(
            sign_review(
                &keys,
                &Review {
                    app_id: "com.example.Hello".into(),
                    rating: Some(5.0),
                    content: "Hi.".into(),
                },
            )
            .unwrap(),
        )
        .unwrap();
        let reviews = feed.reviews().reviews;
        assert_eq!(reviews.len(), 1);
        assert_eq!(reviews[0].name.as_deref(), Some("Alice"));
        assert_eq!(reviews[0].pubkey, keys.get_public_key().unwrap().to_string());
    }
}
