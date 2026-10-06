//! Image references in editor source; binary data remains note-owned on disk.
#[derive(Clone, Debug)]
pub struct ImageRef {
    pub start: usize,
    pub end: usize,
    pub id: String,
}
pub fn occurrences(source: &str) -> Vec<ImageRef> {
    let mut result = Vec::new();
    let mut cursor = 0;
    while let Some(relative) = source[cursor..].find("[[image:") {
        let start = cursor + relative;
        let Some(close) = source[start..].find("]]") else {
            break;
        };
        let end = start + close + 2;
        let id = &source[start + 8..end - 2];
        if uuid::Uuid::parse_str(id).is_ok_and(|uuid| uuid.to_string() == id) {
            result.push(ImageRef {
                start,
                end,
                id: id.into(),
            });
        }
        cursor = end;
    }
    result
}
pub fn marker(id: &str) -> String {
    format!("[[image:{id}]]")
}
/// Text styles must not include the private source bytes of an image atom.
pub fn exclude_from_styles(source: &str, ranges: &mut Vec<(usize, usize)>) {
    for image in occurrences(source) {
        *ranges = ranges
            .iter()
            .flat_map(|&(start, end)| {
                let mut pieces = Vec::new();
                if end <= image.start || start >= image.end {
                    pieces.push((start, end));
                } else {
                    if start < image.start {
                        pieces.push((start, image.start));
                    }
                    if end > image.end {
                        pieces.push((image.end, end));
                    }
                }
                pieces
            })
            .collect();
    }
}
pub fn content_ids(content: &str) -> Vec<String> {
    fn visit(value: &serde_json::Value, ids: &mut Vec<String>) {
        if value.get("type").and_then(|v| v.as_str()) == Some("image") {
            if let Some(id) = value
                .pointer("/attrs/attachmentId")
                .and_then(|v| v.as_str())
            {
                ids.push(id.into());
            }
        }
        for key in ["blocks", "items", "runs"] {
            if let Some(children) = value.get(key).and_then(|v| v.as_array()) {
                for child in children {
                    visit(child, ids);
                }
            }
        }
    }
    let mut ids = Vec::new();
    if let Ok(value) = serde_json::from_str(content) {
        visit(&value, &mut ids);
    }
    ids
}
