//! The OCI recipe, as a library: flatpak app tree in, valid OCI image
//! layout out. Ports `scripts/make-demo-oci.py` (validated against
//! flatpak 1.18.4 sources and Fedora's live registry, see
//! `crates/orchard-core/src/oci.rs`):
//!
//! - the layer tar is the app tree rooted at `files/`, `metadata`, `export/`
//! - the image config's `config.Labels` carries the flatpak keys:
//!   `org.flatpak.ref` (required by flatpak), `org.flatpak.metadata`
//!   (required; the app ini), and the sizes/timestamp labels
//! - manifest layer digest = sha256 of the compressed tar;
//!   config `rootfs.diff_ids[0]` = sha256 of the uncompressed tar
//!
//! Flatpak reconstructs the ostree commit from layers plus labels at
//! install time, so no ostree repo or commit checksum is computed here.
//! The build is deterministic: same tree, same bytes, same digest.

use std::io::Write as _;
use std::os::unix::fs::MetadataExt as _;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};
use base64::Engine as _;

use orchard_core::oci::sha256_digest;

const MEDIA_MANIFEST: &str = "application/vnd.oci.image.manifest.v1+json";
const MEDIA_CONFIG: &str = "application/vnd.oci.image.config.v1+json";
const MEDIA_LAYER: &str = "application/vnd.oci.image.layer.v1.tar+gzip";

/// Everything the later stages need from the build.
pub struct Built {
    pub manifest_digest: String,
    pub layer_digest: String,
    pub diff_id: String,
    /// Sum of the tree's file bytes; also the `installed-size` label.
    pub installed_size: u64,
    /// Compressed layer bytes; the `download-size` label.
    pub download_size: u64,
    pub layout: PathBuf,
}

/// Build the app tree into an OCI layout at `layout`.
pub fn build(
    tree: &Path,
    layout: &Path,
    app_id: &str,
    version: &str,
    ref_arch: &str,
    body: Option<&str>,
) -> Result<Built> {
    let metadata_bytes = std::fs::read(tree.join("metadata"))
        .with_context(|| format!("reading {}", tree.join("metadata").display()))?;
    let metadata_ini = String::from_utf8(metadata_bytes.clone())
        .with_context(|| "metadata ini is not utf-8")?;

    let tar_bytes = build_layer_tar(tree, app_id, version)?;
    let gz_bytes = gzip(&tar_bytes)?;
    let layer_digest = sha256_digest(&gz_bytes);
    let diff_id = sha256_digest(&tar_bytes);
    let installed_size = tree_size(tree)?;
    let download_size = gz_bytes.len() as u64;

    let flatpak_ref = format!("app/{app_id}/{ref_arch}/stable");
    let config = serde_json::json!({
        "architecture": arch_to_docker(ref_arch),
        "config": { "Labels": {
            "org.flatpak.ref": flatpak_ref,
            "org.flatpak.metadata": metadata_ini,
            // The commit's xa.metadata must match the ini byte-for-byte
            // (flatpak's validate_commit_metadata compares them). The
            // label is a GVariant "v" wrapping a string: value bytes,
            // NUL-terminated, then the type character.
            "org.flatpak.commit-metadata.xa.metadata":
                base64::engine::general_purpose::STANDARD
                    .encode([metadata_bytes.as_slice(), b"\x00\x00s"].concat()),
            "org.flatpak.installed-size": installed_size.to_string(),
            "org.flatpak.download-size": download_size.to_string(),
            "org.flatpak.timestamp": "0",
            "org.flatpak.subject": format!("{app_id} {version}"),
            "org.flatpak.body": body.unwrap_or(""),
        }},
        "created": "1970-01-01T00:00:00Z",
        "os": "linux",
        "rootfs": { "type": "layers", "diff_ids": [diff_id] },
    });
    let config_bytes = serde_json::to_vec_pretty(&config)?;
    let config_digest = sha256_digest(&config_bytes);

    let manifest = serde_json::json!({
        "schemaVersion": 2,
        "mediaType": MEDIA_MANIFEST,
        "config": {
            "mediaType": MEDIA_CONFIG,
            "digest": config_digest,
            "size": config_bytes.len(),
        },
        "layers": [{
            "mediaType": MEDIA_LAYER,
            "digest": layer_digest,
            "size": download_size,
        }],
        // OCI manifest annotations mirror the labels; flatpak reads the
        // config labels but some tooling only shows manifest annotations.
        "annotations": config["config"]["Labels"],
    });
    let manifest_bytes = serde_json::to_vec_pretty(&manifest)?;
    let manifest_digest = sha256_digest(&manifest_bytes);
    let manifest_size = manifest_bytes.len() as u64;

    // Write the layout: blobs by digest, then index.json and oci-layout.
    for (digest, blob) in [
        (&config_digest, config_bytes),
        (&layer_digest, gz_bytes),
        (&manifest_digest, manifest_bytes),
    ] {
        let dest = layout.join("blobs/sha256").join(digest.trim_start_matches("sha256:"));
        std::fs::create_dir_all(dest.parent().unwrap())?;
        std::fs::write(dest, blob)?;
    }
    std::fs::write(
        layout.join("index.json"),
        serde_json::to_vec_pretty(&serde_json::json!({
            "schemaVersion": 2,
            "manifests": [{
                "mediaType": MEDIA_MANIFEST,
                "digest": manifest_digest,
                "size": manifest_size,
            }],
        }))?,
    )?;
    std::fs::write(layout.join("oci-layout"), "{\"imageLayoutVersion\":\"1.0.0\"}\n")?;

    Ok(Built {
        manifest_digest,
        layer_digest,
        diff_id,
        installed_size,
        download_size,
        layout: layout.to_path_buf(),
    })
}

