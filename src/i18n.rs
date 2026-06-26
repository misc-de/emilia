//! Internationalization (i18n) via gettext.
//!
//! The source strings are in **English** (gettext `msgid`); translations are
//! stored as `po/<lang>.po` → `<lang>/LC_MESSAGES/emilia.mo`. If a catalog or an
//! entry is missing, the English original text automatically appears.

use std::path::PathBuf;
use std::sync::OnceLock;

use gettextrs::{bind_textdomain_codeset, bindtextdomain, setlocale, textdomain, LocaleCategory};

pub use gettextrs::{gettext, ngettext, npgettext};

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
    // Remember the language the UI is actually built in, *before* a later
    // `switch_language` mutates the environment. The relaunch decision after the
    // first-run setup needs this baseline, not the live (now-mutable) `LANGUAGE`.
    let startup = match lang {
        Some(code) => LANGUAGES
            .iter()
            .map(|(c, _)| *c)
            .find(|c| *c == code)
            .unwrap_or("en"),
        None => system_language_code(),
    };
    let _ = STARTUP_LANG.set(startup);

    if let Some(code) = lang {
        // gettext evaluates LANGUAGE before LC_MESSAGES (as long as the locale is
        // not C/POSIX) – this way the language can be chosen independently of the system.
        std::env::set_var("LANGUAGE", code);
    }

    // Apply the locale from the environment. This can FAIL: when the environment
    // names a locale the runtime doesn't ship, glibc keeps the "C" locale – and
    // under "C"/"POSIX" gettext IGNORES `LANGUAGE`, so the chosen UI language
    // silently stays English. This bites on phones (Flatpak): the GNOME runtime
    // installs only the *configured* system languages, but a user can set German
    // regional formats (`LC_TIME=de_DE.UTF-8`, …) while keeping the display
    // language English – `de_DE.UTF-8` isn't installed, `setlocale(LC_ALL, "")`
    // returns NULL, the locale falls back to "C", and `LANGUAGE=de` is dropped.
    // If that happens, pin a valid, installed UTF-8 locale in the ENVIRONMENT
    // (not just via this one call) so that this *and* any later
    // `setlocale(LC_ALL, "")` (e.g. GTK's own) succeed instead of hitting "C".
    // `C.UTF-8` does NOT help (gettext still ignores LANGUAGE under it), so a
    // real locale like `en_US.UTF-8` – always present in the GNOME runtime – is
    // required; messages then come from the chosen `LANGUAGE` catalog regardless
    // of which glibc locales are installed (the whole point of shipping our own
    // catalogs). The fallback only triggers when the env locale is unavailable,
    // so a working `de_DE`/etc. system locale is never clobbered.
    if setlocale(LocaleCategory::LcAll, "").is_none() {
        for fallback in ["en_US.UTF-8", "C.UTF-8"] {
            std::env::set_var("LC_ALL", fallback);
            if setlocale(LocaleCategory::LcAll, "").is_some() {
                break;
            }
        }
    }

    let dir = locale_dir();
    let _ = bindtextdomain(DOMAIN, dir);
    let _ = bind_textdomain_codeset(DOMAIN, "UTF-8");
    let _ = textdomain(DOMAIN);
}

/// The display language the UI was actually built in, captured by [`init`]
/// before any runtime [`switch_language`] changed the environment. Use this —
/// not [`system_language_code`], which reads the now-mutable `LANGUAGE` — to
/// decide whether a restart is needed to rebuild the already-built main window.
pub fn startup_language_code() -> &'static str {
    STARTUP_LANG
        .get()
        .copied()
        .unwrap_or_else(system_language_code)
}

/// Set once by [`init`]; see [`startup_language_code`].
static STARTUP_LANG: OnceLock<&'static str> = OnceLock::new();

/// Switch the catalog language at runtime — used by the first-run setup so the
/// wizard itself flips to the chosen language immediately, instead of only after
/// the final restart. Sets `LANGUAGE` and flushes glibc's gettext catalog cache.
///
/// gettext does **not** retranslate already-built widgets: it only affects the
/// strings looked up *after* this call, so the caller must rebuild whatever is
/// currently on screen. On non-glibc targets the cache can't be flushed, so the
/// change takes effect on the next launch (the setup persists the choice anyway).
pub fn switch_language(code: &str) {
    // gettext only honors `LANGUAGE` when the LC_MESSAGES locale isn't C/POSIX;
    // `init` already pinned a real UTF-8 locale at startup, so this is enough.
    std::env::set_var("LANGUAGE", code);
    // glibc caches loaded catalogs and only re-reads them when its internal
    // generation counter changes; bumping it makes the next `gettext()` pick up
    // the new `LANGUAGE`. This is the documented way (GNU gettext manual,
    // "Changing the language at run time") to switch without a restart.
    #[cfg(target_env = "gnu")]
    {
        extern "C" {
            static mut _nl_msg_cat_cntr: std::os::raw::c_int;
        }
        // SAFETY: a plain increment of a glibc-owned counter on the main thread.
        unsafe {
            _nl_msg_cat_cntr += 1;
        }
    }
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

/// Context-qualified [`ngettext_n`] (`msgctxt`): same English source, but a
/// distinct translation per context — e.g. "{n} track" reads "{n} Titel"
/// normally but "{n} Track" in the audiobook menus.
pub fn npgettext_n(msgctxt: &str, msgid: &str, msgid_plural: &str, n: u32) -> String {
    npgettext(msgctxt, msgid, msgid_plural, n).replace("{n}", &n.to_string())
}
