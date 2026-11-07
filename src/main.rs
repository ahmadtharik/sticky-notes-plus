#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod acceptance_profile;
mod board;
mod calendar_sync;
#[cfg(windows)]
mod calendar_worker;
mod checklist_presentation;
mod content_identity;
mod date_picker;
mod db;
mod editor;
mod editor_history;
#[cfg(all(windows, feature = "dev-tools"))]
mod editor_probe;
#[cfg(test)]
mod editor_tests;
mod editor_text;
mod google_calendar;
mod hotkey;
mod image_picker;
mod inline_images;
#[cfg(windows)]
mod instance;
#[cfg(windows)]
mod native_image;
mod native_projection;
#[cfg(windows)]
mod native_rich_editor;
mod notification;
#[cfg(all(windows, feature = "dev-tools"))]
mod reliability_probe;
mod settings;
#[cfg(windows)]
mod tooltip;
mod tray;
#[cfg(windows)]
mod updater;
#[cfg(windows)]
mod window_chrome;

slint::include_modules!();

use chrono::{Datelike, Local, NaiveDate};
use db::{attachments, notes, reminders, Db};
use slint::{
    CloseRequestResponse, ComponentHandle, Model, ModelRc, PhysicalPosition, PhysicalSize, Timer,
    TimerMode, VecModel,
};
use std::{
    cell::RefCell,
    path::PathBuf,
    rc::{Rc, Weak},
    sync::{Arc, Mutex},
    time::Duration,
};
use uuid::Uuid;

use board::*;
use editor::*;
use editor_text::*;

#[cfg(windows)]
use i_slint_backend_winit::{WinitWindowAccessor, WinitWindowEventResult};
#[cfg(windows)]
use native_rich_editor::{NativeDocument, NativeRichEditor};

const EMPTY_CONTENT: &str = r#"{"blocks":[{"type":"text","runs":[]}],"plain":""}"#;
const COLORS: [&str; 6] = [
    "#FFF9B1", "#DCE8FF", "#E4D7FF", "#D9F7BE", "#FFD6E7", "#FFE2B8",
];
const AUTOSAVE_DELAY: Duration = Duration::from_secs(1);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct WindowGeometry {
    x: i32,
    y: i32,
    width: u32,
    height: u32,
}

impl WindowGeometry {
    fn from_window(window: &slint::Window) -> Self {
        // Slint's cached geometry may still contain its creation defaults
        // until the queued resize event is processed. Read the realized host.
        #[cfg(windows)]
        if let Some(geometry) = window.with_winit_window(|host| {
            let size = host.inner_size();
            let position = host.outer_position().ok();
            Self {
                x: position.map_or(window.position().x, |value| value.x),
                y: position.map_or(window.position().y, |value| value.y),
                width: size.width,
                height: size.height,
            }
        }) {
            return geometry;
        }
        let position = window.position();
        let size = window.size();
        Self {
            x: position.x,
            y: position.y,
            width: size.width,
            height: size.height,
        }
    }

    fn from_db(value: Option<(f64, f64, f64, f64)>) -> Option<Self> {
        let (x, y, width, height) = value?;
        if !x.is_finite() || !y.is_finite() || !width.is_finite() || !height.is_finite() {
            return None;
        }
        let width = width.round() as u32;
        let height = height.round() as u32;
        (width >= 240 && height >= 180).then_some(Self {
            x: x.round() as i32,
            y: y.round() as i32,
            width,
            height,
        })
    }

    fn apply(self, window: &slint::Window) {
        window.set_size(PhysicalSize::new(self.width, self.height));
        window.set_position(PhysicalPosition::new(self.x, self.y));
    }
}

struct EditorEntry {
    id: String,
    // Own the native child alongside its parent window; restore its procedure
    // before destroying the child. Keep this field before the window handle.
    #[cfg(windows)]
    native_editor: Option<Rc<NativeRichEditor>>,
    window: NoteWindow,
    last_geometry: WindowGeometry,
    save_state: Rc<RefCell<EditorSaveState>>,
}

