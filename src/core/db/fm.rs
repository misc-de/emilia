//! FM radio favorites for [`Library`]: saved frequencies with a name, in the
//! user's order (see [`crate::core::fm`]).

use anyhow::Result;
use rusqlite::OptionalExtension;

use super::Library;
use crate::model::FmStation;

/// Setting key: the frequency last tuned to (10 kHz units).
const LAST_FREQ: &str = "fm_last_freq";

impl Library {
    /// The saved stations in the user's order (new ones at the end).
    pub fn fm_stations(&self) -> Result<Vec<FmStation>> {
        let mut stmt = self
            .conn
            .prepare("SELECT id, freq, name FROM fm_station ORDER BY position, freq")?;
        let rows = stmt.query_map([], |r| {
            Ok(FmStation {
                id: r.get(0)?,
                freq: r.get(1)?,
                name: r.get(2)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// The saved station on a frequency, if any.
    pub fn fm_station_at(&self, freq: u16) -> Result<Option<FmStation>> {
        Ok(self
            .conn
            .query_row(
                "SELECT id, freq, name FROM fm_station WHERE freq = ?1",
                [freq],
                |r| {
                    Ok(FmStation {
                        id: r.get(0)?,
                        freq: r.get(1)?,
                        name: r.get(2)?,
                    })
                },
            )
            .optional()?)
    }

    /// Saves a frequency under `name` (or renames it if it is saved already)
    /// and returns its id.
    pub fn save_fm_station(&self, freq: u16, name: &str) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO fm_station (freq, name, position, added_at)
             VALUES (?1, ?2, (SELECT COALESCE(MAX(position), -1) + 1 FROM fm_station),
                     strftime('%s','now'))
             ON CONFLICT(freq) DO UPDATE SET name = excluded.name",
            rusqlite::params![freq, name],
        )?;
        Ok(self
            .conn
            .query_row("SELECT id FROM fm_station WHERE freq = ?1", [freq], |r| {
                r.get(0)
            })?)
    }

    pub fn rename_fm_station(&self, id: i64, name: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE fm_station SET name = ?2 WHERE id = ?1",
            rusqlite::params![id, name],
        )?;
        Ok(())
    }

    pub fn delete_fm_station(&self, id: i64) -> Result<()> {
        self.conn
            .execute("DELETE FROM fm_station WHERE id = ?1", [id])?;
        Ok(())
    }

    /// Moves a station one place up (`-1`) or down (`+1`) in the list.
    pub fn move_fm_station(&self, id: i64, dir: i32) -> Result<()> {
        let mut ids: Vec<i64> = self.fm_stations()?.into_iter().map(|s| s.id).collect();
        let Some(pos) = ids.iter().position(|&i| i == id) else {
            return Ok(());
        };
        let to = pos as i64 + i64::from(dir);
        if to < 0 || to >= ids.len() as i64 {
            return Ok(());
        }
        ids.swap(pos, to as usize);
        let tx = self.conn.unchecked_transaction()?;
        for (i, id) in ids.iter().enumerate() {
            tx.execute(
                "UPDATE fm_station SET position = ?2 WHERE id = ?1",
                rusqlite::params![id, i as i64],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// The frequency last tuned to (or the default of a first start).
    pub fn fm_last_freq(&self) -> u16 {
        self.get_setting(LAST_FREQ)
            .ok()
            .flatten()
            .and_then(|v| v.parse::<u16>().ok())
            .map(crate::core::fm::snap)
            .unwrap_or(crate::core::fm::FREQ_DEFAULT)
    }

    pub fn set_fm_last_freq(&self, freq: u16) {
        let _ = self.set_setting(LAST_FREQ, &freq.to_string());
    }
}
