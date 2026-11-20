//! The web-browsable catalog (issue #13): one read-only, server-rendered
//! page set at the same host as the API. An app list and a detail view
//! carrying what the catalog document already carries: description,
//! publisher identity, the digest pin, and the commands to add the
//! remote and install. No framework, no client-side JS, no write path,
//! no accounts: if this grows into a web store, it is a separate
//! project.

use orchard_core::model::{CatalogApp, Publisher};

/// Always revalidate: store the response, but check with the server
/// before using it. Combined with the ETag below, a visitor never sees
/// a stale page (the browser must ask), and an unchanged catalog costs
/// a 304 instead of a re-render. max-age caching served stale light
/// copies across theme changes during development: the failure mode
/// this rule removes.
const CACHE_HEADER: &str = "no-cache";

/// A strong ETag for a rendered page: the SHA-256 of the body,
/// truncated. Same body, same tag; any change to the catalog or the
/// markup changes the tag and forces a fresh 200.
pub fn etag(body: &str) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(body.as_bytes());
    format!("\"{}\"", hex(&digest[..8]))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Escape the five HTML-significant characters. Everything variable
/// goes through this, including app fields a publisher controls: a
/// hosted page is an untrusted-input surface.
pub fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            other => out.push(other),
        }
    }
    out
}

fn style() -> &'static str {
    // The light values are the default; the dark override follows the
    // Orchard app's palette (near-black surfaces, muted text, the green
    // accent) and only applies when the visitor's system asks for it.
    ":root{--bg:#f6f5f2;--fg:#1a1a1a;--card:#fff;--border:#e3e1dc;\
     --muted:#8a877f;--accent:#0a6640;--iconless:#e3e1dc;--iconless-fg:#8a877f;\
     --pre-bg:#1a1a1a;--pre-fg:#d5d3cc}\
     @media (prefers-color-scheme:dark){:root{--bg:#141414;--fg:#e6e4de;\
     --card:#1d1d1d;--border:#2c2c2c;--muted:#97948c;--accent:#5bbd8b;\
     --iconless:#2c2c2c;--iconless-fg:#97948c;--pre-bg:#0d0d0d;--pre-fg:#d5d3cc}}\
     body{font-family:system-ui,sans-serif;margin:0;background:var(--bg);color:var(--fg)}\
     main{max-width:64rem;margin:0 auto;padding:1.5rem 1rem 3rem}\
     a{color:var(--accent);text-decoration:none}a:hover{text-decoration:underline}\
     .app{display:flex;gap:1rem;padding:1rem;background:var(--card);\
     border:1px solid var(--border);border-radius:8px;margin-bottom:.6rem;align-items:center}\
     .app img{width:56px;height:56px;border-radius:8px}\
     .app .iconless{width:56px;height:56px;border-radius:8px;background:var(--iconless);\
     flex:none;display:flex;align-items:center;justify-content:center;\
     color:var(--iconless-fg)}\
     h1{font-size:1.4rem}h1 small{font-weight:400;color:var(--muted)}\
     .muted{color:var(--muted)}\
     pre{background:var(--pre-bg);color:var(--pre-fg);padding:.8rem 1rem;\
     border-radius:8px;overflow-x:auto;font-size:.85rem}\
     code{font-family:monospace}.pin{word-break:break-all}\
     dl{display:grid;grid-template-columns:auto 1fr;gap:.3rem 1rem}\
     dt{color:var(--muted)}dd{margin:0}\
     .head{display:flex;gap:1rem;align-items:center;margin:.5rem 0}\
     .head img{width:64px;height:64px;border-radius:8px}\
     .shots{display:flex;gap:.6rem;overflow-x:auto;padding-bottom:.4rem}\
     .shots img{height:240px;width:auto;max-width:none;flex:none;border-radius:8px;\
     border:1px solid var(--border)}\
     .chip{display:inline-block;background:var(--iconless);border-radius:99px;\
     padding:.1rem .6rem;font-size:.8rem;color:var(--iconless-fg);margin-right:.3rem}\
     .review{background:var(--card);border:1px solid var(--border);border-radius:8px;\
     padding:.8rem 1rem;margin-bottom:.6rem}\
     .review .muted{font-size:.85rem}\
     .pager{color:var(--muted)}\
     .sort a{color:var(--accent)}"
}

