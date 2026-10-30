#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod acceptance_profile;
mod calendar_sync;
#[cfg(windows)]
mod calendar_worker;
mod checklist_presentation;
mod content_identity;
mod date_picker;
mod db;
mod editor_history;
#[cfg(windows)]
mod editor_probe;
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
#[cfg(windows)]
mod reliability_probe;
mod settings;
mod tray;
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

fn plain_from_content(content: &str) -> String {
    notes::content_to_plain(content)
}

#[derive(Debug, Clone, PartialEq)]
struct EditorRun {
    kind: &'static str,
    text: String,
    chip_label: String,
    date: Option<String>,
    bold: bool,
}

#[derive(Debug, Clone, PartialEq)]
struct ChecklistState {
    line_index: i32,
    text: String,
    checked: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct DateChipState {
    occurrence: i32,
    start: usize,
    end: usize,
    date: String,
    label: String,
}

fn date_label(value: &str) -> String {
    let (value, repeat) = value.split_once('@').unwrap_or((value, "none"));
    let (day, time) = value
        .split_once('T')
        .map_or((value, None), |(day, time)| (day, Some(time)));
    NaiveDate::parse_from_str(day, "%Y-%m-%d")
        .map(|date| {
            format!(
                "{} {}{}",
                date.format("%b"),
                date.day(),
                time.map_or(String::new(), |time| format!(" · {time}"))
            ) + if repeat == "none" { "" } else { " ↻" }
        })
        .unwrap_or_else(|_| value.to_owned())
}

/// A marker only becomes a chip when its value is a real ISO calendar date.
/// Keeping malformed input as plain text prevents a typo from creating a
/// reminder which cannot be edited through the native date panel.
fn valid_date(value: &str) -> bool {
    let (value, repeat) = value.split_once('@').unwrap_or((value, "none"));
    if !matches!(repeat, "none" | "daily" | "weekly" | "monthly") {
        return false;
    }
    let (day, time) = value
        .split_once('T')
        .map_or((value, None), |(day, time)| (day, Some(time)));
    day.len() == 10
        && NaiveDate::parse_from_str(day, "%Y-%m-%d").is_ok()
        && time.map_or(true, |time| {
            time.len() == 5 && chrono::NaiveTime::parse_from_str(time, "%H:%M").is_ok()
        })
}

/// Return editable date marker occurrences in source order. `start`/`end`
/// cover the entire marker, which makes removal independent of display labels.
fn date_chip_occurrences(source: &str) -> Vec<DateChipState> {
    let mut result = Vec::new();
    let mut cursor = 0;
    while cursor < source.len() {
        let Some(relative_start) = source[cursor..].find("[[date:") else {
            break;
        };
        let start = cursor + relative_start;
        let value_start = start + "[[date:".len();
        let Some(relative_end) = source[value_start..].find("]]") else {
            break;
        };
        let value_end = value_start + relative_end;
        let end = value_end + 2;
        let date = &source[value_start..value_end];
        if valid_date(date) {
            result.push(DateChipState {
                occurrence: i32::try_from(result.len()).unwrap_or(i32::MAX),
                start,
                end,
                date: date.to_owned(),
                label: date_label(date),
            });
        }
        cursor = end;
    }
    result
}

fn replace_date_chip(source: &str, occurrence: i32, date: &str) -> Result<String, &'static str> {
    if !valid_date(date) {
        return Err("Enter a real date as YYYY-MM-DD.");
    }
    let chip = date_chip_occurrences(source)
        .into_iter()
        .find(|chip| chip.occurrence == occurrence)
        .ok_or("That date is no longer in this note.")?;
    let mut updated = source.to_owned();
    updated.replace_range(chip.start..chip.end, &format!("[[date:{date}]]"));
    Ok(updated)
}

fn remove_date_chip(source: &str, occurrence: i32) -> Result<String, &'static str> {
    let chip = date_chip_occurrences(source)
        .into_iter()
        .find(|chip| chip.occurrence == occurrence)
        .ok_or("That date is no longer in this note.")?;
    let mut updated = source.to_owned();
    updated.replace_range(chip.start..chip.end, "");
    Ok(updated)
}

fn append_date_chip(source: &str, date: &str) -> Result<String, &'static str> {
    if !valid_date(date) {
        return Err("Enter a real date as YYYY-MM-DD.");
    }
    let separator = (!source.is_empty() && !source.chars().last().is_some_and(char::is_whitespace))
        .then_some(" ")
        .unwrap_or("");
    Ok(format!("{source}{separator}[[date:{date}]]"))
}

