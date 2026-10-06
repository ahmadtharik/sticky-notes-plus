//! Lossless source/control coordinate projection for native inline date chips.
//! UUID metadata stays in editor-owned ranges; RichEdit text holds date labels.
use uuid::Uuid;

#[derive(Clone, Debug)]
pub struct ChipSpan {
    pub source_start: usize,
    pub source_end: usize,
    pub control_start: usize,
    pub hidden_end: usize,
    pub control_end: usize,
    pub id: String,
    pub image: bool,
}

pub struct Projection {
    pub source: String,
    pub control: String,
    pub chips: Vec<ChipSpan>,
    pub repaired: bool,
}

/// A chip's identity held outside RichEdit's text buffer. The native host
/// supplies live UTF-16 ranges (TOM ITextRange) when it snapshots the control.
#[derive(Clone, Debug)]
pub struct TrackedChip {
    pub control_start: usize,
    pub control_end: usize,
    pub id: String,
    pub date: String,
}

fn units(value: &str) -> usize {
    value.encode_utf16().count()
}

pub fn content_ids(content: &str) -> Vec<String> {
    let mut ids = Vec::new();
    let mut value: serde_json::Value = serde_json::from_str(content).unwrap_or_default();
    visit_chips(&mut value, &mut |attrs| {
        ids.push(
            attrs
                .get("chipId")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
                .to_owned(),
        );
    });
    ids
}

pub fn with_ids(content: &str, ids: &[String]) -> String {
    let mut value: serde_json::Value = serde_json::from_str(content).unwrap();
    let mut index = 0;
    visit_chips(&mut value, &mut |attrs| {
        if let Some(id) = ids.get(index).filter(|id| Uuid::parse_str(id).is_ok()) {
            attrs["chipId"] = id.clone().into();
        }
        index += 1;
    });
    value.to_string()
}

fn visit_chips(value: &mut serde_json::Value, visitor: &mut impl FnMut(&mut serde_json::Value)) {
    match value {
        serde_json::Value::Array(values) => {
            for value in values {
                visit_chips(value, visitor);
            }
        }
        serde_json::Value::Object(object) => {
            if object.get("type").and_then(serde_json::Value::as_str) == Some("dateChip") {
                if let Some(attrs) = object.get_mut("attrs") {
                    visitor(attrs);
                }
            } else {
                // Follow only the content schema, not arbitrary editor metadata.
                for key in ["blocks", "items", "runs"] {
                    if let Some(value) = object.get_mut(key) {
                        visit_chips(value, visitor);
                    }
                }
            }
        }
        _ => {}
    }
}

impl Projection {
    /// Build a document whose control text contains only visible labels. The
    /// returned spans must be associated with native ranges by the host.
    pub fn from_source_visible(source: &str, ids: &[String]) -> Self {
        let mut control = String::new();
        let mut chips = Vec::new();
        let mut cursor = 0;
        let mut seen = std::collections::HashSet::new();
        let mut atoms: Vec<_> = crate::date_chip_occurrences(source)
            .into_iter()
            .enumerate()
            .map(|(index, chip)| (chip.start, chip.end, Some(index), chip.label, String::new()))
            .collect();
        atoms.extend(
            crate::inline_images::occurrences(source)
                .into_iter()
                .map(|image| (image.start, image.end, None, "□".into(), image.id)),
        );
        atoms.sort_by_key(|atom| atom.0);
        for (start, end, index, label, image_id) in atoms {
            if start < cursor {
                continue;
            }
            control.push_str(&source[cursor..start]);
            let image = index.is_none();
            let id = if let Some(index) = index {
                ids.get(index)
                    .filter(|id| Uuid::parse_str(id).is_ok() && seen.insert((*id).clone()))
                    .cloned()
                    .unwrap_or_else(|| Uuid::new_v4().to_string())
            } else {
                image_id
            };
            let control_start = units(&control);
            control.push_str(&if image { label } else { format!(" {label} ") });
            chips.push(ChipSpan {
                source_start: start,
                source_end: end,
                control_start,
                hidden_end: control_start,
                control_end: units(&control),
                id,
                image,
            });
            cursor = end;
        }
        control.push_str(&source[cursor..]);
        Self {
            source: source.to_owned(),
            control,
            chips,
            repaired: false,
        }
    }