/// Extra head tags: the OpenGraph pair (property, content) that turns a
/// shared link into a card. Values are escaped here with everything
/// else.
fn page(title: &str, body: &str, og: &[(&str, &str)]) -> String {
    let og_tags: String = og
        .iter()
        .map(|(property, content)| {
            format!(
                "<meta property=\"{}\" content=\"{}\">",
                escape(property),
                escape(content)
            )
        })
        .collect();
    format!(
        "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\">\
         <meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">\
         <meta name=\"color-scheme\" content=\"light dark\">\
         {og_tags}\
         <title>{}</title><style>{}</style></head><body><main>{}</main></body></html>",
        escape(title),
        style(),
        body
    )
}

/// "3 days ago" from a unix timestamp. Accepts seconds or milliseconds
/// (a value above 100 trillion is milliseconds; the range split is
/// centuries away from any real publish date). Pure: the caller supplies
/// now, so the tests stay deterministic.
pub fn time_ago(unix: i64, now: i64) -> String {
    let seconds = if unix > 100_000_000_000 { unix / 1000 } else { unix };
    let elapsed = (now - seconds).max(0);
    let (n, unit) = match elapsed {
        0..=59 => (elapsed, "second"),
        60..=3599 => (elapsed / 60, "minute"),
        3600..=86399 => (elapsed / 3600, "hour"),
        86400..=2591999 => (elapsed / 86400, "day"),
        2592000..=31535999 => (elapsed / 2592000, "month"),
        _ => (elapsed / 31536000, "year"),
    };
    if n == 1 {
        format!("1 {unit} ago")
    } else {
        format!("{n} {unit}s ago")
    }
}

/// The publisher cell shared by the list and detail pages: the NIP-05
/// address when the catalog carries one (only after validation), else
/// the truncated key. Plus the migration marker when a row succeeded
/// another publisher: the trust-relevant fact a browser can show.
fn publisher_html(publisher: &Publisher, successor_to: Option<&str>) -> String {
    let identity = match &publisher.nip05 {
        Some(nip05) => format!("{} <span class=\"muted\">(NIP-05 verified)</span>", escape(nip05)),
        None => format!(
            "<code>{}</code>",
            escape(&short_key(&publisher.pubkey))
        ),
    };
    let migration = match successor_to {
        Some(prev) => format!(
            " <span class=\"muted\">(key migrated from <code>{}</code>)</span>",
            escape(&short_key(prev))
        ),
        None => String::new(),
    };
    format!("{identity}{migration}")
}

fn short_key(key: &str) -> String {
    if key.len() > 16 {
        format!("{}…{}", &key[..8], &key[key.len() - 8..])
    } else {
        key.to_string()
    }
}

/// The list sort, straight from the GUI app: recently updated (the
/// default) or by name. A query parameter picks it, links toggle it,
/// and the server renders in that order: no client-side JS, per the
/// issue's scope.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sort {
    Updated,
    Name,
}

impl Sort {
    /// From the `sort` query value; anything unrecognized is the
    /// default (updated), the same tolerance the GUI applies to
    /// settings.
    pub fn from_query(value: Option<&str>) -> Sort {
        match value {
            Some("name") => Sort::Name,
            _ => Sort::Updated,
        }
    }

    /// The query value for the other sort, for the toggle link.
    fn toggle_query(self) -> &'static str {
        match self {
            Sort::Updated => "name",
            Sort::Name => "updated",
        }
    }

    /// This sort's query value, for links that must preserve it.
    fn query_value(self) -> &'static str {
        match self {
            Sort::Updated => "updated",
            Sort::Name => "name",
        }
    }

    /// The visible label of the other sort, for the toggle link.
    fn toggle_label(self) -> &'static str {
        match self {
            Sort::Updated => "By name",
            Sort::Name => "Recently updated",
        }
    }

    /// The label of this sort, shown as the active one.
    fn label(self) -> &'static str {
        match self {
            Sort::Updated => "Recently updated",
            Sort::Name => "Name",
        }
    }
}

