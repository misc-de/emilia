//! Podcasts: reading RSS feeds (via the `rss` crate) and providing the
//! episodes. Audio is streamed directly (playbin3 plays http URLs) –
//! nothing is downloaded and no audio file is modified.

use std::io::Read;
use std::time::Duration;

use anyhow::{anyhow, Result};

use crate::core::net;
use crate::model::Episode;

/// Converts an RFC-2822 publication date ("Fri, 29 May 2026 22:00:00 -0000")
/// into a **sortable** key `YYYYMMDDHHMMSS`. The time zone is ignored for
/// sorting; unparsable/missing dates yield `0`.
pub fn pubdate_key(s: Option<&str>) -> i64 {
    let Some(s) = s else { return 0 };
    let tokens: Vec<&str> = s.split_whitespace().collect();
    let month_num = |m: &str| -> Option<i64> {
        [
            "jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec",
        ]
        .iter()
        .position(|x| m.to_ascii_lowercase().starts_with(x))
        .map(|i| i as i64 + 1)
    };
    // Find the month name; before it is the day, after it the year and time.
    let mi = tokens.iter().position(|t| month_num(t).is_some());
    let Some(mi) = mi.filter(|&i| i >= 1) else {
        return 0;
    };
    let month = month_num(tokens[mi]).unwrap_or(0);
    let day: i64 = tokens[mi - 1].trim_matches(',').parse().unwrap_or(0);
    let year: i64 = tokens.get(mi + 1).and_then(|y| y.parse().ok()).unwrap_or(0);
    let (mut h, mut m, mut sec) = (0i64, 0i64, 0i64);
    if let Some(t) = tokens.get(mi + 2) {
        let p: Vec<&str> = t.split(':').collect();
        h = p.first().and_then(|x| x.parse().ok()).unwrap_or(0);
        m = p.get(1).and_then(|x| x.parse().ok()).unwrap_or(0);
        sec = p.get(2).and_then(|x| x.parse().ok()).unwrap_or(0);
    }
    ((((year * 100 + month) * 100 + day) * 100 + h) * 100 + m) * 100 + sec
}

/// Sort/comparison key `YYYYMMDD000000` for a date (day precision).
fn date_key(year: i64, month: i64, day: i64) -> i64 {
    ((year * 100 + month) * 100 + day) * 1_000_000
}

/// Civil date (year, month, day) from days since the Unix epoch
/// (algorithm after Howard Hinnant).
fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = (if z >= 0 { z } else { z - 146_096 }) / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// Comparison key for "~one month ago" (today − 31 days), matching
/// [`pubdate_key`]. Episodes with `pubdate_key >= recent_cutoff_key()` count as
/// new (at most one month back).
pub fn recent_cutoff_key() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let (y, m, d) = civil_from_days(secs / 86_400 - 31);
    date_key(y, m, d)
}

/// Threshold keys (matching [`pubdate_key`]) for grouping the newest
/// episodes – each at midnight: `(today, yesterday, 7 days ago)`.
/// "This week" deliberately means the **last 7 days (rolling)**, not the
/// calendar week; older than that = "This month".
pub fn recent_day_buckets() -> (i64, i64, i64) {
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let today_days = secs / 86_400;
    let key_of = |days: i64| {
        let (y, m, d) = civil_from_days(days);
        date_key(y, m, d)
    };
    let today = key_of(today_days);
    let yesterday = key_of(today_days - 1);
    // Rolling: everything from today − 6 days counts as "this week" (= last 7 days).
    let week_start = key_of(today_days - 6);
    (today, yesterday, week_start)
}

/// Short form of an RFC-2822 date for display: "29 May 2026" (without weekday,
/// time, and time zone). Unparsable input is returned unchanged.
pub fn pubdate_short(s: &str) -> String {
    const MONTHS: [&str; 12] = [
        "jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec",
    ];
    let t: Vec<&str> = s.split_whitespace().collect();
    let mi = t
        .iter()
        .position(|x| MONTHS.iter().any(|m| x.to_ascii_lowercase().starts_with(m)));
    match mi {
        Some(i) if i >= 1 && i + 1 < t.len() => {
            format!("{} {} {}", t[i - 1].trim_matches(','), t[i], t[i + 1])
        }
        _ => s.trim().to_string(),
    }
}

