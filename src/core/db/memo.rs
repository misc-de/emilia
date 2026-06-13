//! Voice-memo queries for [`Library`] (split out of db.rs).
//!
//! Two tables: `memo_category` (user-created, freely managed) and `memo` (the
//! recordings). A memo references at most one category; `category_id = NULL`
//! means "unassigned" and is shown as "General". Categories are assigned
//! **after** recording and can be reassigned at any time.
//!
//! Foreign keys are not enforced in this database (no `PRAGMA foreign_keys`),
//! so deleting a category resets its memos to NULL explicitly here — the same
//! pattern `delete_source` uses to clean up its dependent rows.

// The whole memo data layer is wired up before its UI exists; silence the
// not-yet-called warnings until the Memo page lands.
#![allow(dead_code)]

use anyhow::Result;
use rusqlite::OptionalExtension;

use super::Library;
use crate::model::{MemoCategory, MemoItem};

impl Library {
    // ---- Categories ----

    /// All categories, in manual order (then by id for a stable tiebreak).
    pub fn memo_categories(&self) -> Result<Vec<MemoCategory>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, name, position, created_at FROM memo_category ORDER BY position, id",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(MemoCategory {
                id: r.get(0)?,
                name: r.get(1)?,
                position: r.get(2)?,
                created_at: r.get(3)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Adds a category at the end of the list and returns its new id.
    pub fn add_memo_category(&self, name: &str) -> Result<i64> {
        let position: i64 = self
            .conn
            .query_row(
                "SELECT COALESCE(MAX(position), -1) + 1 FROM memo_category",
                [],
                |r| r.get(0),
            )
            .unwrap_or(0);
        self.conn.execute(
            "INSERT INTO memo_category (name, position, created_at)
             VALUES (?1, ?2, strftime('%s','now'))",
            rusqlite::params![name, position],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Renames a category.
    pub fn rename_memo_category(&self, id: i64, name: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE memo_category SET name = ?2 WHERE id = ?1",
            rusqlite::params![id, name],
        )?;
        Ok(())
    }

    /// Deletes a category. Its memos are **not** deleted: they drop back to
    /// "unassigned" (General). Done in one transaction since foreign keys are
    /// not enforced and would otherwise leave dangling `category_id`s.
    pub fn delete_memo_category(&self, id: i64) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        tx.execute(
            "UPDATE memo SET category_id = NULL WHERE category_id = ?1",
            [id],
        )?;
        tx.execute("DELETE FROM memo_category WHERE id = ?1", [id])?;
        tx.commit()?;
        Ok(())
    }

    /// Deletes a category **together with** its memos in one transaction and
    /// returns the deleted memos' file paths, so the caller can remove the files.
    /// The counterpart to [`Self::delete_memo_category`], which instead keeps the
    /// memos (resets them to "General").
    pub fn delete_memo_category_with_memos(&self, id: i64) -> Result<Vec<String>> {
        let tx = self.conn.unchecked_transaction()?;
        let paths: Vec<String> = {
            let mut stmt = tx.prepare("SELECT path FROM memo WHERE category_id = ?1")?;
            let rows = stmt.query_map([id], |r| r.get::<_, String>(0))?;
            rows.collect::<rusqlite::Result<Vec<_>>>()?
        };
        tx.execute("DELETE FROM memo WHERE category_id = ?1", [id])?;
        tx.execute("DELETE FROM memo_category WHERE id = ?1", [id])?;
        tx.commit()?;
        Ok(paths)
    }

    /// Seeds the starter categories **once**, the first time the app runs. The
    /// names are passed in already localized (the caller wraps them in
    /// `gettext`), so the data layer stays free of i18n timing concerns. A guard
    /// flag in `setting` makes this idempotent and, crucially, stops deleted
    /// defaults from reappearing on the next start.
    pub fn seed_memo_categories(&self, names: &[&str]) -> Result<()> {
        if self.get_setting("memo_categories_seeded")?.is_some() {
            return Ok(());
        }
        let tx = self.conn.unchecked_transaction()?;
        for (i, name) in names.iter().enumerate() {
            tx.execute(
                "INSERT INTO memo_category (name, position, created_at)
                 VALUES (?1, ?2, strftime('%s','now'))",
                rusqlite::params![name, i as i64],
            )?;
        }
        tx.commit()?;
        self.set_setting("memo_categories_seeded", "1")?;
        Ok(())
    }