/// Each editor owns one single-shot timer. It is restarted while typing, so
/// only the final edit in a burst reaches SQLite.
struct EditorSaveState {
    timer: Timer,
    dirty: bool,
    // The editor only reports the replacement text. Keeping the prior UTF-8
    // string lets us move stored style ranges around ordinary typing, paste
    // and deletion without slicing inside a Unicode character.
    last_text: String,
    // The child HWND has its own short debounce. Store only a weak reference
    // here so the one central Slint flush path can drain it before inspecting
    // `dirty`, without extending the native control's lifetime.
    #[cfg(windows)]
    native_editor: Option<Weak<NativeRichEditor>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct DueReminder {
    id: String,
    title: String,
    label: String,
    due_label: String,
}

static ACCEPTANCE_DATA_DIR: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();

fn app_data_dir() -> PathBuf {
    if let Some(path) = ACCEPTANCE_DATA_DIR.get() {
        return path.clone();
    }
    std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")))
        // Keep the native Slint database separate from the legacy Tauri
        // database.  Sharing that file makes the two migration histories
        // collide (notably when one already has is_always_on_top).
        .join("Sticky Notes+ Slint")
}

#[cfg(windows)]
fn show_startup_error(data_dir: &std::path::Path, error: &anyhow::Error) {
    use windows_sys::Win32::UI::WindowsAndMessaging::{MessageBoxW, MB_ICONERROR, MB_OK};
    let wide = |text: String| -> Vec<u16> { text.encode_utf16().chain(Some(0)).collect() };
    let text = wide(format!(
        "Sticky Notes+ couldn't open your notes, so it has to close.\n\n\
         Your notes are in:\n{}\n\nReason: {error}",
        data_dir.display()
    ));
    let title = wide("Sticky Notes+".to_owned());
    unsafe {
        MessageBoxW(
            std::ptr::null_mut(),
            text.as_ptr(),
            title.as_ptr(),
            MB_OK | MB_ICONERROR,
        )
    };
}

/// Saves every open note, attempting all of them. A note that failed is shown
/// with its retryable error. False if any failed.
#[cfg(windows)]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    #[cfg(all(windows, feature = "dev-tools"))]
    if matches!(
        std::env::args().nth(1).as_deref(),
        Some("--reliability-probe" | "--crash-fixture")
    ) {
        reliability_probe::run()?;
        return Ok(());
    }
    #[cfg(windows)]
    if std::env::args().nth(1).as_deref() == Some("--apply-update") {
        let args: Vec<String> = std::env::args().skip(2).collect();
        updater::apply(&args)?;
        return Ok(());
    }
    // Installer-owned uninstall action: no UI, note DB or activation inbox.
    if std::env::args().nth(1).as_deref() == Some("--remove-startup") {
        settings::set_windows_startup(false)?;
        return Ok(());
    }
    if std::env::var_os("SLINT_BACKEND").is_none() {
        std::env::set_var("SLINT_BACKEND", "winit-software");
    }
    // Same backend the variable selects, plus a hook so the drag preview can
    // open without activation.
    #[cfg(windows)]
    if std::env::var("SLINT_BACKEND").as_deref() == Ok("winit-software") {
        use i_slint_backend_winit::winit::platform::windows::WindowAttributesExtWindows;
        let backend = i_slint_backend_winit::Backend::builder()
            .with_renderer_name("software")
            .with_window_attributes_hook(|attributes| {
                if NEXT_WINDOW_INACTIVE.with(|flag| flag.replace(false)) {
                    attributes.with_active(false).with_skip_taskbar(true)
                } else {
                    attributes
                }
            })
            .build()?;
        slint::platform::set_platform(Box::new(backend)).map_err(|error| error.to_string())?;
    }
    #[cfg(all(windows, feature = "dev-tools"))]
    if std::env::args().nth(1).as_deref() == Some("--window-probe") {
        return editor_probe::window_roundtrip();
    }
    #[cfg(all(windows, feature = "dev-tools"))]
    if matches!(
        std::env::args().nth(1).as_deref(),
        Some("--editor-probe" | "--editor-soak" | "--attachment-probe" | "--attachment-soak")
    ) {
        return editor_probe::run();
    }
    #[cfg(all(windows, feature = "dev-tools"))]
    if matches!(
        std::env::args().nth(1).as_deref(),
        Some("--editor-preview" | "--editor-preview-reopen" | "--editor-preview-fallback")
    ) {
        return editor_probe::preview();
    }
    let raw_args: Vec<String> = std::env::args().collect();
    if raw_args.get(1).map(String::as_str) == Some("--acceptance-init") {
        if raw_args.len() != 3 {
            return Err("Usage: --acceptance-init <new-directory>".into());
        }
        acceptance_profile::initialize(std::path::Path::new(&raw_args[2]))?;
        return Ok(());
    }
    let (args, acceptance_path) = acceptance_profile::parse(raw_args)?;
    if let Some(path) = acceptance_path {
        ACCEPTANCE_DATA_DIR
            .set(path)
            .map_err(|_| "Acceptance profile already configured")?;
    }
    let background_launch = args.get(1).map(String::as_str) == Some("--background");
    #[cfg(windows)]
    let instance = {
        let command = if args.get(1).map(String::as_str) == Some("--activate") {
            if args.len() != 3 || notification::Activation::parse(&args[2]).is_none() {
                return Err("Invalid reminder activation".into());
            }
            instance::Command::Notification(args[2].clone())
        } else if background_launch {
            instance::Command::Background
        } else {
            instance::Command::ShowBoard
        };
        let Some(owner) = instance::Instance::start(&app_data_dir(), command)? else {
            return Ok(());
        };
        owner
    };
    let db = match Db::open_in_dir(&app_data_dir()) {
        Ok(db) => Arc::new(Mutex::new(db)),
        Err(error) => {
            // This build has no console, so the reason would otherwise be lost.
            #[cfg(windows)]
            show_startup_error(&app_data_dir(), &error);
            return Err(error.into());
        }
    };
    // Recover files left by a crash between file creation and metadata commit.
    // The primary instance owns the database and imports share this mutex.
    let attachment_cleanup_error = attachments::cleanup_orphans(&db::lock(&db).conn)
        .err()
        .map(|error| format!("Image cleanup needs retry: {error}"));
    let board = BoardWindow::new()?;
    let (preferences, settings_error) = match settings::load(&db::lock(&db).conn) {
        Ok(preferences) => (preferences, String::new()),
        Err(error) => (
            settings::Preferences::default(),
            format!("Could not load settings: {error}. Existing settings were preserved."),
        ),
    };
    board.invoke_apply_theme(preferences.dark_theme);
    #[cfg(windows)]
    {
        let saved = settings::load_board_geometry(&db::lock(&db).conn)
            .ok()
            .flatten();
        BOARD_RESTORE.with(|pending| *pending.borrow_mut() = WindowGeometry::from_db(saved));
        track_board_geometry(&board, &db);
        setup_calendar(&board, &db);
    }
    board.set_setting_dark(preferences.dark_theme);
    board.set_setting_shortcut(preferences.shortcut.clone().into());
    board.set_setting_startup(preferences.start_with_windows);
    board.set_settings_status(settings_error.into());
    {
        let weak = board.as_weak();
        let db = db.clone();
        board.on_preview_import(move || {
            let Some(board) = weak.upgrade() else {
                return;
            };
            PENDING_RESTORE.with(|pending| pending.borrow_mut().take());
            board.set_import_pending(false);
            let path = match image_picker::select_export_file(board.window()) {
                Ok(Some(path)) => path,
                Ok(None) => return,
                Err(error) => {
                    board.set_import_status(format!("Couldn't import: {error}").into());
                    return;
                }
            };
            board.set_data_busy(true);
            board.set_import_status("Reading the export…".into());
            let weak = weak.clone();
            let db = db.clone();
            std::thread::spawn(move || {
                let result = db::restore::preview(&path).and_then(|plan| {
                    let already_here = plan.already_here(&db::lock(&db).conn)?;
                    Ok((plan, already_here))
                });
                let _ = slint::invoke_from_event_loop(move || {
                    let Some(board) = weak.upgrade() else { return };
                    board.set_data_busy(false);
                    match result {
                        Ok((plan, already_here)) => {
                            let damaged = if plan.skipped() > 0 {
                                format!(" {} damaged notes will be skipped.", plan.skipped())
                            } else {
                                String::new()
                            };
                            board.set_import_status(
                                format!(
                                    "{} notes in this export, {} to add. Notes you already have stay as they are.{damaged}",
                                    plan.count(),
                                    plan.count() - already_here
                                )
                                .into(),
                            );
                            PENDING_RESTORE.with(|pending| *pending.borrow_mut() = Some(plan));
                            board.set_import_pending(true);
                        }
                        Err(error) => {
                            board.set_import_status(format!("Couldn't import: {error}").into());
                        }
                    }
                });
            });
        });
    }
    {
        let weak = board.as_weak();
        let db = db.clone();
        board.on_confirm_import(move || {
            let Some(board) = weak.upgrade() else {
                return;
            };
            let Some(plan) = PENDING_RESTORE.with(|pending| pending.borrow_mut().take()) else {
                return;
            };
            board.set_import_pending(false);
            board.set_data_busy(true);
            board.set_import_status("Adding notes…".into());
            let weak = weak.clone();
            let db = db.clone();
            std::thread::spawn(move || {
                let result = db::restore::apply(&db, &plan);
                let _ = slint::invoke_from_event_loop(move || {
                    let Some(board) = weak.upgrade() else { return };
                    board.set_data_busy(false);
                    match result {
                        Ok(restored) => {
                            let mut problems = String::new();
                            if restored.missing_images > 0 {
                                problems += &format!(
                                    " {} images couldn't be restored.",
                                    restored.missing_images
                                );
                            }
                            if restored.skipped_notes > 0 {
                                problems += &format!(
                                    " {} damaged notes were skipped.",
                                    restored.skipped_notes
                                );
                            }
                            board.set_import_status(
                                format!(
                                    "Added {} notes and {} images.{problems}",
                                    restored.notes, restored.images
                                )
                                .into(),
                            );
                            refresh(&board, &db, board.get_query().as_str());
                            refresh_due_reminders(&board, &db);
                        }
                        Err(error) => board.set_import_status(
                            format!("Import failed; no notes were added: {error}").into(),
                        ),
                    }
                });
            });
        });
    }
    {
        let weak = board.as_weak();
        board.on_cancel_import(move || {
            PENDING_RESTORE.with(|pending| pending.borrow_mut().take());
            if let Some(board) = weak.upgrade() {
                board.set_import_pending(false);
                board.set_import_status("Import cancelled.".into());
            }
        });
    }
    if let Some(error) = attachment_cleanup_error {
        board.set_settings_status(error.into());
    }
    let preferences = Rc::new(RefCell::new(preferences));
    {
        let weak = board.as_weak();
        let db = db.clone();
        let preferences = preferences.clone();
        board.on_apply_settings(move |include_shortcut| {
            let Some(board) = weak.upgrade() else {
                return;
            };
            // Toggles save on their own; an unsaved shortcut edit waits for Save.
            let shortcut = if include_shortcut {
                board.get_setting_shortcut().trim().into()
            } else {
                preferences.borrow().shortcut.clone()
            };
            let next = settings::Preferences {
                dark_theme: board.get_setting_dark(),
                shortcut,
                start_with_windows: board.get_setting_startup(),
                ..settings::Preferences::default()
            };
            let result = settings::apply(
                &db::lock(&db).conn,
                &preferences.borrow(),
                &next,
                settings::set_windows_startup,
            );
            match result {
                Ok(()) => {
                    board.invoke_apply_theme(next.dark_theme);
                    *preferences.borrow_mut() = next;
                    board.set_settings_status(if include_shortcut {
                        "Shortcut saved. It applies next launch.".into()
                    } else {
                        "".into()
                    });
                }
                Err(error) => {
                    // Put the toggles back to what is actually saved.
                    let saved = preferences.borrow();
                    board.set_setting_dark(saved.dark_theme);
                    board.set_setting_startup(saved.start_with_windows);
                    board.set_settings_status(format!("Settings not saved: {error}").into())
                }
            }
        });
    }
    let board_weak = board.as_weak();
    let editor_windows: Rc<RefCell<Vec<EditorEntry>>> = Rc::new(RefCell::new(Vec::new()));
    #[cfg(windows)]
    setup_board_card_drag(&board, &db, &editor_windows);
    #[cfg(windows)]
    setup_updates(&board, &db, &editor_windows);
    #[cfg(windows)]
    setup_export(&board, &db, &editor_windows);
    refresh(&board, &db, "");
    // Evaluate immediately rather than making the user wait for the first
    // timer tick after launch.
    refresh_due_reminders(&board, &db);