/// Result of reading a feed: channel data plus episodes (with audio).
pub struct PodcastFeed {
    pub title: String,
    pub image_url: Option<String>,
    pub episodes: Vec<Episode>,
}

/// Decodes remaining HTML/XML entities in feed texts (especially titles):
/// numeric references (`&#128512;`, `&#x1F600;` → emoji/smiley) and common
/// named entities. Many feeds are double-encoded or use HTML entities that
/// the XML parser leaves in place – this brings those characters back out.
pub(crate) fn decode_entities(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    let named = |name: &str| -> Option<char> {
        Some(match name {
            "amp" => '&',
            "lt" => '<',
            "gt" => '>',
            "quot" => '"',
            "apos" => '\'',
            "nbsp" => '\u{00A0}',
            "rsquo" => '\u{2019}',
            "lsquo" => '\u{2018}',
            "rdquo" => '\u{201D}',
            "ldquo" => '\u{201C}',
            "hellip" => '\u{2026}',
            "mdash" => '\u{2014}',
            "ndash" => '\u{2013}',
            "auml" => 'ä',
            "ouml" => 'ö',
            "uuml" => 'ü',
            "Auml" => 'Ä',
            "Ouml" => 'Ö',
            "Uuml" => 'Ü',
            "szlig" => 'ß',
            "eacute" => 'é',
            "copy" => '©',
            "reg" => '®',
            "trade" => '™',
            "deg" => '°',
            "euro" => '€',
            "middot" => '·',
            "bull" => '•',
            _ => return None,
        })
    };
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        let after = &rest[amp + 1..];
        // Entity name up to the ';' (keep it short, otherwise not a real entity).
        if let Some(semi) = after.find(';').filter(|&p| p > 0 && p <= 12) {
            let ent = &after[..semi];
            let decoded =
                if let Some(hex) = ent.strip_prefix("#x").or_else(|| ent.strip_prefix("#X")) {
                    u32::from_str_radix(hex, 16).ok().and_then(char::from_u32)
                } else if let Some(dec) = ent.strip_prefix('#') {
                    dec.parse::<u32>().ok().and_then(char::from_u32)
                } else {
                    named(ent)
                };
            if let Some(c) = decoded {
                out.push(c);
                rest = &after[semi + 1..];
                continue;
            }
        }
        // Not a valid entity → keep the "&" unchanged.
        out.push('&');
        rest = after;
    }
    out.push_str(rest);
    out
}

/// Formats a feed duration uniformly as `h:mm:ss` (or `m:ss` if under one
/// hour). Accepts both `HH:MM:SS`/`MM:SS` and plain seconds (e.g. "3600" or
/// "3600.0"). Returns `None` if nothing sensible can be determined – the
/// caller then shows the original text.
pub fn format_duration(raw: &str) -> Option<String> {
    let s = raw.trim();
    if s.is_empty() {
        return None;
    }
    let total: i64 = if s.contains(':') {
        let mut secs = 0i64;
        for part in s.split(':') {
            let n: i64 = part.trim().parse().ok()?;
            if n < 0 {
                return None;
            }
            secs = secs * 60 + n;
        }
        secs
    } else {
        // Plain seconds, possibly with decimals ("1234.5").
        s.split('.').next()?.trim().parse().ok()?
    };
    if total < 0 {
        return None;
    }
    let (h, m, sec) = (total / 3600, (total % 3600) / 60, total % 60);
    Some(if h > 0 {
        format!("{h}:{m:02}:{sec:02}")
    } else {
        format!("{m}:{sec:02}")
    })
}

