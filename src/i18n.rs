//! Internationalisierung (i18n) über gettext.
//!
//! Die Quelltext-Strings sind in **Englisch** (gettext-`msgid`); Übersetzungen
//! liegen als `po/<lang>.po` → `<lang>/LC_MESSAGES/emilia.mo`. Fehlt ein
//! Katalog oder ein Eintrag, erscheint automatisch der englische Originaltext.

use std::path::PathBuf;

use gettextrs::{
    bind_textdomain_codeset, bindtextdomain, setlocale, textdomain, LocaleCategory,
};

pub use gettextrs::{gettext, ngettext};

/// Textdomain (entspricht dem `.mo`-Dateinamen `emilia.mo`).
pub const DOMAIN: &str = "emilia";

/// Initialisiert gettext. Muss vor jeder Übersetzung laufen (früh in `main`).
///
/// `lang`: `None` folgt der System-Locale (`LANG`/`LC_*`); `Some("de"|"en")`
/// erzwingt die jeweilige Sprache über die `LANGUAGE`-Umgebungsvariable.
pub fn init(lang: Option<&str>) {
    if let Some(code) = lang {
        // gettext wertet LANGUAGE vor LC_MESSAGES aus (sofern die Locale nicht
        // C/POSIX ist) – so lässt sich die Sprache unabhängig vom System wählen.
        std::env::set_var("LANGUAGE", code);
    }
    setlocale(LocaleCategory::LcAll, "");

    let dir = locale_dir();
    let _ = bindtextdomain(DOMAIN, dir);
    let _ = bind_textdomain_codeset(DOMAIN, "UTF-8");
    let _ = textdomain(DOMAIN);
}

/// Verzeichnis mit den `.mo`-Katalogen ermitteln:
/// `EMILIA_LOCALEDIR` (Entwicklung) → `<exe>/../share/locale` (lokal
/// installiert) → `/usr/share/locale` (Systeminstallation).
fn locale_dir() -> PathBuf {
    if let Ok(d) = std::env::var("EMILIA_LOCALEDIR") {
        return PathBuf::from(d);
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(prefix) = exe.parent().and_then(|p| p.parent()) {
            let p = prefix.join("share").join("locale");
            if p.is_dir() {
                return p;
            }
        }
    }
    PathBuf::from("/usr/share/locale")
}

/// `gettext` mit benannten Platzhaltern `{name}`.
///
/// Nötig, weil `format!` keinen Laufzeit-String als Formatvorlage akzeptiert.
/// Beispiel: `gettext_f("Added {n} tracks", &[("n", &n.to_string())])`.
pub fn gettext_f(msgid: &str, args: &[(&str, &str)]) -> String {
    let mut s = gettext(msgid);
    for (key, value) in args {
        s = s.replace(&format!("{{{key}}}"), value);
    }
    s
}

/// `ngettext` (Singular/Plural je `n`) mit automatischem `{n}`-Platzhalter.
///
/// Beispiel: `ngettext_n("{n} album", "{n} albums", count)`.
pub fn ngettext_n(msgid: &str, msgid_plural: &str, n: u32) -> String {
    ngettext(msgid, msgid_plural, n).replace("{n}", &n.to_string())
}
