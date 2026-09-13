//! Notes repository: CRUD over the `notes` table (final-form schema from
//! migration 001), FTS search, soft delete, window geometry.
//! All functions take `&Connection` so they're testable without the app.

use rusqlite::{params, Connection};
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
  pub created_at: String,
  pub modified_at: String,
}

/// Extracts the top-level `plain` field of a content JSON string ("" on error).
pub fn content_to_plain(content_json: &str) -> String {
  let Ok(v) = serde_json::from_str::<serde_json::Value>(content_json) else {
    return String::new();
  };
  v.get("plain")
    .and_then(|p| p.as_str())
    .unwrap_or("")
    .to_string()
}

/// First non-empty line of `plain`, truncated to 60 chars (card title).
fn title_from_plain(plain: &str) -> Option<String> {
  let t = plain
    .lines()
    .find(|l| !l.trim().is_empty())?
    .trim()
    .to_string();
  if t.is_empty() {
    None
  } else {
    Some(t.chars().take(60).collect())
  }
}

fn now() -> String {
  chrono::Utc::now().to_rfc3339()
}

const COLS: &str = "SELECT n.id, n.title, n.content, n.color, n.is_pinned, n.is_archived,
       n.deleted_at, n.window_x, n.window_y, n.window_w, n.window_h, n.is_floating,
       n.created_at, n.modified_at FROM notes n";

fn row_from(r: &rusqlite::Row) -> rusqlite::Result<NoteRow> {
  // SELECT column order: 0 id, 1 title, 2 content, 3 color, 4 is_pinned,
  // 5 is_archived, 6 deleted_at, 7-10 window_x/y/w/h, 11 is_floating,
  // 12 created_at, 13 modified_at
  let (wx, wy, ww, wh) = (
    r.get::<_, Option<f64>>(7)?,
    r.get::<_, Option<f64>>(8)?,
    r.get::<_, Option<f64>>(9)?,
    r.get::<_, Option<f64>>(10)?,
  );
  Ok(NoteRow {
    id: r.get(0)?,
    title: r.get(1)?,
    content: r.get(2)?,
    color: r.get(3)?,
    is_pinned: r.get::<_, i64>(4)? != 0,
    is_archived: r.get::<_, i64>(5)? != 0,
    deleted_at: r.get(6)?,
    window: match (wx, wy, ww, wh) {
      (Some(x), Some(y), Some(w), Some(h)) => Some((x, y, w, h)),
      _ => None,
    },
    is_floating: r.get::<_, i64>(11)? != 0,
    created_at: r.get(12)?,
    modified_at: r.get(13)?,
  })
}

/// Rebuilds the FTS index from `notes` (plain DELETE + INSERT — the FTS5
/// 'delete' special command is broken in the bundled SQLite build).
/// O(n) over all notes; at notes-app scale this is sub-millisecond.
fn sync_fts(conn: &Connection) -> rusqlite::Result<()> {
  conn.execute("DELETE FROM notes_fts", [])?;
  conn.execute(
    "INSERT INTO notes_fts(rowid, title, content_text)
     SELECT rowid, coalesce(title, ''), coalesce(json_extract(content, '$.plain'), '')
     FROM notes",
    [],
  )?;
  Ok(())
}

pub fn create(
  conn: &Connection,
  id: &str,
  content_json: &str,
  color: &str,
) -> rusqlite::Result<NoteRow> {
  let ts = now();
  let plain = content_to_plain(content_json);
  let title = title_from_plain(&plain);
  conn.execute(
    "INSERT INTO notes (id, title, content, color, created_at, modified_at)
     VALUES (?1, ?2, ?3, ?4, ?5, ?5)",
    params![id, title, content_json, color, ts],
  )?;
  sync_fts(conn)?;
  match get(conn, id)? {
    Some(row) => Ok(row),
    // unreachable: we just inserted; defensive only
    None => Err(rusqlite::Error::QueryReturnedNoRows),
  }
}

