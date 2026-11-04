use super::*;
use crate::db::reminders;
#[test]
fn typed_times_are_read_in_common_forms() {
    for (input, expected) in [
        ("14:30", "14:30"),
        ("9:30", "09:30"),
        ("0930", "09:30"),
        ("930", "09:30"),
        ("9.30", "09:30"),
        ("9", "09:00"),
        ("9pm", "21:00"),
        ("2:30 PM", "14:30"),
        ("12am", "00:00"),
        ("12:15 pm", "12:15"),
    ] {
        assert_eq!(normalize_time(input).as_deref(), Some(expected), "{input}");
    }
    for input in [
        "", "24:00", "9:60", "13pm", "0am", "9:3", "12345", "nine", ":30",
    ] {
        assert_eq!(normalize_time(input), None, "{input}");
    }
}

#[test]
fn board_preview_omits_only_an_exact_title_and_bounds_unicode() {
    assert_eq!(
        note_preview("Heading\n\n café   😀\nNext", Some("Heading")),
        "café 😀\nNext"
    );
    assert_eq!(
        note_preview("Longer heading\nBody", Some("Longer")),
        "Longer heading\nBody"
    );
    assert_eq!(note_preview(&"😀".repeat(300), None).chars().count(), 240);
}

#[test]
fn board_preview_hides_checklist_markers() {
    assert_eq!(
        note_preview(
            "[ ] Groceries\n  [ ] oat milk\n[x] bread\n[ ]\nPlain [x] text",
            Some("Groceries")
        ),
        "oat milk\nbread\nPlain [x] text"
    );
    assert_eq!(without_checklist_marker("[x] done"), "done");
}

#[test]
fn restart_restores_only_live_floating_notes_and_retains_returned_geometry() {
    let directory =
        std::env::temp_dir().join(format!("sticky-restoration-test-{}", Uuid::new_v4()));
    let db = Db::open_in_dir(&directory).unwrap();
    for id in ["open", "closed", "archived", "deleted"] {
        notes::create(&db.conn, id, EMPTY_CONTENT, "#FFF9B1").unwrap();
        notes::save_window_geom(&db.conn, id, -120.0, 240.0, 470.0, 440.0, id != "closed").unwrap();
    }
    notes::update_meta(&db.conn, "archived", None, None, Some(true)).unwrap();
    notes::soft_delete(&db.conn, "deleted").unwrap();
    drop(db);
    let db = Db::open_in_dir(&directory).unwrap();
    assert_eq!(floating_note_ids(&db.conn).unwrap(), vec!["open"]);
    assert_eq!(
        notes::get(&db.conn, "closed").unwrap().unwrap().window,
        Some((-120.0, 240.0, 470.0, 440.0))
    );
    notes::save_window_geom(&db.conn, "open", -120.0, 240.0, 470.0, 440.0, false).unwrap();
    assert!(floating_note_ids(&db.conn).unwrap().is_empty());
}

fn reminder_test_db() -> Db {
    let path = std::env::temp_dir().join(format!("slint-reminder-surface-test-{}", Uuid::new_v4()));
    Db::open_in_dir(&path).unwrap()
}

#[test]
fn structured_editor_renders_chip_and_persists_bold() {
    let source = "Ship it by [[date:2026-09-18]] please";
    let runs = parse_editor_runs(source, true);
    assert_eq!(runs.len(), 3);
    assert_eq!(runs[0].kind, "text");
    assert!(runs[0].bold);
    assert_eq!(runs[1].kind, "chip");
    assert_eq!(runs[1].chip_label, "Sep 18");
    assert_eq!(runs[2].text, " please");

    let content = content_for_editor(source, true);
    let value: serde_json::Value = serde_json::from_str(&content).unwrap();
    assert_eq!(value["plain"], "Ship it by Sep 18 please");
    assert_eq!(value["blocks"][0]["runs"][1]["type"], "dateChip");
    assert_eq!(value["blocks"][0]["runs"][1]["attrs"]["date"], "2026-09-18");
    assert_eq!(value["blocks"][0]["runs"][0]["b"], true);

    let (restored, bold) = editor_state_from_content(&content);
    assert_eq!(restored, source);
    assert!(bold);
}

#[test]
fn incomplete_chip_marker_stays_editable_text() {
    let runs = parse_editor_runs("keep [[date:2026", false);
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].text, "keep [[date:2026");
}

