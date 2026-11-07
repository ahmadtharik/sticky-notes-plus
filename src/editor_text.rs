//! Note text, checklist, date-chip and inline-format logic. Pure functions
//! over the editor's source text; nothing here touches a window.

use super::*;

pub(crate) fn plain_from_content(content: &str) -> String {
    notes::content_to_plain(content)
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct EditorRun {
    pub(crate) kind: &'static str,
    pub(crate) text: String,
    pub(crate) chip_label: String,
    pub(crate) date: Option<String>,
    pub(crate) bold: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ChecklistState {
    pub(crate) line_index: i32,
    pub(crate) text: String,
    pub(crate) checked: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DateChipState {
    pub(crate) occurrence: i32,
    pub(crate) start: usize,
    pub(crate) end: usize,
    pub(crate) date: String,
    pub(crate) label: String,
}

pub(crate) fn date_label(value: &str) -> String {
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

/// Reads a typed time such as "9:30", "0930", "9.30", "9pm" or "2:30 pm" as
/// the stored 24-hour "HH:MM".
pub(crate) fn normalize_time(input: &str) -> Option<String> {
    let lower = input.trim().to_ascii_lowercase();
    let (clock, meridiem) = if let Some(clock) = lower.strip_suffix("am") {
        (clock.trim_end(), Some(false))
    } else if let Some(clock) = lower.strip_suffix("pm") {
        (clock.trim_end(), Some(true))
    } else {
        (lower.as_str(), None)
    };
    let (hour, minute) = match clock.split_once([':', '.']) {
        Some((hour, minute)) if minute.len() == 2 => (hour, minute),
        Some(_) => return None,
        None if clock.len() <= 2 => (clock, "00"),
        None if clock.len() <= 4 => clock.split_at(clock.len() - 2),
        None => return None,
    };
    if hour.is_empty()
        || !(hour.len() <= 2
            && hour
                .bytes()
                .chain(minute.bytes())
                .all(|b| b.is_ascii_digit()))
    {
        return None;
    }
    let (mut hour, minute): (u32, u32) = (hour.parse().ok()?, minute.parse().ok()?);
    match meridiem {
        Some(_) if !(1..=12).contains(&hour) => return None,
        Some(pm) => hour = hour % 12 + if pm { 12 } else { 0 },
        None => {}
    }
    (hour < 24 && minute < 60).then(|| format!("{hour:02}:{minute:02}"))
}

/// A marker only becomes a chip when its value is a real ISO calendar date.
/// Keeping malformed input as plain text prevents a typo from creating a
/// reminder which cannot be edited through the native date panel.
pub(crate) fn valid_date(value: &str) -> bool {
    let (value, repeat) = value.split_once('@').unwrap_or((value, "none"));
    if !matches!(repeat, "none" | "daily" | "weekly" | "monthly") {
        return false;
    }
    let (day, time) = value
        .split_once('T')
        .map_or((value, None), |(day, time)| (day, Some(time)));
    day.len() == 10
        && NaiveDate::parse_from_str(day, "%Y-%m-%d").is_ok()
        && time.is_none_or(|time| {
            time.len() == 5 && chrono::NaiveTime::parse_from_str(time, "%H:%M").is_ok()
        })
}

/// Return editable date marker occurrences in source order. `start`/`end`
/// cover the entire marker, which makes removal independent of display labels.
pub(crate) fn date_chip_occurrences(source: &str) -> Vec<DateChipState> {
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

pub(crate) fn replace_date_chip(
    source: &str,
    occurrence: i32,
    date: &str,
) -> Result<String, &'static str> {
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

pub(crate) fn remove_date_chip(source: &str, occurrence: i32) -> Result<String, &'static str> {
    let chip = date_chip_occurrences(source)
        .into_iter()
        .find(|chip| chip.occurrence == occurrence)
        .ok_or("That date is no longer in this note.")?;
    let mut updated = source.to_owned();
    updated.replace_range(chip.start..chip.end, "");
    Ok(updated)
}

pub(crate) fn append_date_chip(source: &str, date: &str) -> Result<String, &'static str> {
    if !valid_date(date) {
        return Err("Enter a real date as YYYY-MM-DD.");
    }
    let separator = if !source.is_empty() && !source.chars().last().is_some_and(char::is_whitespace)
    {
        " "
    } else {
        ""
    };
    Ok(format!("{source}{separator}[[date:{date}]]"))
}

pub(crate) fn push_text_run(runs: &mut Vec<EditorRun>, text: &str, bold: bool) {
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
pub(crate) struct FormatRanges {
    pub(crate) bold: Vec<(usize, usize)>,
    pub(crate) italic: Vec<(usize, usize)>,
    pub(crate) underline: Vec<(usize, usize)>,
    pub(crate) strike: Vec<(usize, usize)>,
}

impl FormatRanges {
    pub(crate) fn style_mut(&mut self, style: &str) -> Option<&mut Vec<(usize, usize)>> {
        match style {
            "bold" => Some(&mut self.bold),
            "italic" => Some(&mut self.italic),
            "underline" => Some(&mut self.underline),
            "strike" => Some(&mut self.strike),
            _ => None,
        }
    }
}

pub(crate) fn parse_range_list(value: Option<&serde_json::Value>) -> Vec<(usize, usize)> {
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
pub(crate) fn parse_format_ranges(value: &str) -> FormatRanges {
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
pub(crate) fn parse_bold_ranges(value: &str) -> Vec<(usize, usize)> {
    parse_format_ranges(value).bold
}

#[cfg(test)]
pub(crate) fn serialize_bold_ranges(ranges: &[(usize, usize)]) -> String {
    serde_json::to_string(
        &ranges
            .iter()
            .filter(|(start, end)| start < end)
            .map(|(start, end)| [*start, *end])
            .collect::<Vec<_>>(),
    )
    .unwrap_or_else(|_| "[]".to_owned())
}

pub(crate) fn serialize_format_ranges(formats: &FormatRanges) -> String {
    serde_json::json!({
        "bold": formats.bold,
        "italic": formats.italic,
        "underline": formats.underline,
        "strike": formats.strike,
    })
    .to_string()
}

pub(crate) fn canonicalize_ranges(source: &str, ranges: &mut Vec<(usize, usize)>) {
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

pub(crate) fn canonicalize_formats(source: &str, formats: &mut FormatRanges) {
    for ranges in [
        &mut formats.bold,
        &mut formats.italic,
        &mut formats.underline,
        &mut formats.strike,
    ] {
        canonicalize_ranges(source, ranges);
    }
}

pub(crate) fn selection_is_covered(ranges: &[(usize, usize)], start: usize, end: usize) -> bool {
    ranges
        .iter()
        .any(|(range_start, range_end)| *range_start <= start && *range_end >= end)
}

pub(crate) fn set_selection_style(
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

pub(crate) fn common_edit_span(old: &str, new: &str) -> (usize, usize, usize) {
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

pub(crate) fn remap_ranges_after_edit(
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

pub(crate) fn remap_formats_after_edit(
    source_before: &str,
    source_after: &str,
    formats: &mut FormatRanges,
) {
    for ranges in [
        &mut formats.bold,
        &mut formats.italic,
        &mut formats.underline,
        &mut formats.strike,
    ] {
        remap_ranges_after_edit(source_before, source_after, ranges);
    }
}

pub(crate) fn range_contains(ranges: &[(usize, usize)], start: usize, end: usize) -> bool {
    ranges
        .iter()
        .any(|(range_start, range_end)| *range_start < end && *range_end > start)
}

pub(crate) fn push_text_run_with_ranges(
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
pub(crate) fn parse_editor_runs(source: &str, bold: bool) -> Vec<EditorRun> {
    parse_editor_runs_with_ranges_at(source, bold, &[], 0)
}

pub(crate) fn parse_editor_runs_with_ranges(
    source: &str,
    bold: bool,
    ranges: &[(usize, usize)],
) -> Vec<EditorRun> {
    parse_editor_runs_with_ranges_at(source, bold, ranges, 0)
}

pub(crate) fn parse_editor_runs_with_ranges_at(
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
pub(crate) fn editor_state_from_content(content: &str) -> (String, bool) {
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
pub(crate) fn content_for_editor(source: &str, bold: bool) -> String {
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

pub(crate) fn checklist_marker(line: &str) -> Option<(bool, usize)> {
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

pub(crate) fn set_checklist_checked(line: &str, checked: bool) -> Option<String> {
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
pub(crate) fn bullet_marker(line: &str) -> Option<usize> {
    let offset = line.len() - line.trim_start().len();
    line[offset..].starts_with("- ").then_some(offset + 2)
}

#[derive(Debug, Clone)]
pub(crate) struct TextReplacement {
    pub(crate) start: usize,
    pub(crate) end: usize,
    pub(crate) replacement: String,
}

#[derive(Debug, Clone)]
pub(crate) struct ListCommandResult {
    pub(crate) source: String,
    pub(crate) caret: usize,
    pub(crate) replacements: Vec<TextReplacement>,
}

pub(crate) fn line_bounds_at(source: &str, offset: usize) -> Option<(usize, usize)> {
    if offset > source.len() || !source.is_char_boundary(offset) {
        return None;
    }
    let start = source[..offset].rfind('\n').map_or(0, |index| index + 1);
    let end = source[offset..]
        .find('\n')
        .map_or(source.len(), |index| offset + index);
    Some((start, end))
}

pub(crate) fn selected_line_bounds(
    source: &str,
    start: usize,
    end: usize,
) -> Option<Vec<(usize, usize)>> {
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

pub(crate) fn apply_replacements(source: &str, replacements: &[TextReplacement]) -> Option<String> {
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

pub(crate) fn remap_formats_after_replacements(
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

pub(crate) fn map_offset_after_replacements(
    offset: usize,
    replacements: &[TextReplacement],
) -> usize {
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

/// Typing the `]` that completes `[ ]` or `[x]` right after a bullet's `- `
/// turns the line into a checklist item. Enter continues a bullet, so without
/// this the box would stay literal text inside the bullet.
pub(crate) fn bullet_to_checklist(source: &str, cursor: usize) -> Option<ListCommandResult> {
    let (line_start, _) = line_bounds_at(source, cursor)?;
    let before = &source[line_start..cursor];
    let leading = before.len() - before.trim_start().len();
    let marker = &before[leading..];
    if !matches!(marker, "- [ " | "- [x" | "- [X") {
        return None;
    }
    let bullet = line_start + leading;
    let replacements = vec![
        TextReplacement {
            start: bullet,
            end: bullet + 2,
            replacement: String::new(),
        },
        TextReplacement {
            start: cursor,
            end: cursor,
            replacement: "]".to_owned(),
        },
    ];
    Some(ListCommandResult {
        source: apply_replacements(source, &replacements)?,
        caret: cursor - 1,
        replacements,
    })
}

pub(crate) fn list_command(
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
        } else {
            let marker_end = bullet_marker(line)?;
            (marker_end, "- ")
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

pub(crate) fn source_from_runs(runs: &[serde_json::Value]) -> (String, bool) {
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

pub(crate) fn editor_state_from_content_with_checklists(content: &str) -> (String, bool, String) {
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

pub(crate) fn runs_to_json_with_prefix(
    runs: &[EditorRun],
    prefix: &str,
) -> (Vec<serde_json::Value>, String) {
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
pub(crate) fn content_for_editor_with_checklists(source: &str, bold: bool) -> String {
    content_for_editor_with_checklists_and_ranges(source, bold, &[])
}

#[cfg(test)]
pub(crate) fn content_for_editor_with_checklists_and_ranges(
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

pub(crate) fn content_for_editor_with_checklists_and_formats(
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

pub(crate) fn checklist_preview_items(source: &str) -> Vec<ChecklistState> {
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

/// Apply a style to every exact phrase match, then persist the effective style
/// as byte ranges. TextEdit's public Slint 1.9 API cannot report a mouse or
/// keyboard selection, so this is the compact, accessible alternative to
/// exposing character offsets in the note UI.
#[cfg(test)]
pub(crate) fn set_bold_for_phrase(
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
