//! Explicit, read-only legacy snapshot; destination writes are all-or-nothing.
use anyhow::{ensure, Context, Result};
use rusqlite::{params, Connection, OpenFlags};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

pub struct ImportPlan {
    rows: Vec<LegacyNote>,
    pub source: PathBuf,
}
struct LegacyNote {
    id: String,
    title: Option<String>,
    content: String,
    color: String,
    pinned: bool,
    archived: bool,
    deleted: Option<String>,
    created: String,
    modified: String,
}
impl ImportPlan {
    pub fn count(&self) -> usize {
        self.rows.len()
    }
    pub fn conflicts(&self, conn: &Connection) -> Result<usize> {
        let mut count = 0;
        for row in &self.rows {
            if conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM notes WHERE id=?1)",
                [&row.id],
                |r| r.get::<_, bool>(0),
            )? {
                count += 1;
            }
        }
        Ok(count)
    }
}

fn convert_run(run: &mut Value) -> Result<()> {
    if let Some(chip) = run.get("chip") {
        let due = chip
            .get("dueAt")
            .and_then(Value::as_str)
            .context("Legacy date is missing dueAt")?;
        let local = chrono::DateTime::parse_from_rfc3339(due)?.with_timezone(&chrono::Local);
        let repeat = chip.get("repeat").and_then(Value::as_str).unwrap_or("none");
        ensure!(
            matches!(repeat, "none" | "daily" | "weekly"),
            "Unknown legacy date recurrence"
        );
        *run = json!({"type":"dateChip", "attrs": {
            "chipId": chip.get("chipId").and_then(Value::as_str).context("Legacy date is missing its ID")?,
            "date":local.format("%Y-%m-%d").to_string(), "time":local.format("%H:%M").to_string(),
            "recurrence":repeat, "label":chip.get("label").and_then(Value::as_str).unwrap_or("")
        }});
    } else {
        ensure!(
            run.get("t").and_then(Value::as_str).is_some(),
            "Unsupported legacy text run"
        );
    }
    Ok(())
}

fn convert_content(raw: &str) -> Result<String> {
    ensure!(raw.len() <= 1024 * 1024, "Legacy note exceeds 1 MiB");
    let mut document: Value = serde_json::from_str(raw)?;
    ensure!(
        document.get("plain").and_then(Value::as_str).is_some(),
        "Legacy plain text is missing"
    );
    for block in document
        .get_mut("blocks")
        .and_then(Value::as_array_mut)
        .context("Legacy blocks are missing")?
    {
        match block.get("type").and_then(Value::as_str) {
            Some("text") => {
                for run in block
                    .get_mut("runs")
                    .and_then(Value::as_array_mut)
                    .context("Legacy text runs are missing")?
                {
                    convert_run(run)?;
                }
            }
            Some("checklist") => {
                for item in block
                    .get_mut("items")
                    .and_then(Value::as_array_mut)
                    .context("Legacy checklist items are missing")?
                {
                    ensure!(
                        item.get("checked").and_then(Value::as_bool).is_some(),
                        "Invalid checklist state"
                    );
                    if item.get("runs").is_none() {
                        let text = item
                            .get("text")
                            .and_then(Value::as_str)
                            .context("Checklist text is missing")?
                            .to_owned();
                        item["runs"] = json!([{ "t":text }]);
                    }
                    for run in item
                        .get_mut("runs")
                        .and_then(Value::as_array_mut)
                        .context("Invalid checklist runs")?
                    {
                        convert_run(run)?;
                    }
                }
            }
            _ => anyhow::bail!("Unsupported legacy block; import cancelled to avoid data loss"),
        }
    }
    Ok(crate::content_identity::reconcile_identities(
        r#"{"blocks":[],"plain":""}"#,
        &document.to_string(),
    )?)
}