/// Tar the app tree deterministically: sorted entries, mtime 0, uid/gid
/// 0, no user/group names, exec bit preserved.
fn build_layer_tar(tree: &Path, app_id: &str, version: &str) -> Result<Vec<u8>> {
    let mut builder = tar::Builder::new(Vec::new());
    for entry in sorted_children(tree)? {
        add_path(&mut builder, &entry, &entry.file_name().unwrap().to_string_lossy())?;
    }
    // The appstream snippet: `flatpak list` reads the version column
    // from files/share/app-info/xmls/<app-id>.xml.gz in the deployed
    // tree, and nothing else (the OCI labels do not reach it). One
    // component, one release entry. The timestamp is 1, not a build
    // date: it only has to beat the parser's initial 0 (with one
    // entry there is nothing newer to lose to), and a build date
    // would break the build's determinism, which re-runs of a tag
    // rely on.
    let appdata = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <components>\n\
         \x20 <component type=\"desktop\">\n\
         \x20   <id>{app_id}</id>\n\
         \x20   <name>{app_id}</name>\n\
         \x20   <releases>\n\
         \x20     <release version=\"{version}\" timestamp=\"1\"/>\n\
         \x20   </releases>\n\
         \x20 </component>\n\
         </components>\n"
    );
    for dir in [
        "files",
        "files/share",
        "files/share/app-info",
        "files/share/app-info/xmls",
    ] {
        let mut header = tar::Header::new_gnu();
        header.set_entry_type(tar::EntryType::Directory);
        header.set_mode(0o755);
        header.set_size(0);
        header.set_mtime(0);
        builder.append_data(&mut header, dir, std::io::empty())?;
    }
    let mut header = tar::Header::new_gnu();
    header.set_entry_type(tar::EntryType::Regular);
    header.set_mode(0o644);
    header.set_mtime(0);
    let xml_gz = gzip(appdata.as_bytes())?;
    header.set_size(xml_gz.len() as u64);
    builder.append_data(
        &mut header,
        &format!("files/share/app-info/xmls/{app_id}.xml.gz"),
        xml_gz.as_slice(),
    )?;
    builder.finish()?;
    Ok(builder.into_inner()?)
}

