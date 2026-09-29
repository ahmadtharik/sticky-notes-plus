//! SQLite access layer: single file in the app-data dir, WAL mode,
//! migrations tracked in `meta.schema_version`.

pub mod migrations;
pub mod notes;

use std::path::Path;

pub struct Db {
    pub conn: rusqlite::Connection,
}

impl Db {
    /// Opens (creating if needed) `<dir>/sticky-notes.db`, enables WAL,
    /// runs pending migrations.
    pub fn open_in_dir(dir: &Path) -> anyhow::Result<Self> {
        let file = dir.join("sticky-notes.db");
        let conn = rusqlite::Connection::open(&file)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        let applied = migrations::apply(&conn)?;
        conn.execute(
            "INSERT INTO meta(key, value) VALUES ('schema_version', ?1)
       ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            [applied.to_string()],
        )?;
        Ok(Db { conn })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static COUNTER: AtomicUsize = AtomicUsize::new(0);

    /// Fresh temp dir, unique per test (tests run in parallel).
    fn tmp_dir() -> std::path::PathBuf {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let d = std::env::temp_dir().join(format!("sn-test-{}-{}", std::process::id(), n));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn test_db() -> Db {
        let dir = tmp_dir();
        Db::open_in_dir(&dir).unwrap()
    }

    #[test]
    fn open_creates_wal_and_migrates_to_current_version() {
        let dir = tmp_dir();
        let db = Db::open_in_dir(&dir).unwrap();
        let mode: String = db
            .conn
            .query_row("PRAGMA journal_mode", [], |r| r.get(0))
            .unwrap();
        assert_eq!(mode, "wal");
        let version: String = db
            .conn
            .query_row(
                "SELECT value FROM meta WHERE key='schema_version'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(version.parse::<i64>().unwrap(), migrations::CURRENT);
    }

    #[test]
    fn reopening_is_idempotent() {
        let dir = tmp_dir();
        Db::open_in_dir(&dir).unwrap();
        let db = Db::open_in_dir(&dir).unwrap(); // second open must not re-apply
        let version: String = db
            .conn
            .query_row(
                "SELECT value FROM meta WHERE key='schema_version'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(version.parse::<i64>().unwrap(), migrations::CURRENT);
    }

    #[test]
    fn notes_table_has_all_spec_columns() {
        let db = test_db();
        let cols: Vec<String> = db
            .conn
            .prepare("PRAGMA table_info(notes)")
            .unwrap()
            .query_map([], |r| r.get::<_, String>(1))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        for c in [
            "id",
            "title",
            "content",
            "color",
            "is_pinned",
            "is_archived",
            "deleted_at",
            "window_x",
            "window_y",
            "window_w",
            "window_h",
            "is_floating",
            "created_at",
            "modified_at",
        ] {
            assert!(cols.contains(&c.to_string()), "missing column {c}");
        }
    }
}
