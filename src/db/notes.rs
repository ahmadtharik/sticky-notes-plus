use super::reminders;
use chrono::Utc;
use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;

#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct NoteRow {
    pub id: String,
    pub title: Option<String>,
    pub content: String,
    pub color: String,
    pub is_pinned: bool,
    pub is_archived: bool,
    pub deleted_at: Option<String>,
    pub window: Option<(f64, f64, f64, f64)>,
    pub is_floating: bool,
    pub is_always_on_top: bool,
    pub created_at: String,
    pub modified_at: String,
}

pub fn content_to_plain(content: &str) -> String {
    serde_json::from_str::<serde_json::Value>(content)
        .ok()
        .and_then(|v| v.get("plain").and_then(|p| p.as_str()).map(str::to_owned))
        .unwrap_or_default()
}

fn title_from_plain(plain: &str) -> Option<String> {
    plain
        .lines()
        .find(|line| !line.trim().is_empty())
        .map(|line| line.trim().chars().take(60).collect())
}

fn now() -> String {
    Utc::now().to_rfc3339()
}

const SELECT: &str = "SELECT id,title,content,color,is_pinned,is_archived,deleted_at,window_x,window_y,window_w,window_h,is_floating,is_always_on_top,created_at,modified_at FROM notes";

fn row(r: &rusqlite::Row<'_>) -> rusqlite::Result<NoteRow> {
    let geometry = (r.get(7)?, r.get(8)?, r.get(9)?, r.get(10)?);
    Ok(NoteRow {
        id: r.get(0)?,
        title: r.get(1)?,
        content: r.get(2)?,
        color: r.get(3)?,
        is_pinned: r.get::<_, i64>(4)? != 0,
        is_archived: r.get::<_, i64>(5)? != 0,
        deleted_at: r.get(6)?,
        window: match geometry {
            (Some(x), Some(y), Some(w), Some(h)) => Some((x, y, w, h)),
            _ => None,
        },
        is_floating: r.get::<_, i64>(11)? != 0,
        is_always_on_top: r.get::<_, i64>(12)? != 0,
        created_at: r.get(13)?,
        modified_at: r.get(14)?,
    })
}

/// Replace one note's FTS entry. Call inside the same transaction that writes
/// the note so search can never observe a mismatched snapshot.
fn sync_note_fts(conn: &Connection, id: &str) -> rusqlite::Result<()> {
    conn.execute(
        "DELETE FROM notes_fts WHERE rowid=(SELECT rowid FROM notes WHERE id=?1)",
        [id],
    )?;
    conn.execute(
        "INSERT INTO notes_fts(rowid,title,content_text)
         SELECT rowid,coalesce(title,''),coalesce(json_extract(content,'$.plain'),'')
         FROM notes WHERE id=?1",
        [id],
    )?;
    Ok(())
}

