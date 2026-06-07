//! Song lyrics: parsing the LRC format and the in-memory model used by the UI.
//!
//! Lyrics come from two places:
//! - **Embedded** in the audio file's tags (unsynchronized), read by
//!   [`crate::core::scanner::read_lyrics`].
//! - **Online** from [LRCLIB](https://lrclib.net) – a free, key-less service that
//!   returns both plain and synchronized (`.lrc`) lyrics. The fetch lives in
//!   [`crate::core::online::OnlineClient::fetch_lyrics`]; the result is cached in
//!   the database so it is fetched at most once per track.
//!
//! Like the rest of the online metadata, nothing is ever written back into the
//! audio file's tags.

/// Lyrics for one track. Always carries the (optional) plain text; `synced`
/// holds the timed lines parsed from an LRC source (empty when unsynchronized).
#[derive(Debug, Clone, Default)]
pub struct Lyrics {
    /// Unsynchronized plain text, if available.
    pub plain: Option<String>,
    /// Timed lines `(milliseconds, text)`, sorted by time. Empty = no `.lrc`.
    pub synced: Vec<(i64, String)>,
    /// Original LRC text behind `synced` – kept so it can be cached without a
    /// parse round-trip. `None` when there is no synchronized text.
    pub synced_raw: Option<String>,
}

impl Lyrics {
    /// Builds a [`Lyrics`] from the raw parts as stored/received: an optional
    /// plain text and an optional raw LRC string (which is parsed here).
    pub fn from_parts(plain: Option<String>, synced_raw: Option<String>) -> Self {
        let plain = plain
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        let synced_raw = synced_raw
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        let synced = synced_raw.as_deref().map(parse_lrc).unwrap_or_default();
        // A raw string that parsed to nothing usable is treated as "no synced".
        let synced_raw = if synced.is_empty() { None } else { synced_raw };
        Self {
            plain,
            synced,
            synced_raw,
        }
    }

    /// True when there are timed (karaoke-capable) lines.
    pub fn has_synced(&self) -> bool {
        !self.synced.is_empty()
    }

    /// True when there is any text at all (plain or synced).
    pub fn has_any(&self) -> bool {
        self.plain.is_some() || !self.synced.is_empty()
    }

    /// Index of the line that is active at playback position `pos_ms` – the last
    /// line whose timestamp is `<= pos_ms`. `None` before the first line (or when
    /// there are no synced lines).
    pub fn active_line(&self, pos_ms: i64) -> Option<usize> {
        if self.synced.is_empty() {
            return None;
        }
        // `partition_point` returns the count of lines strictly before `pos`;
        // the active line is the one just before that boundary.
        let n = self.synced.partition_point(|(t, _)| *t <= pos_ms);
        (n > 0).then(|| n - 1)
    }

    /// Plain text for a static display. Prefers the synced lines (joined without
    /// their timestamps) – they are the cleaner source and match the karaoke
    /// view – and falls back to the explicit plain text. (Some LRCLIB entries
    /// have stray timestamps inside `plainLyrics`, which this avoids.)
    pub fn display_text(&self) -> Option<String> {
        if !self.synced.is_empty() {
            let joined = self
                .synced
                .iter()
                .map(|(_, t)| t.as_str())
                .collect::<Vec<_>>()
                .join("\n");
            if !joined.trim().is_empty() {
                return Some(joined);
            }
        }
        self.plain
            .as_ref()
            .map(|p| p.trim().to_string())
            .filter(|p| !p.is_empty())
    }
}

