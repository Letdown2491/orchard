//! OCI upload metadata for combining architecture jobs into one release.
//! The signing job needs only metadata and access to the hosted artifacts.
use anyhow::{Context, Result, bail};
use orchard_core::oci_asset as asset;
use serde::{Deserialize, Serialize};
use std::fs::{self, File};
use std::path::{Path, PathBuf};

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Prepared {
    pub schema: u32,
    pub app_id: String,
    pub version: String,
    pub platform: String,
    pub package: String,
    pub mime: String,
    pub filename: String,
    pub layout: String,
    pub sha256: String,
    pub size: u64,
    pub url: Option<String>,
    pub components: Vec<Component>,
}

/// Config/layer descriptors and their registry URLs.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Component {
    pub sha256: String,
    pub mime: String,
    pub size: u64,
    pub filename: String,
    pub url: Option<String>,
}

impl Prepared {
    pub fn blob_assets(&self) -> Result<Vec<asset::BlobAsset>> {
        self.components.iter().map(|c| Ok(asset::BlobAsset {
            sha256: c.sha256.clone(), mime: c.mime.clone(), size: c.size,
            url: c.url.clone().context("component needs a hosted URL")?,
        })).collect()
    }
}

pub fn write(
    layout: &Path,
    output: &Path,
    app_id: &str,
    version: &str,
    platform: &str,
    url: Option<String>,
) -> Result<PathBuf> {
    let report = asset::validate_layout(layout)?;
    let expected = format!(
        "app/{}/{}/stable",
        app_id,
        platform
            .strip_prefix("linux-")
            .context("expected Linux platform")?
    );
    if report.flatpak_ref.as_deref() != Some(&expected) {
        bail!("OCI ref does not match the prepared application and platform");
    }
    fs::create_dir_all(output)?;
    let filename = format!("{app_id}-{platform}.manifest.json");
    if Path::new(&filename).file_name().and_then(|v| v.to_str()) != Some(&filename) {
        bail!("invalid app identifier or platform");
    }
    let path = output.join(format!("{app_id}-{platform}.oci.tgz"));
    let gzip = flate2::GzBuilder::new()
        .mtime(0)
        .write(File::create(&path)?, flate2::Compression::default());
    let mut tar = tar::Builder::new(gzip);
    let manifest_path = format!("blobs/sha256/{}", asset::hash_hex(&report.digest)?);
    let manifest: serde_json::Value =
        serde_json::from_slice(&fs::read(layout.join(&manifest_path))?)?;
    let mut files = std::collections::BTreeSet::from([
        "index.json".to_string(),
        "oci-layout".to_string(),
        manifest_path.clone(),
    ]);
    for descriptor in std::iter::once(&manifest["config"])
        .chain(manifest["layers"].as_array().context("missing layers")?)
    {
        files.insert(format!(
            "blobs/sha256/{}",
            asset::hash_hex(descriptor["digest"].as_str().context("missing digest")?)?
        ));
    }
    for relative in files {
        let mut file = File::open(layout.join(&relative))?;
        let mut header = tar::Header::new_gnu();
        header.set_mode(0o644);
        header.set_uid(0);
        header.set_gid(0);
        header.set_mtime(0);
        header.set_size(file.metadata()?.len());
        tar.append_data(&mut header, relative, &mut file)?;
    }
    tar.into_inner()?.finish()?;
    // Keep the manifest beside upload metadata and the bootstrap archive.
    let path = output.join(&filename);
    fs::copy(layout.join(&manifest_path), &path)?;
    let (sha256, size) = asset::file_hash(&path)?;
    let layout_name = layout
        .file_name()
        .and_then(|v| v.to_str())
        .context("layout needs a filename")?;
    if layout.parent() != Some(output) {
        bail!("prepared layout must be beside its metadata");
    }
    let mut components = Vec::new();
    for descriptor in std::iter::once(&manifest["config"])
        .chain(manifest["layers"].as_array().context("missing layers")?)
    {
        let sha256 = asset::hash_hex(descriptor["digest"].as_str().context("missing digest")?)?.to_string();
        let filename = sha256.clone();
        components.push(Component {
            sha256, filename,
            mime: descriptor["mediaType"].as_str().context("missing component MIME")?.into(),
            size: descriptor["size"].as_u64().context("missing component size")?,
            url: None,
        });
    }
    let prepared = Prepared {
        schema: 2,
        app_id: app_id.into(),
        version: version.into(),
        platform: platform.into(),
        package: asset::PACKAGE.into(),
        mime: asset::MIME.into(),
        filename,
        layout: layout_name.into(),
        sha256,
        size,
        url,
        components,
    };
    let metadata = output.join(format!("{app_id}-{platform}.asset.json"));
    fs::write(&metadata, serde_json::to_vec_pretty(&prepared)?)?;
    Ok(metadata)
}

pub fn load_metadata(path: &Path) -> Result<Prepared> {
    let asset: Prepared = serde_json::from_slice(&fs::read(path)?)?;
    if asset.schema != 2 || asset.package != asset::PACKAGE || asset.mime != asset::MIME {
        bail!("unsupported prepared asset format");
    }
    if Path::new(&asset.filename)
        .file_name()
        .and_then(|v| v.to_str())
        != Some(&asset.filename)
    {
        bail!("prepared filename must name a sibling file");
    }
    if asset.sha256.len() != 64 {
        bail!("prepared hash must be bare SHA-256");
    }
    asset::hash_hex(&asset.sha256)?;
    if asset.components.is_empty() { bail!("prepared image needs config and layer components"); }
    for component in &asset.components {
        if component.sha256.len() != 64 { bail!("component hash must be bare SHA-256"); }
        asset::hash_hex(&component.sha256)?;
        if Path::new(&component.filename).file_name().and_then(|v| v.to_str()) != Some(&component.filename) {
            bail!("component filename must name a sibling file");
        }
    }
    Ok(asset)
}