#[test]
fn recurring_timed_chip_roundtrips_source_and_projection() {
    let source = "Meet [[date:2026-09-28T17:30@weekly]] again";
    let chips = date_chip_occurrences(source);
    assert_eq!(chips.len(), 1);
    assert_eq!(chips[0].label, "Sep 28 · 17:30 ↻");
    let content = content_for_editor_with_checklists(source, false);
    let value: serde_json::Value = serde_json::from_str(&content).unwrap();
    let attrs = &value["blocks"][0]["runs"][1]["attrs"];
    assert_eq!(attrs["date"], "2026-09-28");
    assert_eq!(attrs["time"], "17:30");
    assert_eq!(attrs["recurrence"], "weekly");
    assert_eq!(
        editor_state_from_content_with_checklists(&content).0,
        source
    );
    let reminders = reminders::extract_date_chips(&content);
    assert_eq!(reminders[0].recurrence, "weekly");
    assert_eq!(reminders[0].due_time, "17:30");
}

#[test]
fn date_panel_validates_and_edits_the_selected_occurrence() {
    let source = "Ship [[date:2026-09-18]] then [[date:2026-09-19]]";
    let chips = date_chip_occurrences(source);
    assert_eq!(chips.len(), 2);
    assert_eq!(chips[0].label, "Sep 18");
    assert_eq!(chips[1].occurrence, 1);

    let updated = replace_date_chip(source, 1, "2026-10-03").unwrap();
    assert_eq!(updated, "Ship [[date:2026-09-18]] then [[date:2026-10-03]]");
    assert_eq!(
        remove_date_chip(&updated, 0).unwrap(),
        "Ship  then [[date:2026-10-03]]"
    );
    assert_eq!(
        append_date_chip("a note", "2026-10-04").unwrap(),
        "a note [[date:2026-10-04]]"
    );
    assert_eq!(
        replace_date_chip(source, 0, "2026-02-30"),
        Err("Enter a real date as YYYY-MM-DD.")
    );
    assert_eq!(
        append_date_chip("", "tomorrow"),
        Err("Enter a real date as YYYY-MM-DD.")
    );

    // Invalid markers remain ordinary editable text rather than malformed
    // date-chip data that cannot be selected by the panel.
    assert_eq!(date_chip_occurrences("[[date:2026-02-30]]"), Vec::new());
    assert_eq!(
        parse_editor_runs("[[date:2026-02-30]]", false)[0].text,
        "[[date:2026-02-30]]"
    );
}

#[test]
fn date_edit_keeps_other_chip_identity_and_reminder_status() {
    let db = reminder_test_db();
    let source = "First [[date:2026-09-18]] second [[date:2026-09-19]]";
    notes::create(
        &db.conn,
        "date-edit",
        &content_for_editor_with_checklists(source, false),
        "#FFF9B1",
    )
    .unwrap();
    let initial = reminders::list_for_note(&db.conn, "date-edit").unwrap();
    assert_eq!(initial.len(), 2);
    let unchanged_id = initial
        .iter()
        .find(|row| row.due_date == "2026-09-18")
        .unwrap()
        .id
        .clone();
    reminders::set_status(&db.conn, &unchanged_id, "completed").unwrap();

    let updated = replace_date_chip(source, 1, "2026-10-02").unwrap();
    notes::update_content(
        &db.conn,
        "date-edit",
        &content_for_editor_with_checklists(&updated, false),
    )
    .unwrap();
    let after = reminders::list_for_note(&db.conn, "date-edit").unwrap();
    assert_eq!(after.len(), 2);
    assert_eq!(
        after
            .iter()
            .find(|row| row.id == unchanged_id)
            .unwrap()
            .status,
        "completed"
    );
    assert_eq!(
        after
            .iter()
            .find(|row| row.due_date == "2026-10-02")
            .unwrap()
            .status,
        "pending"
    );
}

#[test]
fn due_reminder_surface_actions_update_the_projection() {
    let db = reminder_test_db();
    let due = NaiveDate::from_ymd_opt(2026, 9, 18).unwrap();
    notes::create(
        &db.conn,
        "a",
        &content_for_editor("Call client [[date:2026-09-18]]", false),
        "#FFF9B1",
    )
    .unwrap();
    notes::create(
        &db.conn,
        "b",
        &content_for_editor("Send report [[date:2026-09-18]]", false),
        "#DCE8FF",
    )
    .unwrap();

    let displayed = due_reminders(&db.conn, due).unwrap();
    assert_eq!(displayed.len(), 2);
    assert_eq!(displayed[0].title, "Call client");
    assert_eq!(displayed[0].label, "");
    assert_eq!(displayed[0].due_label, "Sep 18");

    let after_complete = act_on_reminder(&db.conn, &displayed[0].id, "completed", due).unwrap();
    assert_eq!(after_complete.len(), 1);
    let after_dismiss = act_on_reminder(&db.conn, &after_complete[0].id, "dismissed", due).unwrap();
    assert!(after_dismiss.is_empty());

    assert_eq!(
        reminders::list_for_note(&db.conn, "a").unwrap()[0].status,
        "completed"
    );
    assert_eq!(
        reminders::list_for_note(&db.conn, "b").unwrap()[0].status,
        "dismissed"
    );
}

