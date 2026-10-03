//! Note-owned images remain independent of text serialization/autosaves.
//! Originals are immutable; the UI should load only the bounded thumbnail.
use anyhow::{bail, ensure, Context, Result};
use image::{GenericImageView, ImageFormat};
use rusqlite::{params, Connection};
use std::{
    io::{Cursor, Read, Write},
    path::{Path, PathBuf},
};

const MAX_BYTES: u64 = 20 * 1024 * 1024;
const MAX_PIXELS: u64 = 8_000_000;

#[derive(Debug, PartialEq)]
pub struct Attachment {
    pub id: String,
    pub name: String,
    pub mime: String,
    pub bytes: u64,
    pub width: u32,
    pub height: u32,
}

fn storage(conn: &Connection) -> Result<PathBuf> {
    let database = conn
        .path()
        .filter(|p| !p.is_empty())
        .context("Attachments need a saved database")?;
    let parent = Path::new(database)
        .parent()
        .context("Database directory unavailable")?
        .canonicalize()?;
    let root = parent.join("attachments");
    std::fs::create_dir_all(&root)?;
    ensure!(
        !std::fs::symlink_metadata(&root)?.file_type().is_symlink(),
        "Attachment directory cannot be a link"
    );
    ensure!(
        root.canonicalize()?.parent() == Some(parent.as_path()),
        "Attachment directory escaped application storage"
    );
    Ok(root)
}

pub fn thumbnail_path(conn: &Connection, id: &str) -> Result<PathBuf> {
    let uuid = uuid::Uuid::parse_str(id)?;
    ensure!(uuid.to_string() == id, "Invalid attachment identifier");
    let root = storage(conn)?;
    let path = root.join(format!("{id}.thumb.png"));
    ensure!(
        !std::fs::symlink_metadata(&path)?.file_type().is_symlink(),
        "Attachment cannot be a link"
    );
    ensure!(
        path.canonicalize()?.parent() == Some(root.canonicalize()?.as_path()),
        "Attachment escaped storage"
    );
    Ok(path)
}