    /// Recover source syntax using out-of-band chip ranges, rejecting a range
    /// if the user has changed its visible label. Pasted plain labels remain
    /// plain text; pasted source markers receive fresh identities.
    #[cfg(test)]
    pub fn from_visible_control(control: &str, tracked: &[TrackedChip]) -> Self {
        Self::from_visible_control_with_edit(control, tracked, None)
    }

    pub fn from_visible_control_with_edit(
        control: &str,
        tracked: &[TrackedChip],
        edit_at: Option<usize>,
    ) -> Self {
        let mut source = String::new();
        let mut chips = Vec::new();
        let mut cursor = 0;
        let mut repaired = false;
        let mut seen = std::collections::HashSet::new();
        let mut sorted = tracked.to_vec();
        sorted.sort_by_key(|chip| chip.control_start);
        let control_units = units(control);
        for tracked in sorted {
            if tracked.control_end <= tracked.control_start || tracked.control_end > control_units {
                continue;
            }
            let image = tracked.date.starts_with("image:");
            let label = if image {
                "\u{fffc}".into()
            } else {
                format!(" {} ", crate::date_label(&tracked.date))
            };
            let label_units = units(&label);
            let exact = (tracked.control_end - tracked.control_start == label_units)
                .then_some((tracked.control_start, tracked.control_end));
            let prefix = (tracked.control_start + label_units <= tracked.control_end)
                .then_some((tracked.control_start, tracked.control_start + label_units));
            let suffix = (tracked.control_end >= label_units)
                .then_some((tracked.control_end - label_units, tracked.control_end));
            let matches_label = |bounds: (usize, usize)| {
                let start = byte_at_unit(control, bounds.0)?;
                let end = byte_at_unit(control, bounds.1)?;
                (&control[start..end] == label || (image && &control[start..end] == "□"))
                    .then_some((bounds.0, bounds.1, start, end))
            };
            let candidate = exact.and_then(matches_label).or_else(|| {
                let prefix = prefix.and_then(matches_label);
                let suffix = suffix.and_then(matches_label);
                match (prefix, suffix) {
                    (Some(first), Some(last)) if first != last => {
                        if edit_at.is_some_and(|at| at <= first.0) {
                            Some(last)
                        } else if edit_at.is_some_and(|at| at >= first.1) {
                            Some(first)
                        } else {
                            None
                        }
                    }
                    (Some(first), _) => Some(first),
                    (_, Some(last)) => Some(last),
                    _ => None,
                }
            });
            let Some((control_start, control_end, start, end)) = candidate else {
                continue;
            };
            if start < cursor
                || end <= start
                || (!image && !crate::valid_date(&tracked.date))
                || Uuid::parse_str(&tracked.id).is_err()
            {
                continue;
            }
            source.push_str(&control[cursor..start]);
            let source_start = source.len();
            source.push_str(&if image {
                crate::inline_images::marker(&tracked.id)
            } else {
                format!("[[date:{}]]", tracked.date)
            });
            let id = if image || seen.insert(tracked.id.clone()) {
                tracked.id
            } else {
                repaired = true;
                Uuid::new_v4().to_string()
            };
            chips.push(ChipSpan {
                source_start,
                source_end: source.len(),
                control_start,
                hidden_end: control_start,
                control_end,
                id,
                image,
            });
            cursor = end;
        }
        source.push_str(&control[cursor..]);
        let mut projection = Self {
            source,
            control: control.to_owned(),
            chips,
            repaired,
        };
        let new_chips: Vec<_> = crate::date_chip_occurrences(&projection.source)
            .into_iter()
            .filter(|item| {
                !projection
                    .chips
                    .iter()
                    .any(|chip| chip.source_start == item.start)
            })
            .map(|item| {
                let start = projection.to_control(item.start);
                let end = projection.to_control(item.end);
                ChipSpan {
                    source_start: item.start,
                    source_end: item.end,
                    control_start: start,
                    hidden_end: start,
                    control_end: end,
                    id: Uuid::new_v4().to_string(),
                    image: false,
                }
            })
            .collect();
        if !new_chips.is_empty() {
            projection.repaired = true;
            projection.chips.extend(new_chips);
            projection.chips.sort_by_key(|chip| chip.source_start);
        }
        projection
    }