#[test]
fn phrase_formatting_updates_all_exact_matches_and_roundtrips() {
    let source = "café and café";
    let (updated, count) = set_bold_for_phrase(source, false, &[], "café", true).unwrap();
    assert_eq!(count, 2);
    let ranges = parse_bold_ranges(&updated);
    assert_eq!(ranges, vec![(0, 5), (10, 15)]);
    let runs = parse_editor_runs_with_ranges(source, false, &ranges);
    assert_eq!(runs.len(), 3);
    assert_eq!(runs[0].text, "café");
    assert!(runs[0].bold);
    assert_eq!(runs[1].text, " and ");
    assert!(!runs[1].bold);
    assert_eq!(runs[2].text, "café");
    assert!(runs[2].bold);

    let content = content_for_editor_with_checklists_and_ranges(source, false, &ranges);
    let value: serde_json::Value = serde_json::from_str(&content).unwrap();
    assert_eq!(
        value["editor"]["boldRanges"],
        serde_json::json!([[0, 5], [10, 15]])
    );
    assert_eq!(value["blocks"][0]["runs"][0]["b"], true);
    assert_eq!(value["blocks"][0]["runs"][1]["b"], false);

    let (restored, bold, restored_ranges) = editor_state_from_content_with_checklists(&content);
    assert_eq!(restored, source);
    assert!(!bold);
    assert_eq!(parse_bold_ranges(&restored_ranges), ranges);
}

#[test]
fn phrase_formatting_can_clear_a_portion_of_a_whole_bold_note() {
    let (updated, count) = set_bold_for_phrase("alpha beta", true, &[], "beta", false).unwrap();
    assert_eq!(count, 1);
    assert_eq!(parse_bold_ranges(&updated), vec![(0, 6)]);
}

#[test]
fn phrase_formatting_rejects_empty_or_missing_text() {
    assert!(set_bold_for_phrase("note", false, &[], "", true).is_none());
    assert!(set_bold_for_phrase("note", false, &[], "missing", true).is_none());
}

#[test]
fn selection_styles_roundtrip_all_four_formats() {
    let source = "αβ café";
    let mut formats = FormatRanges::default();
    let cafe_start = source.find("café").unwrap();
    let cafe_end = source.len();
    assert!(set_selection_style(source, &mut formats, "bold", cafe_start, cafe_end).unwrap());
    assert!(set_selection_style(source, &mut formats, "italic", cafe_start, cafe_end).unwrap());
    assert!(set_selection_style(source, &mut formats, "underline", cafe_start, cafe_end).unwrap());
    assert!(set_selection_style(source, &mut formats, "strike", cafe_start, cafe_end).unwrap());

    let content = content_for_editor_with_checklists_and_formats(source, false, &formats);
    let value: serde_json::Value = serde_json::from_str(&content).unwrap();
    assert_eq!(
        value["editor"]["formats"]["bold"],
        serde_json::json!([[5, 10]])
    );
    assert_eq!(
        value["editor"]["formats"]["italic"],
        serde_json::json!([[5, 10]])
    );
    assert_eq!(
        value["editor"]["formats"]["underline"],
        serde_json::json!([[5, 10]])
    );
    assert_eq!(
        value["editor"]["formats"]["strike"],
        serde_json::json!([[5, 10]])
    );

    let (restored, _, restored_formats) = editor_state_from_content_with_checklists(&content);
    assert_eq!(restored, source);
    assert_eq!(parse_format_ranges(&restored_formats), formats);
}

#[test]
fn selection_style_toggles_and_rejects_non_unicode_boundaries() {
    let source = "éclair";
    let mut formats = FormatRanges::default();
    assert_eq!(
        set_selection_style(source, &mut formats, "bold", 1, source.len()),
        Err("The selection changed; select the text again.")
    );
    assert!(set_selection_style(source, &mut formats, "bold", 0, 2).unwrap());
    assert_eq!(formats.bold, vec![(0, 2)]);
    assert!(!set_selection_style(source, &mut formats, "bold", 0, 2).unwrap());
    assert!(formats.bold.is_empty());
}

