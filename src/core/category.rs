//! Inhalts-**Merkmal** eines Titels: Musik, Konzert, Podcast oder Hörbuch.
//!
//! Vererbung (spezifischste Ebene gewinnt):
//! `Titel → Album → Interpret → Standard (Musik)`.
//! Eine Ebene kann jede höhere überschreiben. Gespeichert wird nur die
//! abweichende Festlegung; ohne Eintrag gilt die geerbte Ebene.

/// Mögliche Merkmale eines Inhalts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Category {
    Music,
    Concert,
    Podcast,
    Audiobook,
}

impl Category {
    /// Reihenfolge wie in der Auswahl angezeigt.
    pub const ALL: [Category; 4] = [
        Category::Music,
        Category::Concert,
        Category::Podcast,
        Category::Audiobook,
    ];

    /// Standard, wenn nichts festgelegt/geerbt ist.
    pub const DEFAULT: Category = Category::Music;

    /// Stabiler Speicherwert (DB).
    pub fn as_str(self) -> &'static str {
        match self {
            Category::Music => "music",
            Category::Concert => "concert",
            Category::Podcast => "podcast",
            Category::Audiobook => "audiobook",
        }
    }

    /// Anzeigename (Deutsch).
    pub fn label(self) -> &'static str {
        match self {
            Category::Music => "Musik",
            Category::Concert => "Konzert",
            Category::Podcast => "Podcast",
            Category::Audiobook => "Hörbuch",
        }
    }

    pub fn from_str(s: &str) -> Option<Category> {
        match s {
            "music" => Some(Category::Music),
            "concert" => Some(Category::Concert),
            "podcast" => Some(Category::Podcast),
            "audiobook" => Some(Category::Audiobook),
            _ => None,
        }
    }
}

/// Trennzeichen-getrennter Schlüssel für die Album-Ebene (Interpret + Album),
/// damit gleichnamige Alben unterschiedlicher Interpreten nicht kollidieren.
pub fn album_key(artist: &str, album: &str) -> String {
    format!("{artist}\u{1}{album}")
}
