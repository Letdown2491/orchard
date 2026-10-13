//! Publisher CLI: the whole M5 pipeline in one command.
//!
//!     orchard-publish app.publish.toml \
//!       --registry localhost:5000/hello    # push (else local layout only)
//!
//! Steps: parse the manifest, build the app tree into a deterministic OCI
//! layout, push to the publisher's registry (optional), sign the NIP-82
//! events (asset, release, listing, optional profile), send them to the
//! relays. Sandbox permissions never leave the artifact: the ini in the
//! tree rides the image config labels, and the event pins the manifest
//! digest that carries them.

use std::path::PathBuf;

use anyhow::{bail, Result};
use nostr::prelude::*;

mod artifact;
mod manifest;
mod proof;
mod push;

use orchard_nostr::events::{self, Asset, Listing, Profile, Release};
use orchard_nostr::relay;

fn main() {
    if let Err(err) = run() {
        eprintln!("orchard-publish: {err:#}");
        std::process::exit(1);
    }
}

struct Args {
    manifest_path: PathBuf,
    registry: Option<String>,
    /// Override the manifest's push tag: CI's matrix pushes one image
    /// per arch, and two legs writing one tag would flap `latest`
    /// between architectures. The event's location carries the digest
    /// form, so this tag never decides what a client installs; it is
    /// a stable handle for humans and container tools.
    tag: Option<String>,
    relays: Option<Vec<String>>,
    nsec: Option<String>,
    out: Option<PathBuf>,
    arch: Option<String>,
    skip_relays: bool,
    skip_push: bool,
    /// After a successful publish, write the publisher's hex key to
    /// `orchard.txt` at the manifest's repository (a `GITHUB_TOKEN`
    /// with `contents:write` must be set): the repository proof.
    prove: bool,
}

fn parse_args() -> Result<Args> {
    let mut values = std::env::args().skip(1);
    let mut manifest_path = None;
    let mut registry = None;
    let mut tag = None;
    let mut relays = None;
    let mut nsec = std::env::var("ORCHARD_NSEC").ok();
    let mut out = None;
    let mut skip_relays = false;
    let mut skip_push = false;
    let mut prove = false;
    let mut arch = None;
    while let Some(flag) = values.next() {
        let mut next = || {
            values.next().ok_or_else(|| anyhow::anyhow!("missing value for {flag}"))
        };
        match flag.as_str() {
            "--registry" => registry = Some(next()?),
            "--tag" => tag = Some(next()?),
            "--relays" => {
                relays = Some(
                    next()?
                        .split(',')
                        .map(str::trim)
                        .filter(|url| !url.is_empty())
                        .map(str::to_string)
                        .collect(),
                )
            }
            "--nsec" => nsec = Some(next()?),
            "--out" => out = Some(PathBuf::from(next()?)),
            "--arch" => arch = Some(next()?),
            "--skip-relays" => skip_relays = true,
            "--skip-push" => skip_push = true,
            "--prove" => prove = true,
            other if other.starts_with('-') => bail!("unknown flag {other:?}"),
            other => {
                if manifest_path.is_some() {
                    bail!("one manifest per run (got {other:?} after another)")
                }
                manifest_path = Some(PathBuf::from(other));
            }
        }
    }
    Ok(Args {
        manifest_path: manifest_path.ok_or_else(|| anyhow::anyhow!("usage: orchard-publish <manifest.toml> [--registry registry/repo] [--tag tag] [--relays urls]"))?,
        registry,
        tag,
        relays,
        nsec,
        out,
        arch,
        skip_relays,
        skip_push,
        prove,
    })
}

