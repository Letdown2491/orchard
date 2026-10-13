//! Disk cache for remote images (screenshots, remote icons).
//!
//! gpui 0.2.2 ships a NullHttpClient (its URI image source can never
//! fetch), so remote images are downloaded here with ureq and rendered
//! from local files. Entries live under the XDG cache dir, keyed by a
//! stable hash of the URL, and survive restarts.

use std::io::Read as _;
use std::path::{Path, PathBuf};

/// The cache root: `$XDG_CACHE_HOME/orchard` or `$HOME/.cache/orchard`.
fn root() -> PathBuf {
    let base = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".cache")))
        .unwrap_or_else(|| PathBuf::from("/tmp"));
    base.join("orchard")
}

/// FNV-1a: stable across runs without pulling a hashing crate.
fn fnv1a(text: &str) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in text.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// The disk cache's ceiling, in bytes: old entries evict oldest-first
/// (insertion time, not access time) so the cache is bounded by
/// policy instead of growing until the user notices. Generous: a
/// store's worth of icons and screenshots is a few tens of MB.
const IMAGE_CACHE_CAP: u64 = 256 * 1024 * 1024;

/// The Orchard mark: the real raster from the repo's assets, written
/// into the image directory on first use so the toolbar and the About
/// popover can render it with the ordinary `img()` path. The crate's
/// SVG is a placeholder; the rasters are the identity.
const ORCHARD_MARK_PNG: &[u8] = include_bytes!("../../../assets/orchard-128.png");

/// The mark's disk path, resolving (writing) it at most once per
/// process. `None` when even the cache directory is unwritable; the
/// callers fall back to nothing rather than block a render.
pub fn orchard_mark() -> Option<PathBuf> {
    static MARK: std::sync::OnceLock<Option<PathBuf>> = std::sync::OnceLock::new();
    MARK.get_or_init(|| {
        let dir = root();
        std::fs::create_dir_all(&dir).ok()?;
        let path = dir.join("orchard-mark.png");
        if !path.exists() {
            let tmp = dir.join(format!("orchard-mark.{}.part", std::process::id()));
            std::fs::write(&tmp, ORCHARD_MARK_PNG).ok()?;
            std::fs::rename(&tmp, &path).ok()?;
        }
        Some(path)
    })
    .clone()
}

/// Resolve `url` to a local file, downloading through the cache on
/// first use. Blocking; call from a background executor. `None` on
/// network failure (the caller keeps its placeholder).
pub fn fetch(url: &str) -> Option<PathBuf> {
    let dir = root();
    let path = dir.join(format!("{:016x}.img", fnv1a(url)));
    if path.exists() {
        return Some(path);
    }
    let agent = ureq::AgentBuilder::new()
        .timeout(std::time::Duration::from_secs(15))
        .build();
    let response = agent.get(url).call().ok()?;
    if !(200..300).contains(&response.status()) {
        return None;
    }
    let mut bytes = Vec::new();
    response.into_reader().read_to_end(&mut bytes).ok()?;
    if bytes.is_empty() {
        return None;
    }
    std::fs::create_dir_all(&dir).ok()?;
    // tmp + rename so a partial download never lands in the cache
    let tmp = dir.join(format!("{}.{}.part", fnv1a(url), std::process::id()));
    std::fs::write(&tmp, &bytes).ok()?;
    std::fs::rename(&tmp, &path).ok()?;
    enforce_cap(&dir);
    Some(path)
}

/// Evict oldest-written entries until the cache is under the cap.
/// Runs after each new file lands: the walk stats one smallish
/// directory, on a background fetch cadence, never in render.
fn enforce_cap(dir: &Path) {
    let mut entries: Vec<(std::time::SystemTime, u64, PathBuf)> = Vec::new();
    let mut total = 0u64;
    let Ok(files) = std::fs::read_dir(dir) else {
        return;
    };
    for file in files.flatten() {
        let Ok(meta) = file.metadata() else {
            continue;
        };
        if !meta.is_file() {
            continue;
        }
        // the Orchard mark is identity, not cache: exempt from the cap
        // so a full cache never blanks the toolbar
        if file.file_name().to_string_lossy() == "orchard-mark.png" {
            continue;
        }
        total += meta.len();
        entries.push((meta.modified().unwrap_or(std::time::UNIX_EPOCH), meta.len(), file.path()));
    }
    if total <= IMAGE_CACHE_CAP {
        return;
    }
    entries.sort_by_key(|a| a.0);
    for (_, size, path) in entries {
        if total <= IMAGE_CACHE_CAP {
            break;
        }
        if std::fs::remove_file(&path).is_ok() {
            total -= size;
        }
    }
}

/// The absorbed-events cache the embedded indexd writes, or None when
/// even HOME is missing. Shared by main.rs (which names the path when
/// starting indexd) and the settings page (which reports its size and
/// clears it through indexd).
pub fn events_cache_path() -> Option<PathBuf> {
    let data = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/share")))?;
    Some(data.join("orchard/nostr-events.json"))
}

/// Total bytes of cached images on disk.
pub fn image_cache_bytes() -> u64 {
    dir_bytes(&root())
}

/// Where a verified registry artifact lands as a local OCI layout for
/// flatpak to install from: one directory per app id and manifest
/// digest, under the cache root. Cache semantics on purpose: the
/// layout is rebuildable bytes, and the client re-downloads when it
/// is absent. Only the parent chain is created here; the layout
/// directory itself appears by rename when a download completes, so
/// an existing directory is always a complete one.
pub fn artifact_dir(app_id: &str, digest: &str) -> PathBuf {
    let dir = root()
        .join("artifacts")
        .join(app_id)
        .join(digest.trim_start_matches("sha256:"));
    if let Some(parent) = dir.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    dir
}

/// Delete every cached image. The directory is rebuilt lazily on the
/// next fetch; pairing QR files share the dir and are transient by
/// design, so clearing them costs nothing.
pub fn clear_images() {
    let _ = std::fs::remove_dir_all(root());
}

/// Total bytes of absorbed Nostr events on disk, if any.
pub fn event_cache_bytes() -> u64 {
    events_cache_path()
        .map(|path| path.metadata().map(|meta| meta.len()).unwrap_or(0))
        .unwrap_or(0)
}

fn dir_bytes(dir: &PathBuf) -> u64 {
    std::fs::read_dir(dir)
        .map(|entries| {
            entries
                .filter_map(Result::ok)
                .filter_map(|entry| entry.metadata().ok())
                .filter(|meta| meta.is_file())
                .map(|meta| meta.len())
                .sum()
        })
        .unwrap_or(0)
}
