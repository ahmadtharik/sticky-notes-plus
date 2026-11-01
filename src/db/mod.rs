pub mod attachments;
pub mod export;
pub mod migrations;
pub mod notes;
pub mod reminders;
pub mod restore;

use std::path::Path;

/// Locks the database, carrying on after a panic on another thread: SQLite has
/// already rolled back whatever that thread had in progress.
pub fn lock(db: &std::sync::Mutex<Db>) -> std::sync::MutexGuard<'_, Db> {
    db.lock().unwrap_or_else(|poisoned| {
        db.clear_poison();
        poisoned.into_inner()
    })
}

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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    #[test]
    fn a_panic_while_locked_leaves_the_database_usable_by_every_caller() {
        let directory = std::env::temp_dir().join(format!("sticky-lock-{}", uuid::Uuid::new_v4()));
        let db = Arc::new(Mutex::new(Db::open_in_dir(&directory).unwrap()));
        let holder = db.clone();
        let _ = std::thread::spawn(move || {
            let _guard = holder.lock().unwrap();
            panic!("worker failed while holding the lock");
        })
        .join();
        assert!(db.is_poisoned());

        drop(lock(&db));

        assert!(db.lock().is_ok());
        drop(db);
        std::fs::remove_dir_all(directory).unwrap();
    }
}
