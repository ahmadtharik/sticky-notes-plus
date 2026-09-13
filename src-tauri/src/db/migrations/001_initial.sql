-- Migration 001: initial schema (Phases 0-1 subset of spec §5).
-- `notes` is created in its FINAL form so later plans never alter it;
-- date_chips / checklist_items / attachments / calendar_connections come
-- in later migrations.

CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);

CREATE TABLE IF NOT EXISTS notes (
  id TEXT PRIMARY KEY,
  title TEXT,
  content TEXT NOT NULL,               -- JSON string, spec §5 shape (includes top-level "plain")
  color TEXT NOT NULL DEFAULT '#FFF9B1',
  is_pinned INTEGER NOT NULL DEFAULT 0,
  is_archived INTEGER NOT NULL DEFAULT 0,
  deleted_at TEXT,                      -- ISO-8601 UTC; NULL = not deleted
  window_x REAL, window_y REAL, window_w REAL, window_h REAL,
  is_floating INTEGER NOT NULL DEFAULT 0,
  created_at TEXT NOT NULL,
  modified_at TEXT NOT NULL
);

-- Plain (self-stored) FTS5 index. Kept in sync by Rust code (notes::sync_fts)
-- with plain DELETE/INSERT — the FTS5 'delete' special command is broken in
-- the bundled SQLite 3.46 build (returns "SQL logic error"), so no triggers
-- or special commands are used.
CREATE VIRTUAL TABLE IF NOT EXISTS notes_fts USING fts5(title, content_text);