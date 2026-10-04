//! Finding a logo for a station that came without a usable one (added by URL,
//! or Radio-Browser lists a dead favicon). Sources, in order:
//! 1. the favicons Radio-Browser lists for that stream URL,
//! 2. the icons the station's homepage declares (`apple-touch-icon` and sized
//!    `icon` links — big, square, made to be shown as an app tile),
//!    homepage = Radio-Browser's entry, else derived from the stream host
//!    (`streams.bigfm.de` → `www.bigfm.de`),
//! 3. the conventional `/apple-touch-icon.png` of those sites.
//!
//! Every candidate is downloaded and must decode to an image of a minimum size
//! before it's accepted, so soft-404 pages and 16 px favicons are skipped.

use std::io::Read;
use std::time::Duration;

/// Smallest accepted logo edge (px). Smaller favicons look blurry as a tile.
const MIN_EDGE: i32 = 32;
/// Icons tried per homepage (best-ranked first).
const ICONS_PER_PAGE: usize = 5;
/// Cap for a downloaded homepage.
const MAX_HTML_BYTES: u64 = 2 * 1024 * 1024;

/// Searches a logo for the station with this stream URL and caches it.
/// Returns the image URL to store as the station's favicon. **Blocking,
/// network** – only call from worker threads.
pub fn find_logo(stream_url: &str) -> Option<String> {
    let (favicons, mut homepages) =
        crate::core::streaming::logo_hints_by_url(stream_url).unwrap_or_default();
    let mut tried: Vec<String> = Vec::new();
    let mut attempt = |url: &str| -> Option<String> {
        if tried.iter().any(|t| t == url) {
            return None;
        }
        tried.push(url.to_string());
        crate::core::online::cache_station_image_checked(url, MIN_EDGE).map(|_| url.to_string())
    };

    for f in &favicons {
        if let Some(hit) = attempt(f) {
            return Some(hit);
        }
    }
    for h in homepages_from_host(stream_url) {
        if !homepages.contains(&h) {
            homepages.push(h);
        }
    }
    let agent = ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(6))
        .timeout_read(Duration::from_secs(12))
        .build();
    let mut origins: Vec<String> = Vec::new();
    for page in &homepages {
        let Some((final_url, html)) = fetch_html(&agent, page) else {
            continue;
        };
        for icon in page_icons(&html, &final_url)
            .into_iter()
            .take(ICONS_PER_PAGE)
        {
            if let Some(hit) = attempt(&icon) {
                return Some(hit);
            }
        }
        if let Some(o) = origin(&final_url).filter(|o| !origins.contains(o)) {
            origins.push(o);
        }
    }
    origins
        .iter()
        .find_map(|o| attempt(&format!("{o}/apple-touch-icon.png")))
}

/// Downloads a page as text; returns the URL after redirects with the body.
fn fetch_html(agent: &ureq::Agent, url: &str) -> Option<(String, String)> {
    let resp = crate::core::net::get_with_retry_max(agent, url, None, url, 1)
        .ok()
        .flatten()?;
    let final_url = resp.get_url().to_string();
    let mut bytes = Vec::new();
    resp.into_reader()
        .take(MAX_HTML_BYTES)
        .read_to_end(&mut bytes)
        .ok()?;
    Some((final_url, String::from_utf8_lossy(&bytes).into_owned()))
}

/// Homepage guesses from the stream host: its registrable domain with and
/// without `www.` (`streams.bigfm.de` → `https://www.bigfm.de/`,
/// `https://bigfm.de/`).
fn homepages_from_host(stream_url: &str) -> Vec<String> {
    let Some(host) = host(stream_url) else {
        return Vec::new();
    };
    let labels: Vec<&str> = host.split('.').collect();
    if labels.len() < 2 || labels.iter().all(|l| l.chars().all(|c| c.is_ascii_digit())) {
        return Vec::new(); // bare name or IP address
    }
    // Second-level registries like `co.uk` / `com.au` keep one label more.
    let keep = if labels.len() >= 3
        && labels[labels.len() - 1].len() == 2
        && matches!(
            labels[labels.len() - 2],
            "co" | "com" | "org" | "net" | "ac" | "gov"
        ) {
        3
    } else {
        2
    };
    let base = labels[labels.len() - keep..].join(".");
    vec![format!("https://www.{base}/"), format!("https://{base}/")]
}

/// Host part of a URL (lowercase, without port/userinfo).
fn host(url: &str) -> Option<String> {
    let rest = url.split_once("://")?.1;
    let authority = rest.split(['/', '?', '#']).next()?;
    let host = authority.rsplit('@').next()?.split(':').next()?;
    (!host.is_empty()).then(|| host.to_ascii_lowercase())
}

/// `scheme://host[:port]` of a URL.
fn origin(url: &str) -> Option<String> {
    let (scheme, rest) = url.split_once("://")?;
    let authority = rest.split(['/', '?', '#']).next()?;
    (!authority.is_empty()).then(|| format!("{scheme}://{authority}"))
}

