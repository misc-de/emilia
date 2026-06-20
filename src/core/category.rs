//! **Properties** of a content item: in which areas it appears.
//!
//! A track/album/artist can be visible in multiple areas (filesystem, artists,
//! albums, concerts, audiobooks). Stored per level is a comma-separated list of
//! the areas; an **empty** list means "hidden" (visible nowhere).
//!
//! Inheritance (most specific level wins): `track → album → artist → default`.
//! Default = filesystem + artists + albums. Only deviating settings are stored.

/// An area in which a content item can appear.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Area {
    Filesystem,
    Artists,
    Albums,
    Singles,
    Compilations,
    Concerts,
    Audiobooks,
}

impl Area {
    /// All areas in display order.
    pub const ALL: [Area; 7] = [
        Area::Filesystem,
        Area::Artists,
        Area::Albums,
        Area::Singles,
        Area::Compilations,
        Area::Concerts,
        Area::Audiobooks,
    ];

    /// Default visibility when nothing is set/inherited. `Singles`/`Compilations`
    /// are *not* part of the static default: an album is filed there from its
    /// (auto/overridden) [`AlbumKind`] classification — see the kind-aware album
    /// resolution in the DB layer — so the default switches reflect that without
    /// it being a hard-coded base area.
    pub const DEFAULT: [Area; 3] = [Area::Filesystem, Area::Artists, Area::Albums];

    /// Stable storage value (DB).
    pub fn as_str(self) -> &'static str {
        match self {
            Area::Filesystem => "filesystem",
            Area::Artists => "artists",
            Area::Albums => "albums",
            Area::Singles => "singles",
            Area::Compilations => "compilations",
            Area::Concerts => "concerts",
            Area::Audiobooks => "audiobooks",
        }
    }

    /// Display name as the English gettext `msgid` – matching the navigation
    /// menu items. Translate at the display site with `i18n::gettext()`.
    pub fn label(self) -> &'static str {
        match self {
            Area::Filesystem => "Files",
            Area::Artists => "Artists",
            Area::Albums => "Albums",
            Area::Singles => "Singles",
            Area::Compilations => "Compilations",
            Area::Concerts => "Concerts",
            Area::Audiobooks => "Audiobooks",
        }
    }

    /// Associated navigation menu item (stack name), if present. If a menu item
    /// is hidden, the associated area also disappears from the properties
    /// selection. `Audiobooks` has no menu item of its own and therefore remains
    /// always selectable.
    pub fn section(self) -> Option<&'static str> {
        match self {
            Area::Filesystem => Some("files"),
            Area::Artists => Some("artists"),
            Area::Albums => Some("albums"),
            Area::Singles => Some("singles"),
            Area::Compilations => Some("compilations"),
            Area::Concerts => Some("concerts"),
            Area::Audiobooks => None,
        }
    }

    pub fn from_str(s: &str) -> Option<Area> {
        match s {
            "filesystem" => Some(Area::Filesystem),
            "artists" => Some(Area::Artists),
            "albums" => Some(Area::Albums),
            "singles" => Some(Area::Singles),
            "compilations" => Some(Area::Compilations),
            "concerts" => Some(Area::Concerts),
            "audiobooks" => Some(Area::Audiobooks),
            _ => None,
        }
    }
}

/// Parses a stored area list (`"filesystem,albums"`). An empty string yields an
/// **empty** list (= hidden).
pub fn parse_areas(value: &str) -> Vec<Area> {
    value
        .split(',')
        .filter_map(|s| Area::from_str(s.trim()))
        .collect()
}

/// Serializes an area list for the DB (comma-separated, stable order).
pub fn areas_value(areas: &[Area]) -> String {
    Area::ALL
        .iter()
        .filter(|a| areas.contains(a))
        .map(|a| a.as_str())
        .collect::<Vec<_>>()
        .join(",")
}

/// Separator-delimited key for the album level (artist + album), so that
/// identically named albums by different artists do not collide.
pub fn album_key(artist: &str, album: &str) -> String {
    format!("{artist}\u{1}{album}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_and_serialize_roundtrip() {
        assert_eq!(parse_areas(""), vec![]); // hidden
        assert_eq!(
            parse_areas("filesystem,albums"),
            vec![Area::Filesystem, Area::Albums]
        );
        // Serialization in stable ALL order.
        assert_eq!(
            areas_value(&[Area::Albums, Area::Filesystem]),
            "filesystem,albums"
        );
        assert_eq!(areas_value(&Area::DEFAULT), "filesystem,artists,albums");
        assert_eq!(areas_value(&[]), "");
    }
}
