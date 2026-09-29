//! Tauri command boundary for notes.  Keeping this thin lets the repository
//! and service stay independently testable.

use tauri::{AppHandle, Emitter, Manager, State, WebviewUrl, WebviewWindowBuilder};
use uuid::Uuid;

use crate::{
    db::notes::NoteRow,
    service::notes_service::{ListFilter, NotesService},
    DbState,
};

const COLORS: [&str; 6] = [
    "#FFF9B1", "#DCE8FF", "#E4D7FF", "#D9F7BE", "#FFD6E7", "#FFE2B8",
];
const EMPTY_CONTENT: &str = r#"{"blocks":[{"type":"text","runs":[]}],"plain":""}"#;
/// Bound WebView2 memory until an inactive-window manager can collapse LRU notes.
pub const MAX_FLOATING_WINDOWS: usize = 10;
pub const NOTES_CHANGED_EVENT: &str = "notes-changed";

/// Notify every window that the notes projection may have changed.  The board
/// reloads from SQLite after this event, keeping the event payload deliberately
/// small and avoiding a second, potentially stale, copy of note data.
fn emit_notes_changed(app: &AppHandle) -> Result<(), String> {
    app.emit(NOTES_CHANGED_EVENT, ())
        .map_err(|error| error.to_string())
}

fn ensure_floating_window_capacity(open_count: usize) -> Result<(), String> {
    if open_count >= MAX_FLOATING_WINDOWS {
        return Err(format!(
            "maximum of {MAX_FLOATING_WINDOWS} floating note windows reached; collapse or close a note before opening another"
        ));
    }
    Ok(())
}

fn service(state: &State<'_, DbState>) -> NotesService {
    NotesService::from_state(state.inner())
}

fn validate_content(content: &str) -> Result<(), String> {
    let value: serde_json::Value =
        serde_json::from_str(content).map_err(|_| "content must be valid JSON".to_string())?;
    if value
        .get("plain")
        .and_then(serde_json::Value::as_str)
        .is_none()
    {
        return Err("content must contain a top-level plain string".to_string());
    }
    Ok(())
}

#[tauri::command]
pub async fn create_note(app: AppHandle, state: State<'_, DbState>) -> Result<NoteRow, String> {
    let notes = service(&state);
    let count = notes
        .list_notes(&ListFilter {
            include_archived: true,
            deleted: false,
            search: None,
        })?
        .len();
    let note = notes.create_note(
        &Uuid::new_v4().to_string(),
        EMPTY_CONTENT,
        COLORS[count % COLORS.len()],
    )?;
    emit_notes_changed(&app)?;
    Ok(note)
}

#[tauri::command]
pub async fn list_notes(
    state: State<'_, DbState>,
    include_archived: bool,
    deleted: bool,
    search: Option<String>,
) -> Result<Vec<NoteRow>, String> {
    service(&state).list_notes(&ListFilter {
        include_archived,
        deleted,
        search,
    })
}

#[tauri::command]
pub async fn get_note(state: State<'_, DbState>, id: String) -> Result<Option<NoteRow>, String> {
    service(&state).get_note(&id)
}

#[tauri::command]
pub async fn save_note(app: AppHandle, state: State<'_, DbState>, id: String, content: String) -> Result<(), String> {
    validate_content(&content)?;
    service(&state).save_note(&id, &content)?;
    emit_notes_changed(&app)
}

#[tauri::command]
pub async fn update_note_meta(
    app: AppHandle,
    state: State<'_, DbState>,
    id: String,
    color: Option<String>,
    is_pinned: Option<bool>,
    is_archived: Option<bool>,
) -> Result<(), String> {
    service(&state).update_note_meta(&id, color.as_deref(), is_pinned, is_archived)?;
    emit_notes_changed(&app)
}

#[tauri::command]
pub async fn soft_delete(app: AppHandle, state: State<'_, DbState>, id: String) -> Result<(), String> {
    service(&state).soft_delete(&id)?;
    emit_notes_changed(&app)
}

