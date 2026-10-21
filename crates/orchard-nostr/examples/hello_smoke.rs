//! Actions-only Hello fixture: capture a real relay release and install its graph.
use anyhow::{Context, Result, bail, ensure};
use nostr::prelude::*;
use orchard_core::oci;
use orchard_nostr::Feed;
use serde_json::{Value, json};
use std::{collections::BTreeSet, fs, path::Path, time::{Duration, Instant}};

fn check(events: &[Event]) -> Result<Feed> {
    ensure!(events.len() == 8, "expected six assets, one release and one listing");
    let mut feed = Feed::new();
    let mut assets = BTreeSet::new();
    let mut references = BTreeSet::new();
    let mut publishers = BTreeSet::new();
    let mut releases = 0;
    for event in events {
        event.verify()?;
        let v: Value = serde_json::from_str(&event.as_json())?;
        publishers.insert(v["pubkey"].as_str().context("publisher")?.to_string());
        let tags = v["tags"].as_array().context("tags")?;
        let tag = |name: &str| tags.iter().find(|t| t[0] == name).and_then(|t| t[1].as_str());
        match v["kind"].as_u64() {
            Some(3063) => {
                ensure!(tag("T") == Some("flatpak-oci"), "target");
                ensure!(tag("i") == Some("rocks.orchard.Hello"), "asset app");
                ensure!(tag("url").context("url")?.starts_with("http://127.0.0.1:5000/v2/hello/"), "nonlocal asset URL");
                let hash = tag("x").context("hash")?;
                ensure!(hash.len() == 64, "hash length");
                if tag("m") == Some(orchard_core::oci_asset::MIME) {
                    ensure!(tag("oci") == Some(format!("docker://http://127.0.0.1:5000/hello@sha256:{hash}").as_str()), "legacy pin");
                } else {
                    ensure!(tag("oci").is_none(), "component has legacy location");
                }
                assets.insert(event.id.to_string());
            }
            Some(30063) => {
                releases += 1;
                ensure!(tag("d") == Some("rocks.orchard.Hello@0.0.0-smoke"), "release address");
                for t in tags.iter().filter(|t| t[0] == "e") {
                    references.insert(t[1].as_str().context("asset reference")?.to_string());
                }
            }
            Some(32267) => {},
            _ => bail!("unexpected event kind"),
        }
        ensure!(feed.absorb(event.clone())?, "event rejected");
    }
    ensure!(publishers.len() == 1 && releases == 1 && assets.len() == 6 && references == assets, "incomplete combined release");
    for platform in ["linux-x86_64", "linux-aarch64"] {
        let catalog = feed.catalog_for(platform);
        ensure!(catalog.apps.len() == 1, "missing platform {platform}");
        let app = &catalog.apps[0];
        ensure!(app.app_id == "rocks.orchard.Hello" && app.version == "0.0.0-smoke", "wrong app/version");
        ensure!(app.oci_assets.as_ref().context("component graph")?.len() == 2, "missing config/layer");
    }
    Ok(feed)
}

fn main() -> Result<()> {
    let args: Vec<_> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("capture") => {
            let (mut ws, _) = tungstenite::connect("ws://127.0.0.1:8008")?;
            let deadline = Instant::now() + Duration::from_secs(30);
            ws.send(tungstenite::Message::text(json!(["REQ", "hello", {"kinds":[3063,30063,32267]}]).to_string()))?;
            let mut events = Vec::new();
            loop {
                let remaining = deadline.checked_duration_since(Instant::now()).context("relay deadline")?;
                if let tungstenite::stream::MaybeTlsStream::Plain(stream) = ws.get_mut() {
                    stream.set_read_timeout(Some(remaining))?;
                }
                if let tungstenite::Message::Text(text) = ws.read()? {
                    let frame: Value = serde_json::from_str(&text)?;
                    if frame[0] == "EVENT" { events.push(Event::from_json(frame[2].to_string())?); }
                    if frame[0] == "EOSE" { break; }
                }
            }
            check(&events)?;
            fs::write(&args[2], events.iter().map(|e| e.as_json() + "\n").collect::<String>())?;
            println!("PASS: relay returned one signed release with both architecture component sets");
        }
        Some("download") => {
            let events = fs::read_to_string(&args[2])?.lines().map(Event::from_json).collect::<std::result::Result<Vec<_>, _>>()?;
            let feed = check(&events)?;
            let app = feed.catalog_for(&args[3]).apps.remove(0);
            let location = oci::Location::parse(&app.location)?;
            let report = oci::verify_with_assets(&location, app.digest.as_deref(), app.oci_assets.as_deref())?;
            ensure!(report.flatpak_ref.as_deref() == Some(app.flatpak_ref.as_str()), "Flatpak ref mismatch");
            ensure!(report.metadata.as_deref().context("permissions")?.contains("filesystems=home;"), "missing permissions");
            oci::download_layout_with_assets(&location, app.digest.as_deref(), Path::new(&args[4]), app.oci_assets.as_deref())?;
            println!("PASS: signed manifest/config inspected and descriptor-matched layers downloaded for {}", args[3]);
        }
        _ => bail!("usage: hello_smoke capture EVENTS | download EVENTS PLATFORM LAYOUT"),
    }
    Ok(())
}