/// Apps per page on the list. The catalog is small today; the page
/// count grows when Orchard does.
const PER_PAGE: usize = 20;

/// The app list at /apps. One row per app ID, newest release wins
/// (the catalog document carries one row per published release; the
/// client dedups the same way at merge). Ordered by `sort`, cut into
/// pages of PER_PAGE with server-rendered prev/next links.
pub fn list_page(apps: &[CatalogApp], sort: Sort, page_number: usize) -> String {
    let now = orchard_nostr::unix_millis() / 1000;
    let mut newest: Vec<&CatalogApp> = Vec::new();
    'rows: for app in apps {
        for kept in newest.iter_mut() {
            if kept.app_id == app.app_id {
                if app.published_at > kept.published_at {
                    *kept = app;
                }
                continue 'rows;
            }
        }
        newest.push(app);
    }
    match sort {
        Sort::Updated => newest.sort_by(|a, b| b.published_at.cmp(&a.published_at)),
        Sort::Name => {
            newest.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
        }
    }
    let total_pages = (newest.len() / PER_PAGE)
        + usize::from(newest.len() % PER_PAGE > 0)
        + usize::from(newest.is_empty());
    // 1-based; out-of-range pages clamp to the ends instead of 404ing
    // (a stale link lands somewhere sensible)
    let total_pages = total_pages.max(1);
    let current = page_number.clamp(1, total_pages);
    let slice = &newest[(current - 1) * PER_PAGE..newest.len().min(current * PER_PAGE)];
    let cards: String = if slice.is_empty() {
        "<p>No apps published yet. The catalog fills from the relays as \
         publishers sign events.</p>"
            .to_string()
    } else {
        slice
            .iter()
            .map(|app| {
                let icon = match &app.icon_url {
                    Some(url) => format!(
                        "<img src=\"{}\" alt=\"\">",
                        escape(url)
                    ),
                    None => "<span class=\"iconless\">app</span>".to_string(),
                };
                format!(
                    "<div class=\"app\">{icon}<div><a href=\"/apps/{}\"><b>{}</b></a><br>{}\
                     <br><span class=\"muted\">v{} &middot; updated {} &middot; {}</span></div></div>",
                    escape(&app.app_id),
                    escape(&app.name),
                    escape(&app.summary),
                    escape(&app.version),
                    escape(&time_ago(app.published_at, now as i64)),
                    publisher_html(&app.publisher, app.successor_to.as_deref()),
                )
            })
            .collect()
    };
    // prev/next preserve the sort; disabled ends render as plain text
    let (prev, next) = (
        current > 1,
        current < total_pages,
    );
    let nav: String = if total_pages <= 1 {
        String::new()
    } else {
        format!(
            "<p class=\"pager\">{}&nbsp; &nbsp; Page {} of {} &nbsp; &nbsp;{}</p>",
            if prev {
                format!(
                    "<a href=\"/apps?sort={}&page={}\">&larr; Newer</a>",
                    sort.query_value(),
                    current - 1
                )
            } else {
                String::new()
            },
            current,
            total_pages,
            if next {
                format!(
                    "<a href=\"/apps?sort={}&page={}\">Older &rarr;</a>",
                    sort.query_value(),
                    current + 1
                )
            } else {
                String::new()
            },
        )
    };
    page(
        "Orchard catalog",
        format!(
            "<h1>Orchard <small>a permissionless Flatpak store</small></h1>\
             <p class=\"muted\">Publishers ship from their own registries and sign \
             Nostr events; every artifact here is digest-pinned and the client \
             re-verifies before install.</p>\
             <p class=\"sort\">Sorted by <b>{}</b> &middot; \
             <a href=\"/apps?sort={}\">{}</a></p>{cards}{nav}\
             <p class=\"muted\">Browse <a href=\"/v1/catalog\">/v1/catalog</a> for the \
             machine-readable document, or read the source and the publishing guide \
             at <a href=\"https://github.com/Letdown2491/orchard\" \
             rel=\"noopener\">GitHub</a>.</p>",
            sort.label(),
            sort.toggle_query(),
            sort.toggle_label(),
        )
        .as_str(),
        &[
            ("og:title", "Orchard: a permissionless Flatpak store"),
            (
                "og:description",
                "Browse apps published through Orchard's Nostr-based trust model.",
            ),
        ],
    )
}

