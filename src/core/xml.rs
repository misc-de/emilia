//! Shared helpers for the hand-rolled `quick-xml` pull parsers (WebDAV
//! PROPFIND, YouTube Atom feeds).
//!
//! Since quick-xml 0.41 a text node is no longer handed over as one escaped
//! blob: entity and character references arrive as their own `GeneralRef`
//! events, so `a &amp; b` reaches the caller as three events. Callers
//! therefore have to accumulate the pieces of an element themselves.

use quick_xml::events::Event;

/// Appends the character data of a `Text` or `GeneralRef` event to `buf`,
/// resolving the reference. Any other event is ignored.
///
/// An unknown entity is kept verbatim (`&foo;`) instead of swallowing it —
/// the values we read (hrefs, display names, ids, dates) are more useful
/// slightly wrong than truncated.
pub fn push_text(buf: &mut String, ev: &Event) {
    match ev {
        Event::Text(t) => buf.push_str(&t.xml10_content().unwrap_or_default()),
        Event::GeneralRef(r) => {
            let name = r.decode().unwrap_or_default();
            let raw = format!("&{name};");
            match quick_xml::escape::unescape(&raw) {
                Ok(s) => buf.push_str(&s),
                Err(_) => buf.push_str(&raw),
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use quick_xml::events::{BytesRef, BytesText};

    fn collect(xml: &str) -> String {
        let mut reader = quick_xml::Reader::from_str(xml);
        let mut out = String::new();
        loop {
            match reader.read_event() {
                Ok(Event::Eof) | Err(_) => break,
                Ok(ev) => push_text(&mut out, &ev),
            }
        }
        out
    }

    #[test]
    fn entities_are_resolved_across_events() {
        assert_eq!(collect("a &amp; b"), "a & b");
        assert_eq!(collect("&lt;tag&gt;"), "<tag>");
        assert_eq!(collect("&#65;&#x42;"), "AB");
    }

    #[test]
    fn unknown_entities_are_kept_verbatim() {
        let mut buf = String::new();
        push_text(&mut buf, &Event::GeneralRef(BytesRef::new("nbsp")));
        assert_eq!(buf, "&nbsp;");
    }

    #[test]
    fn other_events_are_ignored() {
        let mut buf = String::new();
        push_text(&mut buf, &Event::Comment(BytesText::new("note")));
        push_text(&mut buf, &Event::Eof);
        assert!(buf.is_empty());
    }
}