#[test]
fn formatting_ranges_follow_unicode_typing_and_replacement() {
    let before = "Go café";
    let mut formats = FormatRanges {
        bold: vec![(3, before.len())],
        italic: vec![(3, before.len())],
        ..FormatRanges::default()
    };
    let after_insert = "Go very café";
    remap_formats_after_edit(before, after_insert, &mut formats);
    assert_eq!(formats.bold, vec![(8, after_insert.len())]);
    assert_eq!(formats.italic, vec![(8, after_insert.len())]);

    // Inserting at a styled caret extends that style and still avoids
    // splitting the two-byte é character.
    let before_caret = "éx";
    let after_caret = "é!x";
    let mut caret_formats = FormatRanges {
        underline: vec![(0, "é".len())],
        ..FormatRanges::default()
    };
    remap_formats_after_edit(before_caret, after_caret, &mut caret_formats);
    assert_eq!(caret_formats.underline, vec![(0, 3)]);

    let after_replace = "Go tea";
    remap_formats_after_edit(after_insert, after_replace, &mut formats);
    assert!(formats.bold.is_empty());
    assert!(formats.italic.is_empty());
}

#[test]
fn list_toolbar_handles_unicode_multiline_selection_and_keeps_formats() {
    let source = "α one\nβ two\nplain";
    let result = list_command(source, "bullet", 0, "α one\nβ two".len()).unwrap();
    assert_eq!(result.source, "- α one\n- β two\nplain");
    assert_eq!(result.caret, "- α one\n- β two".len());

    let mut formats = FormatRanges {
        bold: vec![(3, 6), (10, 13)],
        italic: vec![(0, 2)],
        ..FormatRanges::default()
    };
    remap_formats_after_replacements(source, &result.replacements, &mut formats);
    assert_eq!(formats.bold, vec![(5, 8), (14, 17)]);
    assert_eq!(formats.italic, vec![(2, 4)]);

    let tasks = list_command(&result.source, "checklist", 0, result.source.len()).unwrap();
    assert_eq!(tasks.source, "[ ] α one\n[ ] β two\n[ ] plain");
}

#[test]
fn enter_continues_checked_tasks_and_bullets_and_empty_items_exit() {
    let checked = "[x] finish report";
    let next_task = list_command(checked, "enter", checked.len(), checked.len()).unwrap();
    assert_eq!(next_task.source, "[x] finish report\n[ ] ");
    assert_eq!(next_task.caret, "[x] finish report\n[ ] ".len());

    let bullet = "  - write outline";
    let next_bullet = list_command(bullet, "enter", bullet.len(), bullet.len()).unwrap();
    assert_eq!(next_bullet.source, "  - write outline\n  - ");
    assert_eq!(next_bullet.caret, "  - write outline\n  - ".len());

    let empty_task = "  [ ] ";
    let exited_task =
        list_command(empty_task, "enter", empty_task.len(), empty_task.len()).unwrap();
    assert_eq!(exited_task.source, "  ");
    assert_eq!(exited_task.caret, 2);

    let empty_bullet = "- ";
    let exited_bullet = list_command(
        empty_bullet,
        "enter",
        empty_bullet.len(),
        empty_bullet.len(),
    )
    .unwrap();
    assert_eq!(exited_bullet.source, "");
    assert_eq!(exited_bullet.caret, 0);
}

#[test]
fn checklist_roundtrips_state_plain_text_and_date_runs() {
    let source = "Plan\n[ ] Ship by [[date:2026-09-18]]\n[x] Send summary";
    let content = content_for_editor_with_checklists(source, true);
    let value: serde_json::Value = serde_json::from_str(&content).unwrap();
    assert_eq!(value["plain"], "Plan\nShip by Sep 18\nSend summary");
    assert_eq!(value["blocks"][1]["type"], "checklist");
    assert_eq!(value["blocks"][1]["items"][0]["checked"], false);
    assert_eq!(value["blocks"][2]["items"][0]["checked"], true);
    assert_eq!(
        value["blocks"][1]["items"][0]["runs"][1]["attrs"]["date"],
        "2026-09-18"
    );

    let (restored, bold, ranges) = editor_state_from_content_with_checklists(&content);
    assert_eq!(restored, source);
    assert!(bold);
    assert_eq!(parse_format_ranges(&ranges), FormatRanges::default());

    let preview = checklist_preview_items(source);
    assert_eq!(preview.len(), 2);
    assert_eq!(preview[0].text, "Ship by Sep 18");
    assert!(!preview[0].checked);
    assert!(preview[1].checked);
}

