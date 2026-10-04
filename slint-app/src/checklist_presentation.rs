//! Checklist squares occupy the same three UTF-16 positions as source markers.
//! Stored source and date/style offsets remain compatible with existing notes.
pub fn render(text: &str) -> String {
    map_lines(text, |line| {
        let offset = line.len() - line.trim_start().len();
        let (checked, _) = crate::checklist_marker(line)?;
        Some(format!(
            "{}{}  {}",
            &line[..offset],
            if checked { '☑' } else { '☐' },
            &line[offset + 3..]
        ))
    })
}
pub fn restore(text: &str) -> String {
    map_lines(text, |line| {
        let offset = line.len() - line.trim_start().len();
        let rest = &line[offset..];
        let (prefix, marker) = if rest.starts_with("☐  ") {
            ("☐  ", "[ ]")
        } else if rest.starts_with("☑  ") {
            ("☑  ", "[x]")
        } else {
            return None;
        };
        Some(format!(
            "{}{}{}",
            &line[..offset],
            marker,
            &rest[prefix.len()..]
        ))
    })
}
fn map_lines(text: &str, map: impl Fn(&str) -> Option<String>) -> String {
    text.split('\n')
        .map(|line| map(line).unwrap_or_else(|| line.to_owned()))
        .collect::<Vec<_>>()
        .join("\n")
}
pub fn target(source: &str, offset: usize) -> Option<(i32, bool)> {
    let mut start = 0;
    for (index, line) in source.split('\n').enumerate() {
        if let Some((checked, _)) = crate::checklist_marker(line) {
            let marker = start + line.len() - line.trim_start().len();
            if offset >= marker && offset < marker + 3 {
                return Some((index as i32, !checked));
            }
        }
        start += line.len() + 1;
    }
    None
}

/// Source byte ranges of completed task bodies, excluding indentation/markers.
pub fn completed_bodies(source: &str) -> Vec<(usize, usize)> {
    let mut start = 0;
    let mut ranges = Vec::new();
    for line in source.split('\n') {
        if matches!(crate::checklist_marker(line), Some((true, _))) {
            let marker = line.len() - line.trim_start().len();
            let body =
                marker + 3 + line[marker + 3..].len() - line[marker + 3..].trim_start().len();
            if body < line.len() {
                ranges.push((start + body, start + line.len()));
            }
        }
        start += line.len() + 1;
    }
    ranges
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn unicode_dates_indentation_and_empty_tasks_roundtrip() {
        let text = "Title 😀\n  [ ] café [[date:2026-10-10]]\n[x] Done\n[ ]\n☐ ordinary text\n";
        let rendered = render(text);
        assert_eq!(restore(&rendered), text);
        assert_eq!(rendered.encode_utf16().count(), text.encode_utf16().count());
        assert!(rendered.contains("☐   café"));
    }
    #[test]
    fn completed_ranges_exclude_markers_and_plain_lines() {
        let source = "[ ] open\n  [x] café 😀\nPlain [x] text\n[x]";
        let ranges = completed_bodies(source);
        assert_eq!(ranges.len(), 1);
        assert_eq!(&source[ranges[0].0..ranges[0].1], "café 😀");
    }
    #[test]
    fn only_marker_clicks_toggle() {
        assert_eq!(target("😀\n  [ ] task", 7), Some((1, true)));
        assert_eq!(target("[x] task", 1), Some((0, false)));
        assert_eq!(target("[ ] task", 5), None);
        assert_eq!(target("Plain [ ] text", 7), None);
    }
    #[test]
    fn literal_squares_and_non_tasks_stay_text() {
        let text = "☐ literal\nKeep ☑  this\n[ ]not a task";
        assert_eq!(restore(&render(text)), text);
    }
}