pub fn preview(source: &Path, destination: &Connection) -> Result<ImportPlan> {
    let source = source.canonicalize()?;
    ensure!(
        std::fs::metadata(&source)?.is_file(),
        "Select a database file"
    );
    ensure!(
        std::fs::metadata(&source)?.len() <= 256 * 1024 * 1024,
        "Legacy database exceeds 256 MiB"
    );
    if let Some(path) = destination.path() {
        ensure!(
            source != Path::new(path).canonicalize()?,
            "Cannot import the current database"
        );
    }
    let conn = Connection::open_with_flags(
        &source,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    conn.pragma_update(None, "query_only", true)?;
    conn.execute_batch("BEGIN")?;
    let version: String = conn.query_row(
        "SELECT value FROM meta WHERE key='schema_version'",
        [],
        |r| r.get(0),
    )?;
    ensure!(
        version == "1",
        "Only the Tauri schema-v1 database is supported"
    );
    let count: i64 = conn.query_row("SELECT COUNT(*) FROM notes", [], |r| r.get(0))?;
    ensure!(count <= 10_000, "Legacy database exceeds 10,000 notes");
    let mut stmt = conn.prepare("SELECT id,title,content,color,is_pinned,is_archived,deleted_at,created_at,modified_at FROM notes")?;
    let rows = stmt.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, Option<String>>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, String>(3)?,
            r.get::<_, i64>(4)?,
            r.get::<_, i64>(5)?,
            r.get::<_, Option<String>>(6)?,
            r.get::<_, String>(7)?,
            r.get::<_, String>(8)?,
        ))
    })?;
    let mut converted = Vec::new();
    let mut total = 0;
    for row in rows {
        let (id, title, raw, color, pinned, archived, deleted, created, modified) = row?;
        ensure!(!id.is_empty() && id.len() <= 256, "Invalid legacy note ID");
        total += raw.len();
        ensure!(total <= 32 * 1024 * 1024, "Legacy content exceeds 32 MiB");
        ensure!(
            matches!(pinned, 0 | 1) && matches!(archived, 0 | 1),
            "Invalid legacy flags"
        );
        converted.push(LegacyNote {
            id,
            title,
            content: convert_content(&raw)?,
            color,
            pinned: pinned != 0,
            archived: archived != 0,
            deleted,
            created,
            modified,
        });
    }
    Ok(ImportPlan {
        rows: converted,
        source,
    })
}

