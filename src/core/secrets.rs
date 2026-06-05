//! Secret Service bridge via `secret-tool` (part of libsecret).
//!
//! Keeps security-critical data — Nextcloud usernames/app passwords and API
//! keys/tokens — out of the plaintext SQLite database on systems with a Secret
//! Service provider (GNOME Keyring, KWallet via the Secret Service API, …).
//! Every item carries the attribute `application=emilia`. If `secret-tool` is
//! missing or the keyring is unavailable, the store functions return
//! `false`/`None` and callers fall back to the local database.

use std::io::Write;
use std::process::{Command, Stdio};

/// Marks a DB column/setting whose real value lives in the Secret Service. For
/// per-source items the source id is appended (`secret-tool:7`); for named
/// secrets (settings) the bare prefix is stored.
pub const SECRET_PREFIX: &str = "secret-tool:";

/// Stores `value` under the given secret-tool attributes. Returns whether it was
/// actually written to the keyring.
fn store(attrs: &[(&str, &str)], label: &str, value: &str) -> bool {
    let mut cmd = Command::new("secret-tool");
    cmd.arg("store").arg(format!("--label={label}"));
    for (k, v) in attrs {
        cmd.args([*k, *v]);
    }
    let mut child = match cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(child) => child,
        Err(_) => return false,
    };
    let Some(mut stdin) = child.stdin.take() else {
        return false;
    };
    if stdin.write_all(value.as_bytes()).is_err() {
        return false;
    }
    drop(stdin);
    child.wait().map(|s| s.success()).unwrap_or(false)
}

/// Looks up a secret by its attributes. `None` if absent, empty or `secret-tool`
/// is unavailable.
fn lookup(attrs: &[(&str, &str)]) -> Option<String> {
    let mut cmd = Command::new("secret-tool");
    cmd.arg("lookup");
    for (k, v) in attrs {
        cmd.args([*k, *v]);
    }
    let out = cmd.stderr(Stdio::null()).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let value = String::from_utf8(out.stdout).ok()?;
    Some(value.trim_end_matches(['\r', '\n']).to_string()).filter(|s| !s.is_empty())
}

/// Removes the secret(s) matching the attributes.
fn clear(attrs: &[(&str, &str)]) {
    let mut cmd = Command::new("secret-tool");
    cmd.arg("clear");
    for (k, v) in attrs {
        cmd.args([*k, *v]);
    }
    let _ = cmd.stdout(Stdio::null()).stderr(Stdio::null()).status();
}

// --- Per-source credentials (Nextcloud/WebDAV) -----------------------------

fn source_id_text(source_id: i64) -> String {
    source_id.to_string()
}

fn password_attrs(id: &str) -> [(&str, &str); 3] {
    [
        ("application", "emilia"),
        ("kind", "webdav-password"),
        ("source-id", id),
    ]
}

fn username_attrs(id: &str) -> [(&str, &str); 3] {
    [
        ("application", "emilia"),
        ("kind", "webdav-username"),
        ("source-id", id),
    ]
}

/// DB reference stored in place of a per-source secret (`secret-tool:<id>`).
pub fn source_password_ref(source_id: i64) -> String {
    format!("{SECRET_PREFIX}{source_id}")
}

/// DB reference stored in place of a per-source username (`secret-tool:<id>`).
pub fn source_username_ref(source_id: i64) -> String {
    format!("{SECRET_PREFIX}{source_id}")
}

pub fn store_source_password(source_id: i64, label: &str, password: &str) -> bool {
    store(&password_attrs(&source_id_text(source_id)), label, password)
}

pub fn lookup_source_password(source_id: i64) -> Option<String> {
    lookup(&password_attrs(&source_id_text(source_id)))
}

pub fn store_source_username(source_id: i64, label: &str, username: &str) -> bool {
    store(&username_attrs(&source_id_text(source_id)), label, username)
}

pub fn lookup_source_username(source_id: i64) -> Option<String> {
    lookup(&username_attrs(&source_id_text(source_id)))
}

/// Resolves a stored per-source password: a `secret-tool:` reference is looked
/// up in the keyring; any other value is returned verbatim (legacy plaintext).
pub fn resolve_source_password(source_id: i64, stored: &str) -> Option<String> {
    if !stored.starts_with(SECRET_PREFIX) {
        return Some(stored.to_string());
    }
    lookup_source_password(source_id)
}

/// Resolves a stored per-source username (see [`resolve_source_password`]).
pub fn resolve_source_username(source_id: i64, stored: &str) -> Option<String> {
    if !stored.starts_with(SECRET_PREFIX) {
        return Some(stored.to_string());
    }
    lookup_source_username(source_id)
}

/// Removes both keyring items (password + username) of a source.
pub fn clear_source(source_id: i64) {
    let id = source_id_text(source_id);
    clear(&password_attrs(&id));
    clear(&username_attrs(&id));
}

/// Backwards-compatible alias used by `delete_source`; clears all source secrets.
pub fn clear_source_password(source_id: i64) {
    clear_source(source_id);
}

// --- Named secrets (API keys/tokens kept as settings) ----------------------

fn named_attrs(name: &str) -> [(&str, &str); 3] {
    [
        ("application", "emilia"),
        ("kind", "app-secret"),
        ("name", name),
    ]
}

pub fn store_named(name: &str, label: &str, value: &str) -> bool {
    store(&named_attrs(name), label, value)
}

pub fn lookup_named(name: &str) -> Option<String> {
    lookup(&named_attrs(name))
}

pub fn clear_named(name: &str) {
    clear(&named_attrs(name));
}