#[tauri::command]
pub async fn restore_note(app: AppHandle, state: State<'_, DbState>, id: String) -> Result<(), String> {
    service(&state).restore(&id)?;
    emit_notes_changed(&app)
}

#[tauri::command]
pub async fn purge_note(app: AppHandle, state: State<'_, DbState>, id: String) -> Result<(), String> {
    service(&state).purge(&id)?;
    emit_notes_changed(&app)
}

#[tauri::command]
pub async fn save_window_geom(
    state: State<'_, DbState>,
    id: String,
    x: f64,
    y: f64,
    w: f64,
    h: f64,
    floating: bool,
) -> Result<(), String> {
    service(&state).save_window_geom(&id, x, y, w, h, floating)
}

#[tauri::command]
pub async fn open_note_window(
    app: AppHandle,
    state: State<'_, DbState>,
    id: String,
) -> Result<(), String> {
    let label = format!("note-{id}");
    if let Some(window) = app.get_webview_window(&label) {
        return window.set_focus().map_err(|e| e.to_string());
    }
    let open_count = app
        .webview_windows()
        .keys()
        .filter(|label| label.starts_with("note-"))
        .count();
    ensure_floating_window_capacity(open_count)?;
    let note = service(&state)
        .get_note(&id)?
        .ok_or_else(|| "note not found".to_string())?;
    // Include the note id in the URL as a startup fallback. A newly created
    // WebView can evaluate the document before Tauri installs current-window
    // metadata; URL routing keeps the child on the note view during that race.
    let url = WebviewUrl::App(format!("index.html?note={id}").into());
    let mut builder = WebviewWindowBuilder::new(&app, label, url)
        .title("Sticky Notes+")
        .decorations(false)
        .always_on_top(true)
        .inner_size(320.0, 320.0);
    if let Some((x, y, width, height)) = note.window {
        builder = builder.position(x, y).inner_size(width, height);
    } else {
        builder = builder.center();
    }
    builder.build().map(|_| ()).map_err(|e| e.to_string())
}

/// Close a floating note through the native window handle. This is deliberately
/// a command rather than a WebView-only close so it also works when the child
/// WebView is busy or has lost its injected window API.
#[tauri::command]
pub fn close_note_window(app: AppHandle, id: String) -> Result<(), String> {
    let label = format!("note-{id}");
    app.get_webview_window(&label)
        .ok_or_else(|| "note window not found".to_string())?
        .close()
        .map_err(|e| e.to_string())
}

/// Destroy a floating note after its WebView has flushed content.  `close()`
/// goes through the close-request lifecycle; a busy child WebView can leave
/// the native shell alive after its document has already been torn down.
#[tauri::command]
pub fn destroy_note_window(app: AppHandle, id: String) -> Result<(), String> {
    let label = format!("note-{id}");
    app.get_webview_window(&label)
        .ok_or_else(|| "note window not found".to_string())?
        .destroy()
        .map_err(|e| e.to_string())
}

/// Start a native drag from the note's title/blank area.
#[tauri::command]
pub fn drag_note_window(app: AppHandle, id: String) -> Result<(), String> {
    let label = format!("note-{id}");
    app.get_webview_window(&label)
        .ok_or_else(|| "note window not found".to_string())?
        .start_dragging()
        .map_err(|e| e.to_string())
}

/// Focus the existing board window from a floating note.
#[tauri::command]
pub fn open_board(app: AppHandle) -> Result<(), String> {
    let board = app
        .get_webview_window("board")
        .ok_or_else(|| "board window not found; restart the app to recreate it".to_string())?;
    // A hidden window can be focused successfully on some platforms without
    // being made visible, so always restore visibility/state before focusing.
    board.show().map_err(|e| e.to_string())?;
    board.unminimize().map_err(|e| e.to_string())?;
    board.set_focus().map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::{ensure_floating_window_capacity, MAX_FLOATING_WINDOWS};

    #[test]
    fn floating_window_capacity_allows_up_to_cap() {
        assert!(ensure_floating_window_capacity(MAX_FLOATING_WINDOWS - 1).is_ok());
        assert!(ensure_floating_window_capacity(MAX_FLOATING_WINDOWS).is_err());
    }
}