/// The detail page at /apps/{app_id}. `others` carries the same app's
/// older catalog rows (newest first, the route filters); they render as
/// a versions list. `reviews` carries the app's reviews, newest first
/// (the route sorts): count and average render in a Reviews section.
/// The route answers 404 when the id is not in the catalog.
pub fn detail_page(
    base: &str,
    app: &CatalogApp,
    others: &[CatalogApp],
    reviews: &[orchard_core::model::Review],
) -> String {
    // The oci+ remote URL: same host, oci+ scheme (oci+https for https,
    // oci+http for plain http; only a loopback deployment is http).
    let remote_url = if base.starts_with("https://") {
        format!("oci+{base}")
    } else if base.starts_with("http://") {
        format!("oci+{base}")
    } else {
        base.to_string()
    };
    let description = app
        .description
        .as_deref()
        .unwrap_or("")
        .trim()
        .to_string();
    // The identity header and the screenshots: publisher-nominated URLs,
    // escaped like every other field. Screenshots load lazily so a
    // detail page with five of them does not drag the first paint.
    let icon = match &app.icon_url {
        Some(url) => format!("<img src=\"{}\" alt=\"\">", escape(url)),
        None => "<span class=\"iconless\">app</span>".to_string(),
    };
    let screenshots: String = if app.screenshots.is_empty() {
        String::new()
    } else {
        let figures: String = app
            .screenshots
            .iter()
            .map(|url| {
                format!(
                    "<img src=\"{}\" alt=\"{} screenshot\" loading=\"lazy\">",
                    escape(url),
                    escape(&app.name)
                )
            })
            .collect();
        format!("<h2>Screenshots</h2><div class=\"shots\">{figures}</div>")
    };
    let chips: String = if app.categories.is_empty() {
        String::new()
    } else {
        let spans: String = app
            .categories
            .iter()
            .map(|category| format!("<span class=\"chip\">{}</span>", escape(category)))
            .collect();
        format!("<p>{spans}</p>")
    };
    // Reviews: count and average over the rated ones, then the newest
    // few in full. One identity holds at most one review per app, so
    // the count is the reviewer count.
    let reviews_section: String = if reviews.is_empty() {
        String::new()
    } else {
        let rated: Vec<f32> = reviews.iter().filter_map(|r| r.rating).collect();
        let summary = if rated.is_empty() {
            format!(
                "{} {}",
                reviews.len(),
                if reviews.len() == 1 { "review" } else { "reviews" }
            )
        } else {
            let average = rated.iter().sum::<f32>() / rated.len() as f32;
            format!(
                "{} {} &middot; average {average:.1} / 5",
                reviews.len(),
                if reviews.len() == 1 { "review" } else { "reviews" }
            )
        };
        // The newest few render in full; the page stays bounded however
        // many arrive (one identity holds at most one review per app,
        // so the count is the reviewer count). The full corpus lives at
        // /v1/reviews, and a dedicated paginated reviews page is the
        // step to take if apps ever grow real review counts.
        const SHOWN_REVIEWS: usize = 5;
        let cards: String = reviews
            .iter()
            .take(SHOWN_REVIEWS)
            .map(|review| {
                let author = match &review.nip05 {
                    Some(nip05) => nip05.clone(),
                    None => short_key(&review.pubkey),
                };
                let stars = match review.rating {
                    Some(rating) => format!(" &middot; {rating:.1} / 5"),
                    None => String::new(),
                };
                format!(
                    "<div class=\"review\"><b>{}</b><span class=\"muted\">{stars}</span><br>{}</div>",
                    escape(&author),
                    escape(&review.content),
                )
            })
            .collect();
        let more = if reviews.len() > SHOWN_REVIEWS {
            format!(
                "<p class=\"muted\">Showing the {SHOWN_REVIEWS} most recent of {} reviews. \
                 The full history is at <a href=\"/v1/reviews\">/v1/reviews</a>, and the \
                 Orchard client shows all of them.</p>",
                reviews.len()
            )
        } else {
            String::new()
        };
        format!("<h2>Reviews</h2><p class=\"muted\">{summary}</p>{cards}{more}")
    };
    let links: String = match (&app.homepage, &app.repository) {
        (Some(home), Some(repo)) => format!(
            "<p><a href=\"{}\" rel=\"noopener\">Homepage</a> &middot; <a href=\"{}\" \
             rel=\"noopener\">Source repository</a></p>",
            escape(home),
            escape(repo)
        ),
        (Some(home), None) => format!(
            "<p><a href=\"{}\" rel=\"noopener\">Homepage</a></p>",
            escape(home)
        ),
        (None, Some(repo)) => format!(
            "<p><a href=\"{}\" rel=\"noopener\">Source repository</a></p>",
            escape(repo)
        ),
        (None, None) => String::new(),
    };
    let platforms = if app.platforms.is_empty() {
        String::new()
    } else {
        format!("<dt>Platforms</dt><dd>{}</dd>", escape(&app.platforms.join(", ")))
    };
    let versions: String = if others.is_empty() {
        String::new()
    } else {
        let rows: String = others
            .iter()
            .map(|row| {
                let branch_note = if row.branch == "stable" || row.branch == app.branch {
                    String::new()
                } else {
                    format!(" <span class=\"muted\">({})</span>", escape(&row.branch))
                };
                format!(
                    "<dd>v{}{branch_note} &middot; <code class=\"pin\">{}</code></dd>",
                    escape(&row.version),
                    escape(row.digest.as_deref().unwrap_or("no digest"))
                )
            })
            .collect();
        format!("<dt>Other versions on this relay</dt><dl>{rows}</dl>")
    };
    let digest = match &app.digest {
        Some(digest) => format!(
            "<dt>Digest pin</dt><dd><code class=\"pin\">{}</code></dd>",
            escape(digest)
        ),
        None => String::new(),
    };
    page(
        &format!("{} in the Orchard catalog", app.name),
        format!(
            "<p><a href=\"/apps\">&larr; All apps</a></p>\
             <div class=\"head\">{icon}\
             <h1>{} <small>v{}</small></h1></div><p>{}</p>{chips}{links}{screenshots}{reviews_section}\
             <h2>Details</h2><dl>\
             <dt>Publisher</dt><dd>{}</dd>\
             <dt>App ID</dt><dd><code>{}</code></dd>\
             <dt>Flatpak ref</dt><dd><code class=\"pin\">{}</code></dd>\
             {platforms}{digest}{versions}\
             </dl>\
             <h2>Install</h2>\
             <p>Add this Orchard remote, then install the ordinary way. The client \
             verifies the signature and the digest pin before anything runs.</p>\
             <pre>flatpak remote-add --if-not-exists --user orchard {remote_url}\n\
             flatpak install --user orchard {app_id}</pre>\
             <p class=\"muted\">Or from the command line after adding: \
             <code>flatpak update</code> sees new versions of installed apps like \
             any other remote. Orchard itself is free software: \
             <a href=\"https://github.com/Letdown2491/orchard\" rel=\"noopener\">source \
             and publishing guide</a>.</p>",
            escape(&app.name),
            escape(&app.version),
            escape(&description),
            publisher_html(&app.publisher, app.successor_to.as_deref()),
            escape(&app.app_id),
            escape(&app.flatpak_ref),
            remote_url = escape(&remote_url),
            app_id = escape(&app.app_id),
        )
        .as_str(),
        &[
            // OpenGraph: a shared link renders a card. The icon is the
            // image when the publisher supplied one.
            ("og:title", app.name.as_str()),
            ("og:description", app.summary.as_str()),
        ]
        .into_iter()
        .chain(app.icon_url.as_deref().map(|image| ("og:image", image)))
        .collect::<Vec<_>>(),
    )
}