pub fn create(
    conn: &Connection,
    id: &str,
    content: &str,
    color: &str,
) -> rusqlite::Result<NoteRow> {
    let content =
        crate::content_identity::reconcile_identities(r#"{"blocks":[],"plain":""}"#, content)
            .map_err(|error| rusqlite::Error::ToSqlConversionFailure(Box::new(error)))?;
    let ts = now();
    let tx = conn.unchecked_transaction()?;
    tx.execute("INSERT INTO notes(id,title,content,color,created_at,modified_at) VALUES (?1,?2,?3,?4,?5,?5)", params![id, title_from_plain(&content_to_plain(&content)), content, color, ts])?;
    sync_note_fts(&tx, id)?;
    reminders::reconcile_note_in_transaction(&tx, id, &content)?;
    let note = get(&tx, id)?.ok_or(rusqlite::Error::QueryReturnedNoRows)?;
    tx.commit()?;
    Ok(note)
}

pub fn list(
    conn: &Connection,
    include_archived: bool,
    deleted: bool,
    search: Option<&str>,
) -> rusqlite::Result<Vec<NoteRow>> {
    let search = search.map(str::trim).filter(|s| !s.is_empty());
    let mut sql = SELECT.to_string();
    sql.push_str(if deleted {
        " WHERE deleted_at IS NOT NULL"
    } else {
        " WHERE deleted_at IS NULL"
    });
    if !include_archived {
        sql.push_str(" AND is_archived=0");
    }
    if search.is_some() {
        sql.push_str(" AND rowid IN (SELECT rowid FROM notes_fts WHERE notes_fts MATCH ?)");
    }
    sql.push_str(" ORDER BY is_pinned DESC, modified_at DESC");
    let mut stmt = match conn.prepare(&sql) {
        Ok(s) => s,
        Err(_) if search.is_some() => return Ok(vec![]),
        Err(e) => return Err(e),
    };
    let params: Vec<&dyn rusqlite::ToSql> =
        search.iter().map(|s| s as &dyn rusqlite::ToSql).collect();
    let result = match stmt.query_map(params.as_slice(), row) {
        Ok(rows) => rows.collect::<Result<Vec<_>, _>>().or_else(|e| {
            if search.is_some() {
                Ok(vec![])
            } else {
                Err(e)
            }
        }),
        Err(_) if search.is_some() => Ok(vec![]),
        Err(e) => Err(e),
    };
    result
}

pub fn get(conn: &Connection, id: &str) -> rusqlite::Result<Option<NoteRow>> {
    let mut stmt = conn.prepare(&format!("{SELECT} WHERE id=?1"))?;
    let mut rows = stmt.query_map([id], row)?;
    rows.next().transpose()
}

#[cfg(test)]
pub fn update_content(conn: &Connection, id: &str, content: &str) -> rusqlite::Result<()> {
    save_content_and_color(conn, id, content, None)
}

/// Save an editor snapshot. Color is part of the same user action as the
/// content: a close immediately after selecting a color must not lose it.
pub fn save_content_and_color(
    conn: &Connection,
    id: &str,
    content: &str,
    color: Option<&str>,
) -> rusqlite::Result<()> {
    let tx = conn.unchecked_transaction()?;
    save_content_in_transaction(&tx, id, content, color)?;
    tx.commit()
}

pub fn save_content_in_transaction(
    conn: &Connection,
    id: &str,
    content: &str,
    color: Option<&str>,
) -> rusqlite::Result<()> {
    for image in crate::inline_images::content_ids(content) {
        let owned: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM attachments WHERE id=?1 AND note_id=?2)",
            rusqlite::params![image, id],
            |r| r.get(0),
        )?;
        if !owned {
            return Err(rusqlite::Error::InvalidParameterName(
                "Image is not owned by this note".into(),
            ));
        }
    }
    let previous = get(conn, id)?.ok_or(rusqlite::Error::QueryReturnedNoRows)?;
    // A hidden editor can outlive a board action. Never let an autosave for a
    // deleted note recreate its reminder projection (or otherwise resurrect
    // state that the user has sent to Deleted).
    if previous.deleted_at.is_some() {
        return Err(rusqlite::Error::QueryReturnedNoRows);
    }
    let content = crate::content_identity::reconcile_identities(&previous.content, content)
        .map_err(|error| rusqlite::Error::ToSqlConversionFailure(Box::new(error)))?;
    // Opening an empty note re-saves it. That isn't an edit, so it mustn't count
    // as one when the note is later closed still empty.
    let still_empty = content_to_plain(&previous.content).trim().is_empty()
        && content_to_plain(&content).trim().is_empty()
        && color.is_none_or(|color| color == previous.color)
        && crate::inline_images::content_ids(&content).is_empty()
        && reminders::extract_date_chips(&content).is_empty();
    let modified_at = if still_empty {
        previous.modified_at
    } else {
        now()
    };
    let n = conn.execute(
        "UPDATE notes SET content=?2,title=?3,color=coalesce(?4,color),modified_at=?5 WHERE id=?1 AND deleted_at IS NULL",
        params![
            id,
            content,
            title_from_plain(&content_to_plain(&content)),
            color,
            modified_at
        ],
    )?;
    if n == 0 {
        return Err(rusqlite::Error::QueryReturnedNoRows);
    }
    sync_note_fts(conn, id)?;
    reminders::reconcile_note_in_transaction(conn, id, &content)?;
    Ok(())
}