    #[cfg(test)]
    pub fn from_source(source: &str, ids: &[String]) -> Self {
        let mut control = String::new();
        let mut chips = Vec::new();
        let mut cursor = 0;
        let mut seen = std::collections::HashSet::new();
        for (index, chip) in crate::date_chip_occurrences(source).iter().enumerate() {
            control.push_str(&source[cursor..chip.start]);
            let id = ids
                .get(index)
                .filter(|id| Uuid::parse_str(id).is_ok() && seen.insert((*id).clone()))
                .cloned()
                .unwrap_or_else(|| Uuid::new_v4().to_string());
            let control_start = units(&control);
            control.push_str(&format!("[[chip:{id}|{}]]", chip.date));
            let hidden_end = units(&control);
            control.push_str(&format!(" {} ", chip.label));
            chips.push(ChipSpan {
                source_start: chip.start,
                source_end: chip.end,
                control_start,
                hidden_end,
                control_end: units(&control),
                id,
                image: false,
            });
            cursor = chip.end;
        }
        control.push_str(&source[cursor..]);
        Self {
            source: source.to_owned(),
            control,
            chips,
            repaired: false,
        }
    }

    #[cfg(test)]
    pub fn from_control(control: &str) -> Self {
        let mut source = String::new();
        let mut chips = Vec::new();
        let mut cursor = 0;
        let mut repaired = false;
        let mut seen = std::collections::HashSet::new();
        while let Some(relative) = control[cursor..].find("[[chip:") {
            let start = cursor + relative;
            let Some(close) = control[start..].find("]]") else {
                break;
            };
            let hidden_end = start + close + 2;
            let metadata = &control[start + 7..hidden_end - 2];
            let Some((id, date)) = metadata.split_once('|') else {
                source.push_str(&control[cursor..hidden_end]);
                cursor = hidden_end;
                continue;
            };
            let label = format!(" {} ", crate::date_label(date));
            if Uuid::parse_str(id).is_err()
                || !crate::valid_date(date)
                || !control[hidden_end..].starts_with(&label)
            {
                source.push_str(&control[cursor..hidden_end]);
                cursor = hidden_end;
                continue;
            }
            source.push_str(&control[cursor..start]);
            let source_start = source.len();
            source.push_str(&format!("[[date:{date}]]"));
            let end = hidden_end + label.len();
            let id = if seen.insert(id.to_owned()) {
                id.to_owned()
            } else {
                repaired = true;
                Uuid::new_v4().to_string()
            };
            chips.push(ChipSpan {
                source_start,
                source_end: source.len(),
                control_start: units(&control[..start]),
                hidden_end: units(&control[..hidden_end]),
                control_end: units(&control[..end]),
                id,
                image: false,
            });
            cursor = end;
        }
        source.push_str(&control[cursor..]);
        let mut projection = Self {
            source,
            control: control.to_owned(),
            chips,
            repaired,
        };
        // Pasted/typed source syntax becomes a new chip, without borrowing an
        // existing chip's identity merely because it precedes that chip.
        let new_chips: Vec<_> = crate::date_chip_occurrences(&projection.source)
            .into_iter()
            .filter(|item| {
                !projection
                    .chips
                    .iter()
                    .any(|chip| chip.source_start == item.start)
            })
            .map(|item| {
                let start = projection.to_control(item.start);
                let end = projection.to_control(item.end);
                ChipSpan {
                    source_start: item.start,
                    source_end: item.end,
                    control_start: start,
                    hidden_end: start,
                    control_end: end,
                    id: Uuid::new_v4().to_string(),
                    image: false,
                }
            })
            .collect();
        if !new_chips.is_empty() {
            projection.repaired = true;
            projection.chips.extend(new_chips);
            projection.chips.sort_by_key(|chip| chip.source_start);
        }
        projection
    }

