# Sticky Notes+ — Plan 1: Foundation (Phases 0–1)

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A running Windows app with a board window of note cards, per-note windows with text editing, SQLite persistence (WAL), and debounced autosave — plus the two de-risking spikes from the spec (§9).

**Architecture:** Tauri 2: a Rust core owns a single SQLite file (in the app-data dir) and exposes typed commands to a Svelte frontend. The board window lists notes as cards; each note opens in its own Tauri window (plain text only in this plan — rich text arrives in Plan 2). All note state lives in one `notes` table; content is stored as a JSON string with a shape the TipTap editor will consume in Plan 2 (so Plan 1's writes are forward-compatible).

**Tech Stack:** Rust 1.80+ / Tauri 2 / rusqlite (bundled SQLite) / Svelte 5 / Vite 6 / TypeScript (strict) / Vitest. Windows 11 target; app must run via `npm run tauri dev`.

**Spec:** `docs/superpowers/specs/2026-09-09-sticky-notes-design.md` (sections 2, 4, 5, 6-saves, 7, 9, 10)

## Global Constraints

- Windows 11, x64. WebView2 runtime is used as-is (OS-provided); never bundle a runtime.
- One SQLite file: `<app_data_dir>/sticky-notes.db`, opened in WAL mode. App-data dir comes from Tauri's path resolver (`app.path().app_data_dir()`), never hardcoded.
- Schema changes only via numbered migrations in `src-tauri/src/db/migrations.rs`; `meta.schema_version` records the applied count. Never edit an applied migration.
- Note content is stored as a JSON string in the exact shape from spec §5 (`{"blocks":[...]}`), even though Plan 1 uses only one text block — Plan 2's editor reads/writes the same shape.
- Saves: debounced autosave (~1 s after typing pauses) plus immediate save on window blur and window close. SQLite writes go through transactions (rusqlite defaults to one per statement here; use explicit `transaction()` when multi-statement).
- Naming: Rust `snake_case`, TS `camelCase`, Tauri command names `snake_case` (e.g. `save_note`).
- Every Rust unit of logic (db, service) is testable without a running Tauri app: tests open a temp-file database.
- Commit after every task (and after each TDD step pair where the plan says so).

---

### Task 1: Project scaffold (git, Tauri 2 + Svelte 5 + Vite)

**Files:**
- Create: repo root (git init), `src-tauri/` (Tauri scaffold), `src/` (frontend), `package.json`, `vite.config.ts`, `svelte.config.js`, `tsconfig.json`, `src-tauri/Cargo.toml`, `src-tauri/tauri.conf.json`, `src/main.ts`, `src/App.svelte`
- Create: `docs/spikes/` (empty dir for spike write-ups)

**Interfaces:**
- Produces: a `tauri dev` app showing an empty board window titled "Sticky Notes+". Command `get_app_info` (Rust, returns `{ version: String, db_path: String }`) proving the Rust↔frontend bridge works.

- [ ] **Step 1: Initialize git and install prerequisites**

```bash
cd C:/Users/Ahmad/Documents/Projects/sticky-notes
git init
git add docs && git commit -m "docs: approved design spec"
```

Prereqs the engineer must have (install if missing): Rust stable via rustup, Node 20+, Visual Studio Build Tools (C++ workload), WebView2 (present on Win11). Verify: `cargo --version`, `node --version`, `npx tauri --version` (npx fetches it; or `cargo install tauri-cli` — prefer `cargo install tauri-cli --locked`).

- [ ] **Step 2: Scaffold Tauri + Svelte**

```bash
npm create tauri-app@latest . -- --template svelte-ts --manager npm
```

(Answers: package name `sticky-notes`, app title `Sticky Notes+`.) This generates `src-tauri/` and a Svelte+TS frontend. If it refuses a non-empty dir, scaffold into a temp dir and copy the generated files in, preserving our `docs/`.

Pin in `package.json` (verify resolutions after scaffold; adjust to the latest published minor if the pins below are unpublished): `tauri` CLI dep `@tauri-apps/cli@^2`, `@tauri-apps/api@^2`, `svelte@^5`, `vite@^6`, `vitest@^3`, `typescript@^5`. In `src-tauri/Cargo.toml`: `tauri = { version = "2", features = ["devtools"] }`, `rusqlite = { version = "0.32", features = ["bundled"] }`, `serde`, `serde_json`, `thiserror`, `anyhow` (last three `= "1"/"1.0"` as applicable).

- [ ] **Step 3: Window config in `src-tauri/tauri.conf.json`**

Set (under `"tauri" > "windows"` / `"app"` per Tauri 2 schema):

```json
"windows": [
  {
    "label": "board",
    "title": "Sticky Notes+",
    "width": 1100,
    "height": 750,
    "center": true
  }
]
```

- [ ] **Step 4: Minimal Rust command + frontend proof**

`src-tauri/src/lib.rs`:

```rust
mod db; // added in Task 4; for now create an empty file so the build passes

#[tauri::command]
fn get_app_info(app: tauri::AppHandle) -> Result<serde_json::Value, String> {
    let data_dir = app.path().app_data_dir().map_err(|e| e.to_string())?;
    Ok(serde_json::json!({
        "version": env!("CARGO_PKG_VERSION"),
        "app_data_dir": data_dir.to_string_lossy(),
    }))
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .invoke_handler(tauri::generate_handler![get_app_info])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
```

`src/App.svelte` (board window content for now — replaced in Task 7):

```svelte
<script lang="ts">
  import { invoke } from '@tauri-apps/api/core';
  import { onMount } from 'svelte';
  let info: { version: string; app_data_dir: string } | null = null;
  onMount(async () => { info = await invoke('get_app_info'); });
</script>
{#if info}
  <h1>Sticky Notes+ {info.version}</h1>
  <p>data dir: {info.app_data_dir}</p>
{:else}
  <p>loading…</p>
{/if}
```

- [ ] **Step 5: Verify**

Run: `npm run tauri dev`
Expected: window titled "Sticky Notes+" shows the version and the Windows app-data path (e.g. `C:\Users\Ahmad\AppData\Roaming\com.sticky-notes.app`). Kill the app.

- [ ] **Step 6: Tooling + commit**

Add to `package.json` scripts: `"test": "vitest run"`, `"typecheck": "svelte-check"`. Add a root `.gitignore` (node_modules, target, dist, `.svelte-kit`). Verify `npm run typecheck` and `cargo test` (in `src-tauri/`) both pass.

```bash
git add -A
git commit -m "feat: scaffold Tauri 2 + Svelte 5 board shell with Rust command bridge"
```

---

### Task 2: Spike 1 — multi-window RAM probe (throwaway)

**Files:**
- Create: `src-tauri/src/probe.rs` (dev-gated), `docs/spikes/multi-window-ram.md`

**Interfaces:**
- Consumes: Task 1 app.
- Produces: a written verdict in `docs/spikes/multi-window-ram.md` of the form: "N=20 floating windows ⇒ total working set ≈ X MB (board+core Y MB, per-window overhead ≈ Z MB). Verdict: OK / need mitigation (pooling / board-only for inactive notes)." This verdict is an input to Plan 3 (floating windows).

- [ ] **Step 1: Write the probe**

`src-tauri/src/probe.rs` — compiles only under the `probe` cargo feature:

```rust
use tauri::{WebviewWindow, WebviewWindowBuilder, Manager};
use tauri::LogicalSize;

/// Spawns `n` 200x260 windows labeled "probe-0..n-1", each loading the app
/// (same as real note windows will). Dev-only: `cargo build --features probe`.
pub fn spawn(app: &tauri::AppHandle, n: u32) {
    for i in 0..n {
        WebviewWindowBuilder::new(app, &format!("probe-{i}"), tauri::WebviewUrl::App("index.html".into()))
            .decorations(false)
            .always_on_top(true)
            .inner_size(LogicalSize::new(200.0, 260.0))
            .build()
            .expect("probe window");
    }
}
```

Call it from `run()` behind `#[cfg(feature = "probe")]`: after the event loop is ready, `tauri::async_runtime::spawn(async move { tokio::time::sleep(Duration::from_secs(3)).await; probe::spawn(&handle, 20); })` (add `tokio` with the `time` feature — Tauri already pulls it in; `handle` is the `AppHandle` from `Builder`'s `setup` closure). Add `"probe"` to `Cargo.toml [features]` (empty feature, gating only).

- [ ] **Step 2: Run and measure**

Build & run with the feature: `cd src-tauri && cargo run --features probe` (the app binary runs the webview). When 20 probe windows appear, in PowerShell:

```powershell
Get-Process -Name sticky-notes,msedgewebview2 | Measure-Object -Property WorkingSet -Sum
```

Record: total, and `msedgewebview2` count. Repeat with 10 windows for a second data point. Test at 2 windows as the "normal use" baseline.

- [ ] **Step 3: Write the verdict**

`docs/spikes/multi-window-ram.md`: the numbers table, one paragraph of analysis (how much of the total is shared vs per-window), and the verdict line from the Interfaces block. Also record: does WebView2 share renderer processes across our windows (check `Get-Process msedgewebview2 | measure` — fewer processes than windows ⇒ shared)?

- [ ] **Step 4: Commit (probe stays in tree, feature-gated; it's the standing regression check Plan 3 re-runs)**

```bash
git add src-tauri/src/probe.rs src-tauri/src/lib.rs src-tauri/Cargo.toml docs/spikes/multi-window-ram.md
git commit -m "spike: multi-window WebView2 RAM probe + verdict"
```

---

### Task 3: Spike 2 — TipTap inline date chip probe (throwaway)

**Files:**
- Create: `src/probes/chip-probe.svelte` + `src/probes/chip-probe.ts`, `docs/spikes/tiptap-chip.md`

**Interfaces:**
- Consumes: Task 1 frontend.
- Produces: `docs/spikes/tiptap-chip.md` answering: (1) can a TipTap inline `Node` render a styled pill and carry a `chipId` attribute in its serialized JSON? (2) can clicking it open a popover anchored near the caret (and does the popover stay through reflow)? (3) exact JSON shape of a document containing text + chip (paste the actual `editor.getJSON()` output). Verdict: OK / problem + workaround. This document seeds Plan 2's chip design.

- [ ] **Step 1: Add deps**

```bash
npm i @tiptap/core @tiptap/starter-kit @tiptap/svelte
npm i -D @tiptap/pm
```

- [ ] **Step 2: Probe component**

`src/probes/chip-probe.ts`:

```ts
import { mergeAttributes, Node, NodeViewWrapper, type NodeConfig } from '@tiptap/core';
export const DateChip = Node.create<NodeConfig & { chipId: string }>({
  name: 'dateChip',
  group: 'inline',
  inline: true,
  atom: true,          // caret treats it as one unit
  draggable: true,
  addAttributes() {
    return { chipId: { default: 'chip-probe-1' } };
  },
  parseHTML() { return [{ tag: 'span[data-datechip]' }]; },
  renderHTML({ node, HTMLAttributes }) {
    return ['span', mergeAttributes(HTMLAttributes, { 'data-datechip': '' }), '📅 chip'];
  },
  addNodeView() {
    return NodeViewWrapper; // minimal: the plain renderHTML output already shows the pill;
    // the probe only needs: renders as inline pill ✓, JSON carries chipId ✓, click event fires ✓
  },
});
```

(Note: with Svelte, the idiomatic node view is a Svelte component passed to `@tiptap/svelte`'s renderer — if `NodeViewWrapper` is awkward, the probe may just use `renderHTML` for visuals and listen for `click` on `span[data-datechip]` via a document listener; the question being answered is JSON shape + click routing, not the final renderer choice. Record whichever route worked in the write-up.)

`src/probes/chip-probe.svelte`:

```svelte
<script lang="ts">
  import { onMount } from 'svelte';
  import { Editor } from '@tiptap/svelte';
  import StarterKit from '@tiptap/starter-kit';
  import { DateChip } from './chip-probe';
  import { createEditor } from '@tiptap/core';

  let editor: ReturnType<typeof createEditor>;
  let jsonOut = '';
  onMount(() => {
    editor = createEditor({
      extensions: [StarterKit, DateChip],
      content: '<p>Finish the report by </p>',
    });
    // insert a chip after the text
    editor.chain().insertContentAt(editor.state.doc.content.size, { type: 'dateChip', attributes: { chipId: 'chip-probe-1' } }).run();
    setTimeout(() => { jsonOut = JSON.stringify(editor.getJSON(), null, 2); }, 500);
  });
  document.addEventListener('click', (e) => {
    if ((e.target as HTMLElement).closest?.('[data-datechip]')) console.log('CHIP CLICKED — popover can anchor here');
  });
</script>
<Editor editor={editor} class="chip-probe-editor" />
<pre>{jsonOut}</pre>
```

- [ ] **Step 3: Run and record**

Temporarily render `<ChipProbe />` from `App.svelte` (delete after spike). Run `npm run tauri dev`. Verify in devtools: the pill renders inline with the text; `jsonOut` shows a `dateChip` node with `attributes.chipId`; clicking logs "CHIP CLICKED". Paste the actual JSON into the write-up.

- [ ] **Step 4: Write verdict + commit**

`docs/spikes/tiptap-chip.md`: the 3 answers + pasted JSON + verdict line + "route that worked". Remove `<ChipProbe/>` from `App.svelte` (keep the probe files for reference).

```bash
git add -A && git commit -m "spike: TipTap inline date-chip probe + verdict"
```

---

### Task 4: Rust DB layer — open, WAL, migrations

**Files:**
- Create: `src-tauri/src/db/mod.rs`, `src-tauri/src/db/migrations.rs`
- Modify: `src-tauri/src/lib.rs` (wire `DbState` into `.manage()`)

**Interfaces:**
- Produces:
  - `pub struct Db { conn: rusqlite::Connection }` with `Db::open_in_dir(dir: &Path) -> anyhow::Result<Db>` (creates the file, sets WAL, runs migrations, stores `schema_version` in `meta`).
  - `pub struct DbState(pub std::sync::Mutex<Db>)` — the Tauri-managed state every command takes `tauri::State<'_, DbState>`.
  - Migration 001 creates (only what Phases 0–1 need, in the spec §5 shapes): `meta(key TEXT PRIMARY KEY, value TEXT)`, `notes`, `notes_fts` (FTS5, external content over `notes`), plus triggers to keep the FTS index in sync. `date_chips`, `checklist_items`, `attachments`, `calendar_connections` tables are created in later plans' migrations — **but** the `notes` table is created now in its final form (all spec §5 columns) so later plans never alter it.

- [ ] **Step 1: Write the failing tests**

`src-tauri/src/db/migrations.rs` includes the migration list; test in `src-tauri/src/db/mod.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_dir() -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("sn-test-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn open_creates_wal_and_migrates_to_current_version() {
        let dir = tmp_dir(); let _ = std::fs::remove_dir_all(&dir); std::fs::create_dir_all(&dir).unwrap();
        let db = Db::open_in_dir(&dir).unwrap();
        let mode: String = db.conn.query_row("PRAGMA journal_mode", [], |r| r.get(0)).unwrap();
        assert_eq!(mode, "wal");
        let version: i64 = db.conn.query_row(
            "SELECT value FROM meta WHERE key='schema_version'", [], |r| r.get(0)).unwrap();
        assert_eq!(version as i64, migrations::CURRENT);
    }

    #[test]
    fn reopening_is_idempotent() {
        let dir = tmp_dir(); let _ = std::fs::remove_dir_all(&dir); std::fs::create_dir_all(&dir).unwrap();
        Db::open_in_dir(&dir).unwrap();
        let db = Db::open_in_dir(&dir).unwrap(); // second open must not re-apply
        let version: i64 = db.conn.query_row(
            "SELECT value FROM meta WHERE key='schema_version'", [], |r| r.get(0)).unwrap();
        assert_eq!(version as i64, migrations::CURRENT);
    }

    #[test]
    fn notes_table_has_all_spec_columns() {
        let db = test_db(); // helper: open_in_dir on a fresh temp dir
        let cols: Vec<String> = db.conn.prepare("PRAGMA table_info(notes)")
            .unwrap().query_map([], |r| r.get::<_, String>(1))
            .unwrap().collect::<Result<_, _>>().unwrap();
        for c in ["id","title","content","color","is_pinned","is_archived","deleted_at",
                  "window_x","window_y","window_w","window_h","is_floating","created_at","modified_at"] {
            assert!(cols.contains(&c.to_string()), "missing column {c}");
        }
    }
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cd src-tauri && cargo test db::`
Expected: compile errors (`Db`/`migrations` undefined).

- [ ] **Step 3: Implement**

`src-tauri/src/db/migrations.rs` — `pub const CURRENT: i64 = 1;` and `pub fn apply(conn: &rusqlite::Connection) -> rusqlite::Result<i64>` applying, in order and only-if-missing, numbered `&str` scripts. Migration 001:

```sql
CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
CREATE TABLE notes (
  id TEXT PRIMARY KEY,
  title TEXT,
  content TEXT NOT NULL,               -- JSON string, spec §5 shape
  color TEXT NOT NULL DEFAULT '#FFF9B1',
  is_pinned INTEGER NOT NULL DEFAULT 0,
  is_archived INTEGER NOT NULL DEFAULT 0,
  deleted_at TEXT,                      -- ISO-8601 UTC; NULL = not deleted
  window_x REAL, window_y REAL, window_w REAL, window_h REAL,
  is_floating INTEGER NOT NULL DEFAULT 0,
  created_at TEXT NOT NULL,
  modified_at TEXT NOT NULL
);
CREATE VIRTUAL TABLE notes_fts USING fts5(title, content_text, content='notes', content_rowid='rowid');
-- keep FTS in sync:
CREATE TRIGGER notes_ai AFTER INSERT ON notes BEGIN
  INSERT INTO notes_fts(rowid, title, content_text)
  VALUES (new.rowid, coalesce(new.title,''), json_extract(new.content, '$.plain'));
END;
CREATE TRIGGER notes_ad AFTER DELETE ON notes BEGIN
  INSERT INTO notes_fts(notes_fts, rowid, title, content_text)
  VALUES ('delete', old.rowid, coalesce(old.title,''), json_extract(old.content, '$.plain'));
END;
CREATE TRIGGER notes_au AFTER UPDATE OF content, title ON notes BEGIN
  INSERT INTO notes_fts(notes_fts, rowid, title, content_text)
  VALUES ('delete', old.rowid, coalesce(old.title,''), json_extract(old.content, '$.plain'));
  INSERT INTO notes_fts(rowid, title, content_text)
  VALUES (new.rowid, coalesce(new.title,''), json_extract(new.content, '$.plain'));
END;
```

(`content` JSON therefore always includes a top-level `plain` string mirror of the visible text — cheap to build, keeps FTS simple. The content shape from spec §5 gains one field: `{"blocks": [...], "plain": "..."}`.)

`src-tauri/src/db/mod.rs`:

```rust
pub mod migrations;
use std::path::{Path, PathBuf};

pub struct Db { pub conn: rusqlite::Connection }

impl Db {
    pub fn open_in_dir(dir: &Path) -> anyhow::Result<Self> {
        let file = dir.join("sticky-notes.db");
        let conn = rusqlite::Connection::open(&file)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        let applied = migrations::apply(&conn)?;
        conn.execute(
            "INSERT INTO meta(key, value) VALUES ('schema_version', ?1)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            [applied.to_string()],
        )?;
        Ok(Db { conn })
    }
}
```

`migrations::apply` reads the current `meta.schema_version` (0 if missing), applies each numbered migration above it in order inside one transaction, returns the new count.

Wire into `src-tauri/src/lib.rs`:

```rust
use std::sync::Mutex;
struct DbState(Mutex<Db>);
// in run(): let data_dir = app.path().app_data_dir()?; std::fs::create_dir_all(&data_dir)?;
//   app.manage(DbState(Mutex::new(Db::open_in_dir(&data_dir)?)));
```

(Tauri 2: manage state in the `Builder::setup` closure with `app.handle()`, or via `tauri::Manager` — use the pattern the generated scaffold supports.)

- [ ] **Step 4: Run tests to verify pass**

Run: `cd src-tauri && cargo test db::`
Expected: 3 tests PASS.

- [ ] **Step 5: Commit**

```bash
git add src-tauri && git commit -m "feat(db): SQLite open (WAL) + migration runner + notes/FTS schema"
```

---

### Task 5: Rust notes repository — CRUD + search

**Files:**
- Create: `src-tauri/src/db/notes.rs`, `src-tauri/src/service/notes_service.rs`, `src-tauri/src/service/mod.rs`
- Modify: `src-tauri/src/db/mod.rs` (`pub mod notes;`)

**Interfaces:**
- Produces (repository, `src-tauri/src/db/notes.rs`, all `rusqlite::Result`-returning, taking `&Connection`):
  - `create(conn, id, content_json, color) -> Result<NoteRow>`
  - `list(conn, include_archived: bool, deleted: bool, search: Option<&str>) -> Result<Vec<NoteRow>>` (ordered `is_pinned DESC, modified_at DESC`; `search` uses `notes_fts` MATCH; non-deleted means `deleted_at IS NULL`; `deleted=true` lists the soft-deleted set)
  - `get(conn, id) -> Result<Option<NoteRow>>`
  - `update_content(conn, id, content_json) -> Result<()>` (bumps `modified_at`)
  - `update_meta(conn, id, color, is_pinned, is_archived) -> Result<()>`
  - `soft_delete(conn, id) -> Result<()>` (sets `deleted_at = now`)
  - `restore(conn, id) / purge(conn, id) -> Result<()>`
  - `save_window_geom(conn, id, x, y, w, h, floating: bool) -> Result<()>`
  - `pub struct NoteRow { pub id: String, pub title: Option<String>, pub content: String, pub color: String, pub is_pinned: bool, pub is_archived: bool, pub deleted_at: Option<String>, pub window: Option<(f64,f64,f64,f64)>, pub is_floating: bool, pub created_at: String, pub modified_at: String }` (implements `Serialize`)
  - `pub fn content_to_plain(content_json: &str) -> String` — extracts the top-level `plain` field (serde_json), fallback `""`.
  - Timestamps: RFC 3339 UTC strings (`chrono::Utc::now().to_rfc3339()` — add `chrono` dep, feature `clock`).
- Produces (service, thin wrapper over `Mutex<Db>` for the Tauri commands): `NotesService::new(DbState)`, methods `create_note(), list_notes(ListFilter), save_note(id, content_json), update_note_meta(...), soft_delete(id), restore(id), purge(id), search_notes(query)`.

- [ ] **Step 1: Write the failing tests** (in `src-tauri/src/db/notes.rs` bottom, `#[cfg(test)]`)

```rust
#[test]
fn create_and_get_roundtrip() {
    let db = test_db();
    let now = chrono::Utc::now().to_rfc3339();
    notes::create(&db.conn, "n1",
        r#"{"blocks":[{"type":"text","runs":[{"t":"hello"}]}],"plain":"hello"}"#, "#FFF9B1").unwrap();
    let row = notes::get(&db.conn, "n1").unwrap().unwrap();
    assert_eq!(row.color, "#FFF9B1");
    assert!(row.content.contains("hello"));
    assert_eq!(row.modified_at, row.created_at);
}

#[test]
fn soft_delete_hides_from_lists_but_appears_in_deleted() {
    let db = test_db();
    notes::create(&db.conn, "n1", content("a"), "#FFF9B1").unwrap();
    notes::create(&db.conn, "n2", content("b"), "#FFF9B1").unwrap();
    notes::soft_delete(&db.conn, "n1").unwrap();
    let live = notes::list(&db.conn, true, false, None).unwrap();
    assert_eq!(live.len(), 1); assert_eq!(live[0].id, "n2");
    let del = notes::list(&db.conn, true, true, None).unwrap();
    assert_eq!(del.len(), 1); assert_eq!(del[0].id, "n1");
    notes::restore(&db.conn, "n1").unwrap();
    assert_eq!(notes::list(&db.conn, true, false, None).unwrap().len(), 2);
}

#[test]
fn purge_removes_row_and_fts_entry() {
    let db = test_db();
    notes::create(&db.conn, "n1", content("zebra shopping list"), "#FFF9B1").unwrap();
    notes::purge(&db.conn, "n1").unwrap();
    assert!(notes::list(&db.conn, true, true, Some("zebra")).unwrap().is_empty());
}

#[test]
fn search_matches_content_text() {
    let db = test_db();
    notes::create(&db.conn, "n1", content("buy milk and eggs"), "#FFF9B1").unwrap();
    notes::create(&db.conn, "n2", content("quarterly report draft"), "#FFF9B1").unwrap();
    let hits = notes::list(&db.conn, true, false, Some("milk")).unwrap();
    assert_eq!(hits.len(), 1); assert_eq!(hits[0].id, "n1");
}

#[test]
fn window_geometry_roundtrips() {
    let db = test_db();
    notes::create(&db.conn, "n1", content("x"), "#FFF9B1").unwrap();
    notes::save_window_geom(&db.conn, "n1", 10.0, 20.0, 200.0, 260.0, true).unwrap();
    let row = notes::get(&db.conn, "n1").unwrap().unwrap();
    assert_eq!(row.window, Some((10.0, 20.0, 200.0, 260.0)));
    assert!(row.is_floating);
}
// helpers: fn test_db() -> Db (fresh temp dir, like Task 4); fn content(s) -> String (builds the JSON with plain=s)
```

- [ ] **Step 2: Run to verify failure**

Run: `cd src-tauri && cargo test db::notes`
Expected: FAIL (module missing).

- [ ] **Step 3: Implement** `notes.rs` per the Interfaces block. `list` with `Some(search)` builds:

```sql
SELECT n.* FROM notes n
JOIN notes_fts f ON f.rowid = n.rowid
WHERE notes_fts MATCH ?1 AND n.deleted_at IS NULL
ORDER BY n.is_pinned DESC, n.modified_at DESC
```

(bind the raw query string through `MATCH` parameterization — never interpolate user text; if the FTS query syntax errors on weird input, wrap in `match ?1` using a quoted-safe form: prefix each whitespace-separated token with nothing special, but catch `SqliteError` from malformed FTS syntax and return an empty list rather than erroring — assert that in a test: `search_notes("milk (")` returns `Ok(vec![])`, not `Err`). Search without a match clause omits the JOIN.

- [ ] **Step 4: Service + tests**

`src-tauri/src/service/notes_service.rs` — `NotesService` holding `StdMutexGuard` access to `DbState`; each method locks, calls the repo, unlocks (guard dropped before returning; add `#[must_use]`-style care: no locks held across `await` — there is no async here, so plain `lock().unwrap()` with a `poison → panic` policy is acceptable and documented). Unit tests: one per method, reusing `test_db` (inject a `Db` directly — design `NotesService::from_db(Db)` for tests, `new(DbState)` for the app).

- [ ] **Step 5: Run all tests**

Run: `cd src-tauri && cargo test`
Expected: all PASS (db + service).

- [ ] **Step 6: Commit**

```bash
git add src-tauri && git commit -m "feat(db): notes repository (CRUD, soft delete, FTS search, window geom) + service"
```

---

### Task 6: Tauri commands + frontend board (cards, create, search, archive, soft delete)

**Files:**
- Create: `src-tauri/src/commands.rs`
- Create: `src/lib/api.ts`, `src/lib/types.ts`, `src/windows/board/Board.svelte`, `src/windows/board/NoteCard.svelte`
- Modify: `src-tauri/src/lib.rs` (register commands), `src/App.svelte` (route by window label: `board` → `Board.svelte`)

**Interfaces:**
- Produces (commands, all `#[tauri::command]`, `Result<_, String>`; `NoteDto = serde(rename NoteRow)`):
  - `create_note() -> NoteDto` (uuid v4 id, default content `{"blocks":[{"type":"text","runs":[]}],"plain":""}`, default color; cycles the 6 pastel colors by count)
  - `list_notes(include_archived: bool, deleted: bool, search: Option<String>) -> Vec<NoteDto>`
  - `save_note(id: String, content: String) -> ()` (validates `content` parses as JSON with a top-level `plain` string; else Err)
  - `update_note_meta(id: String, color: Option<String>, is_pinned: Option<bool>, is_archived: Option<bool>) -> ()`
  - `soft_delete(id) / restore_note(id) / purge_note(id) -> ()`
  - `save_window_geom(id, x, y, w, h, floating) -> ()`
  - `get_note(id) -> Option<NoteDto>`
  - `open_note_window(id) -> ()` — Rust-side: creates (or focuses) a Tauri window labeled `note-{id}`, 260×260 logical, `decorations: false`, `always_on_top: true`, at saved `window_*` if present else centered. Uses `WebviewWindowBuilder` + `WebviewUrl::App("index.html?id=note-{id}"... )` — Tauri 2 passes the label, so the frontend reads `getCurrentWindow().label` instead (see below); window URL is plain `index.html`.
- Frontend: `src/lib/types.ts` exports `Note` mirroring `NoteDto`. `src/lib/api.ts` wraps `invoke` with typed signatures.

- [ ] **Step 1: Write the failing frontend test** (Vitest, `src/lib/api.test.ts` — mock `@tauri-apps/api/core.invoke` with `vi.mock`)

```ts
import { describe, it, expect, vi, beforeEach } from 'vitest';
vi.mock('@tauri-apps/api/core', () => ({ invoke: vi.fn() }));
import { invoke } from '@tauri-apps/api/core';
import { createNote, listNotes } from './api';

const invokeMock = vi.mocked(invoke);
beforeEach(() => invokeMock.mockReset());

describe('api', () => {
  it('createNote invokes create_note with no args', async () => {
    invokeMock.mockResolvedValue({ id: 'n1' });
    const n = await createNote();
    expect(invokeMock).toHaveBeenCalledWith('create_note');
    expect(n.id).toBe('n1');
  });
  it('listNotes maps filter args', async () => {
    invokeMock.mockResolvedValue([]);
    await listNotes({ search: 'milk' });
    expect(invokeMock).toHaveBeenCalledWith('list_notes', { includeArchived: false, deleted: false, search: 'milk' });
  });
});
```

(Run: `npm run test -- api` — expect FAIL: `./api` not found.)

- [ ] **Step 2: Implement `src/lib/types.ts` + `src/lib/api.ts`** per the command list (TS names camelCase; invoke args objects use the snake_case names Tauri expects by binding `rename_all` — use `#[tauri::command(rename_all = "camelCase")]` on Rust commands so JS sends camelCase, and update the test above to match: `{ includeArchived: ..., deleted: ..., search: ... }`).

- [ ] **Step 3: Implement the Rust commands** in `commands.rs` (thin: take `State<DbState>`, call `NotesService`, `map_err(|e| e.to_string())`). `open_note_window` per the Interfaces block (guard: window already exists → `set_focus()`). Register all in `generate_handler![]`. Register a `WindowEvent::CloseRequested` handler per window: if `label.starts_with("note-")` and the hide-to-tray setting is off (always off in this plan — setting lands in Plan 4's app-behavior phase; for now **note windows close normally**, the board window also closes normally), on close call `save_window_geom` with the window's current bounds and `is_floating = true`… — simplify: on close, `save_window_geom(id, x, y, w, h, true)` so re-open restores the spot; the board window ignores this.

- [ ] **Step 4: Board UI**

`src/App.svelte`: read `getCurrentWindow().label`; if `board` → `<Board/>`; if label starts `note-` → `<NoteWindow noteId={label[5..]}/>` (Task 7 component; `"note-".len() == 5`).

`Board.svelte`: header (title, search input bound to a `search` state with 250 ms debounce, "New note" button, archive toggle), CSS grid of `NoteCard` (grid-template-columns: repeat(auto-fill, minmax(220px, 1fr)); card = colored block, title/first lines of `plain` clamped to 5 lines, footer row: pin ⚲ / archive 🗄 / delete 🗑 buttons shown on hover). Card click → `openNoteWindow(id)` (invoke). New note → `createNote()` then refresh list (refetch pattern: after any mutation call `load()` which re-invokes `list_notes`).

`NoteCard.svelte` props: `{ note: Note, onPin, onArchive, onDelete }` (callbacks call the api then `load()`).

- [ ] **Step 5: Verify manually**

Run: `npm run tauri dev`. Check: New note appears as a pastel card; type search "x" filters live; pin/archive/delete buttons work; restart the app — notes persist from SQLite (the data dir printed in Task 1).

- [ ] **Step 6: Run tests + typecheck, commit**

Run: `npm run test` and `npm run typecheck` (both PASS).

```bash
git add -A && git commit -m "feat(board): note cards, create/search/pin/archive/soft-delete via Tauri commands"
```

---

### Task 7: Note window — text editing with debounce

**Files:**
- Create: `src/windows/note/NoteWindow.svelte`, `src/lib/useDebouncedSave.ts`, `src/lib/content.ts`
- Test: `src/lib/content.test.ts`, `src/lib/useDebouncedSave.test.ts`

**Interfaces:**
- `src/lib/content.ts`: `emptyContent(): string` (the Task 4 JSON shape, empty text), `setContentText(contentJson: string, text: string): string` (replace first text block's runs with a single plain run `{t: text}`, rebuild `plain`), `textOf(contentJson: string): string` (concatenate run `t` fields of text blocks), `buildPlain(contentJson: string): string`.
- `useDebouncedSave(getValue: () => string, save: (v: string) => Promise<void>, ms = 1000): { schedule: () => void, flush: () => Promise<void> }` — `schedule()` arms a `ms` timer (resetting any pending one) that calls `save(getValue())`; `flush()` cancels pending and calls `save(getValue())` now (awaited).
- Produces: `NoteWindow.svelte` — a textarea (Plan 1 is plain text; TipTap lands in Plan 2) with a 16 px drag strip at top (mousedown → `getCurrentWindow().startDragging()`), a small toolbar (close → `appWindow.close()`), and an autosave status dot (saving/idle).

- [ ] **Step 1: Failing tests**

`src/lib/content.test.ts`:

```ts
import { it, expect } from 'vitest';
import { emptyContent, setContentText, textOf } from './content';

it('emptyContent has the spec shape', () => {
  const c = JSON.parse(emptyContent());
  expect(c.blocks).toEqual([{ type: 'text', runs: [] }]);
  expect(c.plain).toBe('');
});
it('setContentText stores text and rebuilds plain', () => {
  const out = JSON.parse(setContentText(emptyContent(), 'buy milk\nand eggs'));
  expect(out.blocks[0].runs).toEqual([{ t: 'buy milk\nand eggs' }]);
  expect(out.plain).toBe('buy milk\nand eggs');
});
it('textOf round-trips', () => {
  expect(textOf(setContentText(emptyContent(), 'hi'))).toBe('hi');
});
```

`src/lib/useDebouncedSave.test.ts` (fake timers):

```ts
import { it, expect, vi } from 'vitest';

it('coalesces rapid saves into one, flush saves immediately', async () => {
  vi.useFakeTimers();
  const save = vi.fn(async () => {});
  const { schedule, flush } = useDebouncedSave(() => 'v', save, 1000);
  schedule(); await vi.advanceTimersByTimeAsync(400);
  schedule(); await vi.advanceTimersByTimeAsync(400);
  expect(save).not.toHaveBeenCalled();
  await vi.advanceTimersByTimeAsync(600);
  expect(save).toHaveBeenCalledTimes(1);
  schedule();
  await flush();
  expect(save).toHaveBeenCalledTimes(2);
  expect(vi.getTimerCount()).toBe(0);
});
```

`useDebouncedSave` signature accordingly: `(ms) => { schedule, flush }` where `schedule()` captures the latest getter value — final shape: `useDebouncedSave(getValue, save, ms)` returns `{ schedule: () => void, flush: () => Promise<void> }`; `schedule()` arms a 1 s timer that calls `save(getValue())`; `flush()` cancels pending and calls `save(getValue())` now.

- [ ] **Step 2: Run to verify failure** — `npm run test -- content useDebouncedSave` → FAIL (modules missing).

- [ ] **Step 3: Implement `content.ts` + `useDebouncedSave.ts`.** `buildPlain` = join of run texts with `\n` at block boundaries.

- [ ] **Step 4: `NoteWindow.svelte`**

Load: `getNote(noteId)` → textarea value = `textOf(content)`. On input: update local state + `schedule()`. `onBlur` of the textarea and window `blur` event → `await flush()`. Before close: the toolbar close button `await flush()` then `appWindow.close()`. Save status dot: green idle / amber while a save is pending or in flight; on error show a red dot + `console.error` (toast surfaces in Plan 4). Drag strip: `<div class="drag" onmousedown={() => appWindow.startDragging()}>`. Style: note `color` from the row as background, border-radius 4, subtle shadow, font `16px system-ui`.

- [ ] **Step 5: Verify manually**

`npm run tauri dev` → open a note from the board → window opens (at saved position after a move+close cycle) → drag the window, type, wait ~1 s → status dot settles green → kill the app with the task bar (no graceful exit) → relaunch, open the note → text intact except possibly the last <1 s of typing.

- [ ] **Step 6: Tests + typecheck, commit**

```bash
git add -A && git commit -m "feat(note): note window with text editing, debounced autosave, window dragging"
```

---

### Task 8: End-to-end verification + milestone closeout

**Files:**
- Modify: none expected (fix whatever verification uncovers); `docs/PLAN1-CHECKLIST.md` (create — the checklist below, checked as verified)

- [ ] **Step 1: Full test sweep**

Run: `cd src-tauri && cargo test && npm run test && npm run typecheck`
Expected: all green. Fix and commit anything red.

- [ ] **Step 2: Manual pass (record results in `docs/PLAN1-CHECKLIST.md`)**

- [ ] **Step 3: Kill -relaunch persistence check** (as in Task 7 step 5, including a note with a moved window position)
- [ ] **Step 4: 10-note load check** — create 10 notes, board scrolls, search "milk" instant
- [ ] **Step 5: Spike verdicts present** — both `docs/spikes/*.md` verdict lines filled in

```bash
git add -A && git commit -m "docs: plan 1 closeout checklist"
```

**Milestone definition of done:** the app runs 24/7-stable in dev, notes + positions + colors survive kill/relaunch, all tests green, both spike verdicts written. Plan 2 (rich text + checklist + chips) starts from here.