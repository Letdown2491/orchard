//! Prepare Flatpak OCI manifests and publish standard NIP-82 releases.
//! Preparation needs no publishing identity. A release may combine prepared
//! artifacts from separate architecture jobs, with one publisher signing it.
use anyhow::{Context, Result, bail};
use nostr::prelude::*;
use orchard_nostr::events::{self, Asset, Listing, Profile, Release};
use orchard_nostr::relay;
use std::path::PathBuf;
mod artifact;
mod manifest;
mod prepared;
mod proof;
mod push;

fn main() {
    if let Err(err) = run() {
        eprintln!("orchard-publish: {err:#}");
        std::process::exit(1);
    }
}

#[derive(Default)]
struct Args {
    manifest_path: PathBuf,
    prepare: bool,
    upload_only: bool,
    legacy_oci: bool,
    assets: Vec<PathBuf>,
    manifest_url: Option<String>,
    out: Option<PathBuf>,
    arch: Option<String>,
    relays: Option<Vec<String>>,
    nsec: Option<String>,
    skip_relays: bool,
    skip_push: bool,
    prove: bool,
    registry: Option<String>,
    tag: Option<String>,
}

fn parse_args() -> Result<Args> {
    let mut values = std::env::args().skip(1);
    let mut args = Args::default();
    let mut manifest = None;
    while let Some(flag) = values.next() {
        let mut next = || {
            values
                .next()
                .with_context(|| format!("missing value for {flag}"))
        };
        match flag.as_str() {
            "--prepare" => args.prepare = true,
            "--upload-only" => args.upload_only = true,
            "--legacy-oci" => args.legacy_oci = true,
            "--asset" => args.assets.push(next()?.into()),
            "--manifest-url" => args.manifest_url = Some(next()?),
            "--out" => args.out = Some(next()?.into()),
            "--arch" => args.arch = Some(next()?),
            "--relays" => args.relays = Some(relay::parse_urls(&next()?)),
            "--nsec" => args.nsec = Some(next()?),
            "--skip-relays" => args.skip_relays = true,
            "--skip-push" => args.skip_push = true,
            "--prove" => args.prove = true,
            "--registry" => args.registry = Some(next()?),
            "--tag" => args.tag = Some(next()?),
            other if other.starts_with('-') => bail!("unknown flag {other:?}"),
            other => {
                if manifest.replace(PathBuf::from(other)).is_some() {
                    bail!("one manifest per run");
                }
            }
        }
    }
    args.manifest_path = manifest.context(
        "usage: orchard-publish <manifest.toml> [--prepare] [--manifest-url URL] [--asset FILE ...]",
    )?;
    if args.prepare
        && (!args.assets.is_empty() || args.prove || args.registry.is_some() || args.upload_only)
    {
        bail!("--prepare cannot publish assets, upload to a registry, or write repository proofs");
    }
    if args.upload_only && (args.registry.is_none() || args.prove || args.skip_push) {
        bail!("--upload-only requires --registry and cannot use --skip-push or write a repository proof");
    }
    if !args.assets.is_empty() && (args.arch.is_some() || args.manifest_url.is_some()) {
        bail!("--asset uses its recorded architecture and URL; build flags cannot override them");
    }
    Ok(args)
}