fn run() -> Result<()> {
    let args = parse_args()?;
    let mut manifest = manifest::load(&args.manifest_path)?;
    // The required relay is publish policy, checked before any build or
    // push: a manifest-driven send must reach the relay the project
    // operates, because the third-party relays can refuse or drop
    // Orchard events at any time. Three paths are exempt:
    // --skip-relays publishes nothing, --relays is the documented
    // local-test override, and a manifest with no [publish] section
    // targets the dev relay.
    if !args.skip_relays
        && args.relays.is_none()
        && let Some(publish) = manifest.publish.as_ref().filter(|p| !p.relays.is_empty())
        && !publish.relays.iter().any(|url| url == relay::REQUIRED_RELAY)
    {
        bail!(
            "[publish] relays must include {}, the required rendezvous relay; \
             --relays bypasses this check for local tests",
            relay::REQUIRED_RELAY
        );
    }
    // CI derives the version from the pushed tag; the manifest's
    // version is the local default.
    if let Ok(from_env) = std::env::var("ORCHARD_PUBLISH_VERSION")
        && !from_env.trim().is_empty()
    {
        manifest.release.version = from_env.trim().to_string();
    }
    // CI's arch legs override the manifest's build arch: the
    // artifact, asset, and release of this run are for that machine,
    // while the listing keeps its full declared coverage
    if let Some(arch) = &args.arch {
        manifest.release.arch = arch.clone();
    }
    let app_id = &manifest.app.id;
    let version = &manifest.release.version;

    // keys: flag beats env beats manifest key file; nothing means
    // generate (with a loud warning, an ephemeral identity is almost
    // never what a publisher wants)
    let keys = match args.nsec.as_deref().map(str::to_string) {
        Some(raw) => Keys::parse(raw.trim())?,
        None => match &manifest.publish.as_ref().and_then(|p| p.key_file.as_ref()) {
            Some(key_file) => {
                let raw = std::fs::read_to_string(key_file).map(|content| content.trim().to_string())
                    .map_err(|err| anyhow::anyhow!("reading key file {}: {err}", key_file.display()))?;
                Keys::parse(&raw)?
            }
            None => {
                let generated = Keys::generate();
                eprintln!(
                    "no key: generated an ephemeral identity, LOSE IT AND LOSE YOUR APP:\n  nsec {}",
                    generated.secret_key().to_secret_hex()
                );
                generated
            }
        },
    };
    let publisher = keys.get_public_key()?.to_string();
    use nostr::nips::nip19::ToBech32 as _;
    let publisher_npub = keys
        .get_public_key()?
        .to_bech32()
        .map_err(|err| anyhow::anyhow!("npub encoding: {err}"))?;
    println!("publisher {publisher} ({publisher_npub})");

    // build
    let out = args
        .out
        .clone()
        .unwrap_or_else(|| PathBuf::from(format!("target/publish/{app_id}")));
    let built = artifact::build(
        &manifest.artifact.tree,
        &out,
        app_id,
        version,
        &manifest.ref_arch(),
        manifest.release.notes.as_deref(),
    )?;
    println!(
        "built {} {}:\n  layer    {}\n  diff id  {}\n  artifact {}",
        app_id,
        version,
        built.layer_digest,
        built.diff_id,
        built.manifest_digest
    );

    // push: the location carries the digest form, so the client's
    // verify and install both fetch the exact bytes this run built;
    // the tag is a per-arch handle for humans and container tools
    let location = match (&args.registry, args.skip_push) {
        (Some(registry_repo), false) => {
            let tag = args
                .tag
                .as_deref()
                .or_else(|| {
                    manifest
                        .publish
                        .as_ref()
                        .map(|p| p.tag.as_str())
                })
                .unwrap_or("latest");
            let digest = push::push_layout(registry_repo, tag, &built.layout)?;
            if digest != built.manifest_digest {
                bail!("registry serves {digest}, built {built_digest}", built_digest = built.manifest_digest);
            }
            println!("pushed as {registry_repo}:{tag}");
            format!("docker://{registry_repo}@{digest}")
        }
        _ => format!("oci:{}", built.layout.display()),
    };

    // sign: asset first so the release can reference its id. The
    // asset and release carry this run's arch (the --arch override
    // for CI matrix legs); the listing carries the full declared
    // coverage, identical from every leg.
    let asset = events::sign_asset(
        &keys,
        &Asset {
            app_id: app_id.clone(),
            version: version.clone(),
            platform: manifest.release.arch.clone(),
            digest: built.manifest_digest.clone(),
            location: location.clone(),
            runtime: manifest.runtime_for_build(),
        },
    )?;
    let release = events::sign_release(
        &keys,
        &Release {
            app_id: app_id.clone(),
            version: version.clone(),
            platform: manifest.release.arch.clone(),
            channel: manifest.release.channel.clone(),
            notes: manifest.release.notes.clone(),
            asset_id: asset.id.to_string(),
        },
    )?;
    let listing = events::sign_listing(
        &keys,
        &Listing {
            app_id: app_id.clone(),
            name: manifest.app.name.clone(),
            summary: manifest.app.summary.clone(),
            description: manifest.app.description.clone(),
            icon: manifest.app.icon.clone(),
            categories: manifest.app.categories.clone(),
            platforms: manifest.listing_platforms(),
            license: manifest.app.license.clone(),
            repository: manifest.app.repository.clone(),
            homepage: manifest.app.homepage.clone(),
        },
    )?;
    let mut signed = vec![asset, release, listing];
    if let Some(publish) = &manifest.publish
        && (publish.author_name.is_some() || publish.nip05.is_some())
    {
        signed.push(events::sign_profile(
            &keys,
            &Profile {
                name: publish.author_name.clone(),
                nip05: publish.nip05.clone(),
            },
        )?);
    }

    // send
    if args.skip_relays {
        println!("{} events signed, not sent (--skip-relays)", signed.len());
    } else {
        let default_relays = vec!["ws://127.0.0.1:8008".to_string()];
        let relays = args.relays.clone().or_else(|| {
            manifest
                .publish
                .as_ref()
                .filter(|p| !p.relays.is_empty())
                .map(|p| p.relays.clone())
        });
        for relay_url in relays.unwrap_or(default_relays) {
            relay::send(&relay_url, &signed)?;
        }
        println!("{} events published to the relays", signed.len());
    }

    println!(
        "done:\n  app       {app_id} {version}\n  artifact  {}\n  location  {location}\n  sizes     {} down, {} installed",
        built.manifest_digest,
        human_bytes(built.download_size),
        human_bytes(built.installed_size)
    );

    // repository proof: after a successful publish, write the
    // publisher's key to the manifest's repository. Needs --prove and
    // a GITHUB_TOKEN with contents:write; a local run without the
    // token skips with a note, never an error.
    if args.prove {
        let repo = manifest.app.repository.as_deref().and_then(proof::github_repo);
        match repo {
            Some(repo) => {
                let token = std::env::var("GITHUB_TOKEN").unwrap_or_default();
                if token.trim().is_empty() {
                    eprintln!("proof: --prove set but GITHUB_TOKEN is not; the repository {repo} keeps whatever proof it has");
                } else {
                    proof::write_proof(&repo, &publisher_npub, token.trim())?;
                    println!("proof: orchard.txt refreshed at {repo}");
                }
            }
            None => eprintln!(
                "proof: --prove set but the manifest's repository is not a github repo; nothing proved"
            ),
        }
    }
    Ok(())
}

/// Same scale the client shows (`human_size` in the store): a size a
/// human can compare against the download page.
fn human_bytes(size: u64) -> String {
    const KB: f64 = 1024.0;
    let size = size as f64;
    if size >= KB * KB {
        format!("{:.1} MiB", size / (KB * KB))
    } else if size >= KB {
        format!("{:.1} KiB", size / KB)
    } else {
        format!("{size} B")
    }
}