/// The cache header for both pages.
pub fn cache_header() -> &'static str {
    CACHE_HEADER
}

#[cfg(test)]
mod tests {
    use super::*;
    use orchard_core::model::{CatalogApp, Publisher};

    #[test]
    fn etag_tracks_the_body() {
        let a = etag("one");
        let b = etag("two");
        assert_ne!(a, b);
        assert_eq!(a, etag("one"));
        assert!(a.starts_with('"') && a.ends_with('"'));
    }

    #[test]
    fn escape_covers_the_significant_five() {
        assert_eq!(escape("a<b>&\"'"), "a&lt;b&gt;&amp;&quot;&#39;");
        assert_eq!(escape("plain"), "plain");
    }

    #[test]
    fn list_page_escapes_publisher_controlled_fields() {
        let app = CatalogApp {
            app_id: "com.example.X".into(),
            name: "<script>alert(1)</script>".into(),
            summary: "sum".into(),
            description: None,
            icon_url: Some("https://e/x.png\"><script>".into()),
            categories: vec![],
            screenshots: vec![],
            publisher: Publisher {
                pubkey: "abc".into(),
                nip05: None,
            },
            version: "1.0".into(),
            platforms: vec![],
            flatpak_ref: "app/com.example.X/x86_64/stable".into(),
            branch: "stable".into(),
            channels: vec![],
            location: "https://ghcr.io/x".into(),
            digest: None,
            oci_assets: None,
            repository: None,
            homepage: None,
            metadata: None,
            notes: None,
            published_at: 0,
            successor_to: None,
        };
        let page = list_page(&[app], Sort::Updated, 1);
        assert!(!page.contains("<script>alert(1)"));
        assert!(page.contains("&lt;script&gt;"));
        assert!(!page.contains("x.png\"><script"));
    }