/// Converts an HTML description text (shownotes) into readable plain text:
/// block/break tags become line breaks, remaining tags are removed,
/// HTML entities are decoded, and superfluous whitespace is collapsed.
pub(crate) fn html_to_text(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_tag = false;
    let mut tag = String::new();
    for c in s.chars() {
        match c {
            '<' => {
                in_tag = true;
                tag.clear();
            }
            '>' if in_tag => {
                in_tag = false;
                // Tag name (without leading "/") up to the first non-letter.
                let name: String = tag
                    .trim()
                    .trim_start_matches('/')
                    .chars()
                    .take_while(|c| c.is_ascii_alphanumeric())
                    .collect::<String>()
                    .to_ascii_lowercase();
                if matches!(
                    name.as_str(),
                    "br" | "p"
                        | "div"
                        | "li"
                        | "tr"
                        | "ul"
                        | "ol"
                        | "blockquote"
                        | "h1"
                        | "h2"
                        | "h3"
                        | "h4"
                        | "h5"
                        | "h6"
                ) && !out.ends_with('\n')
                {
                    out.push('\n');
                }
            }
            _ if in_tag => tag.push(c),
            _ => out.push(c),
        }
    }
    let decoded = decode_entities(&out);
    // Collapse whitespace per line; at most one blank line in a row.
    let mut lines: Vec<String> = Vec::new();
    let mut blank = false;
    for raw in decoded.lines() {
        let line = raw.split_whitespace().collect::<Vec<_>>().join(" ");
        if line.is_empty() {
            if !blank && !lines.is_empty() {
                lines.push(String::new());
            }
            blank = true;
        } else {
            lines.push(line);
            blank = false;
        }
    }
    while lines.last().is_some_and(|l| l.is_empty()) {
        lines.pop();
    }
    lines.join("\n")
}

/// Reads a podcast RSS feed. Only entries with an audio enclosure are taken;
/// errors if no audio episode is found.
pub fn parse_feed(xml: &[u8]) -> Result<PodcastFeed> {
    let channel = rss::Channel::read_from(xml)?;

    let title = {
        let t = decode_entities(channel.title().trim());
        let t = t.trim();
        if t.is_empty() {
            "Podcast".to_string()
        } else {
            t.to_string()
        }
    };
    let image_url = channel.image().map(|i| i.url().to_string()).or_else(|| {
        channel
            .itunes_ext()
            .and_then(|e| e.image().map(String::from))
    });

    let mut episodes = Vec::new();
    for item in channel.items() {
        let Some(enclosure) = item.enclosure() else {
            continue;
        };
        let audio_url = enclosure.url().trim().to_string();
        if audio_url.is_empty() {
            continue;
        }
        let title = item
            .title()
            .map(|t| decode_entities(t.trim()))
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| "Episode".to_string());
        // Shownotes: prefer the full <content:encoded>, otherwise <description>,
        // otherwise the iTunes summary – each reduced to plain text.
        let description = item
            .content()
            .or_else(|| item.description())
            .or_else(|| item.itunes_ext().and_then(|e| e.summary()))
            .map(html_to_text)
            .map(|t| t.trim().to_string())
            .filter(|t| !t.is_empty());
        episodes.push(Episode {
            guid: item.guid().map(|g| g.value().to_string()),
            title,
            audio_url,
            published: item.pub_date().map(String::from),
            duration: item
                .itunes_ext()
                .and_then(|e| e.duration().map(String::from)),
            description,
        });
    }
    if episodes.is_empty() {
        return Err(anyhow!("no episodes with audio found in the feed"));
    }
    Ok(PodcastFeed {
        title,
        image_url,
        episodes,
    })
}

/// Fetches a feed via HTTP and reads it (blocking – use in the worker).
pub fn fetch_feed(url: &str) -> Result<PodcastFeed> {
    let agent = ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(8))
        .timeout_read(Duration::from_secs(20))
        .build();
    // Retry transient failures (5xx/429/transport) like every other fetch; a
    // 404 means the feed is gone.
    let resp = net::get_with_retry(&agent, url, None, "podcast feed")?
        .ok_or_else(|| anyhow!("podcast feed not found"))?;
    let mut bytes = Vec::new();
    resp.into_reader()
        .take(16 * 1024 * 1024) // Cap against a hostile/broken feed streaming endlessly (OOM).
        .read_to_end(&mut bytes)?;
    parse_feed(&bytes)
}

