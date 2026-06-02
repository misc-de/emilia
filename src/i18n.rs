//! Internationalization (i18n) via gettext.
//!
//! The source strings are in **English** (gettext `msgid`); translations are
//! stored as `po/<lang>.po` → `<lang>/LC_MESSAGES/emilia.mo`. If a catalog or an
//! entry is missing, the English original text automatically appears.

use std::path::PathBuf;

use gettextrs::{
    bind_textdomain_codeset, bindtextdomain, setlocale, textdomain, LocaleCategory,
};

pub use gettextrs::{gettext, ngettext};

/// Text domain (corresponds to the `.mo` file name `emilia.mo`).
pub const DOMAIN: &str = "emilia";

/// Initializes gettext. Must run before any translation (early in `main`).
///
/// `lang`: `None` follows the system locale (`LANG`/`LC_*`); `Some("de"|"en")`
/// forces the respective language via the `LANGUAGE` environment variable.
pub fn init(lang: Option<&str>) {
    if let Some(code) = lang {
        // gettext evaluates LANGUAGE before LC_MESSAGES (as long as the locale is
        // not C/POSIX) – this way the language can be chosen independently of the system.
        std::env::set_var("LANGUAGE", code);
    }
    setlocale(LocaleCategory::LcAll, "");

    let dir = locale_dir();
    let _ = bindtextdomain(DOMAIN, dir);
    let _ = bind_textdomain_codeset(DOMAIN, "UTF-8");
    let _ = textdomain(DOMAIN);
}

/// Determine the directory with the `.mo` catalogs:
/// `EMILIA_LOCALEDIR` (development) → `<exe>/../share/locale` (locally
/// installed) → `/usr/share/locale` (system installation).
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

/// `gettext` with named placeholders `{name}`.
///
/// Needed because `format!` does not accept a runtime string as a format
/// template. Example: `gettext_f("Added {n} tracks", &[("n", &n.to_string())])`.
pub fn gettext_f(msgid: &str, args: &[(&str, &str)]) -> String {
    let mut s = gettext(msgid);
    for (key, value) in args {
        s = s.replace(&format!("{{{key}}}"), value);
    }
    s
}

/// `ngettext` (singular/plural depending on `n`) with an automatic `{n}` placeholder.
///
/// Example: `ngettext_n("{n} album", "{n} albums", count)`.
pub fn ngettext_n(msgid: &str, msgid_plural: &str, n: u32) -> String {
    ngettext(msgid, msgid_plural, n).replace("{n}", &n.to_string())
}