pub fn apply(conn: &Connection, plan: &ImportPlan) -> Result<(usize, PathBuf)> {
    let path = Path::new(conn.path().context("Import needs a saved destination")?);
    let backup = path
        .parent()
        .context("Database directory unavailable")?
        .join(format!("before-import-{}.db", uuid::Uuid::new_v4()));
    // VACUUM INTO creates a consistent SQLite snapshot, including committed WAL.
    // Never copy only the main file of a live WAL database.
    conn.execute("VACUUM INTO ?1", [backup.to_string_lossy().as_ref()])?;
    let tx = conn.unchecked_transaction()?;
    let mut imported = 0;
    for row in &plan.rows {
        if tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM notes WHERE id=?1)",
            [&row.id],
            |r| r.get::<_, bool>(0),
        )? {
            continue;
        }
        tx.execute("INSERT INTO notes(id,title,content,color,is_pinned,is_archived,deleted_at,created_at,modified_at,is_floating) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,0)", params![row.id,row.title,row.content,row.color,row.pinned,row.archived,row.deleted,row.created,row.modified])?;
        tx.execute("INSERT INTO notes_fts(rowid,title,content_text) SELECT rowid,coalesce(title,''),json_extract(content,'$.plain') FROM notes WHERE id=?1", [&row.id])?;
        if row.deleted.is_none() {
            super::reminders::reconcile_note_in_transaction(&tx, &row.id, &row.content)?;
        }
        imported += 1;
    }
    tx.commit()?;
    Ok((imported, backup))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{notes, Db};
    fn setup() -> (PathBuf, Db, Connection) {
        let root = std::env::temp_dir().join(format!("sticky-import-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let destination = Db::open_in_dir(&root.join("destination")).unwrap();
        let source = Connection::open(root.join("legacy.db")).unwrap();
        source
            .execute_batch(include_str!(
                "../../../src-tauri/src/db/migrations/001_initial.sql"
            ))
            .unwrap();
        source
            .execute_batch("INSERT INTO meta VALUES ('schema_version','1');")
            .unwrap();
        (root, destination, source)
    }
    fn add(source: &Connection, id: &str, content: &str) {
        source.execute("INSERT INTO notes(id,content,created_at,modified_at) VALUES (?1,?2,'created','modified')", params![id,content]).unwrap();
    }
    const TEXT: &str =
        r#"{"blocks":[{"type":"text","runs":[{"t":"hello 😀","bold":true}]}],"plain":"hello 😀"}"#;
    #[test]
    fn preview_is_read_only_and_apply_preserves_existing_notes_with_backup() {
        let (root, destination, source) = setup();
        add(&source, "new", TEXT);
        add(&source, "existing", TEXT);
        notes::create(
            &destination.conn,
            "existing",
            r#"{"blocks":[],"plain":"untouched"}"#,
            "#D9F7BE",
        )
        .unwrap();
        let before = std::fs::read(root.join("legacy.db")).unwrap();
        let plan = preview(&root.join("legacy.db"), &destination.conn).unwrap();
        assert_eq!(plan.count(), 2);
        assert_eq!(plan.conflicts(&destination.conn).unwrap(), 1);
        assert!(notes::get(&destination.conn, "new").unwrap().is_none());
        let (count, backup) = apply(&destination.conn, &plan).unwrap();
        assert_eq!(count, 1);
        assert_eq!(
            notes::get(&destination.conn, "existing")
                .unwrap()
                .unwrap()
                .color,
            "#D9F7BE"
        );
        assert!(
            !notes::get(&destination.conn, "new")
                .unwrap()
                .unwrap()
                .is_floating
        );
        let saved = Connection::open_with_flags(backup, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
        assert!(notes::get(&saved, "new").unwrap().is_none());
        assert_eq!(std::fs::read(root.join("legacy.db")).unwrap(), before);
        assert_eq!(apply(&destination.conn, &plan).unwrap().0, 0);
    }
    #[test]
    fn failure_rolls_back_every_imported_row_and_fts_entry() {
        let (root, destination, source) = setup();
        add(&source, "a", TEXT);
        add(&source, "b", TEXT);
        let plan = preview(&root.join("legacy.db"), &destination.conn).unwrap();
        destination.conn.execute_batch("CREATE TRIGGER fail_import BEFORE INSERT ON notes WHEN NEW.id='b' BEGIN SELECT RAISE(ABORT,'injected'); END;").unwrap();
        assert!(apply(&destination.conn, &plan).is_err());
        assert_eq!(
            destination
                .conn
                .query_row("SELECT COUNT(*) FROM notes", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            0
        );
        assert_eq!(
            destination
                .conn
                .query_row("SELECT COUNT(*) FROM notes_fts", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            0
        );
    }
    #[test]
    fn rejects_current_database_and_unsupported_content_before_writes() {
        let (root, destination, source) = setup();
        assert!(preview(&root.join("destination/sticky-notes.db"), &destination.conn).is_err());
        add(
            &source,
            "a",
            r#"{"blocks":[{"type":"image","src":"unknown"}],"plain":""}"#,
        );
        assert!(preview(&root.join("legacy.db"), &destination.conn).is_err());
        assert!(notes::get(&destination.conn, "a").unwrap().is_none());
    }
    #[test]
    fn converts_legacy_dates_and_preserves_checklist_and_style() {
        let raw = r#"{"blocks":[{"type":"checklist","items":[{"id":"task","checked":true,"runs":[{"t":"done","italic":true},{"chip":{"chipId":"chip","dueAt":"2026-10-02T15:30:00Z","timezone":"UTC","repeat":"weekly","label":"Meeting"}}]}]}],"plain":"doneMeeting"}"#;
        let converted: Value = serde_json::from_str(&convert_content(raw).unwrap()).unwrap();
        let item = &converted["blocks"][0]["items"][0];
        assert_eq!(item["id"], "task");
        assert_eq!(item["checked"], true);
        assert_eq!(item["runs"][0]["italic"], true);
        assert_eq!(item["runs"][1]["attrs"]["chipId"], "chip");
        assert_eq!(item["runs"][1]["attrs"]["recurrence"], "weekly");
    }

    #[test]
    fn imports_archive_delete_state_and_rebuilds_only_live_reminders() {
        let (root, destination, source) = setup();
        let date = r#"{"blocks":[{"type":"text","runs":[{"chip":{"chipId":"same-chip","dueAt":"2026-10-02T15:30:00Z","repeat":"none","label":"Meeting"}}]}],"plain":"Meeting"}"#;
        for id in ["live", "archive", "deleted"] {
            add(&source, id, date);
        }
        source
            .execute("UPDATE notes SET is_archived=1 WHERE id='archive'", [])
            .unwrap();
        source
            .execute(
                "UPDATE notes SET deleted_at='deleted' WHERE id='deleted'",
                [],
            )
            .unwrap();
        let plan = preview(&root.join("legacy.db"), &destination.conn).unwrap();
        let (_, backup) = apply(&destination.conn, &plan).unwrap();
        assert!(
            notes::get(&destination.conn, "archive")
                .unwrap()
                .unwrap()
                .is_archived
        );
        assert!(notes::get(&destination.conn, "deleted")
            .unwrap()
            .unwrap()
            .deleted_at
            .is_some());
        assert_eq!(
            destination
                .conn
                .query_row("SELECT COUNT(*) FROM reminders", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            2
        );
        assert!(backup.exists());
    }

    #[test]
    fn reads_committed_source_wal_and_rejects_future_schema() {
        let (root, destination, source) = setup();
        source.pragma_update(None, "journal_mode", "WAL").unwrap();
        add(&source, "wal", TEXT);
        assert_eq!(
            preview(&root.join("legacy.db"), &destination.conn)
                .unwrap()
                .count(),
            1
        );
        source
            .execute("UPDATE meta SET value='99' WHERE key='schema_version'", [])
            .unwrap();
        assert!(preview(&root.join("legacy.db"), &destination.conn).is_err());
    }
}