pub fn update_meta(
    conn: &Connection,
    id: &str,
    color: Option<&str>,
    pinned: Option<bool>,
    archived: Option<bool>,
) -> rusqlite::Result<()> {
    let n = conn.execute("UPDATE notes SET color=coalesce(?2,color),is_pinned=coalesce(?3,is_pinned),is_archived=coalesce(?4,is_archived),modified_at=?5 WHERE id=?1 AND deleted_at IS NULL", params![id,color,pinned.map(i64::from),archived.map(i64::from),now()])?;
    if n == 0 {
        return Err(rusqlite::Error::QueryReturnedNoRows);
    }
    Ok(())
}

pub fn soft_delete(conn: &Connection, id: &str) -> rusqlite::Result<()> {
    let tx = conn.unchecked_transaction()?;
    transition_delete(&tx, id, true)?;
    reminders::remove_for_note(&tx, id)?;
    tx.commit()
}
pub fn restore(conn: &Connection, id: &str) -> rusqlite::Result<()> {
    let tx = conn.unchecked_transaction()?;
    transition_delete(&tx, id, false)?;
    let content = get(&tx, id)?
        .ok_or(rusqlite::Error::QueryReturnedNoRows)?
        .content;
    reminders::reconcile_note_in_transaction(&tx, id, &content)?;
    tx.commit()
}
fn transition_delete(conn: &Connection, id: &str, delete: bool) -> rusqlite::Result<()> {
    let n = if delete {
        conn.execute(
            "UPDATE notes SET deleted_at=?2,modified_at=?2 WHERE id=?1 AND deleted_at IS NULL",
            params![id, now()],
        )?
    } else {
        conn.execute(
            "UPDATE notes SET deleted_at=NULL,modified_at=?2 WHERE id=?1 AND deleted_at IS NOT NULL",
            params![id, now()],
        )?
    };
    if n == 0 {
        Err(rusqlite::Error::QueryReturnedNoRows)
    } else {
        Ok(())
    }
}
/// Tidies a note closed with nothing in it. One that was never edited simply
/// goes away; one the user cleared moves to Deleted so it can be recovered.
pub fn discard_if_blank(conn: &Connection, id: &str) -> rusqlite::Result<bool> {
    let Some(note) = get(conn, id)? else {
        return Ok(false);
    };
    if note.deleted_at.is_some() {
        return Ok(false);
    }
    let has_images: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM attachments WHERE note_id=?1)",
        [id],
        |r| r.get(0),
    )?;
    // Content this app didn't write is never treated as blank.
    let plain = serde_json::from_str::<serde_json::Value>(&note.content)
        .ok()
        .and_then(|value| value.get("plain")?.as_str().map(str::to_owned));
    let blank = plain.is_some_and(|plain| plain.trim().is_empty())
        && note.title.unwrap_or_default().trim().is_empty()
        && !has_images
        && reminders::extract_date_chips(&note.content).is_empty();
    if !blank {
        return Ok(false);
    }
    soft_delete(conn, id)?;
    if note.modified_at == note.created_at {
        purge(conn, id)?;
    }
    Ok(true)
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn insert_restored(
    conn: &Connection,
    id: &str,
    content: &str,
    color: &str,
    pinned: bool,
    archived: bool,
    deleted_at: Option<&str>,
    created_at: &str,
    modified_at: &str,
) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO notes(id,title,content,color,is_pinned,is_archived,deleted_at,created_at,modified_at,is_floating) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,0)",
        params![
            id,
            title_from_plain(&content_to_plain(content)),
            content,
            color,
            pinned,
            archived,
            deleted_at,
            created_at,
            modified_at,
        ],
    )?;
    sync_note_fts(conn, id)
}

