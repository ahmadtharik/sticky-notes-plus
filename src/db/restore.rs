//! Brings notes back from a file written by `export`. Existing notes are never
//! changed, so restoring only ever adds what is missing. The file is untrusted.
//! Slow work happens outside the database lock so the app stays responsive.
use super::{attachments, lock, notes, reminders, Db};
use anyhow::{ensure, Context, Result};
use rusqlite::{params, Connection};
use serde_json::{json, Value};
use std::collections::HashSet;
use std::path::{Component, Path, PathBuf};
use std::sync::Mutex;

const MAX_FILE_BYTES: u64 = 200 * 1024 * 1024;
const MAX_NOTE_BYTES: usize = 1024 * 1024;
const REMINDER_STATUSES: [&str; 4] = ["pending", "completed", "done", "dismissed"];

pub struct RestorePlan {
    notes: Vec<RestoreNote>,
    skipped: usize,
    folder: PathBuf,
}

struct RestoreNote {
    id: String,
    content: String,
    fields: Value,
}

#[derive(Debug, PartialEq, Eq)]
pub struct Restored {
    pub notes: usize,
    pub images: usize,
    pub missing_images: usize,
    pub skipped_notes: usize,
}

fn text<'a>(note: &'a Value, key: &str) -> Option<&'a str> {
    note[key].as_str()
}

/// Turns exported content into what the editor saves, or None if it's unusable.
/// Old notes exported as bare text become ordinary text blocks.
fn usable_content(content: &Value) -> Option<String> {
    let value = match content {
        Value::String(plain) => {
            let blocks: Vec<Value> = plain
                .split('\n')
                .map(|line| json!({"type": "text", "runs": [{"t": line}]}))
                .collect();
            json!({"blocks": blocks, "plain": plain})
        }
        Value::Object(_) if content["blocks"].is_array() && content["plain"].is_string() => {
            content.clone()
        }
        _ => return None,
    };
    let content = crate::content_identity::reconcile_identities(
        r#"{"blocks":[],"plain":""}"#,
        &value.to_string(),
    )
    .ok()?;
    (content.len() <= MAX_NOTE_BYTES).then_some(content)
}

/// Reads and checks the file without changing anything. Damaged notes are
/// counted and left out, so one of them can't block the rest of a backup.
pub fn preview(source: &Path) -> Result<RestorePlan> {
    let size = std::fs::metadata(source)?.len();
    ensure!(size <= MAX_FILE_BYTES, "The file is too large");
    let document: Value = serde_json::from_slice(&std::fs::read(source)?)
        .context("This isn't a Sticky Notes+ export")?;
    ensure!(
        document["app"] == "Sticky Notes+",
        "This isn't a Sticky Notes+ export"
    );
    ensure!(
        document["format"] == 1,
        "This export needs a newer version of the app"
    );
    let exported = document["notes"]
        .as_array()
        .context("This export has no notes")?;
    let mut notes = Vec::with_capacity(exported.len());
    for note in exported {
        let id = text(note, "id").filter(|id| !id.is_empty());
        if let (Some(id), Some(content)) = (id, usable_content(&note["content"])) {
            notes.push(RestoreNote {
                id: id.to_owned(),
                content,
                fields: note.clone(),
            });
        }
    }
    Ok(RestorePlan {
        skipped: exported.len() - notes.len(),
        notes,
        folder: source.parent().unwrap_or(Path::new(".")).to_path_buf(),
    })
}

impl RestorePlan {
    pub fn count(&self) -> usize {
        self.notes.len()
    }

    pub fn skipped(&self) -> usize {
        self.skipped
    }

    pub fn already_here(&self, conn: &Connection) -> Result<usize> {
        let mut found = 0;
        for note in &self.notes {
            if note_exists(conn, &note.id)? {
                found += 1;
            }
        }
        Ok(found)
    }