/// The icons a page declares in `<link rel=…>`, best first: apple-touch icons
/// and large sized icons before small ones; SVG last (not every system can
/// decode it). URLs are absolute.
fn page_icons(html: &str, page_url: &str) -> Vec<String> {
    let lower = html.to_ascii_lowercase();
    let mut found: Vec<(i32, String)> = Vec::new();
    let mut pos = 0;
    while let Some(i) = lower[pos..].find("<link") {
        let start = pos + i;
        let end = lower[start..].find('>').map_or(lower.len(), |e| start + e);
        let tag = &html[start..end];
        pos = end;
        let Some(rel) = attr(tag, "rel").map(|r| r.to_ascii_lowercase()) else {
            continue;
        };
        let rels: Vec<&str> = rel.split_whitespace().collect();
        let apple = rels.iter().any(|r| r.starts_with("apple-touch-icon"));
        if !apple && !rels.contains(&"icon") {
            continue;
        }
        let Some(href) = attr(tag, "href").filter(|h| !h.starts_with("data:")) else {
            continue;
        };
        let Some(url) = resolve(page_url, &href.replace("&amp;", "&")) else {
            continue;
        };
        let size = attr(tag, "sizes")
            .and_then(|s| {
                s.split_whitespace()
                    .filter_map(|wh| wh.split(['x', 'X']).next()?.parse::<i32>().ok())
                    .max()
            })
            .unwrap_or(if apple { 180 } else { 16 });
        let svg = url
            .split(['?', '#'])
            .next()
            .unwrap_or(&url)
            .ends_with(".svg");
        let score = if svg { 1 } else { size };
        if !found.iter().any(|(_, u)| *u == url) {
            found.push((score, url));
        }
    }
    found.sort_by_key(|f| std::cmp::Reverse(f.0));
    found.into_iter().map(|(_, u)| u).collect()
}

/// Value of an HTML attribute in a tag (quoted or bare), case-insensitive name.
fn attr(tag: &str, name: &str) -> Option<String> {
    let lower = tag.to_ascii_lowercase();
    let mut from = 0;
    while let Some(i) = lower[from..].find(name) {
        let at = from + i;
        from = at + name.len();
        // Must be a whole attribute name followed by '='.
        let before_ok = at > 0 && lower.as_bytes()[at - 1].is_ascii_whitespace();
        let rest = tag[from..].trim_start();
        if !before_ok || !rest.starts_with('=') {
            continue;
        }
        let rest = rest[1..].trim_start();
        let value = match rest.chars().next()? {
            q @ ('"' | '\'') => rest[1..].split(q).next()?,
            _ => rest.split(|c: char| c.is_whitespace() || c == '>').next()?,
        };
        return Some(value.trim().to_string());
    }
    None
}

/// Resolves `href` against the page URL.
fn resolve(page_url: &str, href: &str) -> Option<String> {
    let href = href.trim();
    if href.is_empty() {
        return None;
    }
    if href.starts_with("http://") || href.starts_with("https://") {
        return Some(href.to_string());
    }
    let scheme = page_url.split_once("://")?.0;
    if let Some(rest) = href.strip_prefix("//") {
        return Some(format!("{scheme}://{rest}"));
    }
    let origin = origin(page_url)?;
    if href.starts_with('/') {
        return Some(format!("{origin}{href}"));
    }
    // Relative to the page's directory.
    let path = page_url[origin.len()..]
        .split(['?', '#'])
        .next()
        .unwrap_or("");
    let dir = path.rsplit_once('/').map_or("", |(d, _)| d);
    Some(format!("{origin}{dir}/{href}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn homepage_guesses_use_the_registrable_domain() {
        assert_eq!(
            homepages_from_host("http://streams.bigfm.de/bigfm-deutschrap-128-aac"),
            vec!["https://www.bigfm.de/", "https://bigfm.de/"]
        );
        assert_eq!(
            homepages_from_host("https://stream.live.vc.bbcmedia.co.uk:443/bbc_radio_one"),
            vec!["https://www.bbcmedia.co.uk/", "https://bbcmedia.co.uk/"]
        );
        assert!(homepages_from_host("http://192.168.0.5:8000/live").is_empty());
        assert!(homepages_from_host("http://localhost/live").is_empty());
    }

    #[test]
    fn icons_are_ranked_and_resolved() {
        let html = r#"<head>
            <link rel="shortcut icon" href="https://file.atsw.de/icons/icon_64.png">
            <link data-n-head="ssr" rel="apple-touch-icon" href="https://file.atsw.de/icons/icon_512.png" sizes="512x512">
            <link rel='icon' type='image/svg+xml' href='/logo.svg'>
            <LINK REL=icon SIZES="16x16 32x32" HREF=fav.png>
            <link rel="stylesheet" href="/style.css">
            <link rel="icon" href="data:image/png;base64,AAAA">
        </head>"#;
        assert_eq!(
            page_icons(html, "https://www.bigfm.de/radio/start?x=1"),
            vec![
                "https://file.atsw.de/icons/icon_512.png",
                "https://www.bigfm.de/radio/fav.png",
                "https://file.atsw.de/icons/icon_64.png",
                "https://www.bigfm.de/logo.svg",
            ]
        );
    }

    #[test]
    fn apple_touch_icon_without_sizes_ranks_high() {
        let html = r#"<link rel="icon" href="/a.png" sizes="96x96">
                      <link rel="apple-touch-icon-precomposed" href="//cdn.example.org/b.png">"#;
        assert_eq!(
            page_icons(html, "http://example.org/"),
            vec!["http://cdn.example.org/b.png", "http://example.org/a.png"]
        );
    }

    #[test]
    fn attr_needs_a_whole_name() {
        assert_eq!(
            attr(r#"<link data-href="x" href="y">"#, "href").as_deref(),
            Some("y")
        );
        assert_eq!(attr(r#"<link hreflang="de">"#, "href"), None);
    }

    /// Live check against the real bigFM site (dead Radio-Browser favicon →
    /// homepage `apple-touch-icon`). Network: `cargo test -- --ignored`.
    #[test]
    #[ignore]
    fn finds_the_bigfm_logo_online() {
        gtk::init().ok();
        let logo = find_logo("http://streams.bigfm.de/bigfm-deutschrap-128-aac");
        assert!(
            logo.is_some_and(|l| l.starts_with("https://")),
            "no logo found"
        );
    }
}
