use rusqlite::Connection;

pub const CURRENT: i64 = 4;

const INITIAL: &str = include_str!("../../schema/001_initial.sql");

pub fn apply(conn: &Connection) -> rusqlite::Result<i64> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);",
    )?;
    let applied: i64 = conn
        .query_row(
            "SELECT value FROM meta WHERE key='schema_version'",
            [],
            |r| {
                let value: String = r.get(0)?;
                Ok(value.parse::<i64>().unwrap_or(0))
            },
        )
        .unwrap_or(0);
    if applied > CURRENT {
        return Err(rusqlite::Error::InvalidParameterName(format!(
            "db schema version {applied} is newer than this app supports ({CURRENT})"
        )));
    }
    let tx = conn.unchecked_transaction()?;

    // Always run the CREATE IF NOT EXISTS portion first. This repairs a DB
    // where meta.schema_version was recorded before initialization completed.
    tx.execute_batch(INITIAL)?;

    // ALTER TABLE has no IF NOT EXISTS form. Check the live schema instead of
    // trusting the version marker; this also makes upgrades from an older
    // Slint/Tauri-shaped database safe and idempotent.
    for (name, definition) in [
        ("title", "title TEXT"),
        ("content", "content TEXT NOT NULL DEFAULT ''"),
        ("color", "color TEXT NOT NULL DEFAULT '#FFF9B1'"),
        ("is_pinned", "is_pinned INTEGER NOT NULL DEFAULT 0"),
        ("is_archived", "is_archived INTEGER NOT NULL DEFAULT 0"),
        ("deleted_at", "deleted_at TEXT"),
        ("window_x", "window_x REAL"),
        ("window_y", "window_y REAL"),
        ("window_w", "window_w REAL"),
        ("window_h", "window_h REAL"),
        ("is_floating", "is_floating INTEGER NOT NULL DEFAULT 0"),
        ("created_at", "created_at TEXT NOT NULL DEFAULT ''"),
        ("modified_at", "modified_at TEXT NOT NULL DEFAULT ''"),
        (
            "is_always_on_top",
            "is_always_on_top INTEGER NOT NULL DEFAULT 0",
        ),
    ] {
        if !has_column(&tx, "notes", name)? {
            tx.execute(&format!("ALTER TABLE notes ADD COLUMN {definition}"), [])?;
        }
    }

    tx.execute_batch(
        "CREATE TABLE IF NOT EXISTS reminders (
                id TEXT PRIMARY KEY,
                note_id TEXT NOT NULL REFERENCES notes(id) ON DELETE CASCADE,
                chip_id TEXT NOT NULL,
                due_date TEXT NOT NULL,
                label TEXT NOT NULL,
                status TEXT NOT NULL DEFAULT 'pending',
                created_at TEXT NOT NULL,
                modified_at TEXT NOT NULL,
                UNIQUE(note_id, chip_id)
            );
            CREATE INDEX IF NOT EXISTS reminders_due_idx
                ON reminders(status, due_date, note_id, chip_id);",
    )?;
    for (name, definition) in [
        ("due_time", "due_time TEXT NOT NULL DEFAULT ''"),
        ("scheduled_date", "scheduled_date TEXT NOT NULL DEFAULT ''"),
        ("recurrence", "recurrence TEXT NOT NULL DEFAULT 'none'"),
        ("snoozed_until", "snoozed_until TEXT"),
        ("notified_for", "notified_for TEXT"),
    ] {
        if !has_column(&tx, "reminders", name)? {
            tx.execute(
                &format!("ALTER TABLE reminders ADD COLUMN {definition}"),
                [],
            )?;
        }
    }
    tx.execute(
        "UPDATE reminders SET scheduled_date=due_date WHERE scheduled_date=''",
        [],
    )?;
    tx.execute_batch(
        "CREATE INDEX IF NOT EXISTS reminders_schedule_idx
         ON reminders(status, scheduled_date, due_time, snoozed_until);",
    )?;
    tx.execute(
        "INSERT INTO meta(key,value) VALUES ('schema_version',?1) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
        [CURRENT.to_string()],
    )?;
    tx.commit()?;
    Ok(CURRENT)
}

fn has_column(
    conn: &rusqlite::Transaction<'_>,
    table: &str,
    column: &str,
) -> rusqlite::Result<bool> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let found = stmt
        .query_map([], |row| row.get::<_, String>(1))?
        .filter_map(Result::ok)
        .any(|name| name == column);
    Ok(found)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    fn columns(conn: &Connection) -> Vec<String> {
        conn.prepare("PRAGMA table_info(notes)")
            .unwrap()
            .query_map([], |row| row.get(1))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap()
    }

    #[test]
    fn existing_always_on_top_column_is_not_added_twice() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE notes (id TEXT PRIMARY KEY, is_always_on_top INTEGER NOT NULL DEFAULT 0);
             CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
             INSERT INTO meta VALUES ('schema_version', '1');",
        )
        .unwrap();

        assert_eq!(apply(&conn).unwrap(), CURRENT);
        assert_eq!(apply(&conn).unwrap(), CURRENT);
        assert_eq!(
            columns(&conn)
                .iter()
                .filter(|c| *c == "is_always_on_top")
                .count(),
            1
        );
    }

    #[test]
    fn partially_initialized_notes_table_is_repaired() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE notes (id TEXT PRIMARY KEY);
             CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
             INSERT INTO meta VALUES ('schema_version', '3');",
        )
        .unwrap();

        apply(&conn).unwrap();
        let cols = columns(&conn);
        for expected in ["content", "created_at", "modified_at", "is_always_on_top"] {
            assert!(cols.iter().any(|c| c == expected), "missing {expected}");
        }
        let reminders: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='reminders'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(reminders, 1);
        let mut stmt = conn.prepare("PRAGMA table_info(reminders)").unwrap();
        let reminder_columns: Vec<String> = stmt
            .query_map([], |row| row.get(1))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        for expected in [
            "due_time",
            "scheduled_date",
            "recurrence",
            "snoozed_until",
            "notified_for",
        ] {
            assert!(reminder_columns.iter().any(|column| column == expected));
        }
    }

    #[test]
    fn version_three_reminders_keep_their_status_and_receive_schedule_fields() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE meta (key TEXT PRIMARY KEY,value TEXT NOT NULL);
             INSERT INTO meta VALUES ('schema_version','3');
             CREATE TABLE notes (id TEXT PRIMARY KEY);
             INSERT INTO notes(id) VALUES ('n');
             CREATE TABLE reminders (
                id TEXT PRIMARY KEY,note_id TEXT NOT NULL,chip_id TEXT NOT NULL,
                due_date TEXT NOT NULL,label TEXT NOT NULL,status TEXT NOT NULL,
                created_at TEXT NOT NULL,modified_at TEXT NOT NULL,
                UNIQUE(note_id,chip_id));
             INSERT INTO reminders VALUES ('r','n','c','2026-09-28','Check','dismissed','a','b');",
        )
        .unwrap();
        assert_eq!(apply(&conn).unwrap(), CURRENT);
        let (date, scheduled, status): (String, String, String) = conn
            .query_row(
                "SELECT due_date,scheduled_date,status FROM reminders WHERE id='r'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(
            (date, scheduled, status),
            ("2026-09-28".into(), "2026-09-28".into(), "dismissed".into())
        );
        assert_eq!(apply(&conn).unwrap(), CURRENT);
    }
}