    /// The image's stored ID and location. Both come from the file, so only a
    /// UUID name directly inside the export's own image folder is accepted.
    fn image_source(&self, image: &Value) -> Option<(String, PathBuf)> {
        let relative = Path::new(text(image, "file")?);
        let parts: Vec<Component> = relative.components().collect();
        let [Component::Normal(_), Component::Normal(_)] = parts[..] else {
            return None;
        };
        let id = relative.file_stem()?.to_str()?;
        let parsed = uuid::Uuid::parse_str(id).ok()?;
        (parsed.to_string() == id).then(|| (id.to_owned(), self.folder.join(relative)))
    }
}

fn note_exists(conn: &Connection, id: &str) -> rusqlite::Result<bool> {
    conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM notes WHERE id=?1)",
        [id],
        |r| r.get(0),
    )
}

/// An image placed in the text without its file would stop the note saving.
fn remove_images_except(value: &mut Value, keep: &HashSet<String>) {
    for key in ["blocks", "items", "runs"] {
        if let Some(children) = value.get_mut(key).and_then(Value::as_array_mut) {
            children.retain(|child| {
                child["type"] != "image"
                    || child
                        .pointer("/attrs/attachmentId")
                        .and_then(Value::as_str)
                        .is_some_and(|id| keep.contains(id))
            });
            for child in children {
                remove_images_except(child, keep);
            }
        }
    }
}

struct StoredImage {
    note: String,
    id: String,
    name: String,
    item: attachments::Attachment,
}

pub fn apply(db: &Mutex<Db>, plan: &RestorePlan) -> Result<Restored> {
    // Keeps orphan cleanup from deleting image files before their rows exist.
    let _import = attachments::ImportGuard::new(attachments::storage(&lock(db).conn)?);
    let mut created = Vec::new();
    let result = restore(db, plan, &mut created);
    if result.is_err() {
        // Only files created by this restore are eligible for cleanup.
        for path in created {
            let _ = std::fs::remove_file(path);
        }
    }
    result
}

/// Each import snapshots the whole database first; older snapshots are only
/// disk use (and an old copy of the encrypted Google token), so keep a few.
const KEPT_RESTORE_BACKUPS: usize = 2;

/// Deletes all but the newest `keep` pre-import backups in `folder`.
fn prune_restore_backups(folder: &Path, keep: usize) {
    let Ok(entries) = std::fs::read_dir(folder) else {
        return;
    };
    let mut backups: Vec<_> = entries
        .flatten()
        .filter(|entry| {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            name.starts_with("before-restore-") && name.ends_with(".db")
        })
        .filter_map(|entry| Some((entry.metadata().ok()?.modified().ok()?, entry.path())))
        .collect();
    backups.sort_by_key(|(modified, _)| std::cmp::Reverse(*modified));
    for (_, path) in backups.into_iter().skip(keep) {
        let _ = std::fs::remove_file(path);
    }
}

