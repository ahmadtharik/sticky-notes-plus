//! Daily automatic backups. Each is an ordinary export, so "Import from
//! export…" brings notes back from it. The newest few are kept.
use super::{export, lock, Db};
use anyhow::{Context, Result};
use chrono::NaiveDate;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

pub const KEPT: usize = 7;
const PREFIX: &str = "Sticky Notes backup ";

/// The `backups` folder next to the database.
pub fn folder(db: &Mutex<Db>) -> Result<PathBuf> {
    let guard = lock(db);
    let database = guard
        .conn
        .path()
        .filter(|path| !path.is_empty())
        .context("Backups need a saved database")?;
    Ok(Path::new(database)
        .parent()
        .context("Database directory unavailable")?
        .join("backups"))
}

fn file_for(folder: &Path, day: NaiveDate) -> PathBuf {
    folder.join(format!("{PREFIX}{}.json", day.format("%Y-%m-%d")))
}

/// Writes today's backup unless it already exists. Returns whether one was
/// written. With no notes nothing is written, so an emptied database can't
/// push the good backups out.
pub fn run_if_due(db: &Mutex<Db>, today: NaiveDate, exported_at: &str) -> Result<bool> {
    let folder = folder(db)?;
    let path = file_for(&folder, today);
    if path.exists() {
        return Ok(false);
    }
    let has_notes: bool =
        lock(db)
            .conn
            .query_row("SELECT EXISTS(SELECT 1 FROM notes)", [], |row| row.get(0))?;
    if !has_notes {
        return Ok(false);
    }
    std::fs::create_dir_all(&folder)?;
    if let Err(error) = export::write(db, &path, exported_at) {
        // A half-written file would stop today's retry.
        let _ = std::fs::remove_file(&path);
        return Err(error);
    }
    prune(&folder, KEPT);
    Ok(true)
}

/// Deletes all but the newest `keep` backups and their image folders.
fn prune(folder: &Path, keep: usize) {
    let Ok(entries) = std::fs::read_dir(folder) else {
        return;
    };
    // Names carry the date, so sorting by name sorts by age.
    let mut stems: Vec<String> = entries
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            let stem = name.strip_suffix(".json")?;
            stem.starts_with(PREFIX).then(|| stem.to_owned())
        })
        .collect();
    stems.sort_unstable_by(|a, b| b.cmp(a));
    for stem in stems.into_iter().skip(keep) {
        let _ = std::fs::remove_file(folder.join(format!("{stem}.json")));
        let _ = std::fs::remove_dir_all(folder.join(format!("{stem}-images")));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::notes;

    #[test]
    fn writes_once_a_day_keeps_the_newest_and_skips_an_empty_database() {
        let dir = std::env::temp_dir().join(format!("slint-backup-test-{}", uuid::Uuid::new_v4()));
        let db = Mutex::new(Db::open_in_dir(&dir).unwrap());
        let day = |n| NaiveDate::from_ymd_opt(2026, 10, n).unwrap();
        assert!(!run_if_due(&db, day(1), "now").unwrap());
        notes::create(&lock(&db).conn, "n", &serde_json::json!({"blocks": [{"type": "text", "runs": [{"t": "hello"}]}], "plain": "hello"}).to_string(), "#FFF9B1").unwrap();
        assert!(run_if_due(&db, day(1), "now").unwrap());
        assert!(!run_if_due(&db, day(1), "now").unwrap());
        let folder = folder(&db).unwrap();
        std::fs::create_dir_all(folder.join(format!("{PREFIX}2026-10-02-images"))).unwrap();
        for n in 2..=9 {
            assert!(run_if_due(&db, day(n), "now").unwrap());
        }
        let mut names: Vec<_> = std::fs::read_dir(&folder)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        assert_eq!(names.len(), KEPT);
        assert_eq!(names[0], format!("{PREFIX}2026-10-03.json"));
        assert_eq!(names[KEPT - 1], format!("{PREFIX}2026-10-09.json"));
        let _ = std::fs::remove_dir_all(dir);
    }
}