/// Lists notes. `deleted=false` → live notes; `deleted=true` → soft-deleted.
/// `search` filters via `notes_fts` MATCH; a malformed FTS query yields an
/// empty list (never an error).
pub fn list(
  conn: &Connection,
  include_archived: bool,
  deleted: bool,
  search: Option<&str>,
) -> rusqlite::Result<Vec<NoteRow>> {
  let mut sql = String::from(COLS);
  // Join on rowid so the FTS match is correlated to the note row (a bare
  // `WHERE notes_fts MATCH ?` without the join would be an uncorrelated
  // cross-reference).
  if search.is_some() {
    sql.push_str(" JOIN notes_fts f ON f.rowid = n.rowid");
  }
  sql.push_str(" WHERE ");
  sql.push_str(if deleted {
    "deleted_at IS NOT NULL"
  } else {
    "deleted_at IS NULL"
  });
  if !include_archived {
    sql.push_str(" AND is_archived = 0");
  }
  let search = search.map(str::trim).filter(|q| !q.is_empty());
  if search.is_some() {
    sql.push_str(" AND notes_fts MATCH ?");
  }
  sql.push_str(" ORDER BY is_pinned DESC, modified_at DESC");
  // `&[&dyn ToSql]` is the rusqlite `Params` form for a variable param count.
  let params: Vec<&dyn rusqlite::ToSql> = search
      .iter()
      .map(|s| &*s as &dyn rusqlite::ToSql)
      .collect();

  let mut prepared = match conn.prepare(&sql) {
    Ok(p) => p,
    // A malformed FTS query makes prepare fail; treat it as "no matches".
    Err(_) if search.is_some() => return Ok(Vec::new()),
    Err(e) => return Err(e),
  };
  let mapped = match prepared.query_map(params.as_slice(), row_from) {
    Ok(m) => m,
    Err(_) if search.is_some() => return Ok(Vec::new()),
    Err(e) => return Err(e),
  };
  match mapped.collect::<Result<Vec<_>, _>>() {
    Ok(rows) => Ok(rows),
    Err(_) if search.is_some() => Ok(Vec::new()),
    Err(e) => Err(e),
  }
}

pub fn get(conn: &Connection, id: &str) -> rusqlite::Result<Option<NoteRow>> {
  let mut stmt = conn.prepare(&format!("{COLS} WHERE n.id = ?1"))?;
  let mut rows = stmt.query_map(params![id], row_from)?;
  match rows.next() {
    Some(r) => Ok(Some(r?)),
    None => Ok(None),
  }
}

/// Replaces the content JSON, refreshes the derived title, bumps modified_at.
pub fn update_content(
  conn: &Connection,
  id: &str,
  content_json: &str,
) -> rusqlite::Result<()> {
  let plain = content_to_plain(content_json);
  let title = title_from_plain(&plain);
  let n = conn.execute(
    "UPDATE notes SET content = ?2, title = ?3, modified_at = ?4 WHERE id = ?1",
    params![id, content_json, title, now()],
  )?;
  if n == 0 {
    return Err(rusqlite::Error::QueryReturnedNoRows);
  }
  sync_fts(conn)?;
  Ok(())
}

pub fn update_meta(
  conn: &Connection,
  id: &str,
  color: Option<&str>,
  is_pinned: Option<bool>,
  is_archived: Option<bool>,
) -> rusqlite::Result<()> {
  let color = color
    .map(|c| c.to_string())
    .or_else(|| conn.query_row("SELECT color FROM notes WHERE id = ?1", params![id], |r| r.get(0)).ok());
  let pinned = is_pinned
    .map(|b| b as i64)
    .or_else(|| conn.query_row("SELECT is_pinned FROM notes WHERE id = ?1", params![id], |r| r.get(0)).ok());
  let archived = is_archived
    .map(|b| b as i64)
    .or_else(|| conn.query_row("SELECT is_archived FROM notes WHERE id = ?1", params![id], |r| r.get(0)).ok());
  let n = conn.execute(
    "UPDATE notes SET color = ?2, is_pinned = ?3, is_archived = ?4, modified_at = ?5
     WHERE id = ?1",
    params![id, color, pinned, archived, now()],
  )?;
  if n == 0 {
    return Err(rusqlite::Error::QueryReturnedNoRows);
  }
  Ok(())
}

pub fn soft_delete(conn: &Connection, id: &str) -> rusqlite::Result<()> {
  let n = conn.execute(
    "UPDATE notes SET deleted_at = ?2, modified_at = ?2 WHERE id = ?1",
    params![id, now()],
  )?;
  if n == 0 {
    return Err(rusqlite::Error::QueryReturnedNoRows);
  }
  Ok(())
}

pub fn restore(conn: &Connection, id: &str) -> rusqlite::Result<()> {
  let n = conn.execute(
    "UPDATE notes SET deleted_at = NULL, modified_at = ?2 WHERE id = ?1",
    params![id, now()],
  )?;
  if n == 0 {
    return Err(rusqlite::Error::QueryReturnedNoRows);
  }
  Ok(())
}

