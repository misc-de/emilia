//! Equalizer queries for [`Library`] (split out of db.rs).

use anyhow::Result;
use rusqlite::OptionalExtension;

use super::Library;

impl Library {
    // ---- Equalizer (10 bands, with inheritance) ----

    /// Stores the 10 band gains (dB) for an output + a level.
    pub fn set_eq(&self, output: &str, scope: &str, key: &str, bands: &[f64; 10]) -> Result<()> {
        let json = serde_json::to_string(bands)?;
        self.conn.execute(
            "INSERT INTO eq_setting (output, scope, key, bands) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(output, scope, key) DO UPDATE SET bands = excluded.bands",
            rusqlite::params![output, scope, key, json],
        )?;
        Ok(())
    }

    /// Reads the bands of a single output/level combination (without inheritance).
    pub fn get_eq(&self, output: &str, scope: &str, key: &str) -> Result<Option<[f64; 10]>> {
        let json: Option<String> = self
            .conn
            .query_row(
                "SELECT bands FROM eq_setting WHERE output = ?1 AND scope = ?2 AND key = ?3",
                rusqlite::params![output, scope, key],
                |r| r.get::<_, String>(0),
            )
            .optional()?;
        Ok(json.and_then(|j| serde_json::from_str::<[f64; 10]>(&j).ok()))
    }

    /// Whether a single output/level EQ setting is active. Missing settings are
    /// treated as active so a newly edited EQ takes effect immediately.
    pub fn eq_enabled(&self, output: &str, scope: &str, key: &str) -> Result<bool> {
        let enabled = self
            .conn
            .query_row(
                "SELECT enabled FROM eq_setting WHERE output = ?1 AND scope = ?2 AND key = ?3",
                rusqlite::params![output, scope, key],
                |r| r.get::<_, i64>(0),
            )
            .optional()?;
        Ok(enabled != Some(0))
    }

    /// Enables/disables one EQ setting without changing its saved band values.
    /// If the setting does not exist yet, create a neutral one so disabled means
    /// "flat override" rather than "inherit from a broader level".
    pub fn set_eq_enabled(
        &self,
        output: &str,
        scope: &str,
        key: &str,
        enabled: bool,
    ) -> Result<()> {
        let neutral = serde_json::to_string(&[0.0; 10])?;
        self.conn.execute(
            "INSERT INTO eq_setting (output, scope, key, bands, enabled)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(output, scope, key) DO UPDATE SET enabled = excluded.enabled",
            rusqlite::params![output, scope, key, neutral, if enabled { 1 } else { 0 }],
        )?;
        Ok(())
    }

    /// All stored equalizer settings (for the device synchronization).
    pub fn all_eq_settings(&self) -> Result<Vec<(String, String, String, [f64; 10])>> {
        let mut stmt = self
            .conn
            .prepare("SELECT output, scope, key, bands FROM eq_setting")?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows.filter_map(|r| r.ok()) {
            let (output, scope, key, json) = row;
            if let Ok(bands) = serde_json::from_str::<[f64; 10]>(&json) {
                out.push((output, scope, key, bands));
            }
        }
        Ok(out)
    }

    /// Removes the setting (falls back to the inherited/default output).
    pub fn clear_eq(&self, output: &str, scope: &str, key: &str) -> Result<()> {
        self.conn.execute(
            "DELETE FROM eq_setting WHERE output = ?1 AND scope = ?2 AND key = ?3",
            rusqlite::params![output, scope, key],
        )?;
        Ok(())
    }

    /// Effective equalizer for track + output. Order: first the concrete
    /// output (track→album→artist→global), then the default output ('')
    /// as the basis. `None` if nothing is set anywhere (→ neutral).
    pub fn resolve_eq(
        &self,
        output: &str,
        artist: Option<&str>,
        album: Option<&str>,
        path: &str,
    ) -> Option<[f64; 10]> {
        let album_key = album.map(|al| crate::core::category::album_key(artist.unwrap_or(""), al));

        // Concrete output first, then the default output as the basis.
        let mut outputs: Vec<&str> = Vec::new();
        if !output.is_empty() {
            outputs.push(output);
        }
        outputs.push("");

        for out in outputs {
            if let Some(b) = self.resolve_eq_setting(out, "track", path) {
                return Some(b);
            }
            if let Some(key) = &album_key {
                if let Some(b) = self.resolve_eq_setting(out, "album", key) {
                    return Some(b);
                }
            }
            if let Some(artist) = artist {
                if let Some(b) = self.resolve_eq_setting(out, "artist", artist) {
                    return Some(b);
                }
            }
            if let Some(b) = self.resolve_eq_setting(out, "global", "") {
                return Some(b);
            }
        }
        None
    }

    /// Effective equalizer for an internet-radio station + output. A station
    /// carries no track metadata, so the inheritance is just station → global,
    /// per output: first the concrete output (station→global), then the default
    /// output ('') as the basis. `None` if nothing is set (→ neutral).
    pub fn resolve_eq_stream(&self, output: &str, station: &str) -> Option<[f64; 10]> {
        let mut outputs: Vec<&str> = Vec::new();
        if !output.is_empty() {
            outputs.push(output);
        }
        outputs.push("");

        for out in outputs {
            if let Some(b) = self.resolve_eq_setting(out, "stream", station) {
                return Some(b);
            }
            if let Some(b) = self.resolve_eq_setting(out, "global", "") {
                return Some(b);
            }
        }
        None
    }

    fn resolve_eq_setting(&self, output: &str, scope: &str, key: &str) -> Option<[f64; 10]> {
        let bands = self.get_eq(output, scope, key).ok().flatten()?;
        if self.eq_enabled(output, scope, key).unwrap_or(true) {
            Some(bands)
        } else {
            Some([0.0; 10])
        }
    }
}
