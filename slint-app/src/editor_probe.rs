//! Explicit, isolated release probe. Never opens the production database.
use crate::*;
use std::{fs::OpenOptions, io::Write, mem::size_of, time::Instant};
use windows_sys::Win32::{
    Foundation::FILETIME,
    System::{
        ProcessStatus::{
            GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS, PROCESS_MEMORY_COUNTERS_EX,
        },
        Threading::{GetCurrentProcess, GetProcessTimes},
    },
};

struct Stats {
    cpu_seconds: f64,
    working_set_mib: f64,
    private_commit_mib: f64,
}

fn stats() -> std::io::Result<Stats> {
    let mut created = FILETIME::default();
    let mut exited = FILETIME::default();
    let mut kernel = FILETIME::default();
    let mut user = FILETIME::default();
    let mut memory = PROCESS_MEMORY_COUNTERS_EX::default();
    memory.cb = size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32;
    unsafe {
        let process = GetCurrentProcess();
        if GetProcessTimes(process, &mut created, &mut exited, &mut kernel, &mut user) == 0
            || GetProcessMemoryInfo(
                process,
                &mut memory as *mut _ as *mut PROCESS_MEMORY_COUNTERS,
                memory.cb,
            ) == 0
        {
            return Err(std::io::Error::last_os_error());
        }
    }
    let ticks =
        |time: FILETIME| (u64::from(time.dwHighDateTime) << 32) | u64::from(time.dwLowDateTime);
    Ok(Stats {
        cpu_seconds: (ticks(kernel) + ticks(user)) as f64 / 10_000_000.0,
        working_set_mib: memory.WorkingSetSize as f64 / 1_048_576.0,
        private_commit_mib: memory.PrivateUsage as f64 / 1_048_576.0,
    })
}

const PHASES: [(&str, f64); 4] = [
    ("warmup", 8.0),
    ("idle", 8.0),
    ("typing", 8.0),
    ("settle", 4.0),
];

/// Human/computer-use acceptance sandbox; production data and shortcuts are
/// untouched. Reopen only a directory explicitly marked by this mode.
pub fn preview() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args().collect();
    if args.len() != 3 {
        return Err("Usage: --editor-preview <new-directory> or --editor-preview-reopen <sandbox-directory>".into());
    }
    let path = PathBuf::from(&args[2]);
    let marker = path.join("acceptance-sandbox.txt");
    let fresh = args[1] == "--editor-preview";
    if fresh {
        std::fs::create_dir(&path)?;
        std::fs::write(&marker, "sticky-native-acceptance-v1")?;
    } else if std::fs::read_to_string(&marker)? != "sticky-native-acceptance-v1" {
        return Err("Not an editor acceptance sandbox".into());
    }
    std::env::set_var("STICKY_NOTES_ENABLE_RICHEDIT", "1");
    let db = Arc::new(Mutex::new(Db::open_in_dir(&path)?));
    let board = BoardWindow::new()?;
    let windows = Rc::new(RefCell::new(Vec::<EditorEntry>::new()));
    // The acceptance board uses only this marked sandbox database. Exercise
    // real reminder actions here without opening the user's production notes.
    {
        let db = db.clone();
        let weak = board.as_weak();
        board.on_snooze_reminder(move |id| {
            let saved = reminders::snooze(
                &db.lock().unwrap().conn,
                &id,
                chrono::Utc::now() + chrono::Duration::minutes(10),
            );
            if saved.is_ok() {
                if let Some(board) = weak.upgrade() {
                    refresh_due_reminders(&board, &db);
                }
            }
        });
    }
    {
        let db = db.clone();
        let weak = board.as_weak();
        board.on_complete_reminder(move |id| {
            let saved = reminders::set_status(&db.lock().unwrap().conn, &id, "completed");
            if saved.is_ok() {
                if let Some(board) = weak.upgrade() {
                    refresh_due_reminders(&board, &db);
                }
            }
        });
    }
    {
        let db = db.clone();
        let weak = board.as_weak();
        board.on_dismiss_reminder(move |id| {
            let saved = reminders::set_status(&db.lock().unwrap().conn, &id, "dismissed");
            if saved.is_ok() {
                if let Some(board) = weak.upgrade() {
                    refresh_due_reminders(&board, &db);
                }
            }
        });
    }
    if fresh {
        for (id, title, color, x) in [
            ("a", "Editor acceptance A", "#FFF9B1", 60.0),
            ("b", "Editor acceptance B", "#6F7078", 480.0),
        ] {
            let source = format!("{title}\nUnicode café 😀\n[x] Finished task\n[ ] Next task\nMeet [[date:2026-09-27T17:30]] and [[date:2026-09-27T17:30]]\n");
            let content = content_for_editor_with_checklists_and_formats(
                &source,
                false,
                &FormatRanges {
                    bold: vec![(0, title.len())],
                    italic: vec![(title.len() + 1, title.len() + 8)],
                    ..Default::default()
                },
            );
            let guard = db.lock().unwrap();
            notes::create(&guard.conn, id, &content, color)?;
            notes::save_window_geom(&guard.conn, id, x, 80.0, 380.0, 470.0, true)?;
        }
    }
    refresh(&board, &db, "");
    refresh_due_reminders(&board, &db);
    {
        let db = db.clone();
        let windows = windows.clone();
        let weak = board.as_weak();
        board.on_open_note(move |id| {
            if let Some(board) = weak.upgrade() {
                open_editor(&board, &db, &id, &windows);
            }
        });
    }
    {
        let db = db.clone();
        let windows = windows.clone();
        let weak = board.as_weak();
        board.on_create_note(move || {
            if let Some(board) = weak.upgrade() {
                let id = Uuid::new_v4().to_string();
                if notes::create(&db.lock().unwrap().conn, &id, EMPTY_CONTENT, "#FFF9B1").is_ok() {
                    refresh(&board, &db, "");
                    open_editor(&board, &db, &id, &windows);
                }
            }
        });
    }
    let rows = notes::list(&db.lock().unwrap().conn, false, false, None)?;
    for row in rows {
        open_editor(&board, &db, &row.id, &windows);
    }
    let geometry_timer = Timer::default();
    {
        let windows = windows.clone();
        let db = db.clone();
        geometry_timer.start(TimerMode::Repeated, Duration::from_millis(250), move || {
            let guard = db.lock().unwrap();
            for entry in windows.borrow_mut().iter_mut() {
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
                let geometry = WindowGeometry::from_window(&entry.window.window());
                if geometry != entry.last_geometry {
                    if notes::save_window_geom(
                        &guard.conn,
                        &entry.id,
                        f64::from(geometry.x),
                        f64::from(geometry.y),
                        f64::from(geometry.width),
                        f64::from(geometry.height),
                        true,
                    )
                    .is_ok()
                    {
                        entry.last_geometry = geometry;
                    }
                }
            }
        });
    }
    slint::run_event_loop()?;
    Ok(())
}