    #[cfg(windows)]
    let activation_timer = Timer::default();
    #[cfg(windows)]
    {
        let weak = board.as_weak();
        let db = db.clone();
        let windows = editor_windows.clone();
        activation_timer.start(TimerMode::Repeated, Duration::from_millis(100), move || {
            let Some(board) = weak.upgrade() else {
                return;
            };
            let commands = match instance.poll() {
                Ok(commands) => commands,
                Err(error) => {
                    eprintln!("Activation inbox unavailable: {error}");
                    return;
                }
            };
            for command in commands {
                match command {
                    instance::Command::Background => (),
                    instance::Command::ShowBoard => {
                        refresh_due_reminders(&board, &db);
                        let _ = show_board(&board);
                        board.window().with_winit_window(|host| host.focus_window());
                    }
                    instance::Command::Notification(uri) => {
                        let Some(activation) = notification::Activation::parse(&uri) else {
                            continue;
                        };
                        let result =
                            reminders::activate(&db::lock(&db).conn, &activation, Local::now());
                        match result {
                            Ok(Some(note_id)) => {
                                if activation.action == notification::Action::Open {
                                    open_editor(&board, &db, &note_id, &windows);
                                }
                                refresh_due_reminders(&board, &db);
                                let _ = show_board(&board);
                            }
                            Ok(None) => (), // Stale notification: never alter a newer occurrence.
                            Err(error) => {
                                board.set_reminder_error(
                                    format!("Could not apply reminder action: {error}").into(),
                                );
                                let _ = show_board(&board);
                            }
                        }
                    }
                }
            }
        });
    }