    fn sample_app(app_id: &str, version: &str, published_at: i64) -> CatalogApp {
        CatalogApp {
            app_id: app_id.into(),
            name: "X".into(),
            summary: "sum".into(),
            description: None,
            icon_url: None,
            categories: vec![],
            screenshots: vec![],
            publisher: Publisher {
                pubkey: "abc".into(),
                nip05: None,
            },
            version: version.into(),
            platforms: vec![],
            flatpak_ref: format!("app/{app_id}/x86_64/stable"),
            branch: "stable".into(),
            channels: vec![],
            location: "https://ghcr.io/x".into(),
            digest: Some("sha256:beef".into()),
            oci_assets: None,
            repository: None,
            homepage: None,
            metadata: None,
            notes: None,
            published_at,
            successor_to: None,
        }
    }

    #[test]
    fn list_page_shows_one_row_per_app_id_newest_wins() {
        let old = sample_app("com.example.X", "0.9.0", 100);
        let new = sample_app("com.example.X", "1.0.0", 200);
        let page = list_page(&[old, new], Sort::Updated, 1);
        assert_eq!(page.matches("com.example.X").count(), 1);
        assert!(page.contains("v1.0.0"));
        assert!(!page.contains("v0.9.0"));
    }

    #[test]
    fn list_page_sort_orders() {
        let mut a = sample_app("com.example.A", "1.0.0", 100);
        a.name = "Aaa".into();
        let mut b = sample_app("com.example.B", "1.0.0", 200);
        b.name = "Bbb".into();
        // updated (the default): the newest first even though it sorts later by name
        let updated = list_page(&[a.clone(), b.clone()], Sort::Updated, 1);
        assert!(updated.find("Bbb").unwrap() < updated.find("Aaa").unwrap());
        // name: alphabetical regardless of update time
        let named = list_page(&[a, b], Sort::Name, 1);
        assert!(named.find("Aaa").unwrap() < named.find("Bbb").unwrap());
        // the toggle link offers the other sort
        assert!(named.contains("/apps?sort=updated"));
        assert!(updated.contains("/apps?sort=name"));
    }

