//! The publish manifest (TOML): everything the publisher owns that is not
//! in the artifact itself. Listing fields become kind 32267 tags, the
//! release fields become kind 30063, and the artifact tree is what gets
//! packed and pushed. Sandbox permissions live in the tree's `metadata`
//! ini, never here.

use anyhow::{bail, Context as _, Result};
use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub struct Manifest {
    pub app: AppMeta,
    pub release: ReleaseMeta,
    pub artifact: ArtifactMeta,
    pub publish: Option<PublishMeta>,
}

#[derive(Debug, Deserialize)]
pub struct AppMeta {
    pub id: String,
    pub name: String,
    pub summary: String,
    pub description: Option<String>,
    /// Path relative to the manifest file; wins over `description` when
    /// both are set.
    #[serde(rename = "description-file")]
    pub description_file: Option<PathBuf>,
    pub icon: Option<String>,
    #[serde(default)]
    pub categories: Vec<String>,
    pub license: Option<String>,
    pub repository: Option<String>,
    /// The project's homepage, published as the `url` tag.
    pub homepage: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct ReleaseMeta {
    pub version: String,
    #[serde(default = "default_channel")]
    pub channel: String,
    /// Flatpak arch tag, e.g. `linux-x86_64`. The build target and
    /// the identity of the asset and release this run signs. The
    /// `--arch` flag overrides it for CI matrix legs.
    #[serde(default = "default_arch")]
    pub arch: String,
    /// Platform tags the LISTING claims coverage for, one per built
    /// platform, e.g. `["linux-x86_64", "linux-aarch64"]`. Every arch
    /// leg publishes the same listing, so the tags must list them
    /// all. Default: just [`ReleaseMeta::arch`].
    #[serde(default)]
    pub platforms: Vec<String>,
    pub notes: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct ArtifactMeta {
    /// The flatpak app tree: `files/`, `metadata`, `export/`. The
    /// `metadata` ini is the sandbox source of truth and is baked into
    /// the image config labels byte-exact.
    pub tree: PathBuf,
}

#[derive(Debug, Deserialize)]
pub struct PublishMeta {
    #[serde(default)]
    pub relays: Vec<String>,
    /// Path to a file holding the publisher key (hex or nsec). Beats
    /// nothing; the ORCHARD_NSEC env beats this.
    #[serde(rename = "key-file")]
    pub key_file: Option<PathBuf>,
    /// OCI tag to push the manifest as.
    #[serde(default = "default_tag")]
    pub tag: String,
    #[serde(rename = "author-name")]
    pub author_name: Option<String>,
    #[serde(rename = "nip05")]
    pub nip05: Option<String>,
}

fn default_channel() -> String {
    "stable".into()
}

fn default_arch() -> String {
    "linux-x86_64".into()
}

fn default_tag() -> String {
    "latest".into()
}

impl Manifest {
    /// The platform tags the listing claims: the declared list, or
    /// the build arch alone when none were listed.
    pub fn listing_platforms(&self) -> Vec<String> {
        if self.release.platforms.is_empty() {
            vec![self.release.arch.clone()]
        } else {
            self.release.platforms.clone()
        }
    }

    /// Flatpak ref architecture (`linux-x86_64` to `x86_64`).
    pub fn ref_arch(&self) -> String {
        self.release
            .arch
            .strip_prefix("linux-")
            .unwrap_or(&self.release.arch)
            .to_string()
    }
}

/// Load and validate a manifest. Paths inside resolve relative to the
/// manifest file's directory.
pub fn load(path: &Path) -> Result<Manifest> {
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("reading {}", path.display()))?;
    let mut manifest: Manifest = toml::from_str(&raw)
        .with_context(|| format!("parsing {}", path.display()))?;
    let base = path.parent().unwrap_or(Path::new("."));

    if manifest.app.id.is_empty() {
        bail!("manifest: app.id is required");
    }
    if manifest.release.version.is_empty() {
        bail!("manifest: release.version is required");
    }
    // resolve the description file and the artifact tree now, so later
    // stages can treat them as plain paths
    if let Some(file) = manifest.app.description_file.take() {
        let resolved = base.join(file);
        manifest.app.description = Some(
            std::fs::read_to_string(&resolved)
                .with_context(|| format!("reading {}", resolved.display()))?,
        );
    }
    manifest.artifact.tree = base.join(&manifest.artifact.tree);
    Ok(manifest)
}

use std::path::{Path, PathBuf};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_resolves_relative_paths() {
        let dir = std::env::temp_dir().join(format!("orchard-manifest-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("tree")).unwrap();
        std::fs::write(dir.join("tree/metadata"), "[Application]\nname=x\n").unwrap();
        std::fs::write(dir.join("description.md"), "Long description.").unwrap();
        let path = dir.join("app.toml");
        std::fs::write(
            &path,
            r#"
[app]
id = "com.example.App"
name = "App"
summary = "Does a thing"
description-file = "description.md"
categories = ["Utilities"]

[release]
version = "1.0.0"
runtime = "org.freedesktop.Platform/x86_64/25.08"

[artifact]
tree = "tree"
"#,
        )
        .unwrap();
        let manifest = load(&path).unwrap();
        assert_eq!(manifest.app.id, "com.example.App");
        assert_eq!(manifest.app.description.as_deref(), Some("Long description."));
        assert!(manifest.artifact.tree.join("metadata").is_file());
        assert_eq!(manifest.release.channel, "stable");
        assert_eq!(manifest.ref_arch(), "x86_64");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn publication_manifest_does_not_require_the_build_tree() {
        let dir = std::env::temp_dir().join(format!("orchard-manifest-bad-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("tree")).unwrap();
        let path = dir.join("app.toml");
        std::fs::write(
            &path,
            r#"
[app]
id = "com.example.App"
name = "App"
summary = "s"

[release]
version = "1.0.0"

[artifact]
tree = "tree"
"#,
        )
        .unwrap();
        assert!(load(&path).is_ok());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