    // The projection is date based, so a modest UI-loop poll is enough to
    // cover day rollover and an app left open overnight without introducing a
    // background scheduler or an unvalidated OS toast dependency.
    let reminder_timer = Timer::default();
    {
        let db = db.clone();
        let weak = board_weak.clone();
        reminder_timer.start(TimerMode::Repeated, Duration::from_secs(30), move || {
            if let Some(board) = weak.upgrade() {
                surface_unannounced_due(&board, &db);
                refresh_due_reminders(&board, &db);
            }
        });
    }

    // Slint does not expose a portable moved/resized callback on every backend.
    // Polling the native window geometry keeps persistence backend-independent and
    // avoids writing to SQLite on every compositor event. The timer is kept alive
    // for the lifetime of the process.
    let geometry_timer = Timer::default();
    {
        let db = db.clone();
        let windows = editor_windows.clone();
        geometry_timer.start(TimerMode::Repeated, Duration::from_millis(250), move || {
            let mut windows = windows.borrow_mut();
            // Release hidden note hosts outside their close callbacks. Native
            // child procedures must not be destroyed while dispatching an event.
            let retired: Vec<_> = windows
                .iter()
                .filter(|entry| {
                    !entry.window.window().is_visible() && !entry.save_state.borrow().dirty
                })
                .map(|entry| entry.id.clone())
                .collect();
            windows.retain(|entry| {
                entry.window.window().is_visible() || entry.save_state.borrow().dirty
            });
            for id in retired {
                let _ = attachments::retire_unplaced(&db::lock(&db).conn, &id);
            }
            for entry in windows.iter_mut() {
                #[cfg(windows)]
                window_chrome::update_active(&entry.window);
                if let Some(native) = &entry.native_editor {
                    let [width, height] = native.client_size();
                    entry.window.set_native_client_width(width);
                    entry.window.set_native_client_height(height);
                    native.set_layout_bounds([
                        entry.window.get_editor_left(),
                        entry.window.get_editor_top(),
                        entry.window.get_editor_width(),
                        entry.window.get_editor_height(),
                    ]);
                }
                let geometry = WindowGeometry::from_window(entry.window.window());
                if geometry != entry.last_geometry {
                    let guard = db::lock(&db);
                    let _ = notes::save_window_geom(
                        &guard.conn,
                        &entry.id,
                        f64::from(geometry.x),
                        f64::from(geometry.y),
                        f64::from(geometry.width),
                        f64::from(geometry.height),
                        true,
                    );
                    entry.last_geometry = geometry;
                }
            }
        });
    }

