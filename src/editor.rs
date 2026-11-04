//! Note windows: opening an editor, keeping its UI in sync and saving it.

use super::*;

#[cfg(windows)]
pub(crate) fn native_document_from_editor(editor: &NoteWindow) -> NativeDocument {
    let formats = parse_format_ranges(&editor.get_editor_bold_ranges());
    NativeDocument {
        text: editor.get_editor_text().to_string(),
        bold: formats.bold,
        italic: formats.italic,
        underline: formats.underline,
        strike: formats.strike,
        chip_ids: serde_json::from_str(&editor.get_editor_chip_identities()).unwrap_or_default(),
    }
}

#[cfg(windows)]
pub(crate) fn apply_native_document_to_editor(editor: &NoteWindow, document: &NativeDocument) {
    editor.set_editor_chip_identities(serde_json::to_string(&document.chip_ids).unwrap().into());
    let formats = FormatRanges {
        bold: document.bold.clone(),
        italic: document.italic.clone(),
        underline: document.underline.clone(),
        strike: document.strike.clone(),
    };
    apply_editor_state(
        editor,
        &document.text,
        false,
        &serialize_format_ranges(&formats),
    );
}

pub(crate) fn apply_editor_state(editor: &NoteWindow, source: &str, bold: bool, bold_ranges: &str) {
    editor.set_editor_text(source.into());
    let preview = native_projection::Projection::from_source_visible(source, &[]).control;
    editor.set_date_preview_text(preview.into());
    editor.set_editor_bold(bold);
    let mut formats = parse_format_ranges(bold_ranges);
    canonicalize_formats(source, &mut formats);
    editor.set_editor_bold_ranges(serialize_format_ranges(&formats).into());
    let runs = parse_editor_runs_with_ranges(source, bold, &formats.bold)
        .into_iter()
        .map(|run| InlineRun {
            kind: run.kind.into(),
            text: run.text.into(),
            chip_label: run.chip_label.into(),
            bold: run.bold,
        })
        .collect::<Vec<_>>();
    editor.set_inline_runs(ModelRc::new(VecModel::from(runs)));
    let checklist_items = checklist_preview_items(source)
        .into_iter()
        .map(|item| ChecklistPreviewItem {
            line_index: item.line_index,
            text: item.text.into(),
            checked: item.checked,
        })
        .collect::<Vec<_>>();
    if !editor
        .get_checklist_items()
        .iter()
        .eq(checklist_items.iter().cloned())
    {
        editor.set_checklist_items(ModelRc::new(VecModel::from(checklist_items)));
    }
    sync_date_chip_model(editor, source);
}

pub(crate) fn sync_date_chip_model(editor: &NoteWindow, source: &str) {
    let chips = date_chip_occurrences(source)
        .into_iter()
        .map(|chip| DateChipItem {
            occurrence: chip.occurrence,
            date: chip.date.into(),
            label: chip.label.into(),
        })
        .collect::<Vec<_>>();
    if !editor.get_date_chips().iter().eq(chips.iter().cloned()) {
        editor.set_date_chips(ModelRc::new(VecModel::from(chips)));
    }
}

pub(crate) fn show_calendar(editor: &NoteWindow, year: i32, month: u32) {
    let first_day = date_picker::first_day_of_week();
    let Some(days) = date_picker::month_days(year, month, first_day) else {
        return;
    };
    let labels: Vec<slint::SharedString> = date_picker::weekday_letters(first_day)
        .into_iter()
        .map(Into::into)
        .collect();
    editor.set_week_labels(ModelRc::new(VecModel::from(labels)));
    editor.set_calendar_year(year);
    editor.set_calendar_month(month as i32);
    editor.set_calendar_caption(
        NaiveDate::from_ymd_opt(year, month, 1)
            .unwrap()
            .format("%B %Y")
            .to_string()
            .into(),
    );
    let selected = editor.get_date_input().to_string();
    let weeks = days
        .chunks(7)
        .map(|week| CalendarWeek {
            days: ModelRc::new(VecModel::from(
                week.iter()
                    .map(|day| CalendarDay {
                        label: day.day().to_string().into(),
                        date: day.format("%Y-%m-%d").to_string().into(),
                        selected: day.format("%Y-%m-%d").to_string() == selected,
                        in_month: day.month() == month,
                    })
                    .collect::<Vec<_>>(),
            )),
        })
        .collect::<Vec<_>>();
    editor.set_calendar_weeks(ModelRc::new(VecModel::from(weeks)));
}

pub(crate) fn initialize_date_picker(editor: &NoteWindow, value: &str) {
    let (value, recurrence) = value.split_once('@').unwrap_or((value, "none"));
    let (day, time) = value.split_once('T').unwrap_or((value, ""));
    let date =
        NaiveDate::parse_from_str(day, "%Y-%m-%d").unwrap_or_else(|_| Local::now().date_naive());
    editor.set_date_input(date.format("%Y-%m-%d").to_string().into());
    editor.set_date_time_input(time.into());
    editor.set_date_repeat(recurrence.into());
    editor.set_date_error("".into());
    show_calendar(editor, date.year(), date.month());
}

