//! Source creation helpers shared by UI components: insert the row, then move
//! the credentials into the Secret Service (only `secret-tool:` references
//! stay in the database) — identical for Nextcloud, SMB and Google Drive.

use anyhow::Result;

use crate::core::db::Library;
use crate::core::gdrive::GdCreds;
use crate::core::remote::{self, KIND_GDRIVE, KIND_SMB, KIND_WEBDAV};
use crate::core::smb::SmbCreds;
use crate::core::webdav::Creds;
use crate::model::Source;

/// Keyring item label of a source's credentials.
pub fn secret_label(kind: &str, name: &str) -> String {
    format!("Emilia {} {name}", remote::kind_name(kind))
}

fn webdav_display_name(base_url: &str) -> String {
    base_url
        .split_once("://")
        .map(|(_, rest)| rest)
        .unwrap_or(base_url)
        .split('/')
        .next()
        .unwrap_or("Nextcloud")
        .to_string()
}

/// Inserts a remote source whose `username`/`password` fields hold the
/// plaintext credentials, then moves both into the Secret Service when
/// available. Returns the source with its DB id set and the fields replaced
/// by references (or left as they were when no keyring is available).
fn add_remote_source(lib: &Library, mut src: Source) -> Result<Source> {
    let password = src.password.clone();
    let username = src.username.clone();
    let id = lib.add_source(&src)?;
    src.id = id;

    let label = secret_label(&src.kind, &src.name);
    if let Some(password) = password.as_deref().filter(|p| !p.is_empty()) {
        if crate::core::secrets::store_source_password(id, &label, password) {
            let password_ref = crate::core::secrets::source_password_ref(id);
            match lib.set_source_password(id, Some(&password_ref)) {
                Ok(()) => src.password = Some(password_ref),
                Err(e) => {
                    tracing::warn!("Secret stored, but source password reference failed: {e}")
                }
            }
        }
    }
    if let Some(username) = username.as_deref().filter(|u| !u.is_empty()) {
        if crate::core::secrets::store_source_username(id, &label, username) {
            let username_ref = crate::core::secrets::source_username_ref(id);
            match lib.set_source_username(id, Some(&username_ref)) {
                Ok(()) => src.username = Some(username_ref),
                Err(e) => {
                    tracing::warn!("Secret stored, but source username reference failed: {e}")
                }
            }
        }
    }
    Ok(src)
}

/// Adds a WebDAV/Nextcloud source. `name` is the tab label; `None` derives it
/// from the host. An explicit name is used when an existing server connection
/// is reused for a second music folder, so the new tab is distinguishable from
/// the first (which carries the bare host name).
pub fn add_webdav_source_named(
    lib: &Library,
    creds: Creds,
    name: Option<String>,
) -> Result<Source> {
    let src = Source {
        id: 0,
        kind: KIND_WEBDAV.into(),
        name: name.unwrap_or_else(|| webdav_display_name(&creds.base_url)),
        position: 0,
        path: None,
        base_url: Some(creds.base_url),
        username: Some(creds.user),
        password: Some(creds.pass),
        music_path: Some(creds.music_path),
    };
    add_remote_source(lib, src)
}

/// Adds an SMB share source. `name` defaults to the share name.
pub fn add_smb_source(lib: &Library, creds: &SmbCreds, name: Option<String>) -> Result<Source> {
    let src = Source {
        id: 0,
        kind: KIND_SMB.into(),
        name: name.unwrap_or_else(|| creds.share.clone()),
        position: 0,
        path: None,
        base_url: Some(creds.location()),
        username: Some(creds.user.clone()),
        password: Some(creds.pass.clone()),
        music_path: Some(creds.music_path.clone()),
    };
    add_remote_source(lib, src)
}

/// Adds a Google Drive source. `name` defaults to the account e-mail.
pub fn add_gdrive_source(lib: &Library, creds: &GdCreds, name: Option<String>) -> Result<Source> {
    let src = Source {
        id: 0,
        kind: KIND_GDRIVE.into(),
        name: name.unwrap_or_else(|| {
            if creds.account.is_empty() {
                "Google Drive".to_string()
            } else {
                creds.account.clone()
            }
        }),
        position: 0,
        path: None,
        base_url: Some(KIND_GDRIVE.into()),
        username: Some(creds.account.clone()),
        password: Some(creds.refresh_token.clone()),
        music_path: Some(creds.music_path.clone()),
    };
    add_remote_source(lib, src)
}

/// Tab label for a second folder on an already-connected server/account: the
/// folder's last segment, or `fallback` for the root.
pub fn folder_tab_name(music_path: &str, fallback: &str) -> String {
    music_path
        .rsplit('/')
        .find(|s| !s.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| fallback.to_string())
}
