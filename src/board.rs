//! The board window: note cards, due reminders, Settings and drag-out.

use super::*;

pub(crate) fn paper_hex(value: &str) -> &str {
    match value {
        "#FFF9B1" => "#fff8ce",
        "#D9F7BE" => "#e2f6d8",
        "#FFD7E5" | "#FFD6E7" => "#fce3ef",
        "#E6D8FF" | "#E4D7FF" => "#eee2fa",
        "#DCE8FF" => "#ddf2fc",
        "#E9E9E9" => "#f1f1f1",
        "#6F7078" => "#454545",
        _ => value,
    }
}
pub(crate) fn color(value: &str) -> slint::Color {
    let value = paper_hex(value);
    let value = if value.len() == 7 && value.starts_with('#') && value.is_ascii() {
        value
    } else {
        "#fff8ce"
    };
    slint::Color::from_argb_u8(
        255,
        u8::from_str_radix(&value[1..3], 16).unwrap_or(255),
        u8::from_str_radix(&value[3..5], 16).unwrap_or(249),
        u8::from_str_radix(&value[5..7], 16).unwrap_or(177),
    )
}

pub(crate) fn visible_notes(
    db: &Arc<Mutex<Db>>,
    query: &str,
    board_view: &str,
) -> Vec<notes::NoteRow> {
    let guard = db::lock(db);
    let search = (!query.trim().is_empty()).then_some(query);
    let rows = notes::list(
        &guard.conn,
        board_view != "notes",
        board_view == "deleted",
        search,
    )
    .unwrap_or_default();
    // `list(include_archived)` intentionally retains its original broad API
    // for callers that need every live note. The board narrows that result to
    // one mutually-exclusive view here.
    if board_view == "archive" {
        rows.into_iter().filter(|note| note.is_archived).collect()
    } else {
        rows
    }
}

thread_local! {
    /// The export waiting for the user to confirm its import; UI thread only.
    pub(crate) static PENDING_RESTORE: RefCell<Option<db::restore::RestorePlan>> = const { RefCell::new(None) };
}

#[cfg(windows)]
thread_local! {
    /// The release last offered in Settings; UI thread only.
    pub(crate) static AVAILABLE_UPDATE: RefCell<Option<updater::Release>> = const { RefCell::new(None) };
}

pub(crate) fn floating_note_ids(conn: &rusqlite::Connection) -> rusqlite::Result<Vec<String>> {
    Ok(notes::list(conn, true, false, None)?
        .into_iter()
        .filter(|note| note.is_floating && !note.is_archived)
        .map(|note| note.id)
        .collect())
}

pub(crate) fn notes_dates(content: &str) -> String {
    db::reminders::extract_date_chips(content)
        .into_iter()
        .map(|chip| chip.label)
        .filter(|label| !label.is_empty())
        .collect::<Vec<_>>()
        .join(" · ")
}

/// Card text without the `[ ]`/`[x]` checklist markers, which the board font
/// can't draw as boxes.
pub(crate) fn without_checklist_marker(line: &str) -> &str {
    checklist_marker(line).map_or(line, |(_, end)| &line[end..])
}

pub(crate) fn note_preview(plain: &str, title: Option<&str>) -> String {
    let mut lines = plain
        .lines()
        .map(without_checklist_marker)
        .filter(|line| !line.trim().is_empty())
        .peekable();
    if lines.peek().is_some_and(|line| Some(line.trim()) == title) {
        lines.next();
    }
    lines
        .map(|line| line.split_whitespace().collect::<Vec<_>>().join(" "))
        .collect::<Vec<_>>()
        .join("\n")
        .chars()
        .take(240)
        .collect()
}