/// Downloads an episode's audio file to `dest` for offline playback (blocking –
/// use in the worker thread). Writes to a temporary `*.part` file first and
/// renames on success, so a cancelled/failed download never leaves a truncated
/// file that would later be treated as a complete offline copy. The transfer is
/// capped at [`crate::core::net::MAX_DOWNLOAD_BYTES`] so a hostile or broken
/// feed cannot fill the disk. Returns the number of bytes written.
pub fn download_episode(url: &str, dest: &std::path::Path) -> Result<u64> {
    let agent = ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(10))
        // Generous *per-read* timeout: a large episode over a slow link may take
        // a while, and the timeout resets on every chunk — so a legitimately slow
        // but progressing download is never killed. It only fires on a true stall
        // (a server that connects then sends nothing) so the worker can't hang
        // forever on a half-open socket.
        .timeout_read(Duration::from_secs(120))
        .build();
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let resp = agent.get(url).call()?;
    net::check_content_length(&resp, net::MAX_DOWNLOAD_BYTES)?;
    let tmp = dest.with_extension("part");
    let written = {
        let mut file = std::fs::File::create(&tmp)?;
        match net::copy_capped(resp.into_reader(), &mut file, net::MAX_DOWNLOAD_BYTES) {
            Ok(n) => {
                file.sync_all()?;
                n
            }
            // Over the size cap (or an I/O error): drop the partial file so it
            // is never mistaken for a complete offline copy.
            Err(e) => {
                drop(file);
                let _ = std::fs::remove_file(&tmp);
                return Err(e);
            }
        }
    };
    if written == 0 {
        let _ = std::fs::remove_file(&tmp);
        return Err(anyhow!("Downloaded episode is empty"));
    }
    std::fs::rename(&tmp, dest)?;
    Ok(written)
}

/// A podcast search result: enough to display it and – when selected –
/// subscribe via the feed address (then the usual subscription flow runs).
#[derive(Debug, Clone)]
pub struct PodcastSearchResult {
    pub title: String,
    pub author: Option<String>,
    pub feed_url: String,
    /// Cover URL (iTunes artwork) – for pre-caching and displaying in the list.
    pub image_url: Option<String>,
}

/// Searches podcasts via the **iTunes Search API** (no API key, no account
/// needed) and returns results including the RSS feed address. Blocking – only
/// call from worker threads. An empty search term yields an empty list.
pub fn search_podcasts(term: &str) -> Result<Vec<PodcastSearchResult>> {
    let term = term.trim();
    if term.is_empty() {
        return Ok(Vec::new());
    }
    let url = format!(
        "https://itunes.apple.com/search?media=podcast&entity=podcast&limit=25&term={}",
        crate::core::online::percent_encode(term),
    );
    let agent = ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(8))
        .timeout_read(Duration::from_secs(20))
        .build();
    // Defensive retry/backoff so a transient hiccup doesn't blank the search.
    let Some(resp) = crate::core::net::get_with_retry(&agent, &url, None, "itunes.apple.com")?
    else {
        return Ok(Vec::new());
    };
    let mut bytes = Vec::new();
    resp.into_reader()
        .take(4 * 1024 * 1024) // Cap against unexpectedly large responses.
        .read_to_end(&mut bytes)?;
    parse_search(&bytes)
}