    // ---- Memos ----

    /// Stores a memo and returns its id. `category_id = None` leaves it
    /// unassigned (the category can be set later via [`Self::set_memo_category`]).
    pub fn add_memo(
        &self,
        path: &str,
        title: &str,
        category_id: Option<i64>,
        duration_ms: i64,
    ) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO memo (path, title, category_id, recorded_at, duration_ms)
             VALUES (?1, ?2, ?3, strftime('%s','now'), ?4)",
            rusqlite::params![path, title, category_id, duration_ms],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// All memos, newest first (the default "Recent" view).
    pub fn memos(&self) -> Result<Vec<MemoItem>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, path, title, category_id, recorded_at, duration_ms
             FROM memo ORDER BY recorded_at DESC, id DESC",
        )?;
        let rows = stmt.query_map([], memo_from_row)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Memos of one category, newest first. `None` selects the unassigned
    /// ("General") memos — `IS ?` matches NULL too.
    pub fn memos_in_category(&self, category_id: Option<i64>) -> Result<Vec<MemoItem>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, path, title, category_id, recorded_at, duration_ms
             FROM memo WHERE category_id IS ?1 ORDER BY recorded_at DESC, id DESC",
        )?;
        let rows = stmt.query_map([category_id], memo_from_row)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Assigns (or, with `None`, clears) a memo's category. This is the
    /// "assign afterwards" / reassign operation.
    pub fn set_memo_category(&self, memo_id: i64, category_id: Option<i64>) -> Result<()> {
        self.conn.execute(
            "UPDATE memo SET category_id = ?2 WHERE id = ?1",
            rusqlite::params![memo_id, category_id],
        )?;
        Ok(())
    }

    /// Renames a memo (its display title).
    pub fn rename_memo(&self, id: i64, title: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE memo SET title = ?2 WHERE id = ?1",
            rusqlite::params![id, title],
        )?;
        Ok(())
    }

    /// Backfills the cached playback length (probed lazily for older rows that
    /// were stored without one), mirroring `set_recording_duration`.
    pub fn set_memo_duration(&self, id: i64, duration_ms: i64) -> Result<()> {
        self.conn.execute(
            "UPDATE memo SET duration_ms = ?2 WHERE id = ?1",
            rusqlite::params![id, duration_ms],
        )?;
        Ok(())
    }

    /// Updates a memo's file path and duration after the waveform editor
    /// re-encoded it (the cut changes the length). Mirrors
    /// `update_recording_file`. For memos the extension stays `.ogg`, so the path
    /// is usually unchanged — but the duration always is.
    pub fn update_memo_file(&self, id: i64, path: &str, duration_ms: i64) -> Result<()> {
        self.conn.execute(
            "UPDATE memo SET path = ?2, duration_ms = ?3 WHERE id = ?1",
            rusqlite::params![id, path, duration_ms],
        )?;
        Ok(())
    }

    /// Removes a memo from management and returns its file path (so the caller
    /// can delete the file), mirroring `delete_recording`.
    pub fn delete_memo(&self, id: i64) -> Result<Option<String>> {
        let path: Option<String> = self
            .conn
            .query_row("SELECT path FROM memo WHERE id = ?1", [id], |r| r.get(0))
            .optional()?;
        self.conn.execute("DELETE FROM memo WHERE id = ?1", [id])?;
        Ok(path)
    }
}

/// Maps a memo row (in the column order used by the queries above).
fn memo_from_row(r: &rusqlite::Row) -> rusqlite::Result<MemoItem> {
    Ok(MemoItem {
        id: r.get(0)?,
        path: r.get(1)?,
        title: r.get(2)?,
        category_id: r.get(3)?,
        recorded_at: r.get(4)?,
        duration_ms: r.get(5)?,
    })
}

