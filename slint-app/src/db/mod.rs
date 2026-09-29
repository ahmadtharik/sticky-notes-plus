pub mod migrations;
pub mod notes;
pub mod reminders;

use std::path::Path;

pub struct Db {
    pub conn: rusqlite::Connection,
}

impl Db {
    pub fn open_in_dir(dir: &Path) -> anyhow::Result<Self> {
        std::fs::create_dir_all(dir)?;
        let conn = rusqlite::Connection::open(dir.join("sticky-notes.db"))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        migrations::apply(&conn)?;
        Ok(Self { conn })
    }
}
