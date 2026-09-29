//! Thin service over the app's `Arc<Mutex<Db>>` for the Tauri command
//! layer. Lock policy: each call takes the lock, calls the repository, and
//! drops the guard before returning (no async here, so poisoning → panic
//! is acceptable and documented).

use std::sync::{Arc, Mutex};

use crate::db::{notes, Db};

#[derive(Clone)]
pub struct NotesService {
    db: Arc<Mutex<Db>>,
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ListFilter {
    pub include_archived: bool,
    pub deleted: bool,
    pub search: Option<String>,
}

impl NotesService {
    /// For the running app: cheap Arc clone of the state-managed `Db`.
    pub fn from_state(state: &Arc<Mutex<Db>>) -> Self {
        Self::from_db(state.clone())
    }

    /// For tests: wrap a fresh `Db` directly.
    pub fn from_db(db: Arc<Mutex<Db>>) -> Self {
        Self { db }
    }

    fn with_conn<T>(
        &self,
        f: impl FnOnce(&rusqlite::Connection) -> rusqlite::Result<T>,
    ) -> Result<T, String> {
        let guard = self.db.lock().map_err(|e| e.to_string())?;
        let conn = &guard.conn;
        f(conn).map_err(|e| e.to_string())
    }

    pub fn create_note(
        &self,
        id: &str,
        content_json: &str,
        color: &str,
    ) -> Result<notes::NoteRow, String> {
        self.with_conn(|c| notes::create(c, id, content_json, color))
    }

    pub fn list_notes(&self, filter: &ListFilter) -> Result<Vec<notes::NoteRow>, String> {
        self.with_conn(|c| {
            notes::list(
                c,
                filter.include_archived,
                filter.deleted,
                filter.search.as_deref(),
            )
        })
    }

    pub fn get_note(&self, id: &str) -> Result<Option<notes::NoteRow>, String> {
        self.with_conn(|c| notes::get(c, id))
    }

    pub fn save_note(&self, id: &str, content_json: &str) -> Result<(), String> {
        self.with_conn(|c| notes::update_content(c, id, content_json))
    }

    pub fn update_note_meta(
        &self,
        id: &str,
        color: Option<&str>,
        is_pinned: Option<bool>,
        is_archived: Option<bool>,
    ) -> Result<(), String> {
        self.with_conn(|c| notes::update_meta(c, id, color, is_pinned, is_archived))
    }

    pub fn soft_delete(&self, id: &str) -> Result<(), String> {
        self.with_conn(|c| notes::soft_delete(c, id))
    }

    pub fn restore(&self, id: &str) -> Result<(), String> {
        self.with_conn(|c| notes::restore(c, id))
    }

    pub fn purge(&self, id: &str) -> Result<(), String> {
        self.with_conn(|c| notes::purge(c, id))
    }

    pub fn save_window_geom(
        &self,
        id: &str,
        x: f64,
        y: f64,
        w: f64,
        h: f64,
        floating: bool,
    ) -> Result<(), String> {
        self.with_conn(|c| notes::save_window_geom(c, id, x, y, w, h, floating))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Db;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static COUNTER: AtomicUsize = AtomicUsize::new(0);

    /// One fresh DB per test (parallel tests must not share a file —
    // SQLite allows a single writer).
    fn service() -> NotesService {
        let d = std::env::temp_dir().join(format!(
            "sn-svc-test-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&d).unwrap();
        let db = Db::open_in_dir(&d).unwrap();
        NotesService::from_db(std::sync::Arc::new(Mutex::new(db)))
    }

    fn content(s: &str) -> String {
        format!(r#"{{"blocks":[{{"type":"text","runs":[{{"t":"{s}"}}]}}],"plain":"{s}"}}"#)
    }

    #[test]
    fn create_list_save_delete_roundtrip() {
        let svc = service();
        let n = svc.create_note("a1", &content("milk"), "#FFF9B1").unwrap();
        assert_eq!(n.id, "a1");

        let all = svc
            .list_notes(&ListFilter {
                include_archived: false,
                deleted: false,
                search: None,
            })
            .unwrap();
        assert_eq!(all.len(), 1);

        svc.save_note("a1", &content("milk and eggs")).unwrap();
        let got = svc.get_note("a1").unwrap().unwrap();
        assert!(got.content.contains("eggs"));

        svc.soft_delete("a1").unwrap();
        assert!(svc
            .list_notes(&ListFilter {
                include_archived: false,
                deleted: false,
                search: None,
            })
            .unwrap()
            .is_empty());
        assert_eq!(
            svc.list_notes(&ListFilter {
                include_archived: false,
                deleted: true,
                search: None
            })
            .unwrap()
            .len(),
            1
        );
    }

    #[test]
    fn search_and_meta_via_service() {
        let svc = service();
        svc.create_note("b1", &content("buy milk"), "#FFF9B1")
            .unwrap();
        svc.create_note("b2", &content("report draft"), "#DCE8FF")
            .unwrap();

        let hits = svc
            .list_notes(&ListFilter {
                include_archived: false,
                deleted: false,
                search: Some("milk".into()),
            })
            .unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id, "b1");

        svc.update_note_meta("b2", None, Some(true), None).unwrap();
        let b2 = svc.get_note("b2").unwrap().unwrap();
        assert!(b2.is_pinned);

        let err = svc.save_note("nope", &content("x"));
        assert!(err.is_err());
    }
}
