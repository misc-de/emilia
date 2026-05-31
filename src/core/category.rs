//! **Eigenschaften** eines Inhalts: in welchen Bereichen er erscheint.
//!
//! Ein Titel/Album/Interpret kann in mehreren Bereichen sichtbar sein
//! (Dateisystem, Interpreten, Alben, Konzerte, Hörbücher). Gespeichert wird je
//! Ebene eine kommaseparierte Liste der Bereiche; eine **leere** Liste bedeutet
//! „ausgeblendet" (nirgends sichtbar).
//!
//! Vererbung (spezifischste Ebene gewinnt): `Titel → Album → Interpret →
//! Standard`. Standard = Dateisystem + Interpreten + Alben. Nur abweichende
//! Festlegungen werden gespeichert.

/// Ein Bereich, in dem ein Inhalt auftauchen kann.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Area {
    Filesystem,
    Artists,
    Albums,
    Concerts,
    Audiobooks,
}

impl Area {
    /// Alle Bereiche in Anzeigereihenfolge.
    pub const ALL: [Area; 5] = [
        Area::Filesystem,
        Area::Artists,
        Area::Albums,
        Area::Concerts,
        Area::Audiobooks,
    ];

    /// Standard-Sichtbarkeit, wenn nichts festgelegt/geerbt ist.
    pub const DEFAULT: [Area; 3] = [Area::Filesystem, Area::Artists, Area::Albums];

    /// Stabiler Speicherwert (DB).
    pub fn as_str(self) -> &'static str {
        match self {
            Area::Filesystem => "filesystem",
            Area::Artists => "artists",
            Area::Albums => "albums",
            Area::Concerts => "concerts",
            Area::Audiobooks => "audiobooks",
        }
    }

    /// Anzeigename als englische gettext-`msgid` – passend zu den
    /// Navigations-Menüpunkten. Am Anzeigeort mit `i18n::gettext()` übersetzen.
    pub fn label(self) -> &'static str {
        match self {
            Area::Filesystem => "Files",
            Area::Artists => "Artists",
            Area::Albums => "Albums",
            Area::Concerts => "Concerts",
            Area::Audiobooks => "Audiobooks",
        }
    }

    /// Zugehöriger Navigations-Menüpunkt (Stack-Name), falls vorhanden. Wird ein
    /// Menüpunkt ausgeblendet, verschwindet der zugehörige Bereich auch aus der
    /// Eigenschaften-Auswahl. `Hörbücher` hat keinen eigenen Menüpunkt und bleibt
    /// daher immer wählbar.
    pub fn section(self) -> Option<&'static str> {
        match self {
            Area::Filesystem => Some("files"),
            Area::Artists => Some("artists"),
            Area::Albums => Some("albums"),
            Area::Concerts => Some("concerts"),
            Area::Audiobooks => None,
        }
    }

    pub fn from_str(s: &str) -> Option<Area> {
        match s {
            "filesystem" => Some(Area::Filesystem),
            "artists" => Some(Area::Artists),
            "albums" => Some(Area::Albums),
            "concerts" => Some(Area::Concerts),
            "audiobooks" => Some(Area::Audiobooks),
            _ => None,
        }
    }
}

/// Parst eine gespeicherte Bereichsliste (`"filesystem,albums"`). Eine leere
/// Zeichenkette ergibt eine **leere** Liste (= ausgeblendet).
pub fn parse_areas(value: &str) -> Vec<Area> {
    value
        .split(',')
        .filter_map(|s| Area::from_str(s.trim()))
        .collect()
}

/// Serialisiert eine Bereichsliste für die DB (kommasepariert, stabile Folge).
pub fn areas_value(areas: &[Area]) -> String {
    Area::ALL
        .iter()
        .filter(|a| areas.contains(a))
        .map(|a| a.as_str())
        .collect::<Vec<_>>()
        .join(",")
}

/// Trennzeichen-getrennter Schlüssel für die Album-Ebene (Interpret + Album),
/// damit gleichnamige Alben unterschiedlicher Interpreten nicht kollidieren.
pub fn album_key(artist: &str, album: &str) -> String {
    format!("{artist}\u{1}{album}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_and_serialize_roundtrip() {
        assert_eq!(parse_areas(""), vec![]); // ausgeblendet
        assert_eq!(parse_areas("filesystem,albums"), vec![Area::Filesystem, Area::Albums]);
        // Serialisierung in stabiler ALL-Reihenfolge.
        assert_eq!(
            areas_value(&[Area::Albums, Area::Filesystem]),
            "filesystem,albums"
        );
        assert_eq!(areas_value(&Area::DEFAULT), "filesystem,artists,albums");
        assert_eq!(areas_value(&[]), "");
    }
}