pub fn load(path: &Path) -> Result<Prepared> {
    let asset = load_metadata(path)?;
    let (hash, size) = asset::file_hash(
        &path
            .parent()
            .unwrap_or(Path::new("."))
            .join(&asset.filename),
    )?;
    if hash != asset.sha256 || size != asset.size {
        bail!("prepared manifest changed after preparation");
    }
    if Path::new(&asset.layout)
        .file_name()
        .and_then(|v| v.to_str())
        != Some(&asset.layout)
    {
        bail!("prepared layout must name a sibling directory");
    }
    let layout = path.parent().unwrap_or(Path::new(".")).join(&asset.layout);
    let report = asset::validate_layout(&layout)?;
    if asset::hash_hex(&report.digest)? != asset.sha256 {
        bail!("prepared layout differs from manifest asset");
    }
    let manifest: serde_json::Value = serde_json::from_slice(&fs::read(path.parent().unwrap_or(Path::new(".")).join(&asset.filename))?)?;
    let descriptors: Vec<_> = std::iter::once(&manifest["config"])
        .chain(manifest["layers"].as_array().context("missing layers")?).collect();
    if descriptors.len() != asset.components.len() { bail!("component list differs from manifest"); }
    for (descriptor, component) in descriptors.iter().zip(&asset.components) {
        if descriptor["digest"].as_str() != Some(&format!("sha256:{}", component.sha256))
            || descriptor["mediaType"].as_str() != Some(&component.mime)
            || descriptor["size"].as_u64() != Some(component.size) {
            bail!("component metadata differs from manifest");
        }
        let (hash, size) = asset::file_hash(&layout.join("blobs/sha256").join(&component.sha256))?;
        if hash != component.sha256 || size != component.size { bail!("prepared component changed"); }
    }
    validate_report(&asset, &report)?;
    Ok(asset)
}

pub fn validate_report(asset: &Prepared, report: &orchard_core::oci::VerifyReport) -> Result<()> {
    if report.manifest_size != asset.size {
        bail!("hosted manifest size differs from prepared asset");
    }
    let expected = format!(
        "app/{}/{}/stable",
        asset.app_id,
        asset
            .platform
            .strip_prefix("linux-")
            .context("expected Linux platform")?
    );
    if report.flatpak_ref.as_deref() != Some(&expected) {
        bail!("manifest ref differs from asset");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn prepares_repeatable_archives_and_detects_changed_bytes() {
        let root = std::env::temp_dir().join(format!("orchard-prepared-{}", std::process::id()));
        let tree = root.join("tree");
        fs::create_dir_all(tree.join("files/bin")).unwrap();
        fs::write(
            tree.join("metadata"),
            "[Application]\nname=com.example.App\nruntime=org.freedesktop.Platform/x86_64/25.08\n",
        )
        .unwrap();
        fs::write(tree.join("files/bin/app"), "application").unwrap();
        let built = crate::artifact::build(
            &tree,
            &root.join("layout"),
            "com.example.App",
            "1",
            "x86_64",
            None,
        )
        .unwrap();
        let path = write(
            &built.layout,
            &root,
            "com.example.App",
            "1",
            "linux-x86_64",
            None,
        )
        .unwrap();
        let first = load(&path).unwrap();
        assert_eq!(first.components.len(), 2);
        assert!(first.components.iter().all(|c| c.url.is_none()));
        for component in &first.components {
            let (hash, size) = asset::file_hash(&built.layout.join("blobs/sha256").join(&component.sha256)).unwrap();
            assert_eq!(hash, component.sha256); assert_eq!(size, component.size);
        }
        let layer_path = built.layout.join("blobs/sha256").join(&first.components[1].sha256);
        let layer_bytes = fs::read(&layer_path).unwrap();
        fs::write(&layer_path, "tampered").unwrap();
        assert!(load(&path).is_err());
        fs::write(&layer_path, layer_bytes).unwrap();
        let remote = root.join("metadata-only");
        fs::create_dir_all(&remote).unwrap();
        let remote_path = remote.join("asset.json");
        fs::copy(&path, &remote_path).unwrap();
        assert_eq!(load_metadata(&remote_path).unwrap().sha256, first.sha256);
        assert!(load(&remote_path).is_err());
        let report = asset::validate_layout(&built.layout).unwrap();
        let mut mismatched = load_metadata(&remote_path).unwrap();
        mismatched.platform = "linux-aarch64".into();
        assert!(validate_report(&mismatched, &report).is_err());
        mismatched.platform = first.platform.clone();
        mismatched.size += 1;
        assert!(validate_report(&mismatched, &report).is_err());
        write(
            &built.layout,
            &root,
            "com.example.App",
            "1",
            "linux-x86_64",
            None,
        )
        .unwrap();
        assert_eq!(load(&path).unwrap().sha256, first.sha256);
        assert_eq!(
            first.sha256,
            asset::hash_hex(&built.manifest_digest).unwrap()
        );
        fs::write(root.join(&first.filename), "changed").unwrap();
        assert!(load(&path).is_err());
        fs::remove_dir_all(root).unwrap();
    }
}