/// Parses LRC text into timed lines `(milliseconds, text)`, sorted by time.
///
/// Accepts the common forms `[mm:ss.xx]`, `[mm:ss.xxx]` and `[mm:ss]`, including
/// several timestamps on one line (`[00:10.00][01:20.00] text`). Metadata tags
/// such as `[ar:…]` / `[length:…]` and lines without a timestamp are dropped.
pub fn parse_lrc(raw: &str) -> Vec<(i64, String)> {
    let mut out: Vec<(i64, String)> = Vec::new();
    for line in raw.lines() {
        let mut rest = line;
        let mut times: Vec<i64> = Vec::new();
        // Peel off all leading `[…]` time tags.
        loop {
            let trimmed = rest.trim_start();
            if !trimmed.starts_with('[') {
                rest = trimmed;
                break;
            }
            let Some(end) = trimmed.find(']') else {
                rest = trimmed;
                break;
            };
            match parse_timestamp(&trimmed[1..end]) {
                Some(ms) => {
                    times.push(ms);
                    rest = &trimmed[end + 1..];
                }
                // A non-timestamp tag (metadata) → not a lyric line.
                None => {
                    rest = trimmed;
                    break;
                }
            }
        }
        if times.is_empty() {
            continue;
        }
        let text = rest.trim().to_string();
        for t in times {
            out.push((t, text.clone()));
        }
    }
    out.sort_by_key(|(t, _)| *t);
    out
}

/// Parses an LRC timestamp body like `01:23.45` (without the brackets) into
/// milliseconds. Returns `None` for anything that is not a valid `mm:ss[.frac]`.
fn parse_timestamp(s: &str) -> Option<i64> {
    let s = s.trim();
    let (min_str, rest) = s.split_once(':')?;
    let minutes: i64 = min_str.trim().parse().ok()?;
    let (sec_str, frac_str) = match rest.split_once(['.', ':']) {
        Some((a, b)) => (a, b),
        None => (rest, ""),
    };
    let seconds: i64 = sec_str.trim().parse().ok()?;
    // Guards against mis-parsing metadata (e.g. a stray "12:99").
    if !(0..60).contains(&seconds) || minutes < 0 {
        return None;
    }
    let frac_ms = if frac_str.is_empty() {
        0
    } else {
        let digits: String = frac_str.chars().take(3).collect();
        if !digits.chars().all(|c| c.is_ascii_digit()) {
            return None;
        }
        let mut v: i64 = digits.parse().ok()?;
        // Scale to milliseconds: 1 digit = tenths, 2 = hundredths, 3 = millis.
        match digits.len() {
            1 => v *= 100,
            2 => v *= 10,
            _ => {}
        }
        v
    };
    Some((minutes * 60 + seconds) * 1000 + frac_ms)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_basic_lrc() {
        let raw = "[ar:Some Artist]\n[00:01.00]first\n[00:03.50]second\n[01:00.00]third";
        let lines = parse_lrc(raw);
        assert_eq!(
            lines,
            vec![
                (1000, "first".to_string()),
                (3500, "second".to_string()),
                (60000, "third".to_string()),
            ]
        );
    }

    #[test]
    fn handles_three_digit_fraction_and_multi_tag() {
        let raw = "[00:10.123][00:20.5] repeated";
        let lines = parse_lrc(raw);
        assert_eq!(
            lines,
            vec![
                (10123, "repeated".to_string()),
                (20500, "repeated".to_string()),
            ]
        );
    }

    #[test]
    fn active_line_tracks_position() {
        let lyr = Lyrics::from_parts(None, Some("[00:01.00]a\n[00:03.00]b".to_string()));
        assert_eq!(lyr.active_line(0), None);
        assert_eq!(lyr.active_line(1500), Some(0));
        assert_eq!(lyr.active_line(3000), Some(1));
        assert_eq!(lyr.active_line(9999), Some(1));
        assert!(lyr.has_synced());
    }

    #[test]
    fn empty_lrc_yields_no_synced() {
        let lyr = Lyrics::from_parts(Some("just plain".to_string()), Some("[ti:x]".to_string()));
        assert!(!lyr.has_synced());
        assert!(lyr.synced_raw.is_none());
        assert_eq!(lyr.display_text(), Some("just plain".to_string()));
    }
}
