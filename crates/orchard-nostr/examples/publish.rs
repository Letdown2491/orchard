//! Sign and publish NIP-82 events for one app release: the hand-rolled
//! test publisher for the M3 vertical slice, kept as a reference for the
//! event side of `orchard-publish` (which adds the artifact build and
//! manifest preparation around it).
//!
//! Usage (all keys/options long-form, no dependencies):
//!
//!     publish \
//!       --nsec 5122b2...            # or ORCHARD_NSEC env; omitted: generate + print
//!       --relay ws://127.0.0.1:8008 # repeatable
//!       --app-id com.example.App
//!       --name "Example App" --summary "Does a thing"
//!       --description "Longer markdown description."
//!       --icon https://example.com/icon.png --category Utilities
//!       --version 1.2.0
//!       --location https://example.com/app.manifest.json
//!       --digest abcd...                  # manifest SHA-256, 64 hex chars
//!       --size 123456                     # manifest bytes
//!       --channel stable --notes "What changed."
//!       --author-name "Example Dev" --nip05 dev@example.com

use anyhow::{bail, Result};
use nostr::prelude::*;
use orchard_nostr::events::{self, Asset, Listing, Profile, Release};
use orchard_nostr::relay;

fn main() {
    if let Err(err) = run() {
        eprintln!("publish failed: {err:#}");
        std::process::exit(1);
    }
}

struct Args {
    nsec: Option<String>,
    relays: Vec<String>,
    app_id: String,
    name: String,
    summary: String,
    description: Option<String>,
    icon: Option<String>,
    categories: Vec<String>,
    screenshots: Vec<String>,
    version: String,
    location: String,
    digest: String,
    size: u64,
    channel: String,
    notes: Option<String>,
    author_name: Option<String>,
    nip05: Option<String>,
    homepage: Option<String>,
    repository: Option<String>,
    arch: String,
}

fn parse_args() -> Result<Args> {
    let mut values = std::env::args().skip(1);
    let mut nsec = std::env::var("ORCHARD_NSEC").ok();
    let mut relays = vec!["ws://127.0.0.1:8008".to_string()];
    let mut app_id = None;
    let mut name = None;
    let mut summary = None;
    let mut description = None;
    let mut icon = None;
    let mut categories = Vec::new();
    let mut screenshots = Vec::new();
    let mut version = None;
    let mut location = None;
    let mut digest = None;
    let mut size = None;
    let mut channel = "stable".to_string();
    let mut notes = None;
    let mut author_name = None;
    let mut nip05 = None;
    let mut homepage = None;
    let mut repository = None;
    let mut arch = "linux-x86_64".to_string();
    while let Some(flag) = values.next() {
        let mut next = || {
            values.next().ok_or_else(|| anyhow::anyhow!("missing value for {flag}"))
        };
        match flag.as_str() {
            "--nsec" => nsec = Some(next()?),
            "--relay" => relays = vec![next()?],
            "--app-id" => app_id = Some(next()?),
            "--name" => name = Some(next()?),
            "--summary" => summary = Some(next()?),
            "--description" => description = Some(next()?),
            "--icon" => icon = Some(next()?),
            "--category" => categories.push(next()?),
            "--screenshot" => screenshots.push(next()?),
            "--version" => version = Some(next()?),
            "--location" => location = Some(next()?),
            "--digest" => digest = Some(next()?),
            "--size" => size = Some(next()?.parse::<u64>()?),
            "--channel" => channel = next()?,
            "--notes" => notes = Some(next()?),
            "--author-name" => author_name = Some(next()?),
            "--nip05" => nip05 = Some(next()?),
            "--homepage" => homepage = Some(next()?),
            "--repository" => repository = Some(next()?),
            "--arch" => arch = next()?,
            other => bail!("unknown argument {other:?}"),
        }
    }
    Ok(Args {
        nsec,
        relays,
        app_id: app_id.ok_or_else(|| anyhow::anyhow!("--app-id required"))?,
        name: name.ok_or_else(|| anyhow::anyhow!("--name required"))?,
        summary: summary.ok_or_else(|| anyhow::anyhow!("--summary required"))?,
        description,
        icon,
        categories,
        screenshots,
        version: version.ok_or_else(|| anyhow::anyhow!("--version required"))?,
        location: location.ok_or_else(|| anyhow::anyhow!("--location required"))?,
        digest: digest.ok_or_else(|| anyhow::anyhow!("--digest required"))?,
        size: size.ok_or_else(|| anyhow::anyhow!("--size required"))?,
        channel,
        notes,
        author_name,
        nip05,
        homepage,
        repository,
        arch,
    })
}

fn run() -> Result<()> {
    let args = parse_args()?;

    // keys: explicit nsec beats env; nothing given means generate once and
    // print so the operator can pin it in ORCHARD_NSEC for next time
    let keys = match args.nsec.as_deref() {
        Some(raw) => Keys::parse(raw.trim())?,
        None => {
            let generated = Keys::generate();
            eprintln!(
                "no key given: generated ephemeral key, USE IT AGAIN OR LOSE THE IDENTITY:\n  nsec {}",
                generated.secret_key().to_secret_hex()
            );
            generated
        }
    };
    println!("publisher {}", keys.get_public_key()?.to_string());

    // asset first: the release references its event id
    let asset = events::sign_asset(
        &keys,
        &Asset {
            app_id: args.app_id.clone(),
            version: args.version.clone(),
            platform: args.arch.clone(),
            digest: args.digest.trim_start_matches("sha256:").to_string(),
            url: args.location.clone(),
            size: args.size,
            filename: "app.manifest.json".into(),
            mime: orchard_core::oci_asset::MIME.into(),
        },
    )?;
    let release = events::sign_release(
        &keys,
        &Release {
            app_id: args.app_id.clone(),
            version: args.version.clone(),
            platforms: vec![args.arch.clone()],
            channel: args.channel.clone(),
            notes: args.notes.clone(),
            asset_ids: vec![asset.id.to_string()],
        },
    )?;
    let listing = events::sign_listing(
        &keys,
        &Listing {
            app_id: args.app_id.clone(),
            name: args.name.clone(),
            summary: args.summary.clone(),
            description: args.description.clone(),
            icon: args.icon.clone(),
            categories: args.categories.clone(),
            platforms: vec![args.arch.clone()],
            screenshots: args.screenshots.clone(),
            license: None,
            repository: args.repository.clone(),
            homepage: args.homepage.clone(),
        },
    )?;

    let mut signed = vec![asset, release, listing];
    if args.author_name.is_some() || args.nip05.is_some() {
        // profile last so a fresh relay session has the listing already
        signed.push(events::sign_profile(
            &keys,
            &Profile {
                name: Some(args.author_name.clone().unwrap_or_else(|| args.name.clone())),
                nip05: args.nip05.clone(),
            },
        )?);
    }

    for relay_url in &args.relays {
        relay::send(relay_url, &signed)?;
    }
    println!("published {} events", signed.len());
    Ok(())
}