    pub fn ids(&self) -> Vec<String> {
        self.chips
            .iter()
            .filter(|chip| !chip.image)
            .map(|chip| chip.id.clone())
            .collect()
    }

    /// Map plain glyphs once, rather than rescanning preceding chips and text
    /// for both ends of every glyph in a formatting snapshot.
    pub fn plain_character_spans(&self) -> Vec<(usize, usize, usize, usize)> {
        let mut spans = Vec::new();
        let mut source_cursor = 0;
        let mut control_cursor = 0;
        for chip in &self.chips {
            for (offset, character) in self.source[source_cursor..chip.source_start].char_indices()
            {
                let start = source_cursor + offset;
                let end = start + character.len_utf8();
                let control_end = control_cursor + character.len_utf16();
                spans.push((start, end, control_cursor, control_end));
                control_cursor = control_end;
            }
            source_cursor = chip.source_end;
            control_cursor = chip.control_end;
        }
        for (offset, character) in self.source[source_cursor..].char_indices() {
            let start = source_cursor + offset;
            let end = start + character.len_utf8();
            let control_end = control_cursor + character.len_utf16();
            spans.push((start, end, control_cursor, control_end));
            control_cursor = control_end;
        }
        spans
    }

    pub fn to_control(&self, offset: usize) -> usize {
        let mut source_cursor = 0;
        let mut control_cursor = 0;
        for chip in &self.chips {
            if offset < chip.source_start {
                break;
            }
            if offset < chip.source_end {
                return chip.control_start;
            }
            source_cursor = chip.source_end;
            control_cursor = chip.control_end;
        }
        control_cursor + units(&self.source[source_cursor..offset.min(self.source.len())])
    }

    pub fn to_source(&self, position: usize) -> usize {
        let mut source_cursor = 0;
        let mut control_cursor = 0;
        for chip in &self.chips {
            if position < chip.control_start {
                break;
            }
            if position < chip.control_end {
                return chip.source_start;
            }
            source_cursor = chip.source_end;
            control_cursor = chip.control_end;
        }
        let mut consumed = control_cursor;
        for (byte, character) in self.source[source_cursor..].char_indices() {
            if position < consumed + character.len_utf16() {
                return source_cursor + byte;
            }
            consumed += character.len_utf16();
        }
        self.source.len()
    }
}

