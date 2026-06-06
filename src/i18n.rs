//! Internationalization (i18n) via gettext.
//!
//! The source strings are in **English** (gettext `msgid`); translations are
//! stored as `po/<lang>.po` → `<lang>/LC_MESSAGES/emilia.mo`. If a catalog or an
//! entry is missing, the English original text automatically appears.

use std::path::PathBuf;

use gettextrs::{bind_textdomain_codeset, bindtextdomain, setlocale, textdomain, LocaleCategory};

pub use gettextrs::{gettext, ngettext};

/// Text domain (corresponds to the `.mo` file name `emilia.mo`).
pub const DOMAIN: &str = "emilia";

/// Selectable display languages as `(stable code, endonym)`. The endonym (the
/// language's own name) is shown untranslated; English is the source language
/// (no catalog needed). Order: Latin scripts first, then Cyrillic, Arabic,
/// Chinese – mirroring the original list. The special "system" choice (follow
/// the OS locale) is **not** part of this list; callers add it where needed.
pub const LANGUAGES: &[(&str, &str)] = &[
    ("de", "Deutsch"),
    ("en", "English"),
    ("es", "Español"),
    ("fr", "Français"),
    ("it", "Italiano"),
    ("sw", "Kiswahili"),
    ("nl", "Nederlands"),
    ("pl", "Polski"),
    ("pt", "Português"),
    ("ru", "Русский"),
    ("ar", "العربية"),
    ("zh", "中文"),
];

/// The supported language code that best matches the **system** locale, or
/// `"en"` (the source language) as a fallback. Reads the locale environment in
/// gettext's precedence order and maps its two-letter prefix onto [`LANGUAGES`].
/// Used only to pre-select an entry (e.g. in the first-run setup); it does not
/// change any setting.
pub fn system_language_code() -> &'static str {
    let raw = ["LC_ALL", "LC_MESSAGES", "LANGUAGE", "LANG"]
        .into_iter()
        .filter_map(|k| std::env::var(k).ok())
        .find(|v| !v.is_empty())
        .unwrap_or_default();
    // Take the first locale of a possibly colon-separated LANGUAGE list, then
    // its language part before any '_'/'.'/'@' (e.g. "de_DE.UTF-8" → "de").
    let code = raw
        .split(':')
        .next()
        .unwrap_or("")
        .split(['_', '.', '@'])
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    LANGUAGES
        .iter()
        .map(|(c, _)| *c)
        .find(|c| *c == code)
        .unwrap_or("en")
}

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
        // Installed layout: <prefix>/bin/emilia → <prefix>/share/locale.
        if let Some(prefix) = exe.parent().and_then(|p| p.parent()) {
            let p = prefix.join("share").join("locale");
            if p.is_dir() {
                return p;
            }
        }
        // Development: the binary is run straight from target/{debug,release},
        // so no catalog is installed anywhere. Fall back to the checkout's
        // `po/` dir (same layout: po/<lang>/LC_MESSAGES/emilia.mo), so the
        // in-app language switch works without setting EMILIA_LOCALEDIR.
        for dir in exe.ancestors() {
            if dir.join("Cargo.toml").is_file() {
                let po = dir.join("po");
                if po.is_dir() {
                    return po;
                }
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