fn restore(db: &Mutex<Db>, plan: &RestorePlan, created: &mut Vec<PathBuf>) -> Result<Restored> {
    let (new_notes, root) = {
        let guard = lock(db);
        let mut new_notes = Vec::new();
        for note in &plan.notes {
            if !note_exists(&guard.conn, &note.id)? {
                new_notes.push(note);
            }
        }
        (new_notes, attachments::storage(&guard.conn)?)
    };
    let mut stored = Vec::new();
    let mut missing_images = 0;
    for note in &new_notes {
        for image in note.fields["images"].as_array().into_iter().flatten() {
            let name = text(image, "name").unwrap_or("image");
            let item = plan.image_source(image).and_then(|(id, source)| {
                let taken = attachments::exists(&lock(db).conn, &id).ok()?;
                let prepared = (!taken).then(|| attachments::prepare_file(&source).ok())??;
                let item = attachments::store_files(&root, &id, prepared, created).ok()?;
                Some((id, item))
            });
            match item {
                Some((id, item)) => stored.push(StoredImage {
                    note: note.id.clone(),
                    id,
                    name: name.to_owned(),
                    item,
                }),
                None => missing_images += 1,
            }
        }
    }

    let guard = lock(db);
    let conn = &guard.conn;
    let database = Path::new(conn.path().context("Restore needs a saved database")?);
    let folder = database
        .parent()
        .context("Database directory unavailable")?;
    let backup = folder.join(format!("before-restore-{}.db", uuid::Uuid::new_v4()));
    // VACUUM INTO creates a consistent snapshot, including committed WAL.
    conn.execute("VACUUM INTO ?1", [backup.to_string_lossy().as_ref()])?;
    prune_restore_backups(folder, KEPT_RESTORE_BACKUPS);
    let now = chrono::Utc::now().to_rfc3339();
    let tx = conn.unchecked_transaction()?;
    let mut restored = 0;
    for note in new_notes {
        if note_exists(&tx, &note.id)? {
            continue;
        }
        let keep: HashSet<String> = stored
            .iter()
            .filter(|image| image.note == note.id)
            .map(|image| image.id.clone())
            .collect();
        let mut content: Value = serde_json::from_str(&note.content)?;
        remove_images_except(&mut content, &keep);
        let content = content.to_string();
        let fields = &note.fields;
        let deleted_at = text(fields, "deleted_at");
        notes::insert_restored(
            &tx,
            &note.id,
            &content,
            text(fields, "color").unwrap_or("#FFF9B1"),
            fields["pinned"].as_bool().unwrap_or(false),
            fields["archived"].as_bool().unwrap_or(false),
            deleted_at,
            text(fields, "created_at").unwrap_or(&now),
            text(fields, "modified_at").unwrap_or(&now),
        )?;
        if deleted_at.is_none() {
            reminders::reconcile_note_in_transaction(&tx, &note.id, &content)?;
            restore_reminder_state(&tx, &note.id, fields)?;
        }
        restored += 1;
    }
    for image in &stored {
        attachments::insert_row(&tx, &image.note, &image.id, &image.name, &image.item)?;
    }
    tx.commit()?;
    Ok(Restored {
        notes: restored,
        images: stored.len(),
        missing_images,
        skipped_notes: plan.skipped,
    })
}