pub fn purge(conn: &Connection, id: &str) -> rusqlite::Result<()> {
    // Permanent deletion is deliberately only legal from the Deleted view.
    // This prevents an accidental direct call from bypassing the soft-delete
    // recovery path.
    let tx = conn.unchecked_transaction()?;
    let rowid: Option<i64> = tx
        .query_row(
            "SELECT rowid FROM notes WHERE id=?1 AND deleted_at IS NOT NULL",
            [id],
            |row| row.get(0),
        )
        .optional()?;
    let Some(rowid) = rowid else {
        return Err(rusqlite::Error::QueryReturnedNoRows);
    };
    tx.execute("DELETE FROM notes_fts WHERE rowid=?1", [rowid])?;
    let n = tx.execute(
        "DELETE FROM notes WHERE id=?1 AND deleted_at IS NOT NULL",
        [id],
    )?;
    debug_assert_eq!(n, 1);
    tx.commit()
}
pub fn save_window_geom(
    conn: &Connection,
    id: &str,
    x: f64,
    y: f64,
    w: f64,
    h: f64,
    floating: bool,
) -> rusqlite::Result<()> {
    let n=conn.execute("UPDATE notes SET window_x=?2,window_y=?3,window_w=?4,window_h=?5,is_floating=?6 WHERE id=?1 AND deleted_at IS NULL",params![id,x,y,w,h,i64::from(floating)])?;
    if n == 0 {
        Err(rusqlite::Error::QueryReturnedNoRows)
    } else {
        Ok(())
    }
}