    {
        let db = db.clone();
        let weak = board_weak.clone();
        board.on_search(move |query| {
            if let Some(board) = weak.upgrade() {
                refresh(&board, &db, &query);
            }
        });
    }
    {
        let db = db.clone();
        let weak = board_weak.clone();
        board.on_select_board_view(move |view| {
            let view = match view.as_str() {
                "archive" | "deleted" => view,
                _ => "notes".into(),
            };
            if let Some(board) = weak.upgrade() {
                board.set_board_view(view);
                board.set_pending_purge_id("".into());
                let query = board.get_query().to_string();
                refresh(&board, &db, &query);
            }
        });
    }
    {
        let db = db.clone();
        let weak = board_weak.clone();
        board.on_snooze_reminder(move |id| {
            let result = {
                let guard = db::lock(&db);
                reminders::snooze(
                    &guard.conn,
                    &id,
                    chrono::Utc::now() + chrono::Duration::minutes(10),
                )
            };
            if result.is_ok() {
                if let Some(board) = weak.upgrade() {
                    refresh_due_reminders(&board, &db);
                }
            }
        });
    }
    {
        let db = db.clone();
        let weak = board_weak.clone();
        board.on_dismiss_reminder(move |id| {
            let result = {
                let guard = db::lock(&db);
                reminders::set_status(&guard.conn, &id, "dismissed")
            };
            if result.is_ok() {
                if let Some(board) = weak.upgrade() {
                    refresh_due_reminders(&board, &db);
                }
            }
        });
    }
    {
        let db = db.clone();
        let weak = board_weak.clone();
        board.on_complete_reminder(move |id| {
            let result = {
                let guard = db::lock(&db);
                reminders::set_status(&guard.conn, &id, "completed")
            };
            if result.is_ok() {
                if let Some(board) = weak.upgrade() {
                    refresh_due_reminders(&board, &db);
                }
            }
        });
    }
    {
        let db = db.clone();
        let weak = board_weak.clone();
        let windows = editor_windows.clone();
        board.on_create_note(move || {
            let id = Uuid::new_v4().to_string();
            {
                let guard = db::lock(&db);
                let count = notes::list(&guard.conn, true, false, None)
                    .map(|n| n.len())
                    .unwrap_or(0);
                let _ = notes::create(
                    &guard.conn,
                    &id,
                    EMPTY_CONTENT,
                    COLORS[count % COLORS.len()],
                );
            }
            // The database guard must be gone before refresh/open_editor: both
            // paths acquire the same mutex again.
            if let Some(board) = weak.upgrade() {
                // New notes always land in the active view; leaving the board
                // on Archive/Deleted would otherwise make the new editor seem
                // to have disappeared as soon as it is closed.
                board.set_board_view("notes".into());
                board.set_pending_purge_id("".into());
                refresh(&board, &db, board.get_query().as_ref());
                open_editor(&board, &db, &id, &windows);
            }
        });
    }
    {
        let db = db.clone();
        let weak = board_weak.clone();
        let windows = editor_windows.clone();
        board.on_open_note(move |id| {
            let id = id.to_string();
            let can_open = {
                let guard = db::lock(&db);
                notes::get(&guard.conn, &id)
                    .ok()
                    .flatten()
                    .is_some_and(|note| note.deleted_at.is_none())
            };
            if can_open {
                if let Some(board) = weak.upgrade() {
                    open_editor(&board, &db, &id, &windows);
                }
            }
        });
    }
    {
        let db = db.clone();
        let weak = board_weak.clone();
        let windows = editor_windows.clone();
        board.on_delete_note(move |id| {
            let id = id.to_string();
            // Flush an open editor before deleting. If the flush fails, its
            // visible retryable error wins over a destructive state change.
            if let Some(entry) = windows.borrow().iter().find(|entry| id == entry.id) {
                if !flush_editor(&db, &id, &entry.window, &entry.save_state, &weak) {
                    let _ = entry.window.show();
                    return;
                }
            }
            let deleted = {
                let guard = db::lock(&db);
                notes::soft_delete(&guard.conn, &id).is_ok()
            };
            if !deleted {
                return;
            }
            // Prevent a queued autosave from firing after deletion. The DB
            // save guard is a second line of defence, but stopping this timer
            // also keeps a hidden, stale editor from reporting a false save.
            if let Some(entry) = windows.borrow().iter().find(|entry| id == entry.id) {
                entry.save_state.borrow().timer.stop();
                entry.save_state.borrow_mut().dirty = false;
                let _ = entry.window.hide();
            }
            if let Some(board) = weak.upgrade() {
                board.set_pending_purge_id("".into());
                let query = board.get_query().to_string();
                refresh(&board, &db, &query);
                refresh_due_reminders(&board, &db);
            }
        });
    }
    {
        let db = db.clone();
        let weak = board_weak.clone();
        board.on_archive_note(move |id, archived| {
            let id = id.to_string();
            let changed = {
                let guard = db::lock(&db);
                notes::update_meta(&guard.conn, &id, None, None, Some(archived)).is_ok()
            };
            if changed {
                if let Some(board) = weak.upgrade() {
                    let query = board.get_query().to_string();
                    refresh(&board, &db, &query);
                }
            }
        });
    }
    {
        let db = db.clone();
        let weak = board_weak.clone();
        board.on_restore_note(move |id| {
            let id = id.to_string();
            let restored = {
                let guard = db::lock(&db);
                notes::restore(&guard.conn, &id).is_ok()
            };
            if restored {
                if let Some(board) = weak.upgrade() {
                    let query = board.get_query().to_string();
                    refresh(&board, &db, &query);
                    refresh_due_reminders(&board, &db);
                }
            }
        });
    }
    {
        let db = db.clone();
        let weak = board_weak.clone();
        board.on_request_purge(move |id| {
            let id = id.to_string();
            let is_deleted = {
                let guard = db::lock(&db);
                notes::get(&guard.conn, &id)
                    .ok()
                    .flatten()
                    .is_some_and(|note| note.deleted_at.is_some())
            };
            if is_deleted {
                if let Some(board) = weak.upgrade() {
                    board.set_pending_purge_id(id.into());
                }
            }
        });
    }
    {
        let db = db.clone();
        let weak = board_weak.clone();
        board.on_confirm_purge(move |id| {
            let id = id.to_string();
            let purged = {
                let guard = db::lock(&db);
                let purged = notes::purge(&guard.conn, &id).is_ok();
                if purged {
                    if let Err(error) = attachments::cleanup_orphans(&guard.conn) {
                        if let Some(board) = weak.upgrade() {
                            board.set_settings_status(
                                format!("Note deleted; image cleanup needs retry: {error}").into(),
                            );
                        }
                    }
                }
                purged
            };
            if let Some(board) = weak.upgrade() {
                board.set_pending_purge_id("".into());
                if purged {
                    let query = board.get_query().to_string();
                    refresh(&board, &db, &query);
                }
            }
        });
    }
    {
        let weak = board_weak.clone();
        board.on_cancel_purge(move || {
            if let Some(board) = weak.upgrade() {
                board.set_pending_purge_id("".into());
            }
        });
    }
    {
        board.window().on_close_requested(move || {
            // Closing the board should hide it, not terminate the application.
            // An open note can then bring it back through its Board button.
            CloseRequestResponse::HideWindow
        });
    }
    {
        let db = db.clone();
        let weak = board_weak.clone();
        board.on_toggle_pin(move |id, value| {
            let guard = db::lock(&db);
            let _ = notes::update_meta(&guard.conn, &id, None, Some(value), None);
            drop(guard);
            if let Some(board) = weak.upgrade() {
                let query = board.get_query().to_string();
                refresh(&board, &db, &query);
            }
        });
    }
    {
        let db = db.clone();
        let weak = board_weak.clone();
        board.on_select_color(move |id, value| {
            let guard = db::lock(&db);
            let _ = notes::update_meta(&guard.conn, &id, Some(&value), None, None);
            drop(guard);
            if let Some(board) = weak.upgrade() {
                let query = board.get_query().to_string();
                refresh(&board, &db, &query);
            }
        });
    }