    #[test]
    fn sort_from_query_defaults_to_updated() {
        assert_eq!(Sort::from_query(None), Sort::Updated);
        assert_eq!(Sort::from_query(Some("name")), Sort::Name);
        assert_eq!(Sort::from_query(Some("nonsense")), Sort::Updated);
    }

    #[test]
    fn list_page_paginates_in_pages_of_twenty() {
        // 25 apps: page 1 carries 20, page 2 the rest, page 3 clamps
        // to page 2
        let apps: Vec<CatalogApp> = (0..25)
            .map(|index| {
                let mut app = sample_app(&format!("com.example.App{index}"), "1.0.0", index);
                app.name = format!("App {index:02}");
                app
            })
            .collect();
        let first = list_page(&apps, Sort::Name, 1);
        let second = list_page(&apps, Sort::Name, 2);
        let third = list_page(&apps, Sort::Name, 3);
        assert_eq!(first.matches("class=\"app\"").count(), 20);
        assert_eq!(second.matches("class=\"app\"").count(), 5);
        assert_eq!(third.matches("class=\"app\"").count(), 5);
        assert!(first.contains("Page 1 of 2"));
        // page 2 is the last: a Newer link back, no Older
        assert!(second.contains("Newer"));
        assert!(second.contains("sort=name&page=1"));
        assert!(!second.contains("Older"));
        // out-of-range lands somewhere sensible, not a panic or 404
        assert_eq!(list_page(&apps, Sort::Name, 99), second);
        assert!(list_page(&apps, Sort::Name, 0).contains("Page 1 of 2"));
        // a single page shows no pager at all (the string also appears
        // in the stylesheet, hence the class attribute)
        assert!(!list_page(&apps[..3], Sort::Name, 1).contains("class=\"pager\""));
    }

    #[test]
    fn time_ago_buckets_the_units() {
        // a realistic seconds timestamp (2023-era)
        let now = 1_700_000_000;
        assert_eq!(time_ago(now - 30, now), "30 seconds ago");
        assert_eq!(time_ago(now - 60, now), "1 minute ago");
        assert_eq!(time_ago(now - 120, now), "2 minutes ago");
        assert_eq!(time_ago(now - 3600, now), "1 hour ago");
        assert_eq!(time_ago(now - 86_400, now), "1 day ago");
        assert_eq!(time_ago(now - 86_400 * 20, now), "20 days ago");
        assert_eq!(time_ago(now - 86_400 * 400, now), "1 year ago");
        // a future stamp (clock skew) is not negative
        assert_eq!(time_ago(now + 500, now), "0 seconds ago");
        // milliseconds are detected and converted
        assert_eq!(time_ago((now - 60) * 1000, now), "1 minute ago");
    }

    #[test]
    fn detail_page_shows_reviews_and_category_chips() {
        use orchard_core::model::Review;
        let mut app = sample_app("com.example.X", "1.0.0", 200);
        app.categories = vec!["Utility".into(), "Network".into()];
        app.icon_url = Some("https://example.com/icon.png".into());
        let reviews = vec![
            Review {
                app_id: "com.example.X".into(),
                pubkey: "abcdefgh12345678abcdefgh12345678".into(),
                name: None,
                nip05: Some("fan@example.com".into()),
                rating: Some(4.5),
                content: "Works great".into(),
                created_at: 300,
            },
            Review {
                app_id: "com.example.X".into(),
                pubkey: "zzzzzzzz12345678abcdefgh12345678".into(),
                name: None,
                nip05: None,
                rating: None,
                content: "Text only <script>".into(),
                created_at: 400,
            },
        ];
        // the route hands the page reviews newest first; the newest
        // renders first and text-only reviews carry no stars
        let sorted: Vec<Review> = vec![reviews[1].clone(), reviews[0].clone()];
        let page = detail_page("https://index.example.com", &app, &[], &sorted);
        assert!(page.contains("2 reviews &middot; average 4.5 / 5"));
        assert!(page.find("Text only").unwrap() < page.find("Works great").unwrap());
        assert!(page.contains("fan@example.com"));
        assert!(page.contains("Works great"));
        assert!(!page.contains("<script>"));
        assert!(page.contains("<span class=\"chip\">Utility</span>"));
        assert!(page.contains("<span class=\"chip\">Network</span>"));
        // og tags name the app and carry the icon
        assert!(page.contains("property=\"og:title\" content=\"X\""));
        assert!(page.contains("property=\"og:image\""));
        // no reviews: no section
        assert!(!detail_page("https://index.example.com", &app, &[], &[]).contains("Reviews"));
    }

