//! Podcasts: RSS-Feeds einlesen (über die `rss`-Crate) und die Episoden
//! bereitstellen. Audio wird direkt gestreamt (playbin3 spielt http-URLs) –
//! es wird nichts heruntergeladen und keine Audiodatei verändert.

use std::io::Read;
use std::time::Duration;

use anyhow::{anyhow, Result};

use crate::model::Episode;

/// Ergebnis des Feed-Einlesens: Kanaldaten plus Episoden (mit Audio).
pub struct PodcastFeed {
    pub title: String,
    pub image_url: Option<String>,
    pub episodes: Vec<Episode>,
}

/// Liest einen Podcast-RSS-Feed. Nur Einträge mit Audio-Enclosure werden
/// übernommen; Fehler, wenn keine Audio-Episode gefunden wird.
pub fn parse_feed(xml: &[u8]) -> Result<PodcastFeed> {
    let channel = rss::Channel::read_from(xml)?;

    let title = {
        let t = channel.title().trim();
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
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or("Episode")
            .to_string();
        episodes.push(Episode {
            guid: item.guid().map(|g| g.value().to_string()),
            title,
            audio_url,
            published: item.pub_date().map(String::from),
            duration: item.itunes_ext().and_then(|e| e.duration().map(String::from)),
        });
    }
    if episodes.is_empty() {
        return Err(anyhow!("keine Episoden mit Audio im Feed gefunden"));
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