pub(crate) fn refresh(board: &BoardWindow, db: &Arc<Mutex<Db>>, query: &str) {
    let cards = visible_notes(db, query, &board.get_board_view())
        .into_iter()
        .map(|note| {
            // Dates aren't text the user wrote; they only show when nothing else does.
            let plain = calendar_sync::without_date_labels(
                &note.content,
                &plain_from_content(&note.content),
            );
            let title = note
                .title
                .as_deref()
                .map(|title| calendar_sync::without_date_labels(&note.content, title))
                .map(|title| without_checklist_marker(&title).trim().to_owned())
                .filter(|title| !title.is_empty());
            let mut preview = note_preview(&plain, title.as_deref());
            if preview.is_empty() {
                preview = notes_dates(&note.content);
            }
            let modified = chrono::DateTime::parse_from_rfc3339(&note.modified_at)
                .ok()
                .map(|time| time.with_timezone(&Local).date_naive());
            let today = Local::now().date_naive();
            let updated_label = match modified {
                Some(date) if date == today => "Today".to_owned(),
                Some(date) if Some(date) == today.pred_opt() => "Yesterday".to_owned(),
                Some(date) => date.format("%b %d").to_string(),
                None => String::new(),
            };
            let (first, more) = preview.split_once('\n').unwrap_or((&preview, ""));
            let more = more.lines().next().unwrap_or("");
            NoteCard {
                id: note.id.into(),
                title: title.unwrap_or_else(|| "Untitled note".into()).into(),
                preview: first.into(),
                preview_more: more.into(),
                color: color(&note.color),
                pinned: note.is_pinned,
                updated_label: updated_label.into(),
            }
        })
        .collect::<Vec<_>>();
    board.set_notes(ModelRc::new(VecModel::from(cards)));
}

/// Read the reminder projection once and turn it into the intentionally small
/// board surface model. This is used at startup, when reopening the board, and
/// by the timer below, so a date rollover does not require restarting the app.
#[cfg(test)]
pub(crate) fn due_reminders(
    conn: &rusqlite::Connection,
    as_of: NaiveDate,
) -> rusqlite::Result<Vec<DueReminder>> {
    due_reminders_from_rows(conn, reminders::due_on(conn, as_of)?)
}

pub(crate) fn due_reminders_at(
    conn: &rusqlite::Connection,
    as_of: chrono::DateTime<Local>,
) -> rusqlite::Result<Vec<DueReminder>> {
    due_reminders_from_rows(conn, reminders::due_at(conn, as_of)?)
}

pub(crate) fn due_reminders_from_rows(
    conn: &rusqlite::Connection,
    rows: Vec<reminders::ReminderRow>,
) -> rusqlite::Result<Vec<DueReminder>> {
    let mut due = Vec::new();
    for reminder in rows {
        // A reminder normally disappears with its note, but skip a stale row
        // defensively rather than presenting an action for a deleted note.
        let Some(note) = notes::get(conn, &reminder.note_id)? else {
            continue;
        };
        if note.deleted_at.is_none() {
            let name = calendar_sync::reminder_name(
                &note.content,
                note.title.as_deref(),
                &reminder.chip_id,
            );
            due.push(DueReminder {
                id: reminder.id,
                title: name.title,
                // Shown before the due date: which note the item is in.
                label: name.note.unwrap_or_default(),
                due_label: date_label(&if reminder.due_time.is_empty() {
                    reminder.scheduled_date
                } else {
                    format!("{}T{}", reminder.scheduled_date, reminder.due_time)
                }),
            });
        }
    }
    Ok(due)
}

#[cfg(test)]
pub(crate) fn act_on_reminder(
    conn: &rusqlite::Connection,
    reminder_id: &str,
    status: &str,
    as_of: NaiveDate,
) -> rusqlite::Result<Vec<DueReminder>> {
    reminders::set_status(conn, reminder_id, status)?;
    due_reminders(conn, as_of)
}

pub(crate) fn refresh_due_reminders(board: &BoardWindow, db: &Arc<Mutex<Db>>) {
    let cards = {
        let guard = db::lock(db);
        due_reminders_at(&guard.conn, Local::now()).unwrap_or_default()
    }
    .into_iter()
    .map(|reminder| ReminderCard {
        id: reminder.id.into(),
        title: reminder.title.into(),
        label: reminder.label.into(),
        due_label: reminder.due_label.into(),
    })
    .collect::<Vec<_>>();
    board.set_due_reminders(ModelRc::new(VecModel::from(cards)));
}