pub(crate) fn sync_editor_presentation(editor: &NoteWindow, note: &notes::NoteRow) {
    editor.set_inline_image_mode(
        serde_json::from_str::<serde_json::Value>(&note.content)
            .ok()
            .and_then(|value| {
                value
                    .pointer("/editor/inlineImages")
                    .and_then(|v| v.as_bool())
            })
            .unwrap_or(false),
    );
    editor.set_note_title(
        note.title
            .clone()
            .unwrap_or_else(|| "Untitled note".into())
            .into(),
    );
    editor.set_note_color(color(&note.color));
    editor.set_note_color_value(note.color.clone().into());
    editor.set_note_always_on_top(note.is_always_on_top);
    editor.set_save_status("".into());
    let (source, bold, bold_ranges) = editor_state_from_content_with_checklists(&note.content);
    editor.set_editor_chip_identities(
        serde_json::to_string(&native_projection::content_ids(&note.content))
            .unwrap()
            .into(),
    );
    apply_editor_state(editor, &source, bold, &bold_ranges);
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct EditorSnapshot {
    pub(crate) content: String,
    pub(crate) color: String,
}

pub(crate) fn editor_snapshot(editor: &NoteWindow) -> EditorSnapshot {
    let text = editor.get_editor_text().to_string();
    let bold = editor.get_editor_bold();
    let formats = parse_format_ranges(&editor.get_editor_bold_ranges());
    let content = native_projection::with_ids(
        &content_for_editor_with_checklists_and_formats(&text, bold, &formats),
        &serde_json::from_str::<Vec<String>>(&editor.get_editor_chip_identities())
            .unwrap_or_default(),
    );
    let content = if editor.get_inline_image_mode() {
        let mut value: serde_json::Value = serde_json::from_str(&content).unwrap();
        value["editor"]["inlineImages"] = true.into();
        value.to_string()
    } else {
        content
    };
    EditorSnapshot {
        content,
        color: editor.get_note_color_value().to_string(),
    }
}

pub(crate) fn persist_snapshot(
    db: &Arc<Mutex<Db>>,
    note_id: &str,
    snapshot: &EditorSnapshot,
) -> Result<(), String> {
    let result = {
        let guard = db::lock(db);
        notes::save_content_and_color(
            &guard.conn,
            note_id,
            &snapshot.content,
            Some(&snapshot.color),
        )
        .map_err(|error| error.to_string())
    };
    result
}

pub(crate) fn mark_editor_dirty(editor: &NoteWindow, state: &Rc<RefCell<EditorSaveState>>) {
    state.borrow_mut().dirty = true;
    editor.set_save_status("Saving…".into());
}

pub(crate) fn reconcile_formats_after_text_edit(
    editor: &NoteWindow,
    state: &Rc<RefCell<EditorSaveState>>,
) {
    let current = editor.get_editor_text().to_string();
    let mut state = state.borrow_mut();
    if state.last_text == current {
        return;
    }
    let mut formats = parse_format_ranges(&editor.get_editor_bold_ranges());
    remap_formats_after_edit(&state.last_text, &current, &mut formats);
    editor.set_editor_bold_ranges(serialize_format_ranges(&formats).into());
    sync_date_chip_model(editor, &current);
    state.last_text = current;
}

pub(crate) fn remember_editor_text(state: &Rc<RefCell<EditorSaveState>>, text: &str) {
    state.borrow_mut().last_text = text.to_owned();
}

#[cfg(windows)]
pub(crate) fn flush_pending_native_change(state: &Rc<RefCell<EditorSaveState>>) {
    let native = state
        .borrow()
        .native_editor
        .as_ref()
        .and_then(Weak::upgrade);
    if let Some(native) = native {
        native.flush_pending_change();
    }
}

/// Save only the editor's current model values. In particular, do not call
/// apply_editor_state here: writing TextEdit.text during a timer callback
/// resets the native caret and makes normal typing feel broken.
pub(crate) fn flush_editor(
    db: &Arc<Mutex<Db>>,
    note_id: &str,
    editor: &NoteWindow,
    state: &Rc<RefCell<EditorSaveState>>,
    board: &slint::Weak<BoardWindow>,
) -> bool {
    #[cfg(windows)]
    flush_pending_native_change(state);
    if !state.borrow().dirty {
        return true;
    }
    let snapshot = editor_snapshot(editor);
    match persist_snapshot(db, note_id, &snapshot) {
        Ok(()) => {
            state.borrow_mut().dirty = false;
            editor.set_save_status("Saved".into());
            // The SQLite lock was released by persist_snapshot before either
            // refresh path can acquire it again.
            if let Some(board) = board.upgrade() {
                let query = board.get_query().to_string();
                refresh(&board, db, &query);
                refresh_due_reminders(&board, db);
            }
            true
        }
        Err(error) => {
            // Keep dirty=true so a later explicit Save, edit, or close retries
            // the exact in-memory contents instead of pretending they landed.
            state.borrow_mut().dirty = true;
            editor.set_save_status(format!("Couldn't save; edits stay open ({error})").into());
            false
        }
    }
}

pub(crate) fn prepare_editor_close(
    db: &Arc<Mutex<Db>>,
    note_id: &str,
    editor: &NoteWindow,
    state: &Rc<RefCell<EditorSaveState>>,
    board: &slint::Weak<BoardWindow>,
) -> bool {
    if !flush_editor(db, note_id, editor, state, board) {
        return false;
    }
    let geometry = WindowGeometry::from_window(editor.window());
    let result = notes::save_window_geom(
        &db::lock(db).conn,
        note_id,
        f64::from(geometry.x),
        f64::from(geometry.y),
        f64::from(geometry.width),
        f64::from(geometry.height),
        false,
    );
    match result {
        Ok(()) => {
            state.borrow().timer.stop();
            discard_blank_note(db, note_id, board);
            true
        }
        Err(error) => {
            editor.set_save_status(format!("Couldn't close; note stays open ({error})").into());
            false
        }
    }
}

/// A note closed with nothing in it leaves no card behind.
pub(crate) fn discard_blank_note(
    db: &Arc<Mutex<Db>>,
    note_id: &str,
    board: &slint::Weak<BoardWindow>,
) {
    let discarded = notes::discard_if_blank(&db::lock(db).conn, note_id);
    if matches!(discarded, Ok(true)) {
        if let Some(board) = board.upgrade() {
            let query = board.get_query().to_string();
            refresh(&board, db, &query);
        }
    }
}

pub(crate) fn schedule_autosave(
    state: &Rc<RefCell<EditorSaveState>>,
    db: Arc<Mutex<Db>>,
    note_id: String,
    editor: slint::Weak<NoteWindow>,
    board: slint::Weak<BoardWindow>,
) {
    let weak_state: Weak<RefCell<EditorSaveState>> = Rc::downgrade(state);
    state
        .borrow()
        .timer
        .start(TimerMode::SingleShot, AUTOSAVE_DELAY, move || {
            let (Some(state), Some(editor)) = (weak_state.upgrade(), editor.upgrade()) else {
                return;
            };
            flush_editor(&db, &note_id, &editor, &state, &board);
        });
}

pub(crate) fn sync_attachments(
    editor: &NoteWindow,
    conn: &rusqlite::Connection,
    id: &str,
    index: i32,
) {
    editor.set_attachment_remove_pending(false);
    match attachments::list(conn, id) {
        Ok(items) => {
            editor.set_attachment_count(items.len() as i32);
            editor.set_attachment_preview(slint::Image::default());
            editor.set_attachment_id("".into());
            if items.is_empty() {
                editor.set_attachment_caption("".into());
                return;
            }
            let index = index.rem_euclid(items.len() as i32);
            editor.set_attachment_index(index);
            let item = &items[index as usize];
            editor.set_attachment_id(item.id.clone().into());
            editor.set_attachment_caption(
                format!("{} / {} · {}", index + 1, items.len(), item.name).into(),
            );
            if editor.get_native_editor_active() {
                return;
            }
            match attachments::thumbnail(conn, &item.id) {
                Ok(image) => editor.set_attachment_preview(image),
                Err(error) => {
                    editor.set_format_status(format!("Image preview unavailable: {error}").into())
                }
            }
        }
        Err(error) => editor.set_format_status(format!("Couldn't load images: {error}").into()),
    }
}

#[cfg(windows)]
pub(crate) fn insert_native_image(
    window: &NoteWindow,
    native: &NativeRichEditor,
    db: &Arc<Mutex<Db>>,
    id: &str,
    prepared: attachments::PreparedImage,
) -> Result<(), String> {
    let image_id = prepared.item.id.clone();
    let candidate = native.document_with_image(&image_id);
    let formats = FormatRanges {
        bold: candidate.bold.clone(),
        italic: candidate.italic.clone(),
        underline: candidate.underline.clone(),
        strike: candidate.strike.clone(),
    };
    let content = native_projection::with_ids(
        &content_for_editor_with_checklists_and_formats(&candidate.text, false, &formats),
        &candidate.chip_ids,
    );
    let mut content: serde_json::Value = serde_json::from_str(&content).unwrap();
    content["editor"]["inlineImages"] = true.into();
    attachments::commit_prepared(&db::lock(db).conn, id, prepared, Some(&content.to_string()))
        .map_err(|error| error.to_string())?;
    window.set_inline_image_mode(true);
    native.commit_image_document(&candidate, &image_id);
    if !native.image_errors().is_empty() {
        window.set_format_status(
            format!(
                "Image saved; preview unavailable: {}",
                native.image_errors().join("; ")
            )
            .into(),
        );
    }
    Ok(())
}

pub(crate) fn open_editor(
    board: &BoardWindow,
    db: &Arc<Mutex<Db>>,
    id: &str,
    windows: &Rc<RefCell<Vec<EditorEntry>>>,
) {
    // Reuse an already-open editor. This makes repeated board clicks and a
    // board reopened from a note focus the existing native window instead of
    // creating duplicate editors for the same note.
    if let Some(entry) = windows.borrow().iter().find(|entry| entry.id == id) {
        sync_attachments(
            &entry.window,
            &db::lock(db).conn,
            id,
            entry.window.get_attachment_index(),
        );
        // Hidden Slint windows can retain a stale scene/model snapshot. Reload
        // all persisted properties before showing a reused editor, then ask the
        // backend for an explicit redraw (otherwise hover can be the first event
        // that causes the preview and checklist models to appear).
        // A failed save deliberately leaves the editor dirty. Reloading from
        // SQLite here would overwrite those visible, retryable edits.
        if !entry.save_state.borrow().dirty {
            let note = {
                let guard = db::lock(db);
                notes::get(&guard.conn, id).ok().flatten()
            };
            if let Some(note) = note {
                sync_editor_presentation(&entry.window, &note);
            }
        }
        let _ = entry.window.show();
        let geometry = WindowGeometry::from_window(entry.window.window());
        let _ = notes::save_window_geom(
            &db::lock(db).conn,
            id,
            f64::from(geometry.x),
            f64::from(geometry.y),
            f64::from(geometry.width),
            f64::from(geometry.height),
            true,
        );
        entry.window.window().request_redraw();
        return;
    }
    let note = {
        let guard = db::lock(db);
        notes::get(&guard.conn, id).ok().flatten()
    };
    let Some(note) = note else {
        return;
    };
    let editor = match NoteWindow::new() {
        Ok(window) => window,
        Err(_) => return,
    };
    #[cfg(windows)]
    editor.set_frameless(true);
    sync_editor_presentation(&editor, &note);
    sync_attachments(&editor, &db::lock(db).conn, id, 0);
    if let Some(geometry) = WindowGeometry::from_db(note.window) {
        geometry.apply(editor.window());
    }
    let weak_editor = editor.as_weak();
    let weak_board = board.as_weak();
    let save_state = Rc::new(RefCell::new(EditorSaveState {
        timer: Timer::default(),
        dirty: false,
        last_text: editor.get_editor_text().to_string(),
        #[cfg(windows)]
        native_editor: None,
    }));
    #[cfg(windows)]
    // Callbacks are installed before the child is attached. This lets the
    // RichEdit notification path use the same debounced SQLite save pipeline
    // as the rest of the note, without a second event loop.
    let native_editor_slot: Rc<RefCell<Option<Rc<NativeRichEditor>>>> = Rc::new(RefCell::new(None));
    #[cfg(windows)]
    {
        let native_for_visibility = native_editor_slot.clone();
        let redraw_window = editor.as_weak();
        editor.on_set_native_editor_visible(move |visible| {
            if let Some(native) = native_for_visibility.borrow().as_ref() {
                native.set_visible(visible);
            }
            if let Some(window) = redraw_window.upgrade() {
                window.window().request_redraw();
            }
        });
        let native_for_history = native_editor_slot.clone();
        let native_for_focus = native_editor_slot.clone();
        editor.on_focus_native_editor(move || {
            if let Some(native) = native_for_focus.borrow().as_ref() {
                native.focus();
            }
        });
        editor.on_editor_history_action(move |redo| {
            if let Some(native) = native_for_history.borrow().as_ref() {
                native.history_action(redo);
            }
        });
        let tip_window = editor.as_weak();
        let tip_timer = Rc::new(Timer::default());
        let tip: Rc<RefCell<Option<tooltip::Tooltip>>> = Rc::new(RefCell::new(None));
        editor.on_toolbar_tip(move |text| {
            tip_timer.stop();
            if text.is_empty() {
                if let Some(tip) = tip.borrow().as_ref() {
                    tip.hide();
                }
                return;
            }
            let tip = tip.clone();
            let window = tip_window.clone();
            tip_timer.start(
                TimerMode::SingleShot,
                std::time::Duration::from_millis(500),
                move || {
                    let mut slot = tip.borrow_mut();
                    if slot.is_none() {
                        *slot = window.upgrade().and_then(|window| {
                            native_rich_editor::parent_hwnd(window.window())
                                .ok()
                                .and_then(tooltip::Tooltip::new)
                        });
                    }
                    if let Some(tip) = slot.as_ref() {
                        tip.show(&text);
                    }
                },
            );
        });
    }

    let weak_saved_editor = weak_editor.clone();
    let weak_saved_board = weak_board.clone();
    let db_save = db.clone();
    let note_id = id.to_string();
    let save_state_for_save = save_state.clone();
    editor.on_save(move |text, bold, bold_ranges| {
        let _ = (text, bold, bold_ranges);
        if let Some(editor) = weak_saved_editor.upgrade() {
            mark_editor_dirty(&editor, &save_state_for_save);
            flush_editor(
                &db_save,
                &note_id,
                &editor,
                &save_state_for_save,
                &weak_saved_board,
            );
        }
    });
    {
        let weak = weak_editor.clone();
        let db = db.clone();
        let id = id.to_owned();
        let state = save_state.clone();
        let board = weak_board.clone();
        #[cfg(windows)]
        let native_for_image = native_editor_slot.clone();
        editor.on_add_image(move || {
            let Some(window) = weak.upgrade() else {
                return;
            };
            window.invoke_dismiss_date_panel();
            if !flush_editor(&db, &id, &window, &state, &board) {
                return;
            }
            let result = image_picker::select(window.window()).and_then(|path| {
                if let Some(path) = path {
                    #[cfg(windows)]
                    if let Some(native) = native_for_image.borrow().as_ref() {
                        let prepared = attachments::prepare_file(&path)?;
                        insert_native_image(&window, native, &db, &id, prepared)
                            .map_err(anyhow::Error::msg)?;
                        flush_editor(&db, &id, &window, &state, &board);
                        return Ok(());
                    }
                    let guard = db::lock(&db);
                    attachments::import_image(&guard.conn, &id, &path)?;
                    let count = attachments::list(&guard.conn, &id)?.len() as i32;
                    sync_attachments(&window, &guard.conn, &id, count - 1);
                    window.set_format_status("Image saved locally.".into());
                }
                Ok(())
            });
            if let Err(error) = result {
                window.set_format_status(format!("Couldn't add image: {error}").into());
            }
        });
    }
    {
        let weak = weak_editor.clone();
        let db = db.clone();
        let id = id.to_owned();
        editor.on_browse_image(move |direction| {
            if let Some(window) = weak.upgrade() {
                sync_attachments(
                    &window,
                    &db::lock(&db).conn,
                    &id,
                    window.get_attachment_index() + direction,
                );
            }
        });
    }
    {
        let weak = weak_editor.clone();
        let db = db.clone();
        let id = id.to_owned();
        editor.on_remove_image(move || {
            if let Some(window) = weak.upgrade() {
                let guard = db::lock(&db);
                let result =
                    attachments::remove(&guard.conn, &id, window.get_attachment_id().as_str());
                sync_attachments(&window, &guard.conn, &id, window.get_attachment_index());
                window.set_format_status(match result {
                    Ok(()) => "Image removed.".into(),
                    Err(error) => format!("Image cleanup needs retry: {error}").into(),
                });
            }
        });
    }
    #[cfg(windows)]
    {
        let drag_origin = Rc::new(RefCell::new(None));
        let weak = weak_editor.clone();
        let origin = drag_origin.clone();
        let drag_board = board.as_weak();
        editor.on_begin_drag(move || {
            if let (Some(window), Some(cursor)) = (weak.upgrade(), window_chrome::cursor_position())
            {
                let geometry = WindowGeometry::from_window(window.window());
                *origin.borrow_mut() =
                    Some((PhysicalPosition::new(geometry.x, geometry.y), cursor));
                if let Some(board) = drag_board.upgrade() {
                    board.set_note_drop_active(board_accepts_note_drop(&board));
                }
            }
        });
        let weak = weak_editor.clone();
        let origin = drag_origin.clone();
        editor.on_move_drag(move || {
            if let (Some(window), Some((position, start)), Some(cursor)) = (
                weak.upgrade(),
                *origin.borrow(),
                window_chrome::cursor_position(),
            ) {
                window
                    .window()
                    .set_position(window_chrome::dragged_position(position, start, cursor));
            }
        });
        let weak = weak_editor.clone();
        let drag_board = board.as_weak();
        editor.on_end_drag(move |cancelled| {
            let origin = drag_origin.borrow_mut().take();
            if let Some(board) = drag_board.upgrade() {
                board.set_note_drop_active(false);
                if let (Some(window), Some((_, start)), Some(cursor)) =
                    (weak.upgrade(), origin, window_chrome::cursor_position())
                {
                    if window_chrome::drop_allowed(
                        window_chrome::drag_moved(start, cursor, window.window().scale_factor()),
                        board_accepts_note_drop(&board),
                        cancelled,
                        window_chrome::client_bounds(board.window()),
                        cursor,
                        window_chrome::DropDestination::IntoBoard,
                    ) {
                        window.invoke_return_to_board();
                    }
                }
            }
        });
    }
    let weak_close = weak_editor.clone();
    let weak_close_board = weak_board.clone();
    let db_close = db.clone();
    let close_id = id.to_owned();
    let save_state_for_close = save_state.clone();
    editor.on_close_note(move || {
        if let Some(window) = weak_close.upgrade() {
            if prepare_editor_close(
                &db_close,
                &close_id,
                &window,
                &save_state_for_close,
                &weak_close_board,
            ) {
                let _ = window.hide();
            }
        }
    });
    let weak_native_close = weak_editor.clone();
    let weak_native_close_board = weak_board.clone();
    let db_native_close = db.clone();
    let native_close_id = id.to_owned();
    let save_state_for_native_close = save_state.clone();
    editor.window().on_close_requested(move || {
        if let Some(window) = weak_native_close.upgrade() {
            if prepare_editor_close(
                &db_native_close,
                &native_close_id,
                &window,
                &save_state_for_native_close,
                &weak_native_close_board,
            ) {
                CloseRequestResponse::HideWindow
            } else {
                CloseRequestResponse::KeepWindowShown
            }
        } else {
            CloseRequestResponse::HideWindow
        }
    });
    let weak_board_button = board.as_weak();
    let db_open_board = db.clone();
    editor.on_open_board(move || {
        if let Some(board) = weak_board_button.upgrade() {
            refresh_due_reminders(&board, &db_open_board);
            let _ = show_board(&board);
        }
    });
    let weak_return = editor.as_weak();
    let weak_return_board = board.as_weak();
    let db_return = db.clone();
    let return_id = id.to_owned();
    let save_state_return = save_state.clone();
    editor.on_return_to_board(move || {
        if let Some(window) = weak_return.upgrade() {
            if prepare_editor_close(
                &db_return,
                &return_id,
                &window,
                &save_state_return,
                &weak_return_board,
            ) && window.hide().is_ok()
            {
                if let Some(board) = weak_return_board.upgrade() {
                    board.set_board_view("notes".into());
                    board.set_query("".into());
                    refresh(&board, &db_return, "");
                    let _ = show_board(&board);
                }
            }
        }
    });
    let weak_board_new_note = board.as_weak();
    editor.on_create_note(move || {
        if let Some(board) = weak_board_new_note.upgrade() {
            board.invoke_create_note();
        }
    });
    // Keep deletion on the established board path. It flushes this editor,
    // stops any queued autosave, refreshes the board, and hides the window;
    // the note menu therefore cannot bypass its persistence safeguards.
    let weak_board_delete_note = board.as_weak();
    let delete_note_id = id.to_owned();
    editor.on_delete_note(move || {
        if let Some(board) = weak_board_delete_note.upgrade() {
            board.invoke_delete_note(delete_note_id.clone().into());
        }
    });
    let weak_color = editor.as_weak();
    let weak_color_board = weak_board.clone();
    let db_color = db.clone();
    let color_id = id.to_owned();
    let save_state_for_color = save_state.clone();
    #[cfg(windows)]
    let native_for_color = native_editor_slot.clone();
    editor.on_choose_color(move |value| {
        if let Some(window) = weak_color.upgrade() {
            window.set_note_color(color(&value));
            #[cfg(windows)]
            if let Some(native) = native_for_color.borrow().as_ref() {
                native.set_palette(&value);
            }
            window.set_note_color_value(value);
            mark_editor_dirty(&window, &save_state_for_color);
            flush_editor(
                &db_color,
                &color_id,
                &window,
                &save_state_for_color,
                &weak_color_board,
            );
        }
    });
    // The panel is part of the note layout, so it cannot escape a resized
    // sticky window. Opening it twice has the conventional toggle behaviour.
    let weak_date_toggle = editor.as_weak();
    let save_state_for_date_toggle = save_state.clone();
    #[cfg(windows)]
    let native_for_date_toggle = native_editor_slot.clone();
    editor.on_toggle_date_panel(move || {
        if let Some(window) = weak_date_toggle.upgrade() {
            let opening = !window.get_date_panel_open();
            window.set_date_panel_open(opening);
            #[cfg(windows)]
            if let Some(native) = native_for_date_toggle.borrow().as_ref() {
                // A native child HWND always paints above Slint's canvas, so
                // suspend it while the canvas-based date overlay is visible.
                native.set_visible(!opening);
            }
            if opening {
                #[cfg(windows)]
                flush_pending_native_change(&save_state_for_date_toggle);
                window.set_selected_date_occurrence(-1);
                initialize_date_picker(
                    &window,
                    &Local::now().date_naive().format("%Y-%m-%d").to_string(),
                );
                window.set_date_anchor_x(12.0);
                window.set_date_anchor_y(24.0);
                #[cfg(windows)]
                if let Some(native) = native_for_date_toggle.borrow().as_ref() {
                    let (_, cursor) = native.selection_byte_offsets(&window.get_editor_text());
                    window.set_date_insert_offset(cursor);
                }
            }
        }
    });
    let weak_date_dismiss = editor.as_weak();
    #[cfg(windows)]
    let native_for_date_dismiss = native_editor_slot.clone();
    editor.on_dismiss_date_panel(move || {
        if let Some(window) = weak_date_dismiss.upgrade() {
            window.set_date_panel_open(false);
            window.set_selected_date_occurrence(-1);
            window.window().request_redraw();
            #[cfg(windows)]
            if let Some(native) = native_for_date_dismiss.borrow().as_ref() {
                native.set_visible(true);
            }
        }
    });
    let weak_date_select = editor.as_weak();
    #[cfg(windows)]
    let save_state_for_date_select = save_state.clone();
    editor.on_select_date_occurrence(move |occurrence| {
        if let Some(window) = weak_date_select.upgrade() {
            #[cfg(windows)]
            flush_pending_native_change(&save_state_for_date_select);
            if let Some(chip) = date_chip_occurrences(&window.get_editor_text())
                .into_iter()
                .find(|chip| chip.occurrence == occurrence)
            {
                window.set_selected_date_occurrence(occurrence);
                initialize_date_picker(&window, &chip.date);
            }
        }
    });
    let month_window = editor.as_weak();
    editor.on_move_calendar_month(move |delta| {
        if let Some(window) = month_window.upgrade() {
            if let Some((year, month)) = date_picker::shift_month(
                window.get_calendar_year(),
                window.get_calendar_month() as u32,
                delta,
            ) {
                show_calendar(&window, year, month);
            }
        }
    });
    let day_window = editor.as_weak();
    editor.on_pick_calendar_day(move |date| {
        if let Some(window) = day_window.upgrade() {
            window.set_date_input(date);
            window.set_date_error("".into());
            show_calendar(
                &window,
                window.get_calendar_year(),
                window.get_calendar_month() as u32,
            );
        }
    });
    let weak_date_apply = editor.as_weak();
    let weak_date_apply_board = weak_board.clone();
    let db_date_apply = db.clone();
    let date_apply_id = id.to_owned();
    let save_state_for_date_apply = save_state.clone();
    #[cfg(windows)]
    let native_for_date_apply = native_editor_slot.clone();
    editor.on_apply_date_selection(move || {
        let Some(window) = weak_date_apply.upgrade() else {
            return;
        };
        #[cfg(windows)]
        flush_pending_native_change(&save_state_for_date_apply);
        let source = window.get_editor_text().to_string();
        let day = window.get_date_input().trim().to_owned();
        let time = window.get_date_time_input().trim().to_owned();
        let date = if time.is_empty() {
            day
        } else if let Some(time) = normalize_time(&time) {
            format!("{day}T{time}")
        } else {
            window.set_date_error("Enter a time like 14:30 or 2:30 pm.".into());
            return;
        };
        let repeat = window.get_date_repeat();
        let date = if repeat == "none" {
            date
        } else {
            format!("{date}@{repeat}")
        };
        let selected = window.get_selected_date_occurrence();
        let mut ids: Vec<String> =
            serde_json::from_str(&window.get_editor_chip_identities()).unwrap_or_default();
        while ids.len() < date_chip_occurrences(&source).len() {
            ids.push(Uuid::new_v4().to_string());
        }
        let updated = if selected >= 0 {
            replace_date_chip(&source, selected, &date)
        } else {
            let offset = usize::try_from(window.get_date_insert_offset())
                .ok()
                .filter(|offset| *offset <= source.len() && source.is_char_boundary(*offset))
                .unwrap_or(source.len());
            let inserted = append_date_chip(&source[..offset], &date).map(|prefix| {
                format!(
                    "{prefix}{}{}",
                    if source[offset..]
                        .chars()
                        .next()
                        .is_some_and(|character| !character.is_whitespace())
                    {
                        " "
                    } else {
                        ""
                    },
                    &source[offset..]
                )
            });
            if inserted.is_ok() {
                ids.insert(
                    date_chip_occurrences(&source)
                        .iter()
                        .filter(|chip| chip.start < offset)
                        .count(),
                    Uuid::new_v4().to_string(),
                );
            }
            inserted
        };
        match updated {
            Ok(updated) => {
                let bold = window.get_editor_bold();
                let mut formats = parse_format_ranges(&window.get_editor_bold_ranges());
                remap_formats_after_edit(&source, &updated, &mut formats);
                window.set_editor_chip_identities(serde_json::to_string(&ids).unwrap().into());
                apply_editor_state(&window, &updated, bold, &serialize_format_ranges(&formats));
                #[cfg(windows)]
                if let Some(native) = native_for_date_apply.borrow().as_ref() {
                    native.set_document(&native_document_from_editor(&window));
                    native.set_visible(true);
                }
                remember_editor_text(&save_state_for_date_apply, &updated);
                window.set_date_panel_open(false);
                window.set_selected_date_occurrence(-1);
                window.set_format_status(
                    if selected >= 0 {
                        "Date updated and saved."
                    } else {
                        "Date added and saved."
                    }
                    .into(),
                );
                mark_editor_dirty(&window, &save_state_for_date_apply);
                flush_editor(
                    &db_date_apply,
                    &date_apply_id,
                    &window,
                    &save_state_for_date_apply,
                    &weak_date_apply_board,
                );
            }
            Err(message) => window.set_date_error(message.into()),
        }
    });
    let weak_date_remove = editor.as_weak();
    let weak_date_remove_board = weak_board.clone();
    let db_date_remove = db.clone();
    let date_remove_id = id.to_owned();
    let save_state_for_date_remove = save_state.clone();
    #[cfg(windows)]
    let native_for_date_remove = native_editor_slot.clone();
    editor.on_remove_selected_date(move || {
        let Some(window) = weak_date_remove.upgrade() else {
            return;
        };
        #[cfg(windows)]
        flush_pending_native_change(&save_state_for_date_remove);
        let source = window.get_editor_text().to_string();
        match remove_date_chip(&source, window.get_selected_date_occurrence()) {
            Ok(updated) => {
                let bold = window.get_editor_bold();
                let mut ids: Vec<String> =
                    serde_json::from_str(&window.get_editor_chip_identities()).unwrap_or_default();
                let selected = window.get_selected_date_occurrence() as usize;
                if selected < ids.len() {
                    ids.remove(selected);
                }
                window.set_editor_chip_identities(serde_json::to_string(&ids).unwrap().into());
                let mut formats = parse_format_ranges(&window.get_editor_bold_ranges());
                remap_formats_after_edit(&source, &updated, &mut formats);
                apply_editor_state(&window, &updated, bold, &serialize_format_ranges(&formats));
                #[cfg(windows)]
                if let Some(native) = native_for_date_remove.borrow().as_ref() {
                    native.set_document(&native_document_from_editor(&window));
                    native.set_visible(true);
                }
                remember_editor_text(&save_state_for_date_remove, &updated);
                window.set_date_panel_open(false);
                window.set_selected_date_occurrence(-1);
                window.set_format_status("Date removed and saved.".into());
                mark_editor_dirty(&window, &save_state_for_date_remove);
                flush_editor(
                    &db_date_remove,
                    &date_remove_id,
                    &window,
                    &save_state_for_date_remove,
                    &weak_date_remove_board,
                );
            }
            Err(message) => window.set_format_status(message.into()),
        }
    });
    let weak_text_edited = editor.as_weak();
    let weak_text_board = weak_board.clone();
    let db_text = db.clone();
    let text_id = id.to_owned();
    let save_state_for_text = save_state.clone();
    editor.on_editor_text_edited(move |_| {
        if let Some(window) = weak_text_edited.upgrade() {
            window.set_format_status("".into());
            reconcile_formats_after_text_edit(&window, &save_state_for_text);
            mark_editor_dirty(&window, &save_state_for_text);
            schedule_autosave(
                &save_state_for_text,
                db_text.clone(),
                text_id.clone(),
                weak_text_edited.clone(),
                weak_text_board.clone(),
            );
        }
    });
    let weak_selection_style = editor.as_weak();
    let weak_selection_style_board = weak_board.clone();
    let db_selection_style = db.clone();
    let selection_style_id = id.to_owned();
    let save_state_for_selection_style = save_state.clone();
    #[cfg(windows)]
    let native_for_selection_style = native_editor_slot.clone();
    editor.on_toggle_selection_style(move |style, anchor, cursor| {
        if let Some(window) = weak_selection_style.upgrade() {
            #[cfg(windows)]
            if let Some(native) = native_for_selection_style.borrow().as_ref() {
                if let Err(message) = native.toggle_selection_style(&style) {
                    window.set_format_status(message.into());
                }
                return;
            }
            let source = window.get_editor_text().to_string();
            let mut formats = parse_format_ranges(&window.get_editor_bold_ranges());
            let start = usize::try_from(anchor.min(cursor)).unwrap_or_default();
            let end = usize::try_from(anchor.max(cursor)).unwrap_or_default();
            match set_selection_style(&source, &mut formats, &style, start, end) {
                Ok(applied) => {
                    window.set_editor_bold_ranges(serialize_format_ranges(&formats).into());
                    let action = if applied { "applied" } else { "removed" };
                    let label = match style.as_str() {
                        "bold" => "Bold",
                        "italic" => "Italic",
                        "underline" => "Underline",
                        "strike" => "Strike-through",
                        _ => "Style",
                    };
                    window.set_format_status(
                        format!("{label} {action} to selection and saved. Inline rendering follows the native editor upgrade.").into(),
                    );
                    mark_editor_dirty(&window, &save_state_for_selection_style);
                    flush_editor(
                        &db_selection_style,
                        &selection_style_id,
                        &window,
                        &save_state_for_selection_style,
                        &weak_selection_style_board,
                    );
                }
                Err(message) => window.set_format_status(message.into()),
            }
        }
    });
    let weak_list_command = editor.as_weak();
    let weak_list_board = weak_board.clone();
    let db_list_command = db.clone();
    let list_command_id = id.to_owned();
    let save_state_for_list_command = save_state.clone();
    #[cfg(windows)]
    let native_for_list_command = native_editor_slot.clone();
    editor.on_apply_list_command(move |command, anchor, cursor| {
        let Some(window) = weak_list_command.upgrade() else {
            return;
        };
        #[cfg(windows)]
        flush_pending_native_change(&save_state_for_list_command);
        let source = window.get_editor_text().to_string();
        #[cfg(windows)]
        let (anchor, cursor) = native_for_list_command
            .borrow()
            .as_ref()
            .map(|native| native.selection_byte_offsets(&source))
            .unwrap_or((anchor, cursor));
        let start = usize::try_from(anchor).unwrap_or(usize::MAX);
        let end = usize::try_from(cursor).unwrap_or(usize::MAX);
        let Some(result) = list_command(&source, &command, start, end) else {
            return;
        };
        let mut formats = parse_format_ranges(&window.get_editor_bold_ranges());
        remap_formats_after_replacements(&source, &result.replacements, &mut formats);
        let serialized = serialize_format_ranges(&formats);
        apply_editor_state(
            &window,
            &result.source,
            window.get_editor_bold(),
            &serialized,
        );
        #[cfg(windows)]
        if let Some(native) = native_for_list_command.borrow().as_ref() {
            native.set_document(&native_document_from_editor(&window));
            native.restore_selection(&result.source, result.caret, result.caret);
        }
        remember_editor_text(&save_state_for_list_command, &result.source);
        window.set_format_status("".into());
        mark_editor_dirty(&window, &save_state_for_list_command);
        flush_editor(
            &db_list_command,
            &list_command_id,
            &window,
            &save_state_for_list_command,
            &weak_list_board,
        );
        window.set_editor_list_command_offset(i32::try_from(result.caret).unwrap_or(-1));
    });
    let weak_always_on_top = editor.as_weak();
    let db_always_on_top = db.clone();
    let always_on_top_id = id.to_owned();
    editor.on_toggle_always_on_top(move || {
        if let Some(window) = weak_always_on_top.upgrade() {
            let value = !window.get_note_always_on_top();
            window.set_note_always_on_top(value);
            let result = notes::set_always_on_top(
                &db::lock(&db_always_on_top).conn,
                &always_on_top_id,
                value,
            )
            .map_err(|error| error.to_string());
            if let Err(error) = result {
                // This metadata action is immediate. Revert its visual state
                // and say why instead of implying that it was stored.
                window.set_note_always_on_top(!value);
                window.set_save_status(format!("Couldn't save setting ({error})").into());
            }
        }
    });
    let weak_checklist = editor.as_weak();
    let weak_checklist_board = weak_board.clone();
    let db_checklist = db.clone();
    let checklist_id = id.to_owned();
    let save_state_for_checklist = save_state.clone();
    #[cfg(windows)]
    let native_for_checklist = native_editor_slot.clone();
    editor.on_toggle_checklist(move |line_index, checked| {
        if let Some(window) = weak_checklist.upgrade() {
            #[cfg(windows)]
            flush_pending_native_change(&save_state_for_checklist);
            let source = window.get_editor_text().to_string();
            let mut lines = source.split('\n').map(str::to_owned).collect::<Vec<_>>();
            if let Some(line) = lines.get_mut(line_index as usize) {
                if let Some(updated) = set_checklist_checked(line, checked) {
                    *line = updated;
                }
            }
            apply_editor_state(
                &window,
                &lines.join("\n"),
                window.get_editor_bold(),
                &window.get_editor_bold_ranges(),
            );
            #[cfg(windows)]
            if let Some(native) = native_for_checklist.borrow().as_ref() {
                let (start, end) = native.selection_byte_offsets(&source);
                native.set_document(&native_document_from_editor(&window));
                native.restore_selection(
                    &window.get_editor_text(),
                    start.max(0) as usize,
                    end.max(0) as usize,
                );
            }
            remember_editor_text(&save_state_for_checklist, window.get_editor_text().as_ref());
            mark_editor_dirty(&window, &save_state_for_checklist);
            flush_editor(
                &db_checklist,
                &checklist_id,
                &window,
                &save_state_for_checklist,
                &weak_checklist_board,
            );
        }
    });
    let _ = editor.show();
    // Apply saved geometry to the realized native window too. A pre-show
    // size request can otherwise be replaced by Winit's creation defaults.
    if let Some(geometry) = WindowGeometry::from_db(note.window) {
        geometry.apply(editor.window());
    }
    #[cfg(windows)]
    let native_editor = {
        // The native RichEdit editor is the default (date chips, mixed
        // formatting). STICKY_NOTES_ENABLE_RICHEDIT=0 selects the Slint
        // TextInput fallback, which is also used if the control cannot attach.
        if std::env::var("STICKY_NOTES_ENABLE_RICHEDIT").is_ok_and(|value| value == "0") {
            None
        } else {
            let weak_native_change = editor.as_weak();
            let weak_native_board = weak_board.clone();
            let db_native_change = db.clone();
            let native_note_id = id.to_owned();
            let native_save_state = save_state.clone();
            match NativeRichEditor::attach(
                editor.window(),
                native_document_from_editor(&editor),
                move |document| {
                    let Some(window) = weak_native_change.upgrade() else {
                        return;
                    };
                    let formats = FormatRanges {
                        bold: document.bold.clone(),
                        italic: document.italic.clone(),
                        underline: document.underline.clone(),
                        strike: document.strike.clone(),
                    };
                    let serialized = serialize_format_ranges(&formats);
                    if window.get_editor_text().as_str() == document.text
                        && window.get_editor_bold_ranges().as_str() == serialized
                        && window.get_editor_chip_identities().as_str()
                            == serde_json::to_string(&document.chip_ids).unwrap()
                    {
                        return;
                    }
                    apply_native_document_to_editor(&window, &document);
                    remember_editor_text(&native_save_state, &document.text);
                    window.set_format_status("".into());
                    mark_editor_dirty(&window, &native_save_state);
                    schedule_autosave(
                        &native_save_state,
                        db_native_change.clone(),
                        native_note_id.clone(),
                        weak_native_change.clone(),
                        weak_native_board.clone(),
                    );
                },
            ) {
                Ok(native) => {
                    let native = Rc::new(native);
                    let paste_native = Rc::downgrade(&native);
                    let paste_window = editor.as_weak();
                    let paste_db = db.clone();
                    let paste_id = id.to_owned();
                    let paste_state = save_state.clone();
                    let paste_board = weak_board.clone();
                    native.on_image_paste(move |result| {
                        let (Some(window), Some(native)) =
                            (paste_window.upgrade(), paste_native.upgrade())
                        else {
                            return;
                        };
                        let result = result.and_then(|bytes| {
                            if !flush_editor(
                                &paste_db,
                                &paste_id,
                                &window,
                                &paste_state,
                                &paste_board,
                            ) {
                                return Err("Save pending edits before pasting an image".into());
                            }
                            let prepared = attachments::prepare_bytes(bytes, "Pasted image.png")
                                .map_err(|error| error.to_string())?;
                            insert_native_image(&window, &native, &paste_db, &paste_id, prepared)?;
                            flush_editor(&paste_db, &paste_id, &window, &paste_state, &paste_board);
                            Ok(())
                        });
                        if let Err(error) = result {
                            window
                                .set_format_status(format!("Couldn't paste image: {error}").into());
                        }
                    });
                    let image_db = db.clone();
                    let image_note = id.to_owned();
                    native.set_image_loader(move |image_id| {
                        let guard = db::lock(&image_db);
                        let owned = attachments::list(&guard.conn, &image_note)
                            .map_err(|error| error.to_string())?
                            .iter()
                            .any(|image| image.id == image_id);
                        if !owned {
                            return Err("Image is not owned by this note".into());
                        }
                        attachments::thumbnail_bytes(&guard.conn, image_id)
                            .map_err(|error| error.to_string())
                    });
                    if !editor.get_inline_image_mode() {
                        let items = attachments::list(&db::lock(db).conn, id).unwrap_or_default();
                        let mut document = native_document_from_editor(&editor);
                        let existing = inline_images::occurrences(&document.text);
                        for image in items {
                            if !existing.iter().any(|item| item.id == image.id) {
                                if !document.text.is_empty() {
                                    document.text.push('\n');
                                }
                                document.text.push_str(&inline_images::marker(&image.id));
                            }
                        }
                        editor.set_inline_image_mode(true);
                        apply_native_document_to_editor(&editor, &document);
                        native.set_document(&document);
                        remember_editor_text(&save_state, &document.text);
                        mark_editor_dirty(&editor, &save_state);
                        flush_editor(db, id, &editor, &save_state, &weak_board);
                    }
                    let clicked_window = editor.as_weak();
                    let toolbar_window = editor.as_weak();
                    native.on_focus_toolbar(move || {
                        if let Some(window) = toolbar_window.upgrade() {
                            window.window().with_winit_window(|host| {
                                use raw_window_handle::{HasWindowHandle, RawWindowHandle};
                                if let Ok(handle) = host.window_handle() {
                                    if let RawWindowHandle::Win32(handle) = handle.as_raw() {
                                        unsafe { windows_sys::Win32::UI::Input::KeyboardAndMouse::SetFocus(handle.hwnd.get() as _) };
                                    }
                                }
                            });
                            window.invoke_focus_toolbar();
                        }
                    });
                    let checkbox_window = editor.as_weak();
                    native.on_checklist_clicked(move |line, checked| {
                        if let Some(window) = checkbox_window.upgrade() {
                            window.invoke_toggle_checklist(line, checked);
                        }
                    });
                    let clicked_native = Rc::downgrade(&native);
                    native.on_chip_clicked(move |occurrence, x, y| {
                        if let Some(window) = clicked_window.upgrade() {
                            window.invoke_select_date_occurrence(occurrence);
                            window.set_date_anchor_x(x);
                            window.set_date_anchor_y(y);
                            window.set_date_panel_open(true);
                            if let Some(native) = clicked_native.upgrade() {
                                native.set_visible(false);
                            }
                        }
                    });
                    native.set_layout_bounds([
                        editor.get_editor_left(),
                        editor.get_editor_top(),
                        editor.get_editor_width(),
                        editor.get_editor_height(),
                    ]);
                    let native_for_resize = native.clone();
                    let frame_window = editor.as_weak();
                    editor.window().on_winit_window_event(move |_, event| {
                        match event {
                        i_slint_backend_winit::winit::event::WindowEvent::RedrawRequested => {
                            let weak = frame_window.clone();
                            if !native_for_resize.allow_parent_frame(move || {
                                if let Some(window) = weak.upgrade() {
                                    window.window().with_winit_window(|native_window| native_window.request_redraw());
                                }
                            }) { return WinitWindowEventResult::PreventDefault; }
                            let post_window = frame_window.clone();
                            let post_native = Rc::downgrade(&native_for_resize);
                            native_for_resize.repaint_after_parent(move || {
                                if let (Some(window), Some(native)) = (post_window.upgrade(), post_native.upgrade()) {
                                    if native.set_layout_bounds([window.get_editor_left(), window.get_editor_top(), window.get_editor_width(), window.get_editor_height()]) {
                                        // Moving the child changes which parent pixels were
                                        // clipped in this frame. Render one fresh parent frame
                                        // to clear the old HWND footprint, not on every poll.
                                        window.window().request_redraw();
                                    }
                                }
                            });
                        }
                        i_slint_backend_winit::winit::event::WindowEvent::Resized(_) => {
                            native_for_resize.layout();
                            if let Some(window) = frame_window.upgrade() {
                                let [width, height] = native_for_resize.client_size();
                                window.set_native_client_width(width);
                                window.set_native_client_height(height);
                            }
                        }
                        i_slint_backend_winit::winit::event::WindowEvent::ScaleFactorChanged {
                            scale_factor,
                            ..
                        } => native_for_resize.update_scale_factor(*scale_factor),
                        _ => {}
                    }
                        WinitWindowEventResult::Propagate
                    });
                    *native_editor_slot.borrow_mut() = Some(native.clone());
                    save_state.borrow_mut().native_editor = Some(Rc::downgrade(&native));
                    editor.set_native_editor_active(true);
                    editor.set_attachment_preview(slint::Image::default());
                    let [width, height] = native.client_size();
                    editor.set_native_client_width(width);
                    editor.set_native_client_height(height);
                    native.set_palette(&editor.get_note_color_value());
                    if !native.image_errors().is_empty() {
                        editor.set_format_status(
                            format!(
                                "Image preview unavailable (reference retained): {}",
                                native.image_errors().join("; ")
                            )
                            .into(),
                        );
                    }
                    Some(native)
                }
                Err(error) => {
                    editor
                        .set_format_status(format!("Native RichEdit unavailable: {error}").into());
                    None
                }
            }
        }
    };
    if editor.get_inline_image_mode() && !editor.get_native_editor_active() {
        editor.set_format_status("Inline-image note is read-only in the fallback editor. Enable the native editor to edit; originals are retained.".into());
    }
    editor.window().request_redraw();
    let geometry = WindowGeometry::from_window(editor.window());
    {
        let guard = db::lock(db);
        let _ = notes::save_window_geom(
            &guard.conn,
            id,
            f64::from(geometry.x),
            f64::from(geometry.y),
            f64::from(geometry.width),
            f64::from(geometry.height),
            true,
        );
    }
    windows.borrow_mut().push(EditorEntry {
        id: id.to_owned(),
        window: editor,
        last_geometry: geometry,
        save_state,
        #[cfg(windows)]
        native_editor,
    });
}