/// Parses the iTunes search response. Results **without** `feedUrl` are
/// discarded – without a feed address there is nothing to subscribe to.
fn parse_search(body: &[u8]) -> Result<Vec<PodcastSearchResult>> {
    let resp: ItunesSearch = serde_json::from_slice(body)?;
    let results = resp
        .results
        .into_iter()
        .filter_map(|r| {
            let feed_url = r.feed_url?.trim().to_string();
            if feed_url.is_empty() {
                return None;
            }
            let title = r
                .collection_name
                .map(|t| decode_entities(t.trim()))
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| "Podcast".to_string());
            Some(PodcastSearchResult {
                title,
                author: r
                    .artist_name
                    .map(|a| decode_entities(a.trim()))
                    .filter(|s| !s.is_empty()),
                feed_url,
                // Prefer the small artwork (enough for the 48 px avatar, loads faster).
                image_url: r
                    .artwork_url_100
                    .or(r.artwork_url_600)
                    .filter(|s| !s.trim().is_empty()),
            })
        })
        .collect();
    Ok(results)
}

#[derive(serde::Deserialize)]
struct ItunesSearch {
    #[serde(default)]
    results: Vec<ItunesPodcast>,
}

#[derive(serde::Deserialize)]
struct ItunesPodcast {
    #[serde(rename = "collectionName", default)]
    collection_name: Option<String>,
    #[serde(rename = "artistName", default)]
    artist_name: Option<String>,
    #[serde(rename = "feedUrl", default)]
    feed_url: Option<String>,
    #[serde(rename = "artworkUrl100", default)]
    artwork_url_100: Option<String>,
    #[serde(rename = "artworkUrl600", default)]
    artwork_url_600: Option<String>,
}

/// Minimal Pango markup escaping (only the characters needed for element text).
fn escape_markup(s: &str) -> String {
    let mut o = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => o.push_str("&amp;"),
            '<' => o.push_str("&lt;"),
            '>' => o.push_str("&gt;"),
            '"' => o.push_str("&quot;"),
            '\'' => o.push_str("&#39;"),
            _ => o.push(c),
        }
    }
    o
}

/// Tries to detect a timestamp `M:SS`/`MM:SS` or `H:MM:SS`/`HH:MM:SS` at byte
/// position `i`. Returns `(length in bytes, milliseconds)`. Boundaries are
/// checked so it does not match into longer numbers (e.g. "12:345" or
/// "2024:01").
fn match_timestamp_at(text: &str, i: usize) -> Option<(usize, i64)> {
    let b = text.as_bytes();
    if i > 0 {
        let p = b[i - 1];
        if p.is_ascii_digit() || p == b':' {
            return None;
        }
    }
    // 1st group: 1–2 digits, then ':'
    let mut j = i;
    while j < b.len() && b[j].is_ascii_digit() {
        j += 1;
    }
    if j == i || j - i > 2 || j >= b.len() || b[j] != b':' {
        return None;
    }
    let g1: i64 = text[i..j].parse().ok()?;
    // 2nd group: exactly 2 digits
    let s2 = j + 1;
    let mut k = s2;
    while k < b.len() && b[k].is_ascii_digit() {
        k += 1;
    }
    if k - s2 != 2 {
        return None;
    }
    let g2: i64 = text[s2..k].parse().ok()?;
    // Optional 3rd group (→ hours:minutes:seconds)
    if k < b.len() && b[k] == b':' {
        let s3 = k + 1;
        let mut l = s3;
        while l < b.len() && b[l].is_ascii_digit() {
            l += 1;
        }
        if l - s3 == 2 {
            let g3: i64 = text[s3..l].parse().ok()?;
            return finish_timestamp(text, i, l, g1 * 3600 + g2 * 60 + g3);
        }
    }
    finish_timestamp(text, i, k, g1 * 60 + g2)
}

fn finish_timestamp(text: &str, start: usize, end: usize, total_secs: i64) -> Option<(usize, i64)> {
    let b = text.as_bytes();
    if end < b.len() {
        let n = b[end];
        if n.is_ascii_digit() || n == b':' {
            return None;
        }
    }
    Some((end - start, total_secs * 1000))
}