    // tray-icon is target-scoped to Windows. The native menu is intentionally
    // small and reliable: show board, create a sticky, or quit. Slint 1.9
    // does not expose a portable tray callback, so poll its event receiver on
    // the existing event loop rather than creating a second GUI loop.
    let tray = match tray::Tray::create() {
        Ok(tray) => Some(tray),
        Err(error) => {
            eprintln!("tray unavailable; continuing without tray: {error}");
            None
        }
    };
    let has_tray = cfg!(windows) && tray.is_some();
    let tray_timer = Timer::default();
    {
        let db = db.clone();
        let weak = board.as_weak();
        let windows = editor_windows.clone();
        tray_timer.start(TimerMode::Repeated, Duration::from_millis(100), move || {
            let Some(tray) = tray.as_ref() else {
                return;
            };
            let Some(command) = tray.poll() else {
                return;
            };
            let Some(board) = weak.upgrade() else {
                return;
            };
            match command {
                tray::Command::ShowBoard => {
                    refresh_due_reminders(&board, &db);
                    let _ = show_board(&board);
                }
                tray::Command::CreateNote => board.invoke_create_note(),
                tray::Command::Quit => {
                    // The save path does not touch this list, so an immutable
                    // borrow is safe while every live editor is flushed.
                    if flush_all_editors(&db, &windows, &weak) {
                        std::process::exit(0);
                    }
                    // A normal Quit must not throw unsaved data away. The
                    // failed editor stays visible with its retryable error.
                    let _ = show_board(&board);
                }
            }
        });
    }

