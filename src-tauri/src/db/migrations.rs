//! Numbered SQL migrations. `CURRENT` is the highest version; `apply` runs
//! every migration newer than `meta.schema_version` in order, inside one
//! transaction. Never edit an applied migration — add the next number.

use rusqlite::Connection;

pub const CURRENT: i64 = 1;

/// Each entry is the full script for migration N (index + 1).
const MIGRATIONS: &[&str] = &[include_str!("migrations/001_initial.sql")];

/// Runs pending migrations; returns the schema version now in effect.
pub fn apply(conn: &Connection) -> rusqlite::Result<i64> {
  conn.execute_batch(
    "CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);",
  )?;
  let applied: i64 = conn
    .query_row("SELECT value FROM meta WHERE key='schema_version'", [], |r| {
      r.get(0)
    })
    .unwrap_or(0);
  if applied > CURRENT {
    return Err(rusqlite::Error::InvalidParameterName(format!(
      "db schema version {applied} is newer than this app supports ({CURRENT})"
    )));
  }

  let tx = conn.unchecked_transaction()?;
  let mut version = applied;
  for (i, script) in MIGRATIONS.iter().enumerate() {
    let v = i as i64 + 1;
    if v > applied {
      tx.execute_batch(script)?;
      version = v;
    }
  }
  tx.execute(
    "INSERT INTO meta(key, value) VALUES ('schema_version', ?1)
     ON CONFLICT(key) DO UPDATE SET value = excluded.value",
    [version.to_string()],
  )?;
  tx.commit()?;
  Ok(version)
}