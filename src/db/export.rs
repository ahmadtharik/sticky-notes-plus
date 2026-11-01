//! Readable copy of every note: one JSON file plus a sibling folder of the
//! original images. Secrets and Google state in `meta` are never included.
use super::{attachments, lock, notes, reminders, Db};
use anyhow::{Context, Result};
use rusqlite::Connection;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

#[derive(Debug, PartialEq, Eq)]
pub struct Summary {
    pub notes: usize,
    pub images: usize,
    pub missing_images: usize,
}

fn extension(mime: &str) -> &'static str {
    match mime {
        "image/jpeg" => "jpg",
        "image/bmp" => "bmp",
        _ => "png",
    }
}

fn images_folder(path: &Path) -> PathBuf {
    let stem = path.file_stem().unwrap_or_default().to_string_lossy();
    path.with_file_name(format!("{stem}-images"))
}

struct Collected {
    note: notes::NoteRow,
    images: Vec<(attachments::Attachment, Result<PathBuf>)>,
    reminders: Vec<reminders::ReminderRow>,
}

fn collect(conn: &Connection) -> Result<Vec<Collected>> {
    let mut rows = notes::list(conn, true, false, None)?;
    rows.extend(notes::list(conn, true, true, None)?);
    rows.into_iter()
        .map(|note| {
            let images = attachments::list(conn, &note.id)?
                .into_iter()
                .map(|image| {
                    let source = attachments::original_path(conn, &image.id);
                    (image, source)
                })
                .collect();
            Ok(Collected {
                images,
                reminders: reminders::list_for_note(conn, &note.id)?,
                note,
            })
        })
        .collect()
}

/// Reads under the lock, then copies images and writes without it, so a large
/// export doesn't stop notes saving.
pub fn write(db: &Mutex<Db>, path: &Path, exported_at: &str) -> Result<Summary> {
    let collected = collect(&lock(db).conn)?;
    let folder = images_folder(path);
    let folder_name = folder
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned();
    let mut summary = Summary {
        notes: 0,
        images: 0,
        missing_images: 0,
    };
    let mut exported = Vec::with_capacity(collected.len());
    for Collected {
        note,
        images: note_images,
        reminders: note_reminders,
    } in collected
    {
        let mut images = Vec::new();
        for (image, source) in note_images {
            // The id is a validated UUID, unlike the user's original file name.
            let file = format!("{}.{}", image.id, extension(&image.mime));
            let copied = source.and_then(|source| {
                std::fs::create_dir_all(&folder)?;
                std::fs::copy(source, folder.join(&file))?;
                Ok(())
            });
            if copied.is_ok() {
                summary.images += 1;
            } else {
                summary.missing_images += 1;
            }
            images.push(json!({
                "name": image.name,
                "file": copied.is_ok().then(|| format!("{folder_name}/{file}")),
            }));
        }
        let note_reminders: Vec<Value> = note_reminders
            .into_iter()
            .map(|reminder| {
                json!({
                    "chip_id": reminder.chip_id,
                    "date": reminder.scheduled_date,
                    "time": reminder.due_time,
                    "repeat": reminder.recurrence,
                    "status": reminder.status,
                    "snoozed_until": reminder.snoozed_until,
                    "notified_for": reminder.notified_for,
                })
            })
            .collect();
        exported.push(json!({
            "id": note.id,
            "title": note.title,
            "text": notes::content_to_plain(&note.content),
            "content": serde_json::from_str::<Value>(&note.content).unwrap_or(Value::String(note.content.clone())),
            "color": note.color,
            "pinned": note.is_pinned,
            "archived": note.is_archived,
            "deleted_at": note.deleted_at,
            "created_at": note.created_at,
            "modified_at": note.modified_at,
            "reminders": note_reminders,
            "images": images,
        }));
        summary.notes += 1;
    }
    let document = json!({
        "app": "Sticky Notes+",
        "format": 1,
        "exported_at": exported_at,
        "notes": exported,
    });
    // Never leave a half-written file under the chosen name.
    let partial = path.with_extension("partial");
    std::fs::write(&partial, serde_json::to_vec_pretty(&document)?)
        .with_context(|| format!("Couldn't write {}", partial.display()))?;
    std::fs::rename(&partial, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&partial);
    })?;
    Ok(summary)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Db;

    #[test]
    fn export_includes_every_note_and_image_but_no_secrets() {
        let directory =
            std::env::temp_dir().join(format!("sticky-export-{}", uuid::Uuid::new_v4()));
        let db = Db::open_in_dir(&directory).unwrap();
        for id in ["live", "archived", "deleted"] {
            let content = format!(r#"{{"blocks":[],"plain":"{id} text"}}"#);
            notes::create(&db.conn, id, &content, "#FFF9B1").unwrap();
        }
        db.conn
            .execute("UPDATE notes SET is_archived=1 WHERE id='archived'", [])
            .unwrap();
        db.conn
            .execute(
                "UPDATE notes SET deleted_at='2026-01-01' WHERE id='deleted'",
                [],
            )
            .unwrap();
        db.conn
            .execute(
                "INSERT INTO meta VALUES ('google_refresh_token', 'sealed-secret')",
                [],
            )
            .unwrap();
        let source = directory.join("my photo.png");
        image::RgbaImage::new(4, 4).save(&source).unwrap();
        let image = attachments::import_image(&db.conn, "live", &source).unwrap();

        let path = directory.join("backup.json");
        let db = Mutex::new(db);
        let summary = write(&db, &path, "2026-10-31").unwrap();

        assert_eq!(
            summary,
            Summary {
                notes: 3,
                images: 1,
                missing_images: 0
            }
        );
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(!text.contains("sealed-secret"));
        let document: Value = serde_json::from_str(&text).unwrap();
        let exported = document["notes"].as_array().unwrap();
        assert_eq!(exported.len(), 3);
        let live = exported.iter().find(|n| n["id"] == "live").unwrap();
        assert_eq!(live["text"], "live text");
        assert_eq!(live["images"][0]["name"], "my photo.png");
        assert_eq!(
            live["images"][0]["file"],
            format!("backup-images/{}.png", image.id)
        );
        assert!(directory
            .join("backup-images")
            .join(format!("{}.png", image.id))
            .is_file());
        assert!(!path.with_extension("partial").exists());
        drop(db);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn a_missing_image_is_reported_without_losing_the_notes() {
        let directory =
            std::env::temp_dir().join(format!("sticky-export-{}", uuid::Uuid::new_v4()));
        let db = Db::open_in_dir(&directory).unwrap();
        notes::create(&db.conn, "note", r#"{"blocks":[],"plain":"x"}"#, "#FFF9B1").unwrap();
        let source = directory.join("a.png");
        image::RgbaImage::new(4, 4).save(&source).unwrap();
        let image = attachments::import_image(&db.conn, "note", &source).unwrap();
        std::fs::remove_file(attachments::original_path(&db.conn, &image.id).unwrap()).unwrap();

        let path = directory.join("backup.json");
        let db = Mutex::new(db);
        let summary = write(&db, &path, "now").unwrap();

        assert_eq!(summary.notes, 1);
        assert_eq!(summary.images, 0);
        assert_eq!(summary.missing_images, 1);
        let document: Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert!(document["notes"][0]["images"][0]["file"].is_null());
        drop(db);
        std::fs::remove_dir_all(directory).unwrap();
    }
}