/// Permanently removes the row (and its FTS entry).
pub fn purge(conn: &Connection, id: &str) -> rusqlite::Result<()> {
  let n = conn.execute("DELETE FROM notes WHERE id = ?1", params![id])?;
  if n == 0 {
    return Err(rusqlite::Error::QueryReturnedNoRows);
  }
  sync_fts(conn)?;
  Ok(())
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
  let n = conn.execute(
    "UPDATE notes SET window_x = ?2, window_y = ?3, window_w = ?4, window_h = ?5,
     is_floating = ?6 WHERE id = ?1",
    params![id, x, y, w, h, floating as i64],
  )?;
  if n == 0 {
    return Err(rusqlite::Error::QueryReturnedNoRows);
  }
  Ok(())
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::db::Db;
  use std::sync::atomic::{AtomicUsize, Ordering};

  static COUNTER: AtomicUsize = AtomicUsize::new(0);

  fn test_db() -> Db {
    let d = std::env::temp_dir().join(format!(
      "sn-notes-test-{}-{}",
      std::process::id(),
      COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&d).unwrap();
    Db::open_in_dir(&d).unwrap()
  }

  fn content(s: &str) -> String {
    format!(
      r#"{{"blocks":[{{"type":"text","runs":[{{"t":"{s}"}}]}}],"plain":"{s}"}}"#
    )
  }

  #[test]
  fn create_and_get_roundtrip() {
    let db = test_db();
    create(&db.conn, "n1", &content("hello"), "#FFF9B1").unwrap();
    let row = get(&db.conn, "n1").unwrap().unwrap();
    assert_eq!(row.color, "#FFF9B1");
    assert!(row.content.contains("hello"));
    assert_eq!(row.modified_at, row.created_at);
  }

  #[test]
  fn soft_delete_hides_from_lists_but_appears_in_deleted() {
    let db = test_db();
    create(&db.conn, "n1", &content("a"), "#FFF9B1").unwrap();
    create(&db.conn, "n2", &content("b"), "#FFF9B1").unwrap();
    soft_delete(&db.conn, "n1").unwrap();
    let live = list(&db.conn, true, false, None).unwrap();
    assert_eq!(live.len(), 1);
    assert_eq!(live[0].id, "n2");
    let del = list(&db.conn, true, true, None).unwrap();
    assert_eq!(del.len(), 1);
    assert_eq!(del[0].id, "n1");
    restore(&db.conn, "n1").unwrap();
    assert_eq!(list(&db.conn, true, false, None).unwrap().len(), 2);
  }

  #[test]
  fn purge_removes_row_and_fts_entry() {
    let db = test_db();
    create(&db.conn, "n1", &content("zebra shopping list"), "#FFF9B1").unwrap();
    purge(&db.conn, "n1").unwrap();
    assert!(list(&db.conn, true, true, Some("zebra")).unwrap().is_empty());
  }

  #[test]
  fn search_matches_content_text() {
    let db = test_db();
    create(&db.conn, "n1", &content("buy milk and eggs"), "#FFF9B1").unwrap();
    create(&db.conn, "n2", &content("quarterly report draft"), "#FFF9B1").unwrap();
    let hits = list(&db.conn, true, false, Some("milk")).unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].id, "n1");
  }

  #[test]
  fn malformed_fts_query_returns_empty_not_error() {
    let db = test_db();
    create(&db.conn, "n1", &content("buy milk"), "#FFF9B1").unwrap();
    let hits = list(&db.conn, true, false, Some("milk (")).unwrap();
    assert!(hits.is_empty());
  }

  #[test]
  fn window_geometry_roundtrips() {
    let db = test_db();
    create(&db.conn, "n1", &content("x"), "#FFF9B1").unwrap();
    save_window_geom(&db.conn, "n1", 10.0, 20.0, 200.0, 260.0, true).unwrap();
    let row = get(&db.conn, "n1").unwrap().unwrap();
    assert_eq!(row.window, Some((10.0, 20.0, 200.0, 260.0)));
    assert!(row.is_floating);
  }

  #[test]
  fn update_content_rebuilds_title_and_bumps_modified() {
    let db = test_db();
    create(&db.conn, "n1", &content("old line"), "#FFF9B1").unwrap();
    let before = get(&db.conn, "n1").unwrap().unwrap().modified_at.clone();
    std::thread::sleep(std::time::Duration::from_millis(20));
    update_content(&db.conn, "n1", &content("fresh title line")).unwrap();
    let row = get(&db.conn, "n1").unwrap().unwrap();
    assert_eq!(row.title.as_deref(), Some("fresh title line"));
    assert!(row.modified_at > before);
  }

  #[test]
  fn update_meta_partial_keeps_other_fields() {
    let db = test_db();
    create(&db.conn, "n1", &content("x"), "#FFF9B1").unwrap();
    update_meta(&db.conn, "n1", None, Some(true), None).unwrap();
    let row = get(&db.conn, "n1").unwrap().unwrap();
    assert!(row.is_pinned);
    assert_eq!(row.color, "#FFF9B1");
    assert!(!row.is_archived);
  }
}