    #[test]
    fn detail_page_caps_the_reviews_on_the_page() {
        let app = sample_app("com.example.X", "1.0.0", 200);
        let reviews: Vec<orchard_core::model::Review> = (0..30)
            .map(|index| orchard_core::model::Review {
                app_id: "com.example.X".into(),
                pubkey: format!("key{index:026}"),
                name: None,
                nip05: None,
                rating: Some(3.0),
                content: format!("review {index}"),
                created_at: index,
            })
            .collect();
        // the route hands the page reviews newest first; the five
        // newest render in full
        let mut newest: Vec<orchard_core::model::Review> = reviews;
        newest.sort_by(|a, b| b.created_at.cmp(&a.created_at));
        let page = detail_page("https://index.example.com", &app, &[], &newest);
        // five render in full (the newest), the summary counts all 30,
        // and the pointer names where the rest live
        assert_eq!(page.matches("class=\"review\"").count(), 5);
        assert!(page.contains("30 reviews"));
        assert!(page.contains("Showing the 5 most recent of 30 reviews"));
        assert!(page.contains("review 29"));
        assert!(!page.contains("review 24<br>"));
    }

    #[test]
    fn detail_page_lists_the_other_versions() {
        let app = sample_app("com.example.X", "1.0.0", 200);
        let old = sample_app("com.example.X", "0.9.0", 100);
        let page = detail_page("https://index.example.com", &app, &[old], &[]);
        assert!(page.contains("Other versions"));
        assert!(page.contains("v0.9.0"));
        assert!(page.contains("sha256:beef"));
    }

    #[test]
    fn detail_page_carries_the_install_commands_and_the_pin() {
        let app = CatalogApp {
            app_id: "com.example.X".into(),
            name: "X".into(),
            summary: "sum".into(),
            description: Some("desc".into()),
            icon_url: Some("https://example.com/icon.png".into()),
            categories: vec![],
            screenshots: vec!["https://example.com/shot.png".into()],
            publisher: Publisher {
                pubkey: "abc".into(),
                nip05: Some("you@example.com".into()),
            },
            version: "1.0".into(),
            platforms: vec!["linux-x86_64".into()],
            flatpak_ref: "app/com.example.X/x86_64/stable".into(),
            branch: "stable".into(),
            channels: vec![],
            location: "https://ghcr.io/x".into(),
            digest: Some("sha256:dead".into()),
            oci_assets: None,
            repository: Some("https://github.com/owner/x".into()),
            homepage: None,
            metadata: None,
            notes: None,
            published_at: 0,
            successor_to: None,
        };
        let page = detail_page("https://index.example.com", &app, &[], &[]);
        assert!(page.contains("flatpak remote-add --if-not-exists --user orchard oci+https://index.example.com"));
        assert!(page.contains("flatpak install --user orchard com.example.X"));
        assert!(page.contains("sha256:dead"));
        assert!(page.contains("you@example.com"));
        assert!(page.contains("NIP-05 verified"));
        // the identity header and the screenshots render from the
        // publisher-nominated URLs
        assert!(page.contains("<img src=\"https://example.com/icon.png\""));
        assert!(page.contains("Screenshots"));
        assert!(page.contains("src=\"https://example.com/shot.png\""));
        assert!(page.contains("loading=\"lazy\""));
    }
}