pub fn run() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() != 4 || !matches!(args[2].as_str(), "1" | "20") {
        return Err("Usage: --editor-probe <1|20> <new-report.json>".into());
    }
    let count: usize = args[2].parse()?;
    // Refuse to overwrite an existing report. The production database is never
    // opened, and there is no tray/hotkey registration in this isolated mode.
    let mut report = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&args[3])?;
    std::env::set_var("STICKY_NOTES_ENABLE_RICHEDIT", "1");
    let sandbox = PathBuf::from(&args[3]).with_extension("sandbox");
    std::fs::create_dir(&sandbox)?;
    let db = Arc::new(Mutex::new(Db::open_in_dir(&sandbox)?));
    let board = BoardWindow::new()?;
    let windows = Rc::new(RefCell::new(Vec::<EditorEntry>::new()));
    let source = "Editable 😀 note\n[ ] Follow up [[date:2026-09-27]]\nNative mixed formatting and Unicode café.\n".repeat(4);
    let content = content_for_editor_with_checklists_and_formats(
        &source,
        false,
        &FormatRanges {
            bold: vec![(0, 8)],
            italic: vec![(9, 13)],
            underline: vec![(14, 18)],
            strike: vec![(0, 4)],
        },
    );
    for index in 0..count {
        let id = format!("probe-{index}");
        {
            let guard = db.lock().unwrap();
            notes::create(&guard.conn, &id, &content, "#FFF9B1")?;
            notes::save_window_geom(
                &guard.conn,
                &id,
                40.0 + (index % 5) as f64 * 150.0,
                40.0 + (index / 5) as f64 * 95.0,
                340.0,
                360.0,
                true,
            )?;
        }
        open_editor(&board, &db, &id, &windows);
    }
    if windows.borrow().len() != count
        || windows
            .borrow()
            .iter()
            .any(|entry| entry.native_editor.is_none())
    {
        return Err("Probe could not create all native editors; no valid measurement".into());
    }
    let processors = std::thread::available_parallelism()?.get();
    let phase = Rc::new(RefCell::new(0usize));
    let injected = Rc::new(RefCell::new(0usize));
    let failures = Rc::new(RefCell::new(None::<String>));
    let results = Rc::new(RefCell::new(Vec::new()));
    let timer = Timer::default();
    {
        let windows = windows.clone();
        let phase = phase.clone();
        let injected = injected.clone();
        let failures = failures.clone();
        let results = results.clone();
        let source_for_focus = source.clone();
        let mut started = Instant::now();
        let mut baseline = stats()?;
        let mut peak_working_set = baseline.working_set_mib;
        timer.start(TimerMode::Repeated, Duration::from_millis(100), move || {
            let current = match stats() {
                Ok(stats) => stats,
                Err(error) => {
                    *failures.borrow_mut() = Some(error.to_string());
                    let _ = slint::quit_event_loop();
                    return;
                }
            };
            let index = *phase.borrow();
            if index >= PHASES.len() {
                return;
            }
            peak_working_set = peak_working_set.max(current.working_set_mib);
            let elapsed = started.elapsed().as_secs_f64();
            if elapsed >= PHASES[index].1 {
                let cpu = current.cpu_seconds - baseline.cpu_seconds;
                results.borrow_mut().push(serde_json::json!({
                    "phase": PHASES[index].0, "elapsed_seconds": elapsed,
                    "cpu_seconds": cpu, "cpu_percent_one_core": cpu / elapsed * 100.0,
                    "cpu_percent_machine": cpu / elapsed * 100.0 / processors as f64,
                    "working_set_mib": current.working_set_mib,
                    "peak_working_set_mib": peak_working_set,
                    "private_commit_mib": current.private_commit_mib,
                    "native_metrics": windows.borrow()[0].native_editor.as_ref().unwrap().take_probe_metrics(),
                }));
                *phase.borrow_mut() += 1;
                if index == 1 {
                    let editors = windows.borrow();
                    editors[0].native_editor.as_ref().unwrap().restore_selection(&source_for_focus, source_for_focus.len(), source_for_focus.len());
                }
                started = Instant::now();
                peak_working_set = current.working_set_mib;
                baseline = current;
                if index + 1 == PHASES.len() {
                    let _ = slint::quit_event_loop();
                }
                return;
            }
            let editors = windows.borrow();
            // Follow live Slint bounds just as the regular geometry timer does.
            for entry in editors.iter() {
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
            }
            if index == 2 {
                editors[0]
                    .native_editor
                    .as_ref()
                    .unwrap()
                    .inject_probe_character('.');
                *injected.borrow_mut() += 1;
            }
        });
    }
    slint::run_event_loop()?;
    timer.stop();
    if let Some(error) = failures.borrow_mut().take() {
        return Err(error.into());
    }
    if *phase.borrow() != PHASES.len() {
        return Err("Probe interrupted before completion".into());
    }
    // Verify debounce reached disk before any explicit close/save flush.
    let expected = format!("{source}{}", ".".repeat(*injected.borrow()));
    let disk = Db::open_in_dir(&sandbox)?;
    let autosaved = notes::get(&disk.conn, "probe-0")?.unwrap();
    let (autosaved_text, _, _) = editor_state_from_content_with_checklists(&autosaved.content);
    if autosaved_text != expected {
        return Err("Disk autosave did not persist input before final flush".into());
    }
    let mut saved = true;
    for entry in windows.borrow().iter() {
        saved &= flush_editor(
            &db,
            &entry.id,
            &entry.window,
            &entry.save_state,
            &board.as_weak(),
        );
    }
    let persisted = notes::get(&db.lock().unwrap().conn, "probe-0")?.unwrap();
    let (actual, _, _) = editor_state_from_content_with_checklists(&persisted.content);
    if !saved || actual != expected {
        return Err("Probe input did not persist correctly".into());
    }
    let payload = serde_json::json!({
        "timestamp_utc": chrono::Utc::now().to_rfc3339(), "release_build": !cfg!(debug_assertions),
        "backend": std::env::var("SLINT_BACKEND").unwrap_or_default(),
        "note_windows": count, "native_editor_windows": count, "logical_processors": processors,
        "initial_utf8_bytes_per_note": source.len(), "injected_characters": *injected.borrow(),
        "typing_workload": "10 ASCII characters/second in one note, real WM_CHAR, normal snapshot/autosave",
        "database": "isolated file-backed SQLite WAL; no production data",
        "disk_autosave_verified_before_flush": true,
        "limitations": "No tray/hotkey timers; overlapping notes; synthetic input is not IME/visual acceptance; short run is not a soak test",
        "persisted_input_verified": true, "phases": &*results.borrow(),
    });
    report.write_all(serde_json::to_string_pretty(&payload)?.as_bytes())?;
    report.flush()?;
    Ok(())
}
