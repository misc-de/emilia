//! Source creation helpers shared by UI components.

use anyhow::Result;

use crate::core::db::Library;
use crate::core::webdav::Creds;
use crate::model::Source;

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

/// Adds a WebDAV/Nextcloud source and stores the app password in the Secret
/// Service when possible. Returns the source with its DB id set.
pub fn add_webdav_source(lib: &Library, creds: Creds) -> Result<Source> {
    let password = creds.pass.clone();
    let username = creds.user.clone();
    let mut src = Source {
        id: 0,
        kind: "webdav".into(),
        name: webdav_display_name(&creds.base_url),
        position: 0,
        path: None,
        base_url: Some(creds.base_url),
        username: Some(creds.user),
        password: Some(creds.pass),
        music_path: Some(creds.music_path),
    };

    let id = lib.add_source(&src)?;
    src.id = id;

    // Move the app password and the username into the Secret Service when
    // available; only a `secret-tool:` reference stays in the database.
    let label = format!("Emilia Nextcloud {}", src.name);
    if crate::core::secrets::store_source_password(id, &label, &password) {
        let password_ref = crate::core::secrets::source_password_ref(id);
        match lib.set_source_password(id, Some(&password_ref)) {
            Ok(()) => src.password = Some(password_ref),
            Err(e) => tracing::warn!("Secret stored, but source password reference failed: {e}"),
        }
    }
    if crate::core::secrets::store_source_username(id, &label, &username) {
        let username_ref = crate::core::secrets::source_username_ref(id);
        match lib.set_source_username(id, Some(&username_ref)) {
            Ok(()) => src.username = Some(username_ref),
            Err(e) => tracing::warn!("Secret stored, but source username reference failed: {e}"),
        }
    }

    Ok(src)
}
