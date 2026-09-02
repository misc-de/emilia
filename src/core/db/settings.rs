//! Settings and secret settings for [`Library`] (split out of db.rs).

use anyhow::Result;
use rusqlite::OptionalExtension;

use super::Library;

impl Library {
    /// Reads a setting value (e.g. the music folder).
    pub fn get_setting(&self, key: &str) -> Result<Option<String>> {
        let value = self
            .conn
            .query_row("SELECT value FROM setting WHERE key = ?1", [key], |r| {
                r.get::<_, String>(0)
            })
            .optional()?;
        Ok(value)
    }

    /// Stores a setting value.
    pub fn set_setting(&self, key: &str, value: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO setting (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            rusqlite::params![key, value],
        )?;
        Ok(())
    }

    /// Reads a security-sensitive setting (API key/token). A `secret-tool:`
    /// sentinel resolves to the Secret Service; a legacy plaintext value is
    /// returned verbatim.
    pub fn get_secret_setting(&self, key: &str) -> Result<Option<String>> {
        match self.get_setting(key)? {
            Some(v) if v == crate::core::secrets::SECRET_PREFIX => {
                Ok(crate::core::secrets::lookup_named(key))
            }
            Some(v) if v.is_empty() => Ok(None),
            other => Ok(other),
        }
    }

    /// Stores a security-sensitive setting in the Secret Service when available
    /// (only a `secret-tool:` sentinel is kept in the DB); otherwise falls back
    /// to a plaintext setting. An empty value clears both.
    pub fn set_secret_setting(&self, key: &str, value: &str) -> Result<()> {
        let value = value.trim();
        if value.is_empty() {
            crate::core::secrets::clear_named(key);
            self.conn
                .execute("DELETE FROM setting WHERE key = ?1", [key])?;
            return Ok(());
        }
        let label = format!("Emilia {key}");
        if crate::core::secrets::store_named(key, &label, value) {
            self.set_setting(key, crate::core::secrets::SECRET_PREFIX)
        } else {
            self.set_setting(key, value)
        }
    }

    /// Best-effort migration of existing **plaintext** secrets into the Secret
    /// Service (run once at startup). Each value is only replaced by its
    /// `secret-tool:` reference after a verifying lookup confirms the keyring
    /// copy — so a missing/unavailable keyring never loses a credential, and the
    /// app keeps working with the plaintext fallback. Once everything is
    /// referenced this is a couple of cheap DB reads.
    pub fn migrate_secrets(&self) {
        use crate::core::secrets;
        // API keys/tokens stored as settings.
        for key in ["acoustid_key", "fanart_key"] {
            if let Ok(Some(v)) = self.get_setting(key) {
                if !v.is_empty()
                    && v != secrets::SECRET_PREFIX
                    && secrets::store_named(key, &format!("Emilia {key}"), &v)
                    && secrets::lookup_named(key).as_deref() == Some(v.as_str())
                {
                    let _ = self.set_setting(key, secrets::SECRET_PREFIX);
                }
            }
        }
        // Nextcloud/WebDAV credentials (username + app password).
        for s in self.list_sources().unwrap_or_default() {
            if s.kind != "webdav" {
                continue;
            }
            let label = format!("Emilia Nextcloud {}", s.name);
            if let Some(pw) = s.password.as_deref() {
                if !pw.is_empty()
                    && !pw.starts_with(secrets::SECRET_PREFIX)
                    && secrets::store_source_password(s.id, &label, pw)
                    && secrets::lookup_source_password(s.id).as_deref() == Some(pw)
                {
                    let _ =
                        self.set_source_password(s.id, Some(&secrets::source_password_ref(s.id)));
                }
            }
            if let Some(user) = s.username.as_deref() {
                if !user.is_empty()
                    && !user.starts_with(secrets::SECRET_PREFIX)
                    && secrets::store_source_username(s.id, &label, user)
                    && secrets::lookup_source_username(s.id).as_deref() == Some(user)
                {
                    let _ =
                        self.set_source_username(s.id, Some(&secrets::source_username_ref(s.id)));
                }
            }
        }
    }
}
