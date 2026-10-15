//! NIP-82 assets identify OCI manifest bytes; descriptors identify child blobs.
use crate::oci::{self, Location, VerifyReport};
use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::Path;
pub const PACKAGE: &str = "flatpak-oci";
pub const MIME: &str = "application/vnd.oci.image.manifest.v1+json";

pub fn hash_hex(value: &str) -> Result<&str> {
    let hex = value.strip_prefix("sha256:").unwrap_or(value);
    if hex.len() != 64 || !hex.bytes().all(|c| c.is_ascii_hexdigit()) {
        bail!("expected a SHA-256 digest");
    }
    Ok(hex)
}

pub fn file_hash(path: &Path) -> Result<(String, u64)> {
    copy_hash(&mut File::open(path)?, &mut std::io::sink())
}

fn copy_hash(reader: &mut impl Read, writer: &mut impl Write) -> Result<(String, u64)> {
    let mut hasher = Sha256::new();
    let mut count = 0;
    let mut buffer = [0u8; 65536];
    loop {
        let n = reader.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        writer.write_all(&buffer[..n])?;
        hasher.update(&buffer[..n]);
        count += n as u64;
    }
    Ok((format!("{:x}", hasher.finalize()), count))
}

pub fn validate_layout(dir: &Path) -> Result<VerifyReport> {
    for name in ["index.json", "oci-layout"] {
        if fs::metadata(dir.join(name))?.len() > 16 * 1024 * 1024 {
            bail!("OCI metadata exceeds size limit");
        }
    }
    let layout: serde_json::Value = serde_json::from_slice(&fs::read(dir.join("oci-layout"))?)?;
    if layout["imageLayoutVersion"] != "1.0.0" {
        bail!("unsupported OCI layout version");
    }
    let report = oci::verify(&Location::OciDir(dir.to_owned()), None)?;
    if report.flatpak_ref.is_none() || report.metadata.is_none() {
        bail!("layout is not a Flatpak OCI image");
    }
    // Check every descriptor, including layers, before Flatpak sees the layout.
    let manifest_path = dir.join("blobs/sha256").join(hash_hex(&report.digest)?);
    let manifest: serde_json::Value = serde_json::from_slice(&fs::read(manifest_path)?)?;
    let layers = manifest["layers"]
        .as_array()
        .context("OCI manifest has no layers")?;
    if layers.is_empty() {
        bail!("OCI image has no layers");
    }
    for descriptor in std::iter::once(&manifest["config"]).chain(layers) {
        let digest = descriptor["digest"]
            .as_str()
            .context("descriptor has no digest")?;
        let expected = hash_hex(digest)?;
        let size = descriptor["size"]
            .as_u64()
            .context("descriptor has no size")?;
        let mut file = File::open(dir.join("blobs/sha256").join(expected))?;
        let (actual, actual_size) = copy_hash(&mut file, &mut std::io::sink())?;
        if actual != expected || actual_size != size {
            bail!("OCI descriptor mismatch");
        }
    }
    Ok(report)
}

/// A signed software asset describing one OCI config or layer.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct BlobAsset {
    pub sha256: String,
    pub url: String,
    pub mime: String,
    pub size: u64,
}