fn push_text_run(runs: &mut Vec<EditorRun>, text: &str, bold: bool) {
    if text.is_empty() {
        return;
    }
    if let Some(last) = runs.last_mut() {
        if last.kind == "text" && last.bold == bold {
            last.text.push_str(text);
            return;
        }
    }
    runs.push(EditorRun {
        kind: "text",
        text: text.to_owned(),
        chip_label: String::new(),
        date: None,
        bold,
    });
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct FormatRanges {
    bold: Vec<(usize, usize)>,
    italic: Vec<(usize, usize)>,
    underline: Vec<(usize, usize)>,
    strike: Vec<(usize, usize)>,
}

impl FormatRanges {
    fn style_mut(&mut self, style: &str) -> Option<&mut Vec<(usize, usize)>> {
        match style {
            "bold" => Some(&mut self.bold),
            "italic" => Some(&mut self.italic),
            "underline" => Some(&mut self.underline),
            "strike" => Some(&mut self.strike),
            _ => None,
        }
    }
}

fn parse_range_list(value: Option<&serde_json::Value>) -> Vec<(usize, usize)> {
    value
        .and_then(|value| value.as_array())
        .into_iter()
        .flatten()
        .filter_map(|value| {
            let pair = value.as_array()?;
            let start = pair.first()?.as_u64()? as usize;
            let end = pair.get(1)?.as_u64()? as usize;
            (start < end).then_some((start, end))
        })
        .collect()
}

/// Read both the original `[[start,end]]` bold format and the phase-two
/// format object. The latter deliberately retains byte offsets because that
/// is what Slint's native TextInput reports for its selection.
fn parse_format_ranges(value: &str) -> FormatRanges {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(value) else {
        return FormatRanges::default();
    };
    if value.is_array() {
        return FormatRanges {
            bold: parse_range_list(Some(&value)),
            ..FormatRanges::default()
        };
    }
    FormatRanges {
        bold: parse_range_list(value.get("bold")),
        italic: parse_range_list(value.get("italic")),
        underline: parse_range_list(value.get("underline")),
        strike: parse_range_list(value.get("strike")),
    }
}

#[cfg(test)]
fn parse_bold_ranges(value: &str) -> Vec<(usize, usize)> {
    parse_format_ranges(value).bold
}

#[cfg(test)]
fn serialize_bold_ranges(ranges: &[(usize, usize)]) -> String {
    serde_json::to_string(
        &ranges
            .iter()
            .filter(|(start, end)| start < end)
            .map(|(start, end)| [*start, *end])
            .collect::<Vec<_>>(),
    )
    .unwrap_or_else(|_| "[]".to_owned())
}

fn serialize_format_ranges(formats: &FormatRanges) -> String {
    serde_json::json!({
        "bold": formats.bold,
        "italic": formats.italic,
        "underline": formats.underline,
        "strike": formats.strike,
    })
    .to_string()
}

#[cfg(windows)]
fn native_document_from_editor(editor: &NoteWindow) -> NativeDocument {
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
fn apply_native_document_to_editor(editor: &NoteWindow, document: &NativeDocument) {
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

fn canonicalize_ranges(source: &str, ranges: &mut Vec<(usize, usize)>) {
    ranges.retain(|(start, end)| {
        start < end
            && *end <= source.len()
            && source.is_char_boundary(*start)
            && source.is_char_boundary(*end)
    });
    ranges.sort_unstable();
    let mut merged = Vec::with_capacity(ranges.len());
    for (start, end) in ranges.drain(..) {
        if let Some((_, previous_end)) = merged.last_mut() {
            if start <= *previous_end {
                *previous_end = (*previous_end).max(end);
                continue;
            }
        }
        merged.push((start, end));
    }
    *ranges = merged;
}

fn canonicalize_formats(source: &str, formats: &mut FormatRanges) {
    for ranges in [
        &mut formats.bold,
        &mut formats.italic,
        &mut formats.underline,
        &mut formats.strike,
    ] {
        canonicalize_ranges(source, ranges);
    }
}

fn selection_is_covered(ranges: &[(usize, usize)], start: usize, end: usize) -> bool {
    ranges
        .iter()
        .any(|(range_start, range_end)| *range_start <= start && *range_end >= end)
}

fn set_selection_style(
    source: &str,
    formats: &mut FormatRanges,
    style: &str,
    start: usize,
    end: usize,
) -> Result<bool, &'static str> {
    if start >= end {
        return Err("Select text before applying a style.");
    }
    if end > source.len() || !source.is_char_boundary(start) || !source.is_char_boundary(end) {
        return Err("The selection changed; select the text again.");
    }
    let ranges = formats.style_mut(style).ok_or("Unsupported text style.")?;
    let remove = selection_is_covered(ranges, start, end);
    let mut next = Vec::new();
    for (range_start, range_end) in ranges.drain(..) {
        if range_end <= start || range_start >= end {
            next.push((range_start, range_end));
        } else {
            if range_start < start {
                next.push((range_start, start));
            }
            if range_end > end {
                next.push((end, range_end));
            }
        }
    }
    if !remove {
        next.push((start, end));
    }
    *ranges = next;
    canonicalize_formats(source, formats);
    Ok(!remove)
}

fn common_edit_span(old: &str, new: &str) -> (usize, usize, usize) {
    let mut prefix = 0;
    for ((old_offset, old_char), (new_offset, new_char)) in
        old.char_indices().zip(new.char_indices())
    {
        if old_char != new_char {
            break;
        }
        prefix = old_offset + old_char.len_utf8();
        debug_assert_eq!(prefix, new_offset + new_char.len_utf8());
    }
    let mut old_end = old.len();
    let mut new_end = new.len();
    while old_end > prefix && new_end > prefix {
        let old_char = old[..old_end].chars().next_back().unwrap();
        let new_char = new[..new_end].chars().next_back().unwrap();
        if old_char != new_char {
            break;
        }
        old_end -= old_char.len_utf8();
        new_end -= new_char.len_utf8();
    }
    (prefix, old_end, new_end)
}

fn remap_ranges_after_edit(
    source_before: &str,
    source_after: &str,
    ranges: &mut Vec<(usize, usize)>,
) {
    let (edit_start, old_end, new_end) = common_edit_span(source_before, source_after);
    let delta = new_end as isize - old_end as isize;
    let mut next = Vec::new();
    for (start, end) in ranges.drain(..) {
        if old_end == edit_start && start < edit_start && end >= edit_start {
            // Native insertion at a styled caret inherits that style.
            next.push((start, (end as isize + delta) as usize));
        } else if end <= edit_start {
            next.push((start, end));
        } else if start >= old_end {
            next.push((
                (start as isize + delta) as usize,
                (end as isize + delta) as usize,
            ));
        } else {
            if start < edit_start {
                next.push((start, edit_start));
            }
            if end > old_end {
                next.push((new_end, (end as isize + delta) as usize));
            }
        }
    }
    *ranges = next;
    canonicalize_ranges(source_after, ranges);
}

fn remap_formats_after_edit(source_before: &str, source_after: &str, formats: &mut FormatRanges) {
    for ranges in [
        &mut formats.bold,
        &mut formats.italic,
        &mut formats.underline,
        &mut formats.strike,
    ] {
        remap_ranges_after_edit(source_before, source_after, ranges);
    }
}

fn range_contains(ranges: &[(usize, usize)], start: usize, end: usize) -> bool {
    ranges
        .iter()
        .any(|(range_start, range_end)| *range_start < end && *range_end > start)
}

fn push_text_run_with_ranges(
    runs: &mut Vec<EditorRun>,
    text: &str,
    default_bold: bool,
    base_offset: usize,
    ranges: &[(usize, usize)],
) {
    let images = inline_images::occurrences(text);
    if !images.is_empty() {
        let mut cursor = 0;
        for image in images {
            push_text_run_with_ranges(
                runs,
                &text[cursor..image.start],
                default_bold,
                base_offset + cursor,
                ranges,
            );
            runs.push(EditorRun {
                kind: "image",
                text: String::new(),
                chip_label: String::new(),
                date: Some(image.id),
                bold: false,
            });
            cursor = image.end;
        }
        push_text_run_with_ranges(
            runs,
            &text[cursor..],
            default_bold,
            base_offset + cursor,
            ranges,
        );
        return;
    }
    if text.is_empty() {
        return;
    }
    let mut chunk_start = 0;
    let mut chunk_bold = default_bold;
    for (offset, character) in text.char_indices() {
        let end = offset + character.len_utf8();
        let bold = default_bold || range_contains(ranges, base_offset + offset, base_offset + end);
        if offset > chunk_start && bold != chunk_bold {
            push_text_run(runs, &text[chunk_start..offset], chunk_bold);
            chunk_start = offset;
        }
        chunk_bold = bold;
    }
    push_text_run(runs, &text[chunk_start..], chunk_bold);
}

/// The source editor deliberately uses a small, transparent marker syntax for
/// inline chips. The read-only preview below the editor turns those markers
/// into native Slint pill elements, so the persistence format does not depend
/// on HTML or a WebView editor.
fn parse_editor_runs(source: &str, bold: bool) -> Vec<EditorRun> {
    parse_editor_runs_with_ranges_at(source, bold, &[], 0)
}

fn parse_editor_runs_with_ranges(
    source: &str,
    bold: bool,
    ranges: &[(usize, usize)],
) -> Vec<EditorRun> {
    parse_editor_runs_with_ranges_at(source, bold, ranges, 0)
}

fn parse_editor_runs_with_ranges_at(
    source: &str,
    bold: bool,
    ranges: &[(usize, usize)],
    base_offset: usize,
) -> Vec<EditorRun> {
    let mut runs = Vec::new();
    let mut cursor = 0;
    while cursor < source.len() {
        let remaining = &source[cursor..];
        let Some(offset) = remaining.find("[[date:") else {
            push_text_run_with_ranges(&mut runs, remaining, bold, base_offset + cursor, ranges);
            break;
        };
        let marker_start = cursor + offset;
        push_text_run_with_ranges(
            &mut runs,
            &source[cursor..marker_start],
            bold,
            base_offset + cursor,
            ranges,
        );
        let value_start = marker_start + "[[date:".len();
        let Some(end_offset) = source[value_start..].find("]]") else {
            push_text_run_with_ranges(
                &mut runs,
                &source[marker_start..],
                bold,
                base_offset + marker_start,
                ranges,
            );
            break;
        };
        let value_end = value_start + end_offset;
        let date = &source[value_start..value_end];
        if !valid_date(date) {
            push_text_run_with_ranges(
                &mut runs,
                &source[marker_start..value_end + 2],
                bold,
                base_offset + marker_start,
                ranges,
            );
        } else {
            runs.push(EditorRun {
                kind: "chip",
                text: String::new(),
                chip_label: date_label(date),
                date: Some(date.to_owned()),
                bold: false,
            });
        }
        cursor = value_end + 2;
    }
    runs
}

#[cfg(test)]
fn editor_state_from_content(content: &str) -> (String, bool) {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(content) else {
        return (plain_from_content(content), false);
    };
    let Some(runs) = value
        .get("blocks")
        .and_then(|blocks| blocks.as_array())
        .and_then(|blocks| blocks.first())
        .and_then(|block| block.get("runs"))
        .and_then(|runs| runs.as_array())
    else {
        return (plain_from_content(content), false);
    };
    let mut source = String::new();
    let has_bold_ranges = value.get("editor").is_some_and(|editor| {
        editor.get("formats").is_some() || editor.get("boldRanges").is_some()
    });
    let mut bold = value
        .get("editor")
        .and_then(|editor| editor.get("bold"))
        .and_then(|value| value.as_bool())
        .unwrap_or(false);
    for run in runs {
        if let Some(text) = run.get("t").and_then(|text| text.as_str()) {
            source.push_str(text);
            if !has_bold_ranges {
                bold |= run
                    .get("b")
                    .and_then(|value| value.as_bool())
                    .unwrap_or(false);
            }
        } else if run.get("type").and_then(|value| value.as_str()) == Some("dateChip") {
            let attrs = run.get("attrs").unwrap_or(run);
            if let Some(date) = attrs
                .get("date")
                .or_else(|| attrs.get("due"))
                .and_then(|date| date.as_str())
            {
                source.push_str("[[date:");
                source.push_str(date);
                if let Some(time) = attrs.get("time").and_then(|value| value.as_str()) {
                    if !time.is_empty() {
                        source.push('T');
                        source.push_str(time);
                    }
                }
                if let Some(repeat) = attrs.get("recurrence").and_then(|value| value.as_str()) {
                    if repeat != "none" && matches!(repeat, "daily" | "weekly" | "monthly") {
                        source.push('@');
                        source.push_str(repeat);
                    }
                }
                source.push_str("]]");
            }
        }
    }
    (source, bold)
}

#[cfg(test)]
fn content_for_editor(source: &str, bold: bool) -> String {
    let runs = parse_editor_runs(source, bold);
    let mut plain = String::new();
    let json_runs = runs
        .iter()
        .enumerate()
        .map(|(index, run)| {
            if run.kind == "chip" {
                plain.push_str(&run.chip_label);
                serde_json::json!({
                    "type": "dateChip",
                    "attrs": {
                        "chipId": format!("slint-date-{}-{}", index, run.date.as_deref().unwrap_or("unknown")),
                        "date": run.date,
                        "label": run.chip_label,
                    }
                })
            } else {
                plain.push_str(&run.text);
                serde_json::json!({"t": run.text, "b": run.bold})
            }
        })
        .collect::<Vec<_>>();
    serde_json::json!({
        "blocks": [{"type": "text", "runs": json_runs}],
        "plain": plain,
        "editor": {"bold": bold}
    })
    .to_string()
}

fn checklist_marker(line: &str) -> Option<(bool, usize)> {
    let offset = line.len() - line.trim_start().len();
    let rest = &line[offset..];
    if rest.starts_with("[ ] ") {
        Some((false, offset + 4))
    } else if rest.starts_with("[x] ") || rest.starts_with("[X] ") {
        Some((true, offset + 4))
    } else if rest == "[ ]" {
        Some((false, offset + 3))
    } else if rest == "[x]" || rest == "[X]" {
        Some((true, offset + 3))
    } else {
        None
    }
}

fn set_checklist_checked(line: &str, checked: bool) -> Option<String> {
    let (_, marker_end) = checklist_marker(line)?;
    let leading_len = line.len() - line.trim_start().len();
    let body = line[marker_end..].trim_start();
    Some(format!(
        "{}{}{}",
        &line[..leading_len],
        if checked { "[x]" } else { "[ ]" },
        if body.is_empty() {
            "".to_owned()
        } else {
            format!(" {}", body)
        }
    ))
}

/// A bullet is stored as deliberately plain editor syntax. Unlike a visual
/// glyph this is stable through copy/paste and lets the search projection omit
/// list punctuation just as it omits checklist markers.
fn bullet_marker(line: &str) -> Option<usize> {
    let offset = line.len() - line.trim_start().len();
    line[offset..].starts_with("- ").then_some(offset + 2)
}

#[derive(Debug, Clone)]
struct TextReplacement {
    start: usize,
    end: usize,
    replacement: String,
}

#[derive(Debug, Clone)]
struct ListCommandResult {
    source: String,
    caret: usize,
    replacements: Vec<TextReplacement>,
}

fn line_bounds_at(source: &str, offset: usize) -> Option<(usize, usize)> {
    if offset > source.len() || !source.is_char_boundary(offset) {
        return None;
    }
    let start = source[..offset].rfind('\n').map_or(0, |index| index + 1);
    let end = source[offset..]
        .find('\n')
        .map_or(source.len(), |index| offset + index);
    Some((start, end))
}

fn selected_line_bounds(source: &str, start: usize, end: usize) -> Option<Vec<(usize, usize)>> {
    if start > end
        || end > source.len()
        || !source.is_char_boundary(start)
        || !source.is_char_boundary(end)
    {
        return None;
    }
    // A selection ending at the beginning of the next line does not include
    // that next line. This matches native text-editor selection semantics.
    let last = if start == end { end } else { end - 1 };
    let (first_start, _) = line_bounds_at(source, start)?;
    let (_, last_end) = line_bounds_at(source, last)?;
    let mut bounds = Vec::new();
    let mut line_start = first_start;
    loop {
        let line_end = source[line_start..]
            .find('\n')
            .map_or(source.len(), |index| line_start + index);
        bounds.push((line_start, line_end));
        if line_end >= last_end {
            break;
        }
        line_start = line_end + 1;
    }
    Some(bounds)
}

fn apply_replacements(source: &str, replacements: &[TextReplacement]) -> Option<String> {
    let mut previous_end = 0;
    for replacement in replacements {
        if replacement.start < previous_end
            || replacement.start > replacement.end
            || replacement.end > source.len()
            || !source.is_char_boundary(replacement.start)
            || !source.is_char_boundary(replacement.end)
        {
            return None;
        }
        previous_end = replacement.end;
    }
    let mut updated = source.to_owned();
    for replacement in replacements.iter().rev() {
        updated.replace_range(replacement.start..replacement.end, &replacement.replacement);
    }
    Some(updated)
}

fn remap_formats_after_replacements(
    source: &str,
    replacements: &[TextReplacement],
    formats: &mut FormatRanges,
) {
    // Apply independent line edits from the bottom up, exactly as the source
    // mutation does. This retains formatting on every selected line instead
    // of treating a multi-line list toggle as one large replacement.
    let mut current = source.to_owned();
    for replacement in replacements.iter().rev() {
        let mut next = current.clone();
        next.replace_range(replacement.start..replacement.end, &replacement.replacement);
        remap_formats_after_edit(&current, &next, formats);
        current = next;
    }
}

fn map_offset_after_replacements(offset: usize, replacements: &[TextReplacement]) -> usize {
    let mut delta = 0isize;
    for replacement in replacements {
        if offset < replacement.start {
            break;
        }
        if offset <= replacement.end {
            return (replacement.start as isize + delta + replacement.replacement.len() as isize)
                .max(0) as usize;
        }
        delta +=
            replacement.replacement.len() as isize - (replacement.end - replacement.start) as isize;
    }
    (offset as isize + delta).max(0) as usize
}

fn list_command(
    source: &str,
    command: &str,
    anchor: usize,
    cursor: usize,
) -> Option<ListCommandResult> {
    let start = anchor.min(cursor);
    let end = anchor.max(cursor);
    if command == "enter" {
        if start != end {
            return None;
        }
        let (line_start, line_end) = line_bounds_at(source, cursor)?;
        let line = &source[line_start..line_end];
        let leading = line.len() - line.trim_start().len();
        let (marker_end, next_marker) = if let Some((_, marker_end)) = checklist_marker(line) {
            (marker_end, "[ ] ")
        } else if let Some(marker_end) = bullet_marker(line) {
            (marker_end, "- ")
        } else {
            return None;
        };
        if line[marker_end..].trim().is_empty() {
            let replacements = vec![TextReplacement {
                start: line_start,
                end: line_end,
                replacement: line[..leading].to_owned(),
            }];
            return Some(ListCommandResult {
                source: apply_replacements(source, &replacements)?,
                caret: line_start + leading,
                replacements,
            });
        }
        // Splitting inside a marker would corrupt it. Keep the marker intact
        // and split immediately after it; the next checklist is always new
        // and unchecked, including when the current one is completed.
        let split_at = cursor.max(line_start + marker_end);
        let replacement = format!("\n{}{}", &line[..leading], next_marker);
        let replacements = vec![TextReplacement {
            start: split_at,
            end: split_at,
            replacement: replacement.clone(),
        }];
        return Some(ListCommandResult {
            source: apply_replacements(source, &replacements)?,
            caret: split_at + replacement.len(),
            replacements,
        });
    }

    let lines = selected_line_bounds(source, start, end)?;
    let mut replacements = Vec::new();
    for (line_start, line_end) in lines {
        let line = &source[line_start..line_end];
        let leading = line.len() - line.trim_start().len();
        let checklist = checklist_marker(line);
        let bullet = bullet_marker(line);
        let replacement = match command {
            "bullet" if bullet.is_some() => TextReplacement {
                start: line_start + leading,
                end: line_start + bullet.unwrap(),
                replacement: String::new(),
            },
            "bullet" if checklist.is_some() => TextReplacement {
                start: line_start + leading,
                end: line_start + checklist.unwrap().1,
                replacement: "- ".to_owned(),
            },
            "bullet" => TextReplacement {
                start: line_start + leading,
                end: line_start + leading,
                replacement: "- ".to_owned(),
            },
            "checklist" if checklist.is_some() => TextReplacement {
                start: line_start + leading,
                end: line_start + checklist.unwrap().1,
                replacement: String::new(),
            },
            "checklist" if bullet.is_some() => TextReplacement {
                start: line_start + leading,
                end: line_start + bullet.unwrap(),
                replacement: "[ ] ".to_owned(),
            },
            "checklist" => TextReplacement {
                start: line_start + leading,
                end: line_start + leading,
                replacement: "[ ] ".to_owned(),
            },
            _ => return None,
        };
        replacements.push(replacement);
    }
    let caret = map_offset_after_replacements(cursor, &replacements);
    Some(ListCommandResult {
        source: apply_replacements(source, &replacements)?,
        caret,
        replacements,
    })
}

fn source_from_runs(runs: &[serde_json::Value]) -> (String, bool) {
    let mut source = String::new();
    let mut bold = false;
    for run in runs {
        if let Some(text) = run.get("t").and_then(|text| text.as_str()) {
            source.push_str(text);
            bold |= run
                .get("b")
                .and_then(|value| value.as_bool())
                .unwrap_or(false);
        } else if run.get("type").and_then(|value| value.as_str()) == Some("image") {
            if let Some(id) = run
                .pointer("/attrs/attachmentId")
                .and_then(|id| id.as_str())
            {
                if uuid::Uuid::parse_str(id).is_ok() {
                    source.push_str(&inline_images::marker(id));
                }
            }
        } else if run.get("type").and_then(|value| value.as_str()) == Some("dateChip") {
            let attrs = run.get("attrs").unwrap_or(run);
            if let Some(date) = attrs
                .get("date")
                .or_else(|| attrs.get("due"))
                .and_then(|date| date.as_str())
            {
                source.push_str("[[date:");
                source.push_str(date);
                if let Some(time) = attrs.get("time").and_then(|value| value.as_str()) {
                    if !time.is_empty() {
                        source.push('T');
                        source.push_str(time);
                    }
                }
                if let Some(repeat) = attrs.get("recurrence").and_then(|value| value.as_str()) {
                    if matches!(repeat, "daily" | "weekly" | "monthly") {
                        source.push('@');
                        source.push_str(repeat);
                    }
                }
                source.push_str("]]");
            }
        }
    }
    (source, bold)
}

fn editor_state_from_content_with_checklists(content: &str) -> (String, bool, String) {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(content) else {
        return (plain_from_content(content), false, "[]".to_owned());
    };
    let Some(blocks) = value.get("blocks").and_then(|blocks| blocks.as_array()) else {
        return (plain_from_content(content), false, "[]".to_owned());
    };
    let mut lines = Vec::new();
    let has_bold_ranges = value.get("editor").is_some_and(|editor| {
        editor.get("formats").is_some() || editor.get("boldRanges").is_some()
    });
    let mut bold = value
        .get("editor")
        .and_then(|editor| editor.get("bold"))
        .and_then(|value| value.as_bool())
        .unwrap_or(false);
    for block in blocks {
        let block_type = block.get("type").and_then(|value| value.as_str());
        if matches!(block_type, Some("checklist") | Some("list")) {
            for item in block
                .get("items")
                .and_then(|items| items.as_array())
                .into_iter()
                .flatten()
            {
                let (text, item_bold) = item
                    .get("runs")
                    .and_then(|runs| runs.as_array())
                    .map(|runs| source_from_runs(runs))
                    .unwrap_or_else(|| {
                        (
                            item.get("text")
                                .and_then(|text| text.as_str())
                                .unwrap_or("")
                                .to_owned(),
                            false,
                        )
                    });
                if !has_bold_ranges {
                    bold |= item_bold;
                }
                let checked = item
                    .get("checked")
                    .and_then(|value| value.as_bool())
                    .unwrap_or(false);
                if block_type == Some("checklist") {
                    lines.push(format!("{} {}", if checked { "[x]" } else { "[ ]" }, text));
                } else {
                    lines.push(format!("- {text}"));
                }
            }
        } else if let Some(runs) = block.get("runs").and_then(|runs| runs.as_array()) {
            let (text, line_bold) = source_from_runs(runs);
            if !has_bold_ranges {
                bold |= line_bold;
            }
            lines.push(text);
        }
    }
    let ranges = value
        .get("editor")
        .and_then(|editor| editor.get("formats").or_else(|| editor.get("boldRanges")))
        .and_then(|ranges| serde_json::to_string(ranges).ok())
        .unwrap_or_else(|| "[]".to_owned());
    (lines.join("\n"), bold, ranges)
}

fn runs_to_json_with_prefix(runs: &[EditorRun], prefix: &str) -> (Vec<serde_json::Value>, String) {
    let mut plain = String::new();
    let json_runs = runs.iter().enumerate().map(|(index, run)| {
        if run.kind == "chip" {
            plain.push_str(&run.chip_label);
            let value = run.date.as_deref().unwrap_or_default();
            let (value, recurrence) = value.split_once('@').unwrap_or((value, "none"));
            let (day, time) = value.split_once('T').map_or((value, None), |(day, time)| (day, Some(time)));
            serde_json::json!({"type": "dateChip", "attrs": {"chipId": format!("slint-date-{}-{}", prefix, index), "date": day, "time": time, "recurrence": recurrence, "label": run.chip_label}})
        } else if run.kind == "image" {
            serde_json::json!({"type": "image", "attrs": {"attachmentId": run.date}})
        } else {
            plain.push_str(&run.text);
            serde_json::json!({"t": run.text, "b": run.bold})
        }
    }).collect();
    (json_runs, plain)
}

#[cfg(test)]
fn content_for_editor_with_checklists(source: &str, bold: bool) -> String {
    content_for_editor_with_checklists_and_ranges(source, bold, &[])
}

#[cfg(test)]
fn content_for_editor_with_checklists_and_ranges(
    source: &str,
    bold: bool,
    ranges: &[(usize, usize)],
) -> String {
    content_for_editor_with_checklists_and_formats(
        source,
        bold,
        &FormatRanges {
            bold: ranges.to_vec(),
            ..FormatRanges::default()
        },
    )
}

fn content_for_editor_with_checklists_and_formats(
    source: &str,
    bold: bool,
    formats: &FormatRanges,
) -> String {
    let mut blocks = Vec::new();
    let mut plain_lines = Vec::new();
    let mut line_offset = 0;
    for (line_index, line) in source.split('\n').enumerate() {
        if let Some((checked, text_start)) = checklist_marker(line) {
            let runs = parse_editor_runs_with_ranges_at(
                &line[text_start..],
                bold,
                &formats.bold,
                line_offset + text_start,
            );
            let (json_runs, plain) =
                runs_to_json_with_prefix(&runs, &format!("item-{}", line_index));
            plain_lines.push(plain.clone());
            blocks.push(serde_json::json!({"type":"checklist", "items":[{"id":format!("slint-checklist-{}", line_index), "text":plain, "checked":checked, "runs":json_runs}]}));
        } else if let Some(text_start) = bullet_marker(line) {
            let runs = parse_editor_runs_with_ranges_at(
                &line[text_start..],
                bold,
                &formats.bold,
                line_offset + text_start,
            );
            let (json_runs, plain) =
                runs_to_json_with_prefix(&runs, &format!("list-{}", line_index));
            plain_lines.push(plain.clone());
            blocks.push(serde_json::json!({"type":"list", "items":[{"id":format!("slint-list-{}", line_index), "text":plain, "runs":json_runs}]}));
        } else {
            let runs = parse_editor_runs_with_ranges_at(line, bold, &formats.bold, line_offset);
            let (json_runs, plain) =
                runs_to_json_with_prefix(&runs, &format!("line-{}", line_index));
            plain_lines.push(plain);
            blocks.push(serde_json::json!({"type":"text", "runs":json_runs}));
        }
        line_offset += line.len() + 1;
    }
    if blocks.is_empty() {
        blocks.push(serde_json::json!({"type":"text", "runs":[]}));
    }
    serde_json::json!({
        "blocks":blocks,
        "plain":plain_lines.join("\n"),
        "editor":{
            "bold":bold,
            // Keep this legacy field for notes created by the first Slint
            // editor. New readers use formats for all four inline styles.
            "boldRanges":formats.bold.iter().map(|(start,end)| serde_json::json!([start,end])).collect::<Vec<_>>(),
            "formats":{
                "bold":formats.bold.iter().map(|(start,end)| serde_json::json!([start,end])).collect::<Vec<_>>(),
                "italic":formats.italic.iter().map(|(start,end)| serde_json::json!([start,end])).collect::<Vec<_>>(),
                "underline":formats.underline.iter().map(|(start,end)| serde_json::json!([start,end])).collect::<Vec<_>>(),
                "strike":formats.strike.iter().map(|(start,end)| serde_json::json!([start,end])).collect::<Vec<_>>()
            }
        }
    })
        .to_string()
}

fn checklist_preview_items(source: &str) -> Vec<ChecklistState> {
    source
        .split('\n')
        .enumerate()
        .filter_map(|(line_index, line)| {
            let (checked, text_start) = checklist_marker(line)?;
            let text = parse_editor_runs(&line[text_start..], false)
                .into_iter()
                .map(|run| {
                    if run.kind == "chip" {
                        run.chip_label
                    } else {
                        run.text
                    }
                })
                .collect();
            Some(ChecklistState {
                line_index: line_index as i32,
                text,
                checked,
            })
        })
        .collect()
}

fn apply_editor_state(editor: &NoteWindow, source: &str, bold: bool, bold_ranges: &str) {
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

fn sync_date_chip_model(editor: &NoteWindow, source: &str) {
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

fn show_calendar(editor: &NoteWindow, year: i32, month: u32) {
    let Some(days) = date_picker::month_days(year, month) else {
        return;
    };
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

fn initialize_date_picker(editor: &NoteWindow, value: &str) {
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

fn sync_editor_presentation(editor: &NoteWindow, note: &notes::NoteRow) {
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
struct EditorSnapshot {
    content: String,
    color: String,
}

fn editor_snapshot(editor: &NoteWindow) -> EditorSnapshot {
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

fn persist_snapshot(
    db: &Arc<Mutex<Db>>,
    note_id: &str,
    snapshot: &EditorSnapshot,
) -> Result<(), String> {
    let result = {
        let guard = db
            .lock()
            .map_err(|_| "the local database lock is unavailable".to_owned())?;
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

fn mark_editor_dirty(editor: &NoteWindow, state: &Rc<RefCell<EditorSaveState>>) {
    state.borrow_mut().dirty = true;
    editor.set_save_status("Saving…".into());
}

fn reconcile_formats_after_text_edit(editor: &NoteWindow, state: &Rc<RefCell<EditorSaveState>>) {
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

fn remember_editor_text(state: &Rc<RefCell<EditorSaveState>>, text: &str) {
    state.borrow_mut().last_text = text.to_owned();
}

#[cfg(windows)]
fn flush_pending_native_change(state: &Rc<RefCell<EditorSaveState>>) {
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
fn flush_editor(
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

fn prepare_editor_close(
    db: &Arc<Mutex<Db>>,
    note_id: &str,
    editor: &NoteWindow,
    state: &Rc<RefCell<EditorSaveState>>,
    board: &slint::Weak<BoardWindow>,
) -> bool {
    if !flush_editor(db, note_id, editor, state, board) {
        return false;
    }
    let geometry = WindowGeometry::from_window(&editor.window());
    let result = notes::save_window_geom(
        &db.lock().unwrap().conn,
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
            true
        }
        Err(error) => {
            editor.set_save_status(format!("Couldn't close; note stays open ({error})").into());
            false
        }
    }
}

fn schedule_autosave(
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

/// Apply a style to every exact phrase match, then persist the effective style
/// as byte ranges. TextEdit's public Slint 1.9 API cannot report a mouse or
/// keyboard selection, so this is the compact, accessible alternative to
/// exposing character offsets in the note UI.
#[cfg(test)]
fn set_bold_for_phrase(
    source: &str,
    whole_note_bold: bool,
    ranges: &[(usize, usize)],
    phrase: &str,
    make_bold: bool,
) -> Option<(String, usize)> {
    if phrase.is_empty() {
        return None;
    }
    let matches = source.match_indices(phrase).collect::<Vec<_>>();
    if matches.is_empty() {
        return None;
    }
    let mut next_ranges = Vec::new();
    let mut open = None;
    for (offset, character) in source.char_indices() {
        let next = offset + character.len_utf8();
        let mut is_bold = whole_note_bold || range_contains(ranges, offset, next);
        if matches.iter().any(|(match_start, value)| {
            offset >= *match_start && next <= *match_start + value.len()
        }) {
            is_bold = make_bold;
        }
        match (open, is_bold) {
            (None, true) => open = Some(offset),
            (Some(begin), false) => {
                next_ranges.push((begin, offset));
                open = None;
            }
            _ => {}
        }
    }
    if let Some(begin) = open {
        next_ranges.push((begin, source.len()));
    }
    Some((serialize_bold_ranges(&next_ranges), matches.len()))
}
fn paper_hex(value: &str) -> &str {
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
fn color(value: &str) -> slint::Color {
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

fn visible_notes(db: &Arc<Mutex<Db>>, query: &str, board_view: &str) -> Vec<notes::NoteRow> {
    let guard = db.lock().expect("database lock poisoned");
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

fn floating_note_ids(conn: &rusqlite::Connection) -> rusqlite::Result<Vec<String>> {
    Ok(notes::list(conn, true, false, None)?
        .into_iter()
        .filter(|note| note.is_floating && !note.is_archived)
        .map(|note| note.id)
        .collect())
}

fn note_preview(plain: &str, title: Option<&str>) -> String {
    let mut lines = plain
        .lines()
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

fn refresh(board: &BoardWindow, db: &Arc<Mutex<Db>>, query: &str) {
    let cards = visible_notes(db, query, &board.get_board_view())
        .into_iter()
        .map(|note| {
            let plain = plain_from_content(&note.content);
            let preview = note_preview(&plain, note.title.as_deref());
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
            NoteCard {
                id: note.id.into(),
                title: note.title.unwrap_or_else(|| "Untitled note".into()).into(),
                preview: preview.into(),
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
fn due_reminders(
    conn: &rusqlite::Connection,
    as_of: NaiveDate,
) -> rusqlite::Result<Vec<DueReminder>> {
    due_reminders_from_rows(conn, reminders::due_on(conn, as_of)?)
}

fn due_reminders_at(
    conn: &rusqlite::Connection,
    as_of: chrono::DateTime<Local>,
) -> rusqlite::Result<Vec<DueReminder>> {
    due_reminders_from_rows(conn, reminders::due_at(conn, as_of)?)
}

fn due_reminders_from_rows(
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
            let source_date = if reminder.due_time.is_empty() {
                reminder.due_date.clone()
            } else {
                format!("{}T{}", reminder.due_date, reminder.due_time)
            };
            let label = if reminder.label == date_label(&source_date) {
                String::new()
            } else {
                reminder.label
            };
            due.push(DueReminder {
                id: reminder.id,
                title: note.title.unwrap_or_default(),
                label,
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

fn act_on_reminder(
    conn: &rusqlite::Connection,
    reminder_id: &str,
    status: &str,
    as_of: NaiveDate,
) -> rusqlite::Result<Vec<DueReminder>> {
    reminders::set_status(conn, reminder_id, status)?;
    due_reminders(conn, as_of)
}

fn refresh_due_reminders(board: &BoardWindow, db: &Arc<Mutex<Db>>) {
    let cards = {
        let guard = db.lock().expect("database lock poisoned");
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
fn show_board(board: &BoardWindow) -> Result<(), slint::PlatformError> {
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
    static BOARD_RESTORE: RefCell<Option<WindowGeometry>> = const { RefCell::new(None) };
}

#[cfg(windows)]
fn restore_board_geometry(board: &BoardWindow, geometry: WindowGeometry) {
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

/// Wire Settings' Google Calendar section to the background sync worker.
/// Status arrives on the worker thread and is applied on the UI thread.
#[cfg(windows)]
fn setup_calendar(board: &BoardWindow, db: &Arc<Mutex<Db>>) {
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
            let config = db
                .lock()
                .ok()
                .and_then(|guard| calendar_sync::load_config(&guard.conn).ok().flatten());
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
fn track_board_geometry(board: &BoardWindow, db: &Arc<Mutex<Db>>) {
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
                        &db.lock().unwrap().conn,
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
fn surface_unannounced_due(board: &BoardWindow, db: &Arc<Mutex<Db>>) {
    let rows = {
        let guard = db.lock().expect("database lock poisoned");
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
        let guard = db.lock().expect("database lock poisoned");
        for row in rows {
            let mut delivered = false;
            #[cfg(windows)]
            {
                let title = notes::get(&guard.conn, &row.note_id)
                    .ok()
                    .flatten()
                    .and_then(|note| note.title)
                    .unwrap_or_default();
                match notification::show(&row, &title) {
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
        if show_board(board).is_ok() {
            let guard = db.lock().expect("database lock poisoned");
            for row in fallback {
                let _ = reminders::mark_announced(&guard.conn, &row);
            }
        }
    }
}

#[cfg(windows)]
fn board_accepts_note_drop(board: &BoardWindow) -> bool {
    board.window().is_visible()
        && board.get_board_view() == "notes"
        && !board.get_settings_open()
        && board.get_pending_purge_id().is_empty()
}

#[cfg(windows)]
fn detach_note_at(
    board: &BoardWindow,
    db: &Arc<Mutex<Db>>,
    id: &str,
    windows: &Rc<RefCell<Vec<EditorEntry>>>,
    cursor: PhysicalPosition,
) -> Result<(), String> {
    let valid = notes::get(&db.lock().unwrap().conn, id)
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
        &db.lock().unwrap().conn,
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
    static NEXT_WINDOW_INACTIVE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

#[cfg(windows)]
fn drag_ghost_position(cursor: PhysicalPosition) -> PhysicalPosition {
    // Same offset detach_note_at uses, so the preview shows where it lands.
    PhysicalPosition::new(cursor.x.saturating_sub(24), cursor.y.saturating_sub(16))
}

#[cfg(windows)]
fn show_drag_ghost(
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
fn setup_board_card_drag(
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

fn main() -> Result<(), Box<dyn std::error::Error>> {
    #[cfg(windows)]
    if matches!(
        std::env::args().nth(1).as_deref(),
        Some("--reliability-probe" | "--crash-fixture")
    ) {
        reliability_probe::run()?;
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
    #[cfg(windows)]
    if std::env::args().nth(1).as_deref() == Some("--window-probe") {
        return editor_probe::window_roundtrip();
    }
    #[cfg(windows)]
    if matches!(
        std::env::args().nth(1).as_deref(),
        Some("--editor-probe" | "--editor-soak" | "--attachment-probe" | "--attachment-soak")
    ) {
        return editor_probe::run();
    }
    #[cfg(windows)]
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
    let db = Arc::new(Mutex::new(Db::open_in_dir(&app_data_dir())?));
    // Recover files left by a crash between file creation and metadata commit.
    // The primary instance owns the database and imports share this mutex.
    let attachment_cleanup_error = attachments::cleanup_orphans(&db.lock().unwrap().conn)
        .err()
        .map(|error| format!("Image cleanup needs retry: {error}"));
    let board = BoardWindow::new()?;
    let (preferences, settings_error) = match settings::load(&db.lock().unwrap().conn) {
        Ok(preferences) => (preferences, String::new()),
        Err(error) => (
            settings::Preferences::default(),
            format!("Could not load settings: {error}. Existing settings were preserved."),
        ),
    };
    board.invoke_apply_theme(preferences.dark_theme);
    #[cfg(windows)]
    {
        let saved = settings::load_board_geometry(&db.lock().unwrap().conn)
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
    let pending_import = Rc::new(RefCell::new(None::<db::import::ImportPlan>));
    {
        let weak = board.as_weak();
        let db = db.clone();
        let pending = pending_import.clone();
        board.on_preview_import(move || {
            let Some(board) = weak.upgrade() else { return; };
            pending.borrow_mut().take();
            board.set_import_pending(false);
            let result = image_picker::select_database(&board.window()).and_then(|path| {
                let Some(path) = path else { return Ok(()); };
                let guard = db.lock().unwrap();
                let plan = db::import::preview(&path, &guard.conn)?;
                let conflicts = plan.conflicts(&guard.conn)?;
                board.set_import_status(format!("{}: {} notes, {} existing IDs skipped. Dates become local minute-level schedules. A database backup is made before import; imported windows stay closed.", plan.source.display(), plan.count(), conflicts).into());
                *pending.borrow_mut() = Some(plan);
                board.set_import_pending(true);
                Ok(())
            });
            if let Err(error) = result { board.set_import_status(format!("Import cancelled: {error}").into()); }
        });
    }
    {
        let weak = board.as_weak();
        let db = db.clone();
        let pending = pending_import.clone();
        board.on_confirm_import(move || {
            let Some(board) = weak.upgrade() else {
                return;
            };
            let Some(plan) = pending.borrow_mut().take() else {
                return;
            };
            board.set_import_pending(false);
            let result = db::import::apply(&db.lock().unwrap().conn, &plan);
            match result {
                Ok((count, backup)) => {
                    board.set_import_status(
                        format!("Imported {count} notes. Backup: {}", backup.display()).into(),
                    );
                    refresh(&board, &db, board.get_query().as_str());
                    refresh_due_reminders(&board, &db);
                }
                Err(error) => board
                    .set_import_status(format!("Import failed; no notes imported: {error}").into()),
            }
        });
    }
    {
        let weak = board.as_weak();
        let pending = pending_import.clone();
        board.on_cancel_import(move || {
            pending.borrow_mut().take();
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
        board.on_apply_settings(move || {
            let Some(board) = weak.upgrade() else {
                return;
            };
            let next = settings::Preferences {
                dark_theme: board.get_setting_dark(),
                shortcut: board.get_setting_shortcut().trim().into(),
                start_with_windows: board.get_setting_startup(),
                ..settings::Preferences::default()
            };
            let result = settings::apply(
                &db.lock().unwrap().conn,
                &preferences.borrow(),
                &next,
                settings::set_windows_startup,
            );
            match result {
                Ok(()) => {
                    board.invoke_apply_theme(next.dark_theme);
                    *preferences.borrow_mut() = next;
                    board.set_settings_status("Saved. Shortcut changes apply next launch.".into());
                }
                Err(error) => {
                    board.set_settings_status(format!("Settings not applied: {error}").into())
                }
            }
        });
    }
    let board_weak = board.as_weak();
    let editor_windows: Rc<RefCell<Vec<EditorEntry>>> = Rc::new(RefCell::new(Vec::new()));
    #[cfg(windows)]
    setup_board_card_drag(&board, &db, &editor_windows);
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
                        let result = reminders::activate(
                            &db.lock().unwrap().conn,
                            &activation,
                            Local::now(),
                        );
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
                let _ = attachments::retire_unplaced(&db.lock().unwrap().conn, &id);
            }
            for entry in windows.iter_mut() {
                #[cfg(windows)]
                window_chrome::update_hover(&entry.window);
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
                    let guard = db.lock().unwrap();
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
                let guard = db.lock().expect("database lock poisoned");
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
                let guard = db.lock().expect("database lock poisoned");
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
                let guard = db.lock().expect("database lock poisoned");
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
                let guard = db.lock().unwrap();
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
                refresh(&board, &db, &board.get_query().to_string());
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
                let guard = db.lock().expect("database lock poisoned");
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
                let guard = db.lock().expect("database lock poisoned");
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
                let guard = db.lock().expect("database lock poisoned");
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
                let guard = db.lock().expect("database lock poisoned");
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
                let guard = db.lock().expect("database lock poisoned");
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
                let guard = db.lock().expect("database lock poisoned");
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
            let guard = db.lock().unwrap();
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
            let guard = db.lock().unwrap();
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
                    let editors = windows.borrow();
                    let mut failed = false;
                    for entry in editors.iter() {
                        if !flush_editor(&db, &entry.id, &entry.window, &entry.save_state, &weak) {
                            failed = true;
                            let _ = entry.window.show();
                        }
                    }
                    if failed {
                        // A normal Quit must not throw unsaved data away. The
                        // failed editor stays visible with its retryable error.
                        let _ = show_board(&board);
                    } else {
                        std::process::exit(0);
                    }
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
    let restore_ids = floating_note_ids(&db.lock().unwrap().conn)?;
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

fn sync_attachments(editor: &NoteWindow, conn: &rusqlite::Connection, id: &str, index: i32) {
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
fn insert_native_image(
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
    attachments::commit_prepared(
        &db.lock().unwrap().conn,
        id,
        prepared,
        Some(&content.to_string()),
    )
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

fn open_editor(
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
            &db.lock().unwrap().conn,
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
                let guard = db.lock().unwrap();
                notes::get(&guard.conn, id).ok().flatten()
            };
            if let Some(note) = note {
                sync_editor_presentation(&entry.window, &note);
            }
        }
        let _ = entry.window.show();
        let geometry = WindowGeometry::from_window(&entry.window.window());
        let _ = notes::save_window_geom(
            &db.lock().unwrap().conn,
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
        let guard = db.lock().unwrap();
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
    sync_attachments(&editor, &db.lock().unwrap().conn, id, 0);
    if let Some(geometry) = WindowGeometry::from_db(note.window) {
        geometry.apply(&editor.window());
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
            let result = image_picker::select(&window.window()).and_then(|path| {
                if let Some(path) = path {
                    #[cfg(windows)]
                    if let Some(native) = native_for_image.borrow().as_ref() {
                        let prepared = attachments::prepare_file(&path)?;
                        insert_native_image(&window, native, &db, &id, prepared)
                            .map_err(anyhow::Error::msg)?;
                        flush_editor(&db, &id, &window, &state, &board);
                        return Ok(());
                    }
                    let guard = db.lock().unwrap();
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
                    &db.lock().unwrap().conn,
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
                let guard = db.lock().unwrap();
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
            ) {
                if window.hide().is_ok() {
                    if let Some(board) = weak_return_board.upgrade() {
                        board.set_board_view("notes".into());
                        board.set_query("".into());
                        refresh(&board, &db_return, "");
                        let _ = show_board(&board);
                    }
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
        } else {
            format!("{day}T{time}")
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
        let (anchor, cursor) = native_for_list_command.borrow().as_ref()
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
        apply_editor_state(&window, &result.source, window.get_editor_bold(), &serialized);
        #[cfg(windows)]
        if let Some(native) = native_for_list_command.borrow().as_ref() {
            native.set_document(&native_document_from_editor(&window));
            native.restore_selection(&result.source, result.caret, result.caret);
        }
        remember_editor_text(&save_state_for_list_command, &result.source);
        window.set_format_status(
            "List structure saved. Task checkboxes below remain a preview while editing stays native text."
                .into(),
        );
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
            let result = match db_always_on_top.lock() {
                Ok(guard) => notes::set_always_on_top(&guard.conn, &always_on_top_id, value)
                    .map_err(|error| error.to_string()),
                Err(_) => Err("the local database lock is unavailable".to_owned()),
            };
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
            remember_editor_text(
                &save_state_for_checklist,
                &window.get_editor_text().to_string(),
            );
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
        geometry.apply(&editor.window());
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
                &editor.window(),
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
                        let guard = image_db
                            .lock()
                            .map_err(|_| "Database unavailable".to_owned())?;
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
                        let items =
                            attachments::list(&db.lock().unwrap().conn, id).unwrap_or_default();
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
    let geometry = WindowGeometry::from_window(&editor.window());
    {
        let guard = db.lock().unwrap();
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

#[cfg(test)]
mod editor_tests {
    use super::*;
    use crate::db::reminders;
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
    fn restart_restores_only_live_floating_notes_and_retains_returned_geometry() {
        let directory =
            std::env::temp_dir().join(format!("sticky-restoration-test-{}", Uuid::new_v4()));
        let db = Db::open_in_dir(&directory).unwrap();
        for id in ["open", "closed", "archived", "deleted"] {
            notes::create(&db.conn, id, EMPTY_CONTENT, "#FFF9B1").unwrap();
            notes::save_window_geom(&db.conn, id, -120.0, 240.0, 470.0, 440.0, id != "closed")
                .unwrap();
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
        let path =
            std::env::temp_dir().join(format!("slint-reminder-surface-test-{}", Uuid::new_v4()));
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
        assert_eq!(displayed[0].title, "Call client Sep 18");
        assert_eq!(displayed[0].label, "");
        assert_eq!(displayed[0].due_label, "Sep 18");

        let after_complete = act_on_reminder(&db.conn, &displayed[0].id, "completed", due).unwrap();
        assert_eq!(after_complete.len(), 1);
        let after_dismiss =
            act_on_reminder(&db.conn, &after_complete[0].id, "dismissed", due).unwrap();
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
        assert!(
            set_selection_style(source, &mut formats, "underline", cafe_start, cafe_end).unwrap()
        );
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
            let guard = db.lock().unwrap();
            notes::create(&guard.conn, "missing", EMPTY_CONTENT, "#FFF9B1").unwrap();
        }
        assert!(persist_snapshot(&db, "missing", &snapshot).is_ok());
        let saved = db
            .lock()
            .unwrap()
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
}
