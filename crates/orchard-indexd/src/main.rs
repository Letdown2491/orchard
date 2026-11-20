//! Standalone runner: serve a catalog file over HTTP without the GPUI client.
//!
//!   orchard-indexd [path/to/catalog.json]
//!
//! Defaults to `./demo/catalog.json` when it exists, then
//! `$XDG_DATA_HOME/orchard/catalog.json`. The client embeds this library and
//! runs the same service in-process; the standalone binary exists for
//! debugging and for driving `flatpak` from scripts.
//!
//! Relays come from `ORCHARD_RELAYS` (comma-separated NIP-01 URLs); when
//! set, verified NIP-82 events merge over the catalog file and persist to
//! `$XDG_DATA_HOME/orchard/nostr-events.json`.

use std::sync::Arc;

use anyhow::Context as _;
use orchard_core::CatalogFile;
use orchard_indexd::ServerState;

fn load_catalog() -> anyhow::Result<CatalogFile> {
    let mut candidates: Vec<std::path::PathBuf> = Vec::new();
    if let Some(arg) = std::env::args().nth(1) {
        candidates.push(arg.into());
    }
    candidates.push("demo/catalog.json".into());
    if let Some(data) = std::env::var_os("XDG_DATA_HOME") {
        candidates.push(std::path::PathBuf::from(data).join("orchard/catalog.json"));
    }
    if let Some(home) = std::env::var_os("HOME") {
        candidates.push(
            std::path::PathBuf::from(home)
                .join(".local/share/orchard/catalog.json"),
        );
    }

    for path in &candidates {
        match std::fs::read(path) {
            Ok(bytes) => {
                let catalog: CatalogFile = serde_json::from_slice(bytes.as_slice())
                    .with_context(|| format!("parsing {}", path.display()))?;
                eprintln!("loaded catalog from {}", path.display());
                return Ok(catalog);
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => continue,
            Err(err) => return Err(err).with_context(|| path.display().to_string()),
        }
    }
    anyhow::bail!("no catalog found (tried {:?})", candidates)
}

/// The settings: the same file the client uses, so the standalone
/// runner and the embedded instance agree. Environment overrides
/// still win, per resolve_set.
fn settings_handle() -> orchard_core::SettingsHandle {
    orchard_core::SettingsHandle::load_default()
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let addr = std::env::var("ORCHARD_INDEXD_ADDR")
        .unwrap_or_else(|_| "127.0.0.1:7780".to_string())
        .parse()?;

    let catalog = load_catalog()?;
    let settings = settings_handle();
    let sets = orchard_indexd::relay_sets(&settings);
    if !sets.events.is_empty() {
        println!("subscribing to relays: {}", sets.events.join(", "));
    }

    let state = ServerState {
        document: Arc::new(catalog),
        relays_reset: None,
        relays_health: None,
    };
    let cache = std::env::var_os("XDG_DATA_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| {
            std::path::PathBuf::from(home).join(".local/share")
        }))
        .map(|data| data.join("orchard/nostr-events.json"));

    println!("orchard-indexd serving on http://{addr}");
    orchard_indexd::serve(addr, state, settings, cache).await
}
