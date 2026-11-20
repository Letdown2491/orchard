//! Dev probe: run the client's exact relay pipeline in isolation. Subscribes
//! with the client's filter, absorbs into a Feed, prints what the feed holds
//! and the projected catalog. Exists to see where events get lost.
//!
//!     feed_probe 'ws://127.0.0.1:8008'

use std::sync::{Arc, Mutex};

fn main() {
    let url = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "ws://127.0.0.1:8008".into());
    let feed = Arc::new(Mutex::new(orchard_nostr::Feed::new()));
    let watcher = Arc::clone(&feed);
    let on_change = Arc::new(move || {
        let feed = watcher.lock().expect("feed lock");
        let jsons = feed.event_jsons();
        let mut kinds: std::collections::BTreeMap<u64, usize> = std::collections::BTreeMap::new();
        for json in &jsons {
            if let Ok(event) = serde_json::from_str::<serde_json::Value>(json) {
                if let Some(kind) = event["kind"].as_u64() {
                    *kinds.entry(kind).or_default() += 1;
                }
            }
        }
        println!("absorbed: {} events, kinds {:?}", jsons.len(), kinds);
        for app in feed.catalog().apps {
            println!(
                "  {} | publisher {} | successor_to {}",
                app.app_id,
                &app.publisher.pubkey[..12],
                app.successor_to
                    .as_deref()
                    .map(|k| k[..12].to_string())
                    .unwrap_or_else(|| "none".into()),
            );
        }
        println!("---");
    });
    std::thread::spawn(move || {
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        orchard_nostr::relay::run_forever(
            &url,
            Arc::clone(&feed),
            on_change,
            stop,
            Default::default(),
        );
    });
    std::thread::sleep(std::time::Duration::from_secs(8));
    std::process::exit(0);
}