/// Finished and already-announced reminders must not fire again after a restore.
fn restore_reminder_state(conn: &Connection, note_id: &str, note: &Value) -> Result<()> {
    for reminder in note["reminders"].as_array().into_iter().flatten() {
        let (Some(chip_id), Some(status)) = (text(reminder, "chip_id"), text(reminder, "status"))
        else {
            continue;
        };
        if !REMINDER_STATUSES.contains(&status) {
            continue;
        }
        conn.execute(
            "UPDATE reminders SET status=?1, scheduled_date=coalesce(?2,scheduled_date), snoozed_until=?3, notified_for=?4 WHERE note_id=?5 AND chip_id=?6",
            params![
                status,
                text(reminder, "date"),
                text(reminder, "snoozed_until"),
                text(reminder, "notified_for"),
                note_id,
                chip_id,
            ],
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_newest_restore_backups_are_kept() {
        let folder = std::env::temp_dir().join(format!("sticky-backups-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&folder).unwrap();
        let mut made = Vec::new();
        for index in 0..4 {
            let path = folder.join(format!("before-restore-{index}.db"));
            std::fs::write(&path, "db").unwrap();
            let time =
                std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_000 + index);
            std::fs::File::options()
                .write(true)
                .open(&path)
                .unwrap()
                .set_modified(time)
                .unwrap();
            made.push(path);
        }
        let other = folder.join("sticky-notes.db");
        std::fs::write(&other, "keep").unwrap();
        prune_restore_backups(&folder, 2);
        let left: Vec<bool> = made.iter().map(|path| path.exists()).collect();
        assert_eq!(left, [false, false, true, true]);
        assert!(other.exists());
    }
    use crate::db::export;

    fn folder(prefix: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!("{prefix}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    fn open(directory: &Path) -> Mutex<Db> {
        Mutex::new(Db::open_in_dir(directory).unwrap())
    }

    const DATED: &str = r#"{"blocks":[{"type":"text","runs":[{"type":"dateChip","attrs":{"chipId":"chip","date":"2020-01-02","time":"09:00","label":"Jan 2"}}]}],"plain":"Jan 2"}"#;

    #[test]
    fn export_then_restore_brings_everything_back() {
        let source_dir = folder("sticky-restore-a");
        let source = open(&source_dir);
        {
            let conn = &lock(&source).conn;
            notes::create(conn, "plain", r#"{"blocks":[],"plain":"hello"}"#, "#D9F7BE").unwrap();
            notes::create(conn, "gone", r#"{"blocks":[],"plain":"bye"}"#, "#FFF9B1").unwrap();
            notes::create(conn, "dated", DATED, "#FFF9B1").unwrap();
            conn.execute(
                "UPDATE notes SET deleted_at='2026-01-01' WHERE id='gone'",
                [],
            )
            .unwrap();
            conn.execute(
                "UPDATE reminders SET status='dismissed' WHERE note_id='dated'",
                [],
            )
            .unwrap();
        }
        let picture = source_dir.join("my photo.png");
        image::RgbaImage::new(4, 4).save(&picture).unwrap();
        let image = attachments::import_image(&lock(&source).conn, "plain", &picture).unwrap();
        let file = source_dir.join("backup.json");
        export::write(&source, &file, "now").unwrap();

        let target_dir = folder("sticky-restore-b");
        let target = open(&target_dir);
        let plan = preview(&file).unwrap();
        assert_eq!(
            (
                plan.count(),
                plan.already_here(&lock(&target).conn).unwrap()
            ),
            (3, 0)
        );
        let restored = apply(&target, &plan).unwrap();

        assert_eq!(
            (restored.notes, restored.images, restored.missing_images),
            (3, 1, 0)
        );
        {
            let conn = &lock(&target).conn;
            assert_eq!(notes::list(conn, true, false, None).unwrap().len(), 2);
            assert_eq!(notes::list(conn, true, true, None).unwrap().len(), 1);
            assert_eq!(
                notes::list(conn, true, false, Some("hello")).unwrap().len(),
                1
            );
            let kept = attachments::list(conn, "plain").unwrap();
            assert_eq!(
                (kept[0].id.as_str(), kept[0].name.as_str()),
                (image.id.as_str(), "my photo.png")
            );
            assert!(attachments::original_path(conn, &image.id).is_ok());
            assert_eq!(
                reminders::list_for_note(conn, "dated").unwrap()[0].status,
                "dismissed"
            );
            // Restoring again adds nothing and never overwrites.
            notes::save_content_and_color(conn, "plain", r#"{"blocks":[],"plain":"edited"}"#, None)
                .unwrap();
        }
        let again = preview(&file).unwrap();
        assert_eq!(again.already_here(&lock(&target).conn).unwrap(), 3);
        assert_eq!(apply(&target, &again).unwrap().notes, 0);
        assert_eq!(
            notes::list(&lock(&target).conn, true, false, Some("edited"))
                .unwrap()
                .len(),
            1
        );
        drop((source, target));
        std::fs::remove_dir_all(source_dir).unwrap();
        std::fs::remove_dir_all(target_dir).unwrap();
    }

    #[test]
    fn a_note_whose_image_is_missing_can_still_be_saved() {
        let directory = folder("sticky-restore-e");
        let db = open(&directory);
        let id = uuid::Uuid::new_v4();
        let body = json!({
            "app": "Sticky Notes+", "format": 1,
            "notes": [{"id": "n", "content": {"blocks": [{"type": "text", "runs": [
                {"t": "before"},
                {"type": "image", "attrs": {"attachmentId": id.to_string()}},
            ]}], "plain": "before"}, "images": [{"name": "x", "file": null}]}],
        });
        let path = directory.join("backup.json");
        std::fs::write(&path, body.to_string()).unwrap();

        let restored = apply(&db, &preview(&path).unwrap()).unwrap();

        assert_eq!((restored.notes, restored.missing_images), (1, 1));
        let conn = &lock(&db).conn;
        let note = notes::get(conn, "n").unwrap().unwrap();
        assert!(crate::inline_images::content_ids(&note.content).is_empty());
        assert!(note.content.contains("before"));
        notes::save_content_and_color(conn, "n", &note.content, None).unwrap();
    }

    #[test]
    fn other_files_are_refused_and_change_nothing() {
        let directory = folder("sticky-restore-c");
        for (name, body) in [
            ("a.json", "not json"),
            ("b.json", r#"{"app":"Other","format":1,"notes":[]}"#),
            ("c.json", r#"{"app":"Sticky Notes+","format":2,"notes":[]}"#),
            ("d.json", r#"{"app":"Sticky Notes+","format":1}"#),
        ] {
            let path = directory.join(name);
            std::fs::write(&path, body).unwrap();
            assert!(preview(&path).is_err(), "{name}");
        }
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn damaged_notes_are_skipped_and_old_text_notes_are_kept() {
        let directory = folder("sticky-restore-f");
        let db = open(&directory);
        let huge = "x".repeat(MAX_NOTE_BYTES + 1);
        let body = json!({
            "app": "Sticky Notes+", "format": 1,
            "notes": [
                {"id": "good", "content": {"blocks": [], "plain": "fine"}},
                {"id": "old", "content": "line one\nline two"},
                {"content": {"blocks": [], "plain": "no id"}},
                {"id": "shapeless", "content": {}},
                {"id": "number", "content": 7},
                {"id": "huge", "content": huge},
            ],
        });
        let path = directory.join("backup.json");
        std::fs::write(&path, body.to_string()).unwrap();

        let plan = preview(&path).unwrap();
        assert_eq!((plan.count(), plan.skipped()), (2, 4));
        let restored = apply(&db, &plan).unwrap();
        assert_eq!((restored.notes, restored.skipped_notes), (2, 4));
        let conn = &lock(&db).conn;
        let old = notes::get(conn, "old").unwrap().unwrap();
        assert_eq!(notes::content_to_plain(&old.content), "line one\nline two");
        assert_eq!(
            notes::list(conn, true, false, Some("two")).unwrap().len(),
            1
        );
    }

    #[test]
    fn image_paths_cannot_leave_the_export_folder() {
        let directory = folder("sticky-restore-d");
        let db = open(&directory);
        let outside = directory.join("secret.png");
        image::RgbaImage::new(4, 4).save(&outside).unwrap();
        let id = uuid::Uuid::new_v4();
        let export = directory.join("sub");
        std::fs::create_dir_all(&export).unwrap();
        for bad in [
            format!("../{id}.png"),
            format!("{}/{id}.png", outside.display()),
            format!("images/../../{id}.png"),
            "images/not-a-uuid.png".to_owned(),
            format!("{id}.png"),
        ] {
            let body = json!({
                "app": "Sticky Notes+", "format": 1,
                "notes": [{"id": format!("n{}", uuid::Uuid::new_v4()), "content": {"blocks": [], "plain": ""},
                           "images": [{"name": "x", "file": bad}]}],
            });
            let path = export.join("backup.json");
            std::fs::write(&path, body.to_string()).unwrap();
            let restored = apply(&db, &preview(&path).unwrap()).unwrap();
            assert_eq!((restored.images, restored.missing_images), (0, 1), "{bad}");
        }
        drop(db);
        std::fs::remove_dir_all(directory).unwrap();
    }
}