/// softbuffer's Win32 backend reports its retained buffer as current after the
/// board was hidden, so Slint's partial renderer would repaint nothing over the
/// cleared window. Changing a background dependency forces a full frame.
pub(crate) fn show_board(board: &BoardWindow) -> Result<(), slint::PlatformError> {
    board.show()?;
    board.set_repaint_epoch((board.get_repaint_epoch() + 1) % 2);
    #[cfg(windows)]
    if let Some(geometry) = BOARD_RESTORE.with(|pending| pending.borrow_mut().take()) {
        restore_board_geometry(board, geometry);
    }
    Ok(())
}

#[cfg(windows)]
thread_local! {
    /// Saved board placement, applied on the first show: like editors, a
    /// pre-show request can be replaced by Winit's creation defaults, and a
    /// background launch may not show the board until much later.
    pub(crate) static BOARD_RESTORE: RefCell<Option<WindowGeometry>> = const { RefCell::new(None) };
}

#[cfg(windows)]
pub(crate) fn restore_board_geometry(board: &BoardWindow, geometry: WindowGeometry) {
    let monitors = board
        .window()
        .with_winit_window(|host| {
            host.available_monitors()
                .map(|monitor| {
                    let (position, size) = (monitor.position(), monitor.size());
                    (
                        PhysicalPosition::new(position.x, position.y),
                        PhysicalSize::new(size.width, size.height),
                    )
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let position = PhysicalPosition::new(geometry.x, geometry.y);
    if window_chrome::title_on_monitor(position, geometry.width, &monitors) {
        geometry.apply(board.window());
    } else {
        // Its monitor is gone: keep the size, let Windows place the window.
        board
            .window()
            .set_size(PhysicalSize::new(geometry.width, geometry.height));
    }
}

pub(crate) fn flush_all_editors(
    db: &Arc<Mutex<Db>>,
    editors: &Rc<RefCell<Vec<EditorEntry>>>,
    board: &slint::Weak<BoardWindow>,
) -> bool {
    let mut saved = true;
    for entry in editors.borrow().iter() {
        if !flush_editor(db, &entry.id, &entry.window, &entry.save_state, board) {
            saved = false;
            let _ = entry.window.show();
        }
    }
    saved
}

#[cfg(windows)]
pub(crate) fn setup_export(
    board: &BoardWindow,
    db: &Arc<Mutex<Db>>,
    editors: &Rc<RefCell<Vec<EditorEntry>>>,
) {
    let weak = board.as_weak();
    let db = db.clone();
    let editors = editors.clone();
    board.on_export_notes(move || {
        let Some(board) = weak.upgrade() else { return };
        let now = Local::now();
        let suggested = format!("Sticky Notes {}.json", now.format("%Y-%m-%d"));
        let path = match image_picker::select_export(board.window(), &suggested) {
            Ok(Some(path)) => path,
            Ok(None) => return,
            Err(error) => {
                board.set_export_status(format!("Export failed: {error}").into());
                return;
            }
        };
        if !flush_all_editors(&db, &editors, &weak) {
            board.set_export_status("A note couldn't be saved, so nothing was exported.".into());
            return;
        }
        board.set_data_busy(true);
        board.set_export_status("Exporting…".into());
        let weak = weak.clone();
        let db = db.clone();
        std::thread::spawn(move || {
            let result = db::export::write(&db, &path, &now.to_rfc3339());
            let _ = slint::invoke_from_event_loop(move || {
                let Some(board) = weak.upgrade() else { return };
                board.set_data_busy(false);
                board.set_export_status(
                    match result {
                        Ok(summary) if summary.missing_images > 0 => format!(
                            "Exported {} notes and {} images. {} images were missing and not exported.",
                            summary.notes, summary.images, summary.missing_images
                        ),
                        Ok(summary) => format!(
                            "Exported {} notes and {} images to {}.",
                            summary.notes,
                            summary.images,
                            path.display()
                        ),
                        Err(error) => format!("Export failed: {error}"),
                    }
                    .into(),
                );
            });
        });
    });
}

#[cfg(windows)]
pub(crate) fn setup_updates(
    board: &BoardWindow,
    db: &Arc<Mutex<Db>>,
    editors: &Rc<RefCell<Vec<EditorEntry>>>,
) {
    board.set_update_version(format!("Version {}", updater::CURRENT_VERSION).into());
    let weak = board.as_weak();
    board.on_check_for_updates(move || {
        let Some(board) = weak.upgrade() else { return };
        if board.get_update_busy() {
            return;
        }
        let failure = updater::take_last_failure();
        board.set_update_busy(true);
        board.set_update_status("Checking for updates…".into());
        let weak = weak.clone();
        std::thread::spawn(move || {
            let _apartment = calendar_worker::Apartment::multithreaded();
            let result = google_calendar::WinRtTransport::new()
                .and_then(|transport| updater::check(&transport));
            let _ = slint::invoke_from_event_loop(move || {
                let Some(board) = weak.upgrade() else { return };
                let mut status = match &result {
                    Ok(Some(release)) => format!("Version {} is available.", release.version),
                    Ok(None) => "You're up to date.".to_owned(),
                    Err(_) => "Couldn't check for updates. Try again later.".to_owned(),
                };
                if let Some(failure) = failure {
                    status = format!("The last update didn't install ({failure}). {status}");
                }
                board.set_update_available(matches!(result, Ok(Some(_))));
                board.set_update_busy(false);
                board.set_update_status(status.into());
                AVAILABLE_UPDATE.with(|slot| *slot.borrow_mut() = result.ok().flatten());
            });
        });
    });
    let weak = board.as_weak();
    board.on_install_update(move || {
        let Some(board) = weak.upgrade() else { return };
        let Some(release) = AVAILABLE_UPDATE.with(|slot| slot.borrow().clone()) else {
            return;
        };
        if board.get_update_busy() {
            return;
        }
        if updater::installed_exe().is_none() {
            board.set_update_status("Updates install only in the installed app.".into());
            return;
        }
        board.set_update_busy(true);
        board.set_update_status(format!("Downloading version {}…", release.version).into());
        let weak = weak.clone();
        std::thread::spawn(move || {
            let _apartment = calendar_worker::Apartment::multithreaded();
            let result = updater::download(&release);
            let _ = slint::invoke_from_event_loop(move || {
                let Some(board) = weak.upgrade() else { return };
                board.set_update_busy(false);
                match result {
                    Ok(msi) => board.invoke_finish_update(msi.to_string_lossy().as_ref().into()),
                    Err(error) => board
                        .set_update_status(format!("The update didn't download: {error}").into()),
                }
            });
        });
    });
    let weak = board.as_weak();
    let db = db.clone();
    let editors = editors.clone();
    board.on_finish_update(move |msi| {
        let Some(board) = weak.upgrade() else { return };
        let Some(installed) = updater::installed_exe() else {
            return;
        };
        // Same rule as Quit: never exit over unsaved notes.
        if !flush_all_editors(&db, &editors, &weak) {
            board.set_update_status(
                "A note couldn't be saved, so the update wasn't installed.".into(),
            );
            return;
        }
        match updater::start_install(std::path::Path::new(msi.as_str()), &installed) {
            Ok(()) => std::process::exit(0),
            Err(error) => {
                board.set_update_status(format!("The update couldn't start: {error}").into())
            }
        }
    });
}

/// Wire Settings' Google Calendar section to the background sync worker.
/// Status arrives on the worker thread and is applied on the UI thread.
#[cfg(windows)]
pub(crate) fn setup_calendar(board: &BoardWindow, db: &Arc<Mutex<Db>>) {
    use calendar_sync::ALERT_CHOICES;
    use calendar_worker::{CalendarWorker, ConnectOptions, SyncStatus};
    let alert_minutes = |index: i32| {
        ALERT_CHOICES
            .get(usize::try_from(index).unwrap_or(1))
            .copied()
            .unwrap_or(10)
    };
    board.set_calendar_configured(google_calendar::oauth_client().is_some());
    let weak = board.as_weak();
    let status_db = db.clone();
    let worker = CalendarWorker::start(db.clone(), move |status| {
        let weak = weak.clone();
        let db = status_db.clone();
        let _ = slint::invoke_from_event_loop(move || {
            let Some(board) = weak.upgrade() else {
                return;
            };
            let config = calendar_sync::load_config(&db::lock(&db).conn)
                .ok()
                .flatten();
            board.set_calendar_connected(config.is_some() && status != SyncStatus::ReconnectNeeded);
            if let Some(config) = config {
                let index = ALERT_CHOICES
                    .iter()
                    .position(|minutes| *minutes == config.alert_minutes)
                    .unwrap_or(1);
                board.set_calendar_alert_index(index as i32);
                board.set_calendar_include_text(config.include_note_text);
            }
            board.set_calendar_status(status.text().into());
        });
    });
    let weak = board.as_weak();
    let connect_worker = worker.clone();
    board.on_calendar_connect(move || {
        if let Some(board) = weak.upgrade() {
            connect_worker.connect(ConnectOptions {
                sync_existing: board.get_calendar_sync_existing(),
                alert_minutes: alert_minutes(board.get_calendar_alert_index()),
                include_note_text: board.get_calendar_include_text(),
            });
        }
    });
    let weak = board.as_weak();
    let options_db = db.clone();
    let options_worker = worker.clone();
    board.on_calendar_save_options(move || {
        let Some(board) = weak.upgrade() else {
            return;
        };
        match calendar_worker::update_options(
            &options_db,
            alert_minutes(board.get_calendar_alert_index()),
            board.get_calendar_include_text(),
        ) {
            Ok(()) => options_worker.settings_changed(),
            Err(error) => board.set_calendar_status(format!("Could not save: {error}").into()),
        }
    });
    let sync_worker = worker.clone();
    board.on_calendar_sync_now(move || sync_worker.sync_now());
    let weak = board.as_weak();
    board.on_calendar_disconnect(move || {
        if let Some(board) = weak.upgrade() {
            worker.disconnect(board.get_calendar_delete_calendar());
        }
    });
}

/// Persist board moves and resizes, debounced so a drag writes once.
#[cfg(windows)]
pub(crate) fn track_board_geometry(board: &BoardWindow, db: &Arc<Mutex<Db>>) {
    let timer = Rc::new(Timer::default());
    let weak = board.as_weak();
    let db = db.clone();
    board.window().on_winit_window_event(move |_, event| {
        use i_slint_backend_winit::winit::event::WindowEvent;
        if matches!(event, WindowEvent::Moved(_) | WindowEvent::Resized(_)) {
            let weak = weak.clone();
            let db = db.clone();
            timer.start(
                TimerMode::SingleShot,
                Duration::from_millis(500),
                move || {
                    let Some(board) = weak.upgrade() else {
                        return;
                    };
                    let minimized = board
                        .window()
                        .with_winit_window(|host| host.is_minimized().unwrap_or(false))
                        .unwrap_or(true);
                    if minimized || !board.window().is_visible() {
                        return;
                    }
                    let geometry = WindowGeometry::from_window(board.window());
                    let _ = settings::save_board_geometry(
                        &db::lock(&db).conn,
                        (
                            f64::from(geometry.x),
                            f64::from(geometry.y),
                            f64::from(geometry.width),
                            f64::from(geometry.height),
                        ),
                    );
                },
            );
        }
        WinitWindowEventResult::Propagate
    });
}

/// Surface newly due reminders once per occurrence, including after the app
/// wakes or starts with overdue notes. The board is the reliable action
/// surface until an installed Windows toast identity is available.
pub(crate) fn surface_unannounced_due(board: &BoardWindow, db: &Arc<Mutex<Db>>) {
    let rows = {
        let guard = db::lock(db);
        reminders::unannounced_due_at(&guard.conn, Local::now())
            .unwrap_or_default()
            .into_iter()
            .filter(|row| {
                notes::get(&guard.conn, &row.note_id)
                    .ok()
                    .flatten()
                    .is_some_and(|note| note.deleted_at.is_none())
            })
            .collect::<Vec<_>>()
    };
    if rows.is_empty() {
        return;
    }
    refresh_due_reminders(board, db);
    let mut fallback = Vec::new();
    {
        let guard = db::lock(db);
        for row in rows {
            let mut delivered = false;
            #[cfg(windows)]
            {
                let name = notes::get(&guard.conn, &row.note_id)
                    .ok()
                    .flatten()
                    .map(|note| {
                        calendar_sync::reminder_name(
                            &note.content,
                            note.title.as_deref(),
                            &row.chip_id,
                        )
                    });
                let (title, detail) = name.map_or_else(Default::default, |name| {
                    (name.title, name.note.unwrap_or_else(|| row.label.clone()))
                });
                match notification::show(&row, &title, &detail) {
                    Ok(shown) => delivered = shown,
                    Err(error) => eprintln!("Windows reminder delivery unavailable: {error:#}"),
                }
            }
            if delivered {
                let _ = reminders::mark_announced(&guard.conn, &row);
            } else {
                fallback.push(row);
            }
        }
    }
    if !fallback.is_empty() {
        board.set_reminder_panel_open(true);
        board.set_settings_open(false);
        if board.get_board_view() != "notes" {
            board.set_board_view("notes".into());
            let query = board.get_query().to_string();
            refresh(board, db, &query);
        }
        if show_board(board).is_ok() {
            let guard = db::lock(db);
            for row in fallback {
                let _ = reminders::mark_announced(&guard.conn, &row);
            }
        }
    }
}

#[cfg(windows)]
pub(crate) fn board_accepts_note_drop(board: &BoardWindow) -> bool {
    board.window().is_visible()
        && board.get_board_view() == "notes"
        && !board.get_settings_open()
        && board.get_pending_purge_id().is_empty()
}

#[cfg(windows)]
pub(crate) fn detach_note_at(
    board: &BoardWindow,
    db: &Arc<Mutex<Db>>,
    id: &str,
    windows: &Rc<RefCell<Vec<EditorEntry>>>,
    cursor: PhysicalPosition,
) -> Result<(), String> {
    let valid = notes::get(&db::lock(db).conn, id)
        .map_err(|error| error.to_string())?
        .is_some_and(|note| note.deleted_at.is_none() && !note.is_archived);
    if !valid {
        return Err("This note is no longer available in the notes list.".into());
    }
    open_editor(board, db, id, windows);
    let window = windows
        .borrow()
        .iter()
        .find(|entry| entry.id == id)
        .map(|entry| entry.window.clone_strong())
        .ok_or_else(|| "Could not open the detached note.".to_string())?;
    let position = PhysicalPosition::new(cursor.x.saturating_sub(24), cursor.y.saturating_sub(16));
    window.window().set_position(position);
    let geometry = WindowGeometry::from_window(window.window());
    notes::save_window_geom(
        &db::lock(db).conn,
        id,
        f64::from(position.x),
        f64::from(position.y),
        f64::from(geometry.width),
        f64::from(geometry.height),
        true,
    )
    .map_err(|error| format!("Note opened, but its position could not be saved: {error}"))?;
    Ok(())
}

#[cfg(windows)]
thread_local! {
    /// Read by the backend's window-attributes hook for the next window only.
    /// The drag preview must not take activation, or the board's pointer
    /// capture would end mid-drag. Winit only honours this on a first show,
    /// hence one new preview window per drag.
    pub(crate) static NEXT_WINDOW_INACTIVE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

#[cfg(windows)]
pub(crate) fn drag_ghost_position(cursor: PhysicalPosition) -> PhysicalPosition {
    // Same offset detach_note_at uses, so the preview shows where it lands.
    PhysicalPosition::new(cursor.x.saturating_sub(24), cursor.y.saturating_sub(16))
}

#[cfg(windows)]
pub(crate) fn show_drag_ghost(
    board: &BoardWindow,
    id: &str,
    cursor: PhysicalPosition,
) -> Option<DragGhostWindow> {
    let card = board.get_notes().iter().find(|card| card.id == id)?;
    NEXT_WINDOW_INACTIVE.with(|flag| flag.set(true));
    let ghost = DragGhostWindow::new();
    NEXT_WINDOW_INACTIVE.with(|flag| flag.set(false));
    let ghost = ghost.ok()?;
    ghost.set_title_text(card.title);
    ghost.set_card_color(card.color);
    ghost.window().set_position(drag_ghost_position(cursor));
    ghost.show().ok()?;
    ghost.window().set_position(drag_ghost_position(cursor));
    Some(ghost)
}

#[cfg(windows)]
pub(crate) fn setup_board_card_drag(
    board: &BoardWindow,
    db: &Arc<Mutex<Db>>,
    windows: &Rc<RefCell<Vec<EditorEntry>>>,
) {
    let origin = Rc::new(RefCell::new(None::<(String, PhysicalPosition, bool)>));
    let ghost = Rc::new(RefCell::new(None::<DragGhostWindow>));
    let begin_origin = origin.clone();
    let begin_ghost = ghost.clone();
    let weak = board.as_weak();
    board.on_begin_card_drag(move |id| {
        *begin_origin.borrow_mut() = None;
        if let Some(stale) = begin_ghost.borrow_mut().take() {
            let _ = stale.hide();
        }
        if let Some(board) = weak.upgrade() {
            board.set_card_drag_active(false);
            if board_accepts_note_drop(&board) {
                if let Some(cursor) = window_chrome::cursor_position() {
                    *begin_origin.borrow_mut() = Some((id.to_string(), cursor, false));
                }
            }
        }
    });
    let move_origin = origin.clone();
    let move_ghost = ghost.clone();
    let weak = board.as_weak();
    board.on_move_card_drag(move || {
        let Some(board) = weak.upgrade() else {
            return false;
        };
        let mut state = move_origin.borrow_mut();
        let Some((id, start, moved)) = state.as_mut() else {
            return false;
        };
        if let Some(cursor) = window_chrome::cursor_position() {
            *moved |= window_chrome::drag_moved(*start, cursor, board.window().scale_factor());
            if *moved {
                let mut ghost = move_ghost.borrow_mut();
                match ghost.as_ref() {
                    Some(window) => window.window().set_position(drag_ghost_position(cursor)),
                    None => *ghost = show_drag_ghost(&board, id, cursor),
                }
            }
        }
        board.set_card_drag_active(*moved);
        *moved
    });
    let weak = board.as_weak();
    let db = db.clone();
    let windows = windows.clone();
    board.on_end_card_drag(move |cancelled| {
        if let Some(window) = ghost.borrow_mut().take() {
            let _ = window.hide();
        }
        let state = origin.borrow_mut().take();
        let Some(board) = weak.upgrade() else {
            return true;
        };
        board.set_card_drag_active(false);
        let Some((id, start, mut moved)) = state else {
            return false;
        };
        if let Some(cursor) = window_chrome::cursor_position() {
            moved |= window_chrome::drag_moved(start, cursor, board.window().scale_factor());
            if window_chrome::drop_allowed(
                moved,
                board_accepts_note_drop(&board),
                cancelled,
                window_chrome::client_bounds(board.window()),
                cursor,
                window_chrome::DropDestination::OutOfBoard,
            ) {
                if let Err(error) = detach_note_at(&board, &db, &id, &windows, cursor) {
                    board.set_window_status(error.into());
                }
            }
        }
        moved
    });
}