pub fn set_always_on_top(conn: &Connection, id: &str, value: bool) -> rusqlite::Result<()> {
    let n = conn.execute(
        "UPDATE notes SET is_always_on_top=?2,modified_at=?3 WHERE id=?1 AND deleted_at IS NULL",
        params![id, i64::from(value), now()],
    )?;
    if n == 0 {
        Err(rusqlite::Error::QueryReturnedNoRows)
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Db;
    fn db() -> Db {
        let p = std::env::temp_dir().join(format!("slint-notes-test-{}", uuid::Uuid::new_v4()));
        Db::open_in_dir(&p).unwrap()
    }
    fn content(text: &str) -> String {
        serde_json::json!({"blocks":[{"type":"text","runs":[{"t":text}]}],"plain":text}).to_string()
    }
    #[test]
    fn crud_fts_and_metadata_roundtrip() {
        let d = db();
        create(&d.conn, "n1", &content("buy milk"), "#FFF9B1").unwrap();
        assert_eq!(list(&d.conn, false, false, Some("milk")).unwrap().len(), 1);
        update_content(&d.conn, "n1", &content("fresh title")).unwrap();
        update_meta(&d.conn, "n1", None, Some(true), None).unwrap();
        assert!(get(&d.conn, "n1").unwrap().unwrap().is_pinned);
        soft_delete(&d.conn, "n1").unwrap();
        assert!(list(&d.conn, false, false, None).unwrap().is_empty());
        restore(&d.conn, "n1").unwrap();
        // Purge is intentionally guarded: a restored note remains recoverable
        // until it is explicitly sent back to Deleted.
        assert!(purge(&d.conn, "n1").is_err());
        soft_delete(&d.conn, "n1").unwrap();
        purge(&d.conn, "n1").unwrap();
        assert!(get(&d.conn, "n1").unwrap().is_none());
    }

    #[test]
    fn archived_notes_keep_due_reminders_while_deleted_notes_do_not() {
        let d = db();
        let content = serde_json::json!({
            "plain": "Pay bill",
            "blocks": [{"type":"text", "runs":[{
                "type":"dateChip", "attrs":{
                    "chipId":"bill", "date":"2026-09-26", "label":"Pay bill"
                }
            }]}]
        })
        .to_string();
        create(&d.conn, "n1", &content, "#FFF9B1").unwrap();
        let due = chrono::NaiveDate::from_ymd_opt(2026, 9, 26).unwrap();

        update_meta(&d.conn, "n1", None, None, Some(true)).unwrap();
        assert!(get(&d.conn, "n1").unwrap().unwrap().is_archived);
        assert_eq!(reminders::due_on(&d.conn, due).unwrap().len(), 1);

        soft_delete(&d.conn, "n1").unwrap();
        assert!(reminders::due_on(&d.conn, due).unwrap().is_empty());
        assert_eq!(list(&d.conn, true, true, None).unwrap().len(), 1);

        restore(&d.conn, "n1").unwrap();
        assert_eq!(reminders::due_on(&d.conn, due).unwrap().len(), 1);
        soft_delete(&d.conn, "n1").unwrap();
        purge(&d.conn, "n1").unwrap();
        assert!(get(&d.conn, "n1").unwrap().is_none());
    }

    #[test]
    fn window_geometry_roundtrips_with_floating_state() {
        let d = db();
        create(&d.conn, "n1", &content("window"), "#FFF9B1").unwrap();
        save_window_geom(&d.conn, "n1", -120.0, 240.0, 420.0, 520.0, true).unwrap();
        let note = get(&d.conn, "n1").unwrap().unwrap();
        assert_eq!(note.window, Some((-120.0, 240.0, 420.0, 520.0)));
        assert!(note.is_floating);
    }

    #[test]
    fn only_notes_with_nothing_in_them_are_discarded() {
        let db = crate::db::Db::open_in_dir(
            &std::env::temp_dir().join(format!("sticky-blank-{}", uuid::Uuid::new_v4())),
        )
        .unwrap();
        let conn = &db.conn;
        let empty = r#"{"blocks":[],"plain":"  "}"#;
        create(conn, "blank", empty, "#FFF9B1").unwrap();
        create(conn, "text", r#"{"blocks":[],"plain":"hi"}"#, "#FFF9B1").unwrap();
        create(
            conn,
            "dated",
            r#"{"blocks":[{"type":"text","runs":[{"type":"dateChip","attrs":{"chipId":"c","date":"2030-01-01"}}]}],"plain":""}"#,
            "#FFF9B1",
        )
        .unwrap();
        create(conn, "picture", empty, "#FFF9B1").unwrap();
        conn.execute(
            "INSERT INTO attachments(id,note_id,name,mime,bytes,width,height) VALUES ('a','picture','a','image/png',1,1,1)",
            [],
        )
        .unwrap();
        create(conn, "foreign", r#"{"blocks":[]}"#, "#FFF9B1").unwrap();
        create(
            conn,
            "cleared",
            r#"{"blocks":[],"plain":"was here"}"#,
            "#FFF9B1",
        )
        .unwrap();
        conn.execute(
            "UPDATE notes SET modified_at='later' WHERE id='cleared'",
            [],
        )
        .unwrap();
        save_content_and_color(conn, "cleared", empty, None).unwrap();

        // Opening an empty note re-saves it; that alone isn't an edit.
        save_content_and_color(conn, "blank", empty, Some("#FFF9B1")).unwrap();
        assert!(discard_if_blank(conn, "blank").unwrap());
        for kept in ["text", "dated", "picture", "foreign"] {
            assert!(!discard_if_blank(conn, kept).unwrap(), "{kept}");
        }
        assert!(!discard_if_blank(conn, "missing").unwrap());
        assert!(discard_if_blank(conn, "cleared").unwrap());
        assert_eq!(list(conn, true, false, None).unwrap().len(), 4);
        let deleted = list(conn, true, true, None).unwrap();
        assert_eq!(deleted.len(), 1);
        assert_eq!(deleted[0].id, "cleared");
        assert!(get(conn, "blank").unwrap().is_none());
    }

    #[test]
    fn create_then_list_and_open_roundtrips() {
        let d = db();
        create(&d.conn, "new-note", &content("new note"), "#FFF9B1").unwrap();

        let visible = list(&d.conn, true, false, None).unwrap();
        assert_eq!(visible.len(), 1);
        assert_eq!(visible[0].id, "new-note");

        let opened = get(&d.conn, "new-note").unwrap().unwrap();
        assert_eq!(content_to_plain(&opened.content), "new note");
    }

    #[test]
    fn always_on_top_roundtrips() {
        let d = db();
        create(&d.conn, "n1", &content("window"), "#FFF9B1").unwrap();
        assert!(!get(&d.conn, "n1").unwrap().unwrap().is_always_on_top);
        set_always_on_top(&d.conn, "n1", true).unwrap();
        assert!(get(&d.conn, "n1").unwrap().unwrap().is_always_on_top);
        set_always_on_top(&d.conn, "n1", false).unwrap();
        assert!(!get(&d.conn, "n1").unwrap().unwrap().is_always_on_top);
    }

    #[test]
    fn editor_save_persists_content_and_color_together() {
        let d = db();
        create(&d.conn, "n1", &content("before"), "#FFF9B1").unwrap();
        save_content_and_color(&d.conn, "n1", &content("after"), Some("#DCE8FF")).unwrap();
        let note = get(&d.conn, "n1").unwrap().unwrap();
        assert_eq!(content_to_plain(&note.content), "after");
        assert_eq!(note.color, "#DCE8FF");
    }

    #[test]
    fn create_rolls_back_note_fts_and_reminders_when_projection_fails() {
        let d = db();
        d.conn
            .execute_batch(
                "CREATE TRIGGER fail_reminder_insert BEFORE INSERT ON reminders
                 BEGIN SELECT RAISE(ABORT, 'injected reminder failure'); END;",
            )
            .unwrap();

        let pending = serde_json::json!({
            "plain": "new searchable note",
            "blocks": [{"type":"text", "runs":[{"type":"dateChip", "attrs":{
                "chipId":"due", "date":"2026-10-01"
            }}]}]
        })
        .to_string();
        let result = create(&d.conn, "n1", &pending, "#DCE8FF");
        assert!(result.is_err());
        assert!(get(&d.conn, "n1").unwrap().is_none());
        assert!(list(&d.conn, true, false, Some("searchable"))
            .unwrap()
            .is_empty());
        assert!(reminders::list_for_note(&d.conn, "n1").unwrap().is_empty());
    }

    #[test]
    fn editor_save_rolls_back_content_color_fts_and_reminders_on_failure() {
        let d = db();
        let original = serde_json::json!({
            "plain": "old searchable text",
            "blocks": [{"type":"text", "runs":[{"type":"dateChip", "attrs":{
                "chipId":"old", "date":"2026-10-01"
            }}]}]
        })
        .to_string();
        create(&d.conn, "n1", &original, "#FFF9B1").unwrap();
        d.conn
            .execute_batch(
                "CREATE TRIGGER fail_new_reminder BEFORE INSERT ON reminders
                 WHEN NEW.note_id = 'n1' AND NEW.chip_id = 'new'
                 BEGIN SELECT RAISE(ABORT, 'injected reminder failure'); END;",
            )
            .unwrap();
        let changed = serde_json::json!({
            "plain": "new searchable text",
            "blocks": [{"type":"text", "runs":[{"type":"dateChip", "attrs":{
                "chipId":"new", "date":"2026-10-02"
            }}]}]
        })
        .to_string();

        assert!(save_content_and_color(&d.conn, "n1", &changed, Some("#DCE8FF")).is_err());
        let note = get(&d.conn, "n1").unwrap().unwrap();
        assert_eq!(note.content, original);
        assert_eq!(note.color, "#FFF9B1");
        assert_eq!(list(&d.conn, true, false, Some("old")).unwrap().len(), 1);
        assert!(list(&d.conn, true, false, Some("new")).unwrap().is_empty());
        let reminders = reminders::list_for_note(&d.conn, "n1").unwrap();
        assert_eq!(reminders.len(), 1);
        assert_eq!(reminders[0].chip_id, "old");
    }

    #[test]
    fn delete_and_restore_roll_back_when_reminder_projection_fails() {
        let d = db();
        let dated = serde_json::json!({
            "plain": "scheduled",
            "blocks": [{"type":"text", "runs":[{"type":"dateChip", "attrs":{
                "chipId":"due", "date":"2026-10-01"
            }}]}]
        })
        .to_string();
        create(&d.conn, "n1", &dated, "#FFF9B1").unwrap();
        d.conn
            .execute_batch(
                "CREATE TRIGGER fail_reminder_delete BEFORE DELETE ON reminders
                 WHEN OLD.note_id = 'n1'
                 BEGIN SELECT RAISE(ABORT, 'injected reminder failure'); END;",
            )
            .unwrap();
        assert!(soft_delete(&d.conn, "n1").is_err());
        assert!(get(&d.conn, "n1").unwrap().unwrap().deleted_at.is_none());
        assert_eq!(reminders::list_for_note(&d.conn, "n1").unwrap().len(), 1);
        d.conn
            .execute_batch("DROP TRIGGER fail_reminder_delete;")
            .unwrap();

        soft_delete(&d.conn, "n1").unwrap();
        d.conn
            .execute_batch(
                "CREATE TRIGGER fail_reminder_restore BEFORE INSERT ON reminders
                 WHEN NEW.note_id = 'n1'
                 BEGIN SELECT RAISE(ABORT, 'injected reminder failure'); END;",
            )
            .unwrap();
        assert!(restore(&d.conn, "n1").is_err());
        assert!(get(&d.conn, "n1").unwrap().unwrap().deleted_at.is_some());
        assert!(reminders::list_for_note(&d.conn, "n1").unwrap().is_empty());
    }

    #[test]
    fn saving_a_deleted_note_is_rejected_without_changing_it() {
        let d = db();
        create(&d.conn, "n1", &content("before"), "#FFF9B1").unwrap();
        soft_delete(&d.conn, "n1").unwrap();
        assert!(save_content_and_color(&d.conn, "n1", &content("after"), Some("#DCE8FF")).is_err());
        let note = get(&d.conn, "n1").unwrap().unwrap();
        assert_eq!(content_to_plain(&note.content), "before");
        assert_eq!(note.color, "#FFF9B1");
        assert!(reminders::list_for_note(&d.conn, "n1").unwrap().is_empty());
    }

    #[test]
    fn moving_date_line_preserves_dismissed_reminder_after_editor_save() {
        let d = db();
        let chip = |id| {
            serde_json::json!({"type":"dateChip", "attrs":{
                "chipId":id, "date":"2026-10-01", "label":"Oct 1"
            }})
        };
        let original = serde_json::json!({"plain":"Pay bill Oct 1", "blocks":[
            {"type":"text", "runs":[{"t":"Pay bill "},chip("slint-date-line-0-0")]}
        ]})
        .to_string();
        create(&d.conn, "identity-note", &original, "#FFF9B1").unwrap();
        let before = reminders::list_for_note(&d.conn, "identity-note")
            .unwrap()
            .remove(0);
        assert!(!before.chip_id.starts_with("slint-date-line"));
        reminders::set_status(&d.conn, &before.id, "dismissed").unwrap();
        let moved = serde_json::json!({"plain":"New heading\nPay bill Oct 1", "blocks":[
            {"type":"text", "runs":[{"t":"New heading"}]},
            {"type":"text", "runs":[{"t":"Pay bill "},chip("slint-date-line-1-0")]}
        ]})
        .to_string();
        save_content_and_color(&d.conn, "identity-note", &moved, None).unwrap();
        let after = reminders::list_for_note(&d.conn, "identity-note").unwrap();
        assert_eq!(after.len(), 1);
        assert_eq!(after[0].id, before.id);
        assert_eq!(after[0].status, "dismissed");
    }
}