fn add_path(builder: &mut tar::Builder<Vec<u8>>, path: &Path, arcname: &str) -> Result<()> {
    let meta = path.symlink_metadata()?;
    let mut header = tar::Header::new_gnu();
    header.set_mtime(0);
    header.set_uid(0);
    header.set_gid(0);
    header.set_username("")?;
    header.set_groupname("")?;
    if meta.is_symlink() {
        header.set_entry_type(tar::EntryType::Symlink);
        header.set_mode(0o777);
        header.set_size(0);
        builder.append_link(&mut header, arcname, path)?;
        return Ok(());
    }
    if meta.is_dir() {
        header.set_entry_type(tar::EntryType::Directory);
        header.set_mode(0o755);
        header.set_size(0);
        builder.append_data(&mut header, arcname, std::io::empty())?;
        for child in sorted_children(path)? {
            add_path(builder, &child, &format!("{arcname}/{}", child.file_name().unwrap().to_string_lossy()))?;
        }
        return Ok(());
    }
    header.set_entry_type(tar::EntryType::Regular);
    header.set_mode(if meta.mode() & 0o111 != 0 { 0o755 } else { 0o644 });
    header.set_size(meta.len());
    let file = std::fs::File::open(path)?;
    builder.append_data(&mut header, arcname, file)?;
    Ok(())
}

fn sorted_children(path: &Path) -> Result<Vec<PathBuf>> {
    let mut children: Vec<PathBuf> = std::fs::read_dir(path)?
        .collect::<std::io::Result<Vec<_>>>()?
        .into_iter()
        .map(|entry| entry.path())
        .collect();
    children.sort();
    Ok(children)
}

/// Gzip with mtime 0 so the layer digest is reproducible.
fn gzip(bytes: &[u8]) -> Result<Vec<u8>> {
    let mut encoder = GzBuilder::new().mtime(0).write(Vec::new(), flate2::Compression::default());
    encoder.write_all(bytes)?;
    Ok(encoder.finish()?)
}

fn tree_size(tree: &Path) -> Result<u64> {
    let mut total = 0;
    for entry in walk(tree)? {
        if entry.is_file() {
            total += entry.metadata()?.len();
        }
    }
    Ok(total)
}

fn walk(path: &Path) -> Result<Vec<PathBuf>> {
    let mut all = Vec::new();
    let mut stack = vec![path.to_path_buf()];
    while let Some(current) = stack.pop() {
        for child in sorted_children(&current)? {
            if child.is_dir() {
                stack.push(child);
            } else {
                all.push(child);
            }
        }
    }
    Ok(all)
}

/// Flatpak image configs use Docker's arch names: `amd64`, `arm64`.
fn arch_to_docker(ref_arch: &str) -> String {
    match ref_arch {
        "x86_64" => "amd64".into(),
        "aarch64" => "arm64".into(),
        other => other.into(),
    }
}

use flate2::GzBuilder;

#[cfg(test)]
mod tests {
    use super::*;

    fn demo_tree() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("orchard-artifact-{}", std::process::id()));
        let tree = dir.join("tree");
        std::fs::create_dir_all(tree.join("files/bin")).unwrap();
        std::fs::write(tree.join("metadata"), "[Application]\nname=com.x\nruntime=org.r/x/a\ncommand=x\n").unwrap();
        std::fs::write(tree.join("files/bin/x"), "#!/bin/sh\necho hi\n").unwrap();
        dir
    }

    #[test]
    fn build_is_deterministic_and_self_consistent() {
        let dir = demo_tree();
        let tree = dir.join("tree");
        let a = build(&tree, &dir.join("layout-a"), "com.x", "1.0.0", "x86_64", None).unwrap();
        let b = build(&tree, &dir.join("layout-b"), "com.x", "1.0.0", "x86_64", None).unwrap();
        // same tree, same bytes, same manifest digest
        assert_eq!(a.manifest_digest, b.manifest_digest);
        // the written blob hashes back to the digest in the index
        let manifest_path = dir
            .join("layout-a/blobs/sha256")
            .join(a.manifest_digest.trim_start_matches("sha256:"));
        let manifest_bytes = std::fs::read(&manifest_path).unwrap();
        assert_eq!(sha256_digest(&manifest_bytes), a.manifest_digest);
        // verify() accepts the layout with the pin
        let report = orchard_core::oci::verify(
            &orchard_core::oci::Location::parse(&format!("oci:{}", dir.join("layout-a").display())).unwrap(),
            Some(&a.manifest_digest),
        )
        .unwrap();
        assert_eq!(report.digest, a.manifest_digest);
        // the metadata label on disk equals the tree's ini byte-exact:
        // verify() reads the label, the tree read is the publisher's truth
        let tree_ini = std::fs::read_to_string(tree.join("metadata")).unwrap();
        assert_eq!(report.metadata.as_deref(), Some(tree_ini.as_str()));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