    // The native registration thread never touches Slint. It only forwards
    // WM_HOTKEY events to this timer, so creating a note remains on Slint's
    // UI event loop just like the board and tray actions do.
    let shortcut = preferences.borrow().shortcut.clone();
    let quick_create = match hotkey::QuickCreate::register(&shortcut) {
        Ok(hotkey) => Some(hotkey),
        Err(error) => {
            board.set_settings_status(
                format!("Shortcut {shortcut} could not be registered: {error}").into(),
            );
            eprintln!(
                "global quick-create shortcut ({}) unavailable; continuing without it: {error}",
                shortcut
            );
            None
        }
    };
    let quick_create_timer = Timer::default();
    {
        let weak = board.as_weak();
        quick_create_timer.start(TimerMode::Repeated, Duration::from_millis(50), move || {
            let Some(hotkey) = quick_create.as_ref() else {
                return;
            };
            if hotkey.poll_quick_create() {
                if let Some(board) = weak.upgrade() {
                    board.invoke_create_note();
                }
            }
        });
    }
    if !background_launch || !has_tray {
        show_board(&board)?;
    }
    let restore_ids = floating_note_ids(&db::lock(&db).conn)?;
    for id in restore_ids {
        open_editor(&board, &db, &id, &editor_windows);
    }
    surface_unannounced_due(&board, &db);
    if has_tray {
        // Hiding the last window must not disable reminders, activation or
        // the tray. Explicit Quit flushes notes before terminating the app.
        slint::run_event_loop_until_quit()?;
    } else {
        slint::run_event_loop()?;
    }
    Ok(())
}