/// Chapters (timestamp + label) from the shownotes. Per line the first
/// timestamp is taken; the label is the remaining line text (preferably
/// **after** the timestamp), stripped of separators. Ascending by time, only
/// the first chapter per time. For seekbar markers + hover display.
pub fn parse_chapters(text: &str) -> Vec<(i64, String)> {
    fn strip(s: &str) -> &str {
        s.trim().trim_matches(|c: char| {
            c.is_whitespace()
                || matches!(
                    c,
                    '-' | '–' | '—' | ':' | '·' | '•' | '|' | '(' | ')' | '[' | ']' | '.' | ','
                )
        })
    }
    let mut out: Vec<(i64, String)> = Vec::new();
    for line in text.lines() {
        let b = line.as_bytes();
        let mut i = 0;
        while i < b.len() {
            if b[i].is_ascii_digit() {
                if let Some((len, ms)) = match_timestamp_at(line, i) {
                    let after = strip(&line[i + len..]);
                    let before = strip(&line[..i]);
                    let label = if !after.is_empty() { after } else { before };
                    out.push((ms, label.to_string()));
                    break;
                }
            }
            i += 1;
        }
    }
    out.sort_by_key(|(ms, _)| *ms);
    out.dedup_by_key(|(ms, _)| *ms);
    out
}

/// Converts timestamps in shownotes (e.g. "12:34", "1:02:03") into clickable
/// Pango links `emilia-seek:<ms>`; the rest of the text is markup-escaped.
/// Returns Pango markup (for `gtk::Label` with `use_markup`).
pub fn linkify_timestamps(text: &str) -> String {
    let b = text.as_bytes();
    let mut out = String::with_capacity(text.len() + 32);
    let mut run = 0; // start of the current plain-text section
    let mut i = 0;
    while i < b.len() {
        if b[i].is_ascii_digit() {
            if let Some((len, ms)) = match_timestamp_at(text, i) {
                out.push_str(&escape_markup(&text[run..i]));
                out.push_str("<a href=\"emilia-seek:");
                out.push_str(&ms.to_string());
                out.push_str("\">");
                out.push_str(&escape_markup(&text[i..i + len]));
                out.push_str("</a>");
                i += len;
                run = i;
                continue;
            }
        }
        i += 1;
    }
    out.push_str(&escape_markup(&text[run..]));
    out
}

#[cfg(test)]
mod tests {
    use super::{format_duration, html_to_text, linkify_timestamps, parse_feed, parse_search};

    const SAMPLE: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<rss version="2.0" xmlns:itunes="http://www.itunes.com/dtds/podcast-1.0.dtd">
  <channel>
    <title>Test Podcast</title>
    <image><url>https://example.com/cover.jpg</url></image>
    <item>
      <title>Folge 1</title>
      <pubDate>Mon, 01 Jan 2024 10:00:00 +0000</pubDate>
      <guid>ep-1</guid>
      <description>&lt;p&gt;Hallo &amp; Welt&lt;/p&gt;</description>
      <enclosure url="https://example.com/ep1.mp3" length="123" type="audio/mpeg"/>
      <itunes:duration>00:30:00</itunes:duration>
    </item>
    <item>
      <title>Folge 2</title>
      <enclosure url="https://example.com/ep2.mp3" length="456" type="audio/mpeg"/>
    </item>
    <item>
      <title>Hinweis ohne Audio</title>
    </item>
  </channel>
</rss>"#;

    #[test]
    fn parses_channel_and_audio_episodes() {
        let feed = parse_feed(SAMPLE.as_bytes()).unwrap();
        assert_eq!(feed.title, "Test Podcast");
        assert_eq!(
            feed.image_url.as_deref(),
            Some("https://example.com/cover.jpg")
        );
        // The entry without <enclosure> is skipped → 2 episodes.
        assert_eq!(feed.episodes.len(), 2);

        let ep1 = &feed.episodes[0];
        assert_eq!(ep1.title, "Folge 1");
        assert_eq!(ep1.audio_url, "https://example.com/ep1.mp3");
        assert_eq!(ep1.guid.as_deref(), Some("ep-1"));
        assert_eq!(ep1.duration.as_deref(), Some("00:30:00"));
        // Shownotes: HTML reduced to plain text.
        assert_eq!(ep1.description.as_deref(), Some("Hallo & Welt"));

        assert_eq!(feed.episodes[1].title, "Folge 2");
        assert!(feed.episodes[1].duration.is_none());
        assert!(feed.episodes[1].description.is_none());
    }