#[test]
fn list_and_checklist_persist_as_structured_blocks_and_remain_searchable() {
    let source = "- Buy oat milk\n[ ] Call Ana\n[x] File receipt";
    let content = content_for_editor_with_checklists(source, false);
    let value: serde_json::Value = serde_json::from_str(&content).unwrap();
    assert_eq!(value["blocks"][0]["type"], "list");
    assert_eq!(value["blocks"][0]["items"][0]["text"], "Buy oat milk");
    assert_eq!(value["blocks"][1]["type"], "checklist");
    assert_eq!(value["plain"], "Buy oat milk\nCall Ana\nFile receipt");
    assert_eq!(
        editor_state_from_content_with_checklists(&content).0,
        source
    );

    let path = std::env::temp_dir().join(format!("slint-list-test-{}", Uuid::new_v4()));
    let db = Db::open_in_dir(&path).unwrap();
    notes::create(&db.conn, "list-note", &content, "#FFF9B1").unwrap();
    let matches = notes::list(&db.conn, false, false, Some("oat")).unwrap();
    assert_eq!(matches.len(), 1);
    assert_eq!(notes::content_to_plain(&matches[0].content), value["plain"]);
}

#[test]
fn reopening_persisted_content_rehydrates_editor_and_preview_state() {
    let source = "Plan\n[ ] Ship by [[date:2026-09-18]]\n[x] Send summary";
    let persisted = content_for_editor_with_checklists(source, true);

    // This is the same persisted-content path used by open_editor. Keep
    // the assertions on every presentation model so a future lifecycle
    // change cannot make the editor text appear while leaving the preview
    // models empty until a pointer event causes a redraw.
    let (restored, bold, ranges) = editor_state_from_content_with_checklists(&persisted);
    assert_eq!(restored, source);
    assert!(bold);
    assert_eq!(parse_format_ranges(&ranges), FormatRanges::default());

    let runs = parse_editor_runs_with_ranges(&restored, bold, &parse_bold_ranges(&ranges));
    assert!(runs
        .iter()
        .any(|run| run.kind == "chip" && run.chip_label == "Sep 18"));
    let preview = checklist_preview_items(&restored);
    assert_eq!(preview.len(), 2);
    assert_eq!(preview[0].text, "Ship by Sep 18");
    assert!(!preview[0].checked);
    assert!(preview[1].checked);
}

#[test]
fn checklist_toggle_preserves_indentation_and_body() {
    assert_eq!(
        set_checklist_checked("  [ ] review", true).as_deref(),
        Some("  [x] review")
    );
    assert_eq!(set_checklist_checked("[x]", false).as_deref(), Some("[ ]"));
}

#[test]
fn checklist_plain_text_is_searchable_after_save() {
    let path = std::env::temp_dir().join(format!("slint-checklist-test-{}", Uuid::new_v4()));
    let db = Db::open_in_dir(&path).unwrap();
    let content = content_for_editor_with_checklists("[ ] Buy oat milk", false);
    notes::create(&db.conn, "checklist-note", &content, "#FFF9B1").unwrap();
    let matches = notes::list(&db.conn, false, false, Some("oat")).unwrap();
    assert_eq!(matches.len(), 1);
    assert_eq!(notes::content_to_plain(&matches[0].content), "Buy oat milk");
}

#[test]
fn autosave_failure_is_returned_so_the_same_snapshot_can_be_retried() {
    let path = std::env::temp_dir().join(format!("slint-autosave-test-{}", Uuid::new_v4()));
    let db = Arc::new(Mutex::new(Db::open_in_dir(&path).unwrap()));
    let snapshot = EditorSnapshot {
        content: content_for_editor_with_checklists("retry me", false),
        color: "#DCE8FF".to_owned(),
    };

    // A missing note simulates a persistence failure without needing a
    // platform-specific locked database. The caller receives the error
    // and therefore keeps this snapshot dirty for a later retry.
    assert!(persist_snapshot(&db, "missing", &snapshot).is_err());
    {
        let guard = db::lock(&db);
        notes::create(&guard.conn, "missing", EMPTY_CONTENT, "#FFF9B1").unwrap();
    }
    assert!(persist_snapshot(&db, "missing", &snapshot).is_ok());
    let saved = db::lock(&db)
        .conn
        .query_row("SELECT color FROM notes WHERE id='missing'", [], |row| {
            row.get::<_, String>(0)
        })
        .unwrap();
    assert_eq!(saved, "#DCE8FF");
}

#[test]
fn text_autosave_uses_a_one_second_quiet_period() {
    assert_eq!(AUTOSAVE_DELAY, Duration::from_secs(1));
}
