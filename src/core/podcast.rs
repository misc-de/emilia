//! Podcasts: RSS-Feeds einlesen (über die `rss`-Crate) und die Episoden
//! bereitstellen. Audio wird direkt gestreamt (playbin3 spielt http-URLs) –
//! es wird nichts heruntergeladen und keine Audiodatei verändert.

use std::io::Read;
use std::time::Duration;

use anyhow::{anyhow, Result};

use crate::model::Episode;

/// Wandelt ein RFC-2822-Veröffentlichungsdatum („Fri, 29 May 2026 22:00:00
/// -0000") in einen **sortierbaren** Schlüssel `YYYYMMDDHHMMSS`. Zeitzone wird
/// für die Sortierung ignoriert; unparsbare/fehlende Daten ergeben `0`.
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
    // Monatsnamen suchen; davor steht der Tag, danach Jahr und Uhrzeit.
    let mi = tokens.iter().position(|t| month_num(t).is_some());
    let Some(mi) = mi.filter(|&i| i >= 1) else { return 0 };
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

/// Sortier-/Vergleichsschlüssel `YYYYMMDD000000` für ein Datum (Tag genau).
fn date_key(year: i64, month: i64, day: i64) -> i64 {
    ((year * 100 + month) * 100 + day) * 1_000_000
}

/// Bürgerliches Datum (Jahr, Monat, Tag) aus Tagen seit der Unix-Epoche
/// (Algorithmus nach Howard Hinnant).
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

/// Vergleichsschlüssel für „vor ~einem Monat" (heute − 31 Tage), passend zu
/// [`pubdate_key`]. Episoden mit `pubdate_key >= recent_cutoff_key()` gelten als
/// neu (höchstens einen Monat zurück).
pub fn recent_cutoff_key() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let (y, m, d) = civil_from_days(secs / 86_400 - 31);
    date_key(y, m, d)
}

/// Schwellen-Schlüssel (passend zu [`pubdate_key`]) für die Gruppierung der
/// neuesten Episoden – jeweils Mitternacht: `(heute, gestern, vor 7 Tagen)`.
/// „Diese Woche" meint bewusst die **letzten 7 Tage (rollierend)**, nicht die
/// Kalenderwoche; älter als das = „Diesen Monat".
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
    // Rollierend: alles ab heute − 6 Tagen zählt zu „diese Woche" (= letzte 7 Tage).
    let week_start = key_of(today_days - 6);
    (today, yesterday, week_start)
}

/// Kurzform eines RFC-2822-Datums für die Anzeige: „29 May 2026" (ohne Wochentag,
/// Uhrzeit und Zeitzone). Unparsbares wird unverändert zurückgegeben.
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

/// Ergebnis des Feed-Einlesens: Kanaldaten plus Episoden (mit Audio).
pub struct PodcastFeed {
    pub title: String,
    pub image_url: Option<String>,
    pub episodes: Vec<Episode>,
}

/// Dekodiert verbliebene HTML-/XML-Entities in Feed-Texten (v. a. Titel):
/// numerische Referenzen (`&#128512;`, `&#x1F600;` → Emoji/Smiley) und gängige
/// benannte Entities. Viele Feeds sind doppelt kodiert oder nutzen HTML-Entities,
/// die der XML-Parser stehen lässt – das holt diese Zeichen wieder hervor.
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
        // Entity-Namen bis zum ';' (kurz halten, sonst kein echtes Entity).
        if let Some(semi) = after.find(';').filter(|&p| p > 0 && p <= 12) {
            let ent = &after[..semi];
            let decoded = if let Some(hex) =
                ent.strip_prefix("#x").or_else(|| ent.strip_prefix("#X"))
            {
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
        // Kein gültiges Entity → das „&" unverändert übernehmen.
        out.push('&');
        rest = after;
    }
    out.push_str(rest);
    out
}

/// Liest einen Podcast-RSS-Feed. Nur Einträge mit Audio-Enclosure werden
/// übernommen; Fehler, wenn keine Audio-Episode gefunden wird.
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
    let image_url = channel
        .image()
        .map(|i| i.url().to_string())
        .or_else(|| channel.itunes_ext().and_then(|e| e.image().map(String::from)));

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
        episodes.push(Episode {
            guid: item.guid().map(|g| g.value().to_string()),
            title,
            audio_url,
            published: item.pub_date().map(String::from),
            duration: item.itunes_ext().and_then(|e| e.duration().map(String::from)),
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

/// Holt einen Feed per HTTP und liest ihn ein (blockierend – im Worker nutzen).
pub fn fetch_feed(url: &str) -> Result<PodcastFeed> {
    let agent = ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(8))
        .timeout_read(Duration::from_secs(20))
        .build();
    let mut bytes = Vec::new();
    agent
        .get(url)
        .call()?
        .into_reader()
        .read_to_end(&mut bytes)?;
    parse_feed(&bytes)
}

#[cfg(test)]
mod tests {
    use super::parse_feed;

    const SAMPLE: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<rss version="2.0" xmlns:itunes="http://www.itunes.com/dtds/podcast-1.0.dtd">
  <channel>
    <title>Test Podcast</title>
    <image><url>https://example.com/cover.jpg</url></image>
    <item>
      <title>Folge 1</title>
      <pubDate>Mon, 01 Jan 2024 10:00:00 +0000</pubDate>
      <guid>ep-1</guid>
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
        assert_eq!(feed.image_url.as_deref(), Some("https://example.com/cover.jpg"));
        // Der Eintrag ohne <enclosure> wird übersprungen → 2 Episoden.
        assert_eq!(feed.episodes.len(), 2);

        let ep1 = &feed.episodes[0];
        assert_eq!(ep1.title, "Folge 1");
        assert_eq!(ep1.audio_url, "https://example.com/ep1.mp3");
        assert_eq!(ep1.guid.as_deref(), Some("ep-1"));
        assert_eq!(ep1.duration.as_deref(), Some("00:30:00"));

        assert_eq!(feed.episodes[1].title, "Folge 2");
        assert!(feed.episodes[1].duration.is_none());
    }

    #[test]
    fn errors_when_no_audio() {
        let xml = r#"<rss version="2.0"><channel><title>X</title>
            <item><title>Nur Text</title></item></channel></rss>"#;
        assert!(parse_feed(xml.as_bytes()).is_err());
    }
}