#[cfg(test)]
mod tests {
    use crate::core::db::Library;

    #[test]
    fn category_crud_and_delete_resets_memos() {
        let lib = Library::open_in_memory().unwrap();
        let work = lib.add_memo_category("Work").unwrap();
        let idea = lib.add_memo_category("Idea").unwrap();
        assert_eq!(lib.memo_categories().unwrap().len(), 2);

        let a = lib
            .add_memo("/tmp/a.ogg", "Memo A", Some(work), 1000)
            .unwrap();
        lib.add_memo("/tmp/b.ogg", "Memo B", None, 0).unwrap();

        // Assign afterwards / reassign.
        lib.set_memo_category(a, Some(idea)).unwrap();
        assert_eq!(lib.memos_in_category(Some(idea)).unwrap().len(), 1);
        assert_eq!(lib.memos_in_category(None).unwrap().len(), 1); // Memo B

        // Deleting a category drops its memos back to unassigned, never deletes.
        lib.delete_memo_category(idea).unwrap();
        assert_eq!(lib.memos().unwrap().len(), 2);
        assert_eq!(lib.memos_in_category(None).unwrap().len(), 2);
        assert!(lib.memo_categories().unwrap().iter().all(|c| c.id != idea));
    }

    #[test]
    fn delete_category_with_memos_removes_both_and_returns_paths() {
        let lib = Library::open_in_memory().unwrap();
        let work = lib.add_memo_category("Work").unwrap();
        lib.add_memo("/tmp/a.ogg", "Memo A", Some(work), 0).unwrap();
        lib.add_memo("/tmp/b.ogg", "Memo B", Some(work), 0).unwrap();
        lib.add_memo("/tmp/c.ogg", "Memo C", None, 0).unwrap();

        let mut paths = lib.delete_memo_category_with_memos(work).unwrap();
        paths.sort();
        assert_eq!(paths, vec!["/tmp/a.ogg", "/tmp/b.ogg"]);
        // The category and its two memos are gone; the unassigned one remains.
        assert!(lib.memo_categories().unwrap().is_empty());
        assert_eq!(lib.memos().unwrap().len(), 1);
        assert_eq!(lib.memos_in_category(None).unwrap().len(), 1);
    }

    #[test]
    fn update_memo_file_changes_path_and_duration() {
        let lib = Library::open_in_memory().unwrap();
        let id = lib.add_memo("/tmp/a.ogg", "A", None, 0).unwrap();
        lib.update_memo_file(id, "/tmp/a-cut.ogg", 4200).unwrap();
        let m = lib.memos().unwrap();
        assert_eq!(m[0].path, "/tmp/a-cut.ogg");
        assert_eq!(m[0].duration_ms, 4200);
    }

    #[test]
    fn memos_are_newest_first() {
        let lib = Library::open_in_memory().unwrap();
        // recorded_at resolves to the same second, so id is the tiebreak (DESC).
        let first = lib.add_memo("/tmp/1.ogg", "First", None, 0).unwrap();
        let second = lib.add_memo("/tmp/2.ogg", "Second", None, 0).unwrap();
        let ids: Vec<i64> = lib.memos().unwrap().iter().map(|m| m.id).collect();
        assert_eq!(ids, vec![second, first]);
    }

    #[test]
    fn seed_runs_once() {
        let lib = Library::open_in_memory().unwrap();
        lib.seed_memo_categories(&["Idea", "Task", "Note", "Music"])
            .unwrap();
        // Idempotent: a second call (and even different names) does nothing.
        lib.seed_memo_categories(&["Other"]).unwrap();
        let names: Vec<String> = lib
            .memo_categories()
            .unwrap()
            .into_iter()
            .map(|c| c.name)
            .collect();
        assert_eq!(names, vec!["Idea", "Task", "Note", "Music"]);
    }
}