pub fn list(conn: &Connection, note_id: &str) -> Result<Vec<Attachment>> {
    let mut statement = conn.prepare(
        "SELECT id,name,mime,bytes,width,height FROM attachments WHERE note_id=?1 ORDER BY rowid",
    )?;
    let rows = statement.query_map([note_id], |r| {
        Ok(Attachment {
            id: r.get(0)?,
            name: r.get(1)?,
            mime: r.get(2)?,
            bytes: r.get(3)?,
            width: r.get(4)?,
            height: r.get(5)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// Call under the same database mutex as imports. Only UUID-named, unreferenced
/// regular files directly inside application storage are eligible for deletion.
pub fn cleanup_orphans(conn: &Connection) -> Result<()> {
    let root = storage(conn)?;
    for entry in std::fs::read_dir(&root)? {
        let entry = entry?;
        if !entry.file_type()?.is_file() {
            continue;
        }
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        let Some(id) = name
            .strip_suffix(".original")
            .or_else(|| name.strip_suffix(".thumb.png"))
        else {
            continue;
        };
        let Ok(uuid) = uuid::Uuid::parse_str(id) else {
            continue;
        };
        if uuid.to_string() != id {
            continue;
        }
        let referenced: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM attachments WHERE id=?1)",
            [id],
            |r| r.get(0),
        )?;
        if !referenced {
            std::fs::remove_file(entry.path())?;
        }
    }
    Ok(())
}

pub fn remove(conn: &Connection, note_id: &str, id: &str) -> Result<()> {
    conn.execute("DELETE FROM attachments WHERE note_id=?1 AND id=?2 AND EXISTS(SELECT 1 FROM notes WHERE id=?1 AND deleted_at IS NULL)", params![note_id, id])?;
    cleanup_orphans(conn)
}

pub fn thumbnail(conn: &Connection, id: &str) -> Result<slint::Image> {
    let path = thumbnail_path(conn, id)?;
    let mut bytes = Vec::new();
    std::fs::File::open(path)?
        .take(1024 * 1024 + 1)
        .read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= 1024 * 1024, "Invalid thumbnail size");
    let mut reader = image::io::Reader::with_format(Cursor::new(bytes), ImageFormat::Png);
    let mut limits = image::io::Limits::default();
    limits.max_image_width = Some(320);
    limits.max_image_height = Some(240);
    limits.max_alloc = Some(1024 * 1024);
    reader.limits(limits);
    let pixels = reader.decode()?.to_rgba8();
    Ok(slint::Image::from_rgba8(slint::SharedPixelBuffer::<
        slint::Rgba8Pixel,
    >::clone_from_slice(
        pixels.as_raw(),
        pixels.width(),
        pixels.height(),
    )))
}

pub fn import_image(conn: &Connection, note_id: &str, source: &Path) -> Result<Attachment> {
    let file = std::fs::File::open(source)?;
    ensure!(file.metadata()?.is_file(), "Select a regular image file");
    ensure!(file.metadata()?.len() <= MAX_BYTES, "Image exceeds 20 MiB");
    let mut bytes = Vec::new();
    file.take(MAX_BYTES + 1).read_to_end(&mut bytes)?;
    ensure!(bytes.len() as u64 <= MAX_BYTES, "Image exceeds 20 MiB");
    let format = image::guess_format(&bytes)?;
    let mime = match format {
        ImageFormat::Png => "image/png",
        ImageFormat::Jpeg => "image/jpeg",
        _ => bail!("Only PNG and JPEG images are supported"),
    };
    let (width, height) =
        image::io::Reader::with_format(Cursor::new(&bytes), format).into_dimensions()?;
    ensure!(
        width > 0 && height > 0 && u64::from(width) * u64::from(height) <= MAX_PIXELS,
        "Image exceeds the 8 megapixel limit"
    );
    let mut reader = image::io::Reader::with_format(Cursor::new(&bytes), format);
    let mut limits = image::io::Limits::default();
    limits.max_alloc = Some(64 * 1024 * 1024);
    limits.max_image_width = Some(8192);
    limits.max_image_height = Some(8192);
    reader.limits(limits);
    let decoded = reader.decode()?;
    ensure!(
        decoded.dimensions() == (width, height),
        "Image dimensions changed during decoding"
    );
    let mut thumbnail = Cursor::new(Vec::new());
    decoded
        .thumbnail(320, 240)
        .write_to(&mut thumbnail, image::ImageOutputFormat::Png)?;
    let item = Attachment {
        id: uuid::Uuid::new_v4().to_string(),
        name: source
            .file_name()
            .context("Image name unavailable")?
            .to_string_lossy()
            .chars()
            .take(200)
            .collect(),
        mime: mime.into(),
        bytes: bytes.len() as u64,
        width,
        height,
    };
    let tx = conn.unchecked_transaction()?;
    let live: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM notes WHERE id=?1 AND deleted_at IS NULL)",
        [note_id],
        |r| r.get(0),
    )?;
    ensure!(live, "Cannot attach an image to a missing or deleted note");
    let root = storage(&tx)?;
    let original_path = root.join(format!("{}.original", item.id));
    let thumbnail_path = root.join(format!("{}.thumb.png", item.id));
    let mut created = Vec::new();
    let result = (|| -> Result<()> {
        for (path, data) in [
            (&original_path, bytes.as_slice()),
            (&thumbnail_path, thumbnail.get_ref().as_slice()),
        ] {
            let mut output = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(path)?;
            created.push(path.clone());
            output.write_all(data)?;
            output.sync_all()?;
        }
        tx.execute("INSERT INTO attachments(id,note_id,name,mime,bytes,width,height) VALUES (?1,?2,?3,?4,?5,?6,?7)", params![item.id, note_id, item.name, item.mime, item.bytes, width, height])?;
        tx.commit()?;
        Ok(())
    })();
    if result.is_err() {
        // Only files created by this operation are eligible for cleanup.
        for path in created {
            let _ = std::fs::remove_file(path);
        }
    }
    result?;
    Ok(item)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{notes, Db};

    #[test]
    fn images_survive_text_save_delete_restore_and_database_reopen() {
        let directory =
            std::env::temp_dir().join(format!("sticky-attachments-{}", uuid::Uuid::new_v4()));
        let db = Db::open_in_dir(&directory).unwrap();
        notes::create(&db.conn, "note", r#"{"blocks":[],"plain":""}"#, "#FFF9B1").unwrap();
        let source = directory.join("example.png");
        image::RgbaImage::new(640, 480).save(&source).unwrap();
        let item = import_image(&db.conn, "note", &source).unwrap();
        let thumbnail = image::open(thumbnail_path(&db.conn, &item.id).unwrap()).unwrap();
        assert_eq!(thumbnail.dimensions(), (320, 240));
        notes::save_content_and_color(
            &db.conn,
            "note",
            r#"{"blocks":[],"plain":"edited"}"#,
            Some("#FFF9B1"),
        )
        .unwrap();
        notes::soft_delete(&db.conn, "note").unwrap();
        assert!(import_image(&db.conn, "note", &source).is_err());
        notes::restore(&db.conn, "note").unwrap();
        drop(db);
        let db = Db::open_in_dir(&directory).unwrap();
        assert_eq!(list(&db.conn, "note").unwrap(), vec![item]);
        assert!(thumbnail_path(&db.conn, "../outside").is_err());
    }

    #[test]
    fn malformed_images_and_missing_notes_leave_no_attachment_files() {
        let directory =
            std::env::temp_dir().join(format!("sticky-attachments-{}", uuid::Uuid::new_v4()));
        let db = Db::open_in_dir(&directory).unwrap();
        let source = directory.join("fake.png");
        std::fs::write(&source, b"not an image").unwrap();
        assert!(import_image(&db.conn, "missing", &source).is_err());
        image::RgbaImage::new(1, 1).save(&source).unwrap();
        assert!(import_image(&db.conn, "missing", &source).is_err());
        assert!(!directory.join("attachments").exists());
    }

    #[test]
    fn failed_database_write_cleans_only_newly_created_files() {
        let directory =
            std::env::temp_dir().join(format!("sticky-attachments-{}", uuid::Uuid::new_v4()));
        let db = Db::open_in_dir(&directory).unwrap();
        notes::create(&db.conn, "note", r#"{"blocks":[],"plain":""}"#, "#FFF9B1").unwrap();
        let source = directory.join("example.png");
        image::RgbaImage::new(1, 1).save(&source).unwrap();
        let original = import_image(&db.conn, "note", &source).unwrap();
        db.conn.execute_batch("CREATE TRIGGER reject_attachment BEFORE INSERT ON attachments BEGIN SELECT RAISE(ABORT, 'test failure'); END;").unwrap();
        assert!(import_image(&db.conn, "note", &source).is_err());
        assert_eq!(list(&db.conn, "note").unwrap(), vec![original]);
        assert_eq!(
            std::fs::read_dir(directory.join("attachments"))
                .unwrap()
                .count(),
            2
        );
        assert!(source.exists());
    }

    #[test]
    fn removal_and_purge_preserve_other_notes_and_unrecognized_files() {
        let directory =
            std::env::temp_dir().join(format!("sticky-attachments-{}", uuid::Uuid::new_v4()));
        let db = Db::open_in_dir(&directory).unwrap();
        for id in ["one", "two"] {
            notes::create(&db.conn, id, r#"{"blocks":[],"plain":""}"#, "#FFF9B1").unwrap();
        }
        let source = directory.join("image.png");
        image::RgbaImage::new(4, 4).save(&source).unwrap();
        let first = import_image(&db.conn, "one", &source).unwrap();
        let second = import_image(&db.conn, "two", &source).unwrap();
        let unrelated = directory.join("attachments/keep.txt");
        std::fs::write(&unrelated, "keep").unwrap();
        remove(&db.conn, "two", &first.id).unwrap();
        assert_eq!(list(&db.conn, "one").unwrap().len(), 1);
        remove(&db.conn, "one", &first.id).unwrap();
        assert!(list(&db.conn, "one").unwrap().is_empty());
        assert!(!directory
            .join(format!("attachments/{}.original", first.id))
            .exists());
        assert!(thumbnail(&db.conn, &second.id).is_ok());
        notes::soft_delete(&db.conn, "two").unwrap();
        cleanup_orphans(&db.conn).unwrap();
        assert!(thumbnail(&db.conn, &second.id).is_ok());
        notes::purge(&db.conn, "two").unwrap();
        cleanup_orphans(&db.conn).unwrap();
        assert!(!directory
            .join(format!("attachments/{}.original", second.id))
            .exists());
        assert!(unrelated.exists());
        assert!(source.exists());
    }
}