fn byte_at_unit(value: &str, target: usize) -> Option<usize> {
    let mut units = 0;
    for (byte, character) in value.char_indices() {
        if units == target {
            return Some(byte);
        }
        units += character.len_utf16();
    }
    (units == target).then_some(value.len())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn linear_glyph_spans_match_point_mapping_with_unicode_and_adjacent_chips() {
        for source in [
            "",
            "😀 café\n日本語",
            "[[date:2026-09-27]]",
            "😀[[date:2026-09-27]][[date:2026-09-28]] café\n日本語",
        ] {
            let projection = Projection::from_source_visible(source, &[]);
            let expected: Vec<_> = source
                .char_indices()
                .filter_map(|(start, character)| {
                    if projection
                        .chips
                        .iter()
                        .any(|chip| start >= chip.source_start && start < chip.source_end)
                    {
                        return None;
                    }
                    let end = start + character.len_utf8();
                    Some((
                        start,
                        end,
                        projection.to_control(start),
                        projection.to_control(end),
                    ))
                })
                .collect();
            assert_eq!(projection.plain_character_spans(), expected);
        }
    }
    #[test]
    fn source_marker_inserted_before_existing_chip_gets_its_own_identity() {
        let existing = Projection::from_source("[[date:2026-09-27T17:30]]", &[]);
        let p =
            Projection::from_control(&format!("[[date:2026-09-27T17:30]] {}", existing.control));
        assert!(p.repaired);
        assert_eq!(p.ids()[1], existing.ids()[0]);
        assert_ne!(p.ids()[0], p.ids()[1]);
        let rendered = Projection::from_source(&p.source, &p.ids());
        assert_eq!(Projection::from_control(&rendered.control).ids(), p.ids());
    }
    #[test]
    fn duplicate_dates_keep_distinct_ids_through_prose_changes() {
        let source = "😀 [[date:2026-09-27]] then [[date:2026-09-27]]";
        let ids = vec![Uuid::new_v4().to_string(), Uuid::new_v4().to_string()];
        let p = Projection::from_source(source, &ids);
        let restored = Projection::from_control(&format!("prefix {}", p.control));
        assert_eq!(restored.source, format!("prefix {source}"));
        assert_eq!(restored.ids(), ids);
        for byte in [
            0,
            4,
            p.chips[0].source_start,
            p.chips[0].source_end,
            source.len(),
        ] {
            assert_eq!(p.to_source(p.to_control(byte)), byte);
        }
    }
    #[test]
    fn pasted_duplicate_identity_is_repaired_and_invalid_metadata_is_plain() {
        let p = Projection::from_source("[[date:2026-09-27]]", &[]);
        let pasted = Projection::from_control(&format!("{}{}", p.control, p.control));
        assert!(pasted.repaired);
        assert_ne!(pasted.ids()[0], pasted.ids()[1]);
        let invalid = "[[chip:invalid|2026-09-27]] Sep 27 ";
        assert_eq!(Projection::from_control(invalid).source, invalid);
    }

    #[test]
    fn accessible_projection_must_not_serialize_chip_ids() {
        let id = "94bd6cfb-919e-488a-9a7e-43fd2980a426";
        let projection = Projection::from_source_visible("[[date:2026-09-27]]", &[id.to_owned()]);

        assert!(!projection.control.contains(id));
        assert!(!projection.control.contains("[[chip:"));
        assert!(projection.control.contains("Sep 27"));
    }

    #[test]
    fn invalid_live_ranges_remain_plain_text_without_panicking() {
        let control = "😀 Sep 27 ";
        for (start, end) in [(8, 2), (2, 2), (1, 9), (2, usize::MAX)] {
            let restored = Projection::from_visible_control(
                control,
                &[TrackedChip {
                    control_start: start,
                    control_end: end,
                    id: Uuid::new_v4().to_string(),
                    date: "2026-09-27".into(),
                }],
            );
            assert_eq!(restored.source, control);
            assert!(restored.chips.is_empty());
        }
    }

    #[test]
    fn visible_projection_keeps_identity_out_of_text_and_recovers_duplicate_dates() {
        let ids = vec![Uuid::new_v4().to_string(), Uuid::new_v4().to_string()];
        let source = "😀 [[date:2026-09-27]] then [[date:2026-09-27]]";
        let shown = Projection::from_source_visible(source, &ids);
        assert!(!shown.control.contains("[[chip:"));
        assert!(!shown.control.contains(&ids[0]));
        assert_eq!(shown.chips.len(), 2);
        let tracked: Vec<_> = shown
            .chips
            .iter()
            .map(|chip| TrackedChip {
                control_start: chip.control_start + 2,
                control_end: chip.control_end + 2,
                id: chip.id.clone(),
                date: "2026-09-27".into(),
            })
            .collect();
        let restored = Projection::from_visible_control(&format!("X {}", shown.control), &tracked);
        assert_eq!(restored.source, format!("X {source}"));
        assert_eq!(restored.ids(), ids);
    }

    #[test]
    fn visible_projection_drops_deleted_chip_and_repairs_pasted_source_marker() {
        let shown = Projection::from_source_visible("A [[date:2026-09-27]] B", &[]);
        let original_id = shown.ids()[0].clone();
        let tracked = TrackedChip {
            control_start: shown.chips[0].control_start,
            control_end: shown.chips[0].control_end,
            id: original_id.clone(),
            date: "2026-09-27".into(),
        };
        let replaced = Projection::from_visible_control("A  B", &[]);
        assert!(replaced.chips.is_empty());
        let pasted = Projection::from_visible_control(
            &format!("[[date:2026-09-27]] {}", shown.control),
            &[TrackedChip {
                control_start: tracked.control_start + 20,
                control_end: tracked.control_end + 20,
                ..tracked
            }],
        );
        assert_eq!(pasted.chips.len(), 2);
        assert_ne!(pasted.ids()[0], original_id);
        assert_eq!(pasted.ids()[1], original_id);
    }
}