    #[test]
    fn formats_duration_variants() {
        assert_eq!(format_duration("00:42:13").as_deref(), Some("42:13"));
        assert_eq!(format_duration("1:02:03").as_deref(), Some("1:02:03"));
        assert_eq!(format_duration("10:00").as_deref(), Some("10:00"));
        assert_eq!(format_duration("3600").as_deref(), Some("1:00:00"));
        assert_eq!(format_duration("3623.5").as_deref(), Some("1:00:23"));
        assert_eq!(format_duration("90").as_deref(), Some("1:30"));
        assert!(format_duration("").is_none());
        assert!(format_duration("keine").is_none());
    }

    #[test]
    fn strips_html_to_plain_text() {
        let html =
            "<p>Erste Zeile</p><p>Zweite &amp; Zeile</p><br>Dritte<ul><li>A</li><li>B</li></ul>";
        assert_eq!(
            html_to_text(html),
            "Erste Zeile\nZweite & Zeile\nDritte\nA\nB"
        );
    }

    #[test]
    fn linkifies_timestamps_and_escapes_rest() {
        let md = linkify_timestamps("Intro 0:30, Thema 12:34 & 1:02:03 Ende");
        assert!(
            md.contains("<a href=\"emilia-seek:30000\">0:30</a>"),
            "{md}"
        );
        assert!(
            md.contains("<a href=\"emilia-seek:754000\">12:34</a>"),
            "{md}"
        );
        assert!(
            md.contains("<a href=\"emilia-seek:3723000\">1:02:03</a>"),
            "{md}"
        );
        assert!(md.contains("&amp;"), "{md}");
        // No false matches in longer numbers.
        let none = linkify_timestamps("Jahr 2024:01 und 12:345 sind keine Marke");
        assert!(!none.contains("emilia-seek"), "{none}");
    }

    #[test]
    fn parse_chapters_extracts_time_and_label() {
        let notes =
            "00:00 Intro\n07:13 - Markt-Update\nThema XY 1:02:03\n00:00 Dublette\nohne Zeit";
        let ch = super::parse_chapters(notes);
        assert_eq!(
            ch,
            vec![
                (0, "Intro".to_string()),
                (433_000, "Markt-Update".to_string()),
                (3_723_000, "Thema XY".to_string()),
            ]
        );
        assert!(super::parse_chapters("kein Zeitcode 2024:01 12:345").is_empty());
    }

    #[test]
    fn errors_when_no_audio() {
        let xml = r#"<rss version="2.0"><channel><title>X</title>
            <item><title>Nur Text</title></item></channel></rss>"#;
        assert!(parse_feed(xml.as_bytes()).is_err());
    }

    #[test]
    fn search_parses_results_and_skips_entries_without_feed() {
        let json = br#"{"resultCount":2,"results":[
            {"collectionName":"Tech Talk","artistName":"Alice",
             "feedUrl":"https://example.com/feed.xml",
             "artworkUrl100":"https://example.com/100.jpg",
             "artworkUrl600":"https://example.com/600.jpg"},
            {"collectionName":"Kein Feed","artistName":"Bob"}
        ]}"#;
        let r = parse_search(json).unwrap();
        // The entry without feedUrl is skipped → 1 result.
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].title, "Tech Talk");
        assert_eq!(r[0].author.as_deref(), Some("Alice"));
        assert_eq!(r[0].feed_url, "https://example.com/feed.xml");
        // The small artwork is preferred.
        assert_eq!(
            r[0].image_url.as_deref(),
            Some("https://example.com/100.jpg")
        );
    }
}