fn run() -> Result<()> {
    let args = parse_args()?;
    let mut manifest = manifest::load(&args.manifest_path)?;
    if let Ok(version) = std::env::var("ORCHARD_PUBLISH_VERSION") {
        if !version.trim().is_empty() {
            manifest.release.version = version.trim().into();
        }
    }
    if let Some(arch) = &args.arch {
        manifest.release.arch = arch.clone();
    }
    if !args.prepare && !args.upload_only && !args.skip_relays && args.relays.is_none() {
        if let Some(publish) = manifest.publish.as_ref().filter(|p| !p.relays.is_empty()) {
            if !publish
                .relays
                .iter()
                .any(|url| url == relay::REQUIRED_RELAY)
            {
                bail!(
                    "[publish] relays must include {}; --relays overrides for local tests",
                    relay::REQUIRED_RELAY
                );
            }
        }
    }
    let app_id = &manifest.app.id;
    let version = &manifest.release.version;
    let mut paths = args.assets.clone();
    if paths.is_empty() {
        let out = args
            .out
            .clone()
            .unwrap_or_else(|| PathBuf::from(format!("target/publish/{app_id}")));
        let built = artifact::build(
            &manifest.artifact.tree,
            &out.join(format!("{app_id}-{}.layout", manifest.release.arch)),
            app_id,
            version,
            &manifest.ref_arch(),
            manifest.release.notes.as_deref(),
        )?;
        eprintln!(
            "built {}: manifest {}, layer {}, diff {}, download {} bytes, installed {} bytes",
            app_id,
            built.manifest_digest,
            built.layer_digest,
            built.diff_id,
            built.download_size,
            built.installed_size
        );
        let path = prepared::write(
            &built.layout,
            &out,
            app_id,
            version,
            &manifest.release.arch,
            args.manifest_url.clone(),
        )?;
        eprintln!("prepared {}", path.display());
        if args.prepare {
            return Ok(());
        }
        paths.push(path);
    }
    let mut assets = paths
        .iter()
        .map(|path| {
            if args.registry.is_some() && !args.skip_push {
                prepared::load(path)
            } else {
                prepared::load_metadata(path)
            }
        })
        .collect::<Result<Vec<_>>>()?;
    // Validate identity before uploading; signing requires complete coverage.
    validate_assets(&manifest, &assets, !args.upload_only)?;
    for (path, asset) in paths.iter().zip(&mut assets) {
        if let Some(registry) = &args.registry {
            let digest = if args.skip_push {
                format!("sha256:{}", asset.sha256)
            } else {
                let tag = args
                    .tag
                    .as_deref()
                    .or_else(|| manifest.publish.as_ref().map(|p| p.tag.as_str()))
                    .unwrap_or("latest");
                let layout = path
                    .parent()
                    .unwrap_or(std::path::Path::new("."))
                    .join(&asset.layout);
                let digest =
                    push::push_layout(registry, &format!("{tag}-{}", asset.platform), &layout)?;
                if orchard_core::oci_asset::hash_hex(&digest)? != asset.sha256 {
                    bail!("registry manifest differs from prepared artifact");
                }
                digest
            };
            asset.url = Some(push::manifest_url(registry, &digest)?);
            for component in &mut asset.components {
                component.url = Some(push::blob_url(registry, &format!("sha256:{}", component.sha256))?);
            }
            std::fs::write(path, serde_json::to_vec_pretty(&asset)?)?;
        }
        let url = asset
            .url
            .as_deref()
            .context("publish needs --registry or a prepared manifest URL")?;
        let location = orchard_core::oci::Location::parse(url)?;
        let components = asset.blob_assets()?;
        if !args.skip_relays {
            let report = orchard_core::oci::verify_with_assets(
                &location, Some(&format!("sha256:{}", asset.sha256)), Some(&components))?;
            prepared::validate_report(asset, &report)?;
        }
    }
    if args.upload_only {
        return Ok(());
    }
    // Validate transition locations before loading keys or publishing anything.
    let legacy_locations = assets.iter().map(|asset| {
        if args.legacy_oci {
            Ok(Some(push::legacy_location(asset.url.as_deref().context("manifest needs a URL")?, &asset.sha256)?))
        } else { Ok(None) }
    }).collect::<Result<Vec<_>>>()?;
    // Preparation and upload-only return before loading a signing identity.
    let keys = match args.nsec.or_else(|| std::env::var("ORCHARD_NSEC").ok()) {
        Some(raw) => Keys::parse(raw.trim())?,
        None => match manifest.publish.as_ref().and_then(|p| p.key_file.as_ref()) {
            Some(path) => Keys::parse(std::fs::read_to_string(path)?.trim())?,
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
    let mut signed = Vec::new();
    for (asset, legacy) in assets.iter().zip(&legacy_locations) {
        let components = std::iter::once((
            &asset.sha256, &asset.mime, asset.size, &asset.filename, &asset.url,
        )).chain(asset.components.iter().map(|c| (&c.sha256, &c.mime, c.size, &c.filename, &c.url)));
        for (hash, mime, size, filename, url) in components {
            signed.push(events::sign_asset_with_legacy_location(&keys, &Asset {
                app_id: app_id.clone(), version: version.clone(),
                platform: asset.platform.clone(), digest: hash.clone(),
                url: url.clone().context("component needs a public URL")?,
                size, filename: filename.clone(), mime: mime.clone(),
            }, if mime == orchard_core::oci_asset::MIME { legacy.as_deref() } else { None })?);
        }
    }
    signed.push(events::sign_release(
        &keys,
        &Release {
            app_id: app_id.clone(),
            version: version.clone(),
            channel: manifest.release.channel.clone(),
            notes: manifest.release.notes.clone(),
            platforms: assets.iter().map(|a| a.platform.clone()).collect(),
            asset_ids: signed.iter().map(|e| e.id.to_string()).collect(),
        },
    )?);
    signed.push(events::sign_listing(
        &keys,
        &Listing {
            app_id: app_id.clone(),
            name: manifest.app.name.clone(),
            summary: manifest.app.summary.clone(),
            description: manifest.app.description.clone(),
            icon: manifest.app.icon.clone(),
            screenshots: manifest.app.screenshots.clone(),
            categories: manifest.app.categories.clone(),
            platforms: manifest.listing_platforms(),
            license: manifest.app.license.clone(),
            repository: manifest.app.repository.clone(),
            homepage: manifest.app.homepage.clone(),
        },
    )?);
    if let Some(publish) = &manifest.publish {
        if publish.author_name.is_some() || publish.nip05.is_some() {
            signed.push(events::sign_profile(
                &keys,
                &Profile {
                    name: publish.author_name.clone(),
                    nip05: publish.nip05.clone(),
                },
            )?);
        }
    }
    if args.skip_relays {
        for event in &signed {
            println!("{}", event.as_json());
        }
    } else {
        let relays = args
            .relays
            .or_else(|| {
                manifest
                    .publish
                    .as_ref()
                    .filter(|p| !p.relays.is_empty())
                    .map(|p| p.relays.clone())
            })
            .unwrap_or_else(|| vec!["ws://127.0.0.1:8008".into()]);
        for url in relays {
            relay::send(&url, &signed)?;
        }
        println!("published {} assets in one release", assets.len());
    }
    if args.prove && !args.skip_relays {
        use nostr::nips::nip19::ToBech32;
        let repo = manifest
            .app
            .repository
            .as_deref()
            .and_then(proof::github_repo)
            .context("--prove requires a GitHub repository URL")?;
        let token = std::env::var("GITHUB_TOKEN").context("--prove requires GITHUB_TOKEN")?;
        proof::write_proof(&repo, &keys.get_public_key()?.to_bech32()?, &token)?;
    }
    Ok(())
}

fn validate_assets(
    manifest: &manifest::Manifest,
    assets: &[prepared::Prepared],
    complete: bool,
) -> Result<()> {
    let mut platforms = std::collections::BTreeSet::new();
    for asset in assets {
        if asset.app_id != manifest.app.id || asset.version != manifest.release.version {
            bail!("prepared asset belongs to another application or version");
        }
        if !platforms.insert(asset.platform.clone()) {
            bail!("duplicate prepared platform");
        }
        if let Some(url) = &asset.url {
            if !(url.starts_with("https://") || url.starts_with("http://")) {
                bail!("manifest URL must use HTTP or HTTPS");
            }
        }
    }
    let expected: std::collections::BTreeSet<_> =
        manifest.listing_platforms().into_iter().collect();
    if (complete && platforms != expected) || !platforms.is_subset(&expected) {
        bail!("publish all declared platforms together: expected {expected:?}, got {platforms:?}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn publication_requires_exact_platform_coverage_and_matching_identity() {
        let manifest: manifest::Manifest = toml::from_str(
            r#"
[app]
id = "com.example.App"
name = "App"
summary = "Example"
[release]
version = "1"
platforms = ["linux-x86_64", "linux-aarch64"]
[artifact]
tree = "absent"
"#,
        )
        .unwrap();
        let make = |platform: &str| prepared::Prepared {
            schema: 2,
            app_id: "com.example.App".into(),
            version: "1".into(),
            platform: platform.into(),
            package: "flatpak-oci".into(),
            components: Vec::new(),
            mime: orchard_core::oci_asset::MIME.into(),
            filename: format!("{platform}.json"),
            layout: format!("{platform}.layout"),
            sha256: "a".repeat(64),
            size: 1,
            url: Some(format!("https://example.com/{platform}.tgz")),
        };
        assert!(validate_assets(&manifest, &[make("linux-x86_64")], true).is_err());
        assert!(
            validate_assets(
                &manifest,
                &[make("linux-x86_64"), make("linux-x86_64")],
                true
            )
            .is_err()
        );
        assert!(validate_assets(&manifest, &[make("linux-x86_64")], false).is_ok());
        assert!(validate_assets(&manifest, &[make("linux-riscv64")], false).is_err());
        let mut assets = [make("linux-x86_64"), make("linux-aarch64")];
        assert!(validate_assets(&manifest, &assets, true).is_ok());
        assets[1].version = "2".into();
        assert!(validate_assets(&manifest, &assets, true).is_err());
    }
}
