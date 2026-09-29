//! Stable identities for structured note content.
//!
//! The editor serializes from plain text and therefore cannot carry an item or
//! date-chip id through an edit by itself.  `reconcile_identities` is the
//! boundary guard: pass it the last persisted JSON and the newly serialized
//! JSON immediately before saving.  It restores only unambiguous existing
//! identities and gives everything else a UUID.
//!
//! Matching deliberately favours safety over guessing.  A date chip is
//! matched by its date, label, and the non-chip text surrounding it in the
//! same item/block.  A checklist item is matched by its text (not `checked`).
//! Duplicate equal candidates are ambiguous and receive new UUIDs rather than
//! allowing a reminder or completed-task identity to jump to a neighbour.

use serde_json::Value;
use std::{
    collections::{BTreeMap, BTreeSet},
    error::Error,
    fmt,
};
use uuid::Uuid;

#[derive(Debug)]
pub enum ContentIdentityError {
    InvalidJson(serde_json::Error),
    InvalidDocument(&'static str),
}

impl fmt::Display for ContentIdentityError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidJson(error) => write!(formatter, "invalid structured note JSON: {error}"),
            Self::InvalidDocument(message) => {
                write!(formatter, "invalid structured note document: {message}")
            }
        }
    }
}

impl Error for ContentIdentityError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::InvalidJson(error) => Some(error),
            Self::InvalidDocument(_) => None,
        }
    }
}

impl From<serde_json::Error> for ContentIdentityError {
    fn from(error: serde_json::Error) -> Self {
        Self::InvalidJson(error)
    }
}

#[derive(Clone, Copy, Debug)]
struct ItemLocation {
    block: usize,
    item: usize,
}

#[derive(Clone, Copy, Debug)]
struct ChipLocation {
    block: usize,
    item: Option<usize>,
    run: usize,
}

#[derive(Clone, Debug)]
struct ItemIdentity {
    location: ItemLocation,
    id: Option<String>,
    key: String,
}

#[derive(Clone, Debug)]
struct ChipIdentity {
    location: ChipLocation,
    id: Option<String>,
    key: String,
    relaxed_key: String,
}

/// Reconcile generated editor identities with the prior persisted document.
///
/// Existing explicit ids in `next_json` always win.  The current Slint
/// serializer's location-derived `slint-*` ids are recognised as generated,
/// never treated as authoritative, and replaced by a matching stable id or a
/// fresh UUID.  The returned JSON has UUID ids for all newly-created or
/// ambiguous checklist items and date chips.
pub fn reconcile_identities(
    previous_json: &str,
    next_json: &str,
) -> Result<String, ContentIdentityError> {
    let previous: Value = serde_json::from_str(previous_json)?;
    let mut next: Value = serde_json::from_str(next_json)?;
    ensure_document(&previous)?;
    ensure_document(&next)?;

    let previous_items = collect_items(&previous);
    let mut next_items = collect_items(&next);
    let previous_chips = collect_chips(&previous);
    let mut next_chips = collect_chips(&next);

    let mut used_ids = BTreeSet::new();
    collect_explicit_ids(&next_items, &mut used_ids);
    collect_explicit_chip_ids(&next_chips, &mut used_ids);

    let semantically_unchanged = same_document_except_identities(&previous, &next);
    reconcile_items(
        &previous_items,
        &mut next_items,
        &mut next,
        &mut used_ids,
        semantically_unchanged,
    );
    reconcile_chips(
        &previous_chips,
        &mut next_chips,
        &mut next,
        &mut used_ids,
        semantically_unchanged,
    );

    Ok(serde_json::to_string(&next)?)
}

fn ensure_document(value: &Value) -> Result<(), ContentIdentityError> {
    value
        .get("blocks")
        .and_then(Value::as_array)
        .ok_or(ContentIdentityError::InvalidDocument(
            "missing blocks array",
        ))
        .map(|_| ())
}

fn collect_explicit_ids(items: &[ItemIdentity], used: &mut BTreeSet<String>) {
    for item in items {
        if let Some(id) = item.id.as_deref().filter(|id| !is_generated_id(id)) {
            used.insert(id.to_owned());
        }
    }
}

fn collect_explicit_chip_ids(chips: &[ChipIdentity], used: &mut BTreeSet<String>) {
    for chip in chips {
        if let Some(id) = chip.id.as_deref().filter(|id| !is_generated_id(id)) {
            used.insert(id.to_owned());
        }
    }
}

fn collect_items(document: &Value) -> Vec<ItemIdentity> {
    let Some(blocks) = document.get("blocks").and_then(Value::as_array) else {
        return Vec::new();
    };
    let mut identities = Vec::new();
    for (block_index, block) in blocks.iter().enumerate() {
        if block.get("type").and_then(Value::as_str) != Some("checklist") {
            continue;
        }
        let Some(items) = block.get("items").and_then(Value::as_array) else {
            continue;
        };
        for (item_index, item) in items.iter().enumerate() {
            // `checked` is purposefully excluded: checking a task must not
            // make it become a different task.
            let text = item_text(item);
            identities.push(ItemIdentity {
                location: ItemLocation {
                    block: block_index,
                    item: item_index,
                },
                id: item.get("id").and_then(Value::as_str).map(str::to_owned),
                key: format!("checklist\u{1f}{text}"),
            });
        }
    }
    identities
}

fn item_text(item: &Value) -> String {
    item.get("text")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .unwrap_or_else(|| scope_text(item))
}

fn collect_chips(document: &Value) -> Vec<ChipIdentity> {
    let Some(blocks) = document.get("blocks").and_then(Value::as_array) else {
        return Vec::new();
    };
    let mut identities = Vec::new();
    for (block_index, block) in blocks.iter().enumerate() {
        if let Some(runs) = block.get("runs").and_then(Value::as_array) {
            collect_chips_from_runs(runs, block_index, None, &mut identities);
        }
        if let Some(items) = block.get("items").and_then(Value::as_array) {
            for (item_index, item) in items.iter().enumerate() {
                if let Some(runs) = item.get("runs").and_then(Value::as_array) {
                    collect_chips_from_runs(runs, block_index, Some(item_index), &mut identities);
                }
            }
        }
    }
    identities
}

fn collect_chips_from_runs(
    runs: &[Value],
    block: usize,
    item: Option<usize>,
    output: &mut Vec<ChipIdentity>,
) {
    let non_chip_text: Vec<&str> = runs
        .iter()
        .map(|run| {
            if is_date_chip(run) {
                ""
            } else {
                run.get("t").and_then(Value::as_str).unwrap_or("")
            }
        })
        .collect();
    let scope = normalize_text(&non_chip_text.concat());
    for (run_index, run) in runs.iter().enumerate() {
        if !is_date_chip(run) {
            continue;
        }
        let attrs = run.get("attrs").unwrap_or(&Value::Null);
        let date = attrs.get("date").and_then(Value::as_str).unwrap_or("");
        let label = attrs.get("label").and_then(Value::as_str).unwrap_or("");
        let before = normalize_text(&non_chip_text[..run_index].concat());
        let after = normalize_text(&non_chip_text[run_index + 1..].concat());
        output.push(ChipIdentity {
            location: ChipLocation {
                block,
                item,
                run: run_index,
            },
            id: attrs
                .get("chipId")
                .and_then(Value::as_str)
                .map(str::to_owned),
            key: format!(
                "dateChip\u{1f}{date}\u{1f}{label}\u{1f}{scope}\u{1f}{before}\u{1f}{after}"
            ),
            // A single date+label in a note remains safe to carry across a
            // nearby prose edit, even though the contextual match changed.
            relaxed_key: format!("dateChip\u{1f}{date}\u{1f}{label}"),
        });
    }
}

fn scope_text(value: &Value) -> String {
    value
        .get("runs")
        .and_then(Value::as_array)
        .map(|runs| {
            runs.iter()
                .map(|run| {
                    if is_date_chip(run) {
                        run.get("attrs")
                            .and_then(|attrs| attrs.get("label"))
                            .and_then(Value::as_str)
                            .unwrap_or("")
                    } else {
                        run.get("t").and_then(Value::as_str).unwrap_or("")
                    }
                })
                .collect()
        })
        .unwrap_or_default()
}

fn normalize_text(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn is_date_chip(value: &Value) -> bool {
    value.get("type").and_then(Value::as_str) == Some("dateChip")
}

fn reconcile_items(
    previous: &[ItemIdentity],
    next: &mut [ItemIdentity],
    document: &mut Value,
    used: &mut BTreeSet<String>,
    semantically_unchanged: bool,
) {
    let assignments = unambiguous_assignments(
        previous
            .iter()
            .filter(|item| explicit_id(&item.id).is_some()),
        next.iter()
            .enumerate()
            .filter(|(_, item)| generated_or_missing(&item.id)),
        |item| item.key.as_str(),
        |item| explicit_id(&item.id).unwrap(),
        false,
    );
    for (next_index, previous_id) in assignments {
        assign_item_id(document, next[next_index].location, &previous_id);
        next[next_index].id = Some(previous_id.clone());
        used.insert(previous_id);
    }
    // If an autosave only regenerated location ids, equal duplicate task text
    // can retain ids by document order.  Do not take this path after a real
    // edit: a deletion/insertion of equal tasks is inherently ambiguous.
    if semantically_unchanged {
        let assignments = unambiguous_assignments(
            previous
                .iter()
                .filter(|item| explicit_id(&item.id).is_some_and(|id| !used.contains(id))),
            next.iter()
                .enumerate()
                .filter(|(_, item)| generated_or_missing(&item.id)),
            |item| item.key.as_str(),
            |item| explicit_id(&item.id).unwrap(),
            true,
        );
        for (next_index, previous_id) in assignments {
            assign_item_id(document, next[next_index].location, &previous_id);
            next[next_index].id = Some(previous_id.clone());
            used.insert(previous_id);
        }
    }
    for item in next
        .iter_mut()
        .filter(|item| generated_or_missing(&item.id))
    {
        let id = fresh_uuid(used);
        assign_item_id(document, item.location, &id);
        item.id = Some(id);
    }
}

fn reconcile_chips(
    previous: &[ChipIdentity],
    next: &mut [ChipIdentity],
    document: &mut Value,
    used: &mut BTreeSet<String>,
    semantically_unchanged: bool,
) {
    let assignments = unambiguous_assignments(
        previous
            .iter()
            .filter(|chip| explicit_id(&chip.id).is_some()),
        next.iter()
            .enumerate()
            .filter(|(_, chip)| generated_or_missing(&chip.id)),
        |chip| chip.key.as_str(),
        |chip| explicit_id(&chip.id).unwrap(),
        false,
    );
    for (next_index, previous_id) in assignments {
        assign_chip_id(document, next[next_index].location, &previous_id);
        next[next_index].id = Some(previous_id.clone());
        used.insert(previous_id);
    }
    if semantically_unchanged {
        let assignments = unambiguous_assignments(
            previous
                .iter()
                .filter(|chip| explicit_id(&chip.id).is_some_and(|id| !used.contains(id))),
            next.iter()
                .enumerate()
                .filter(|(_, chip)| generated_or_missing(&chip.id)),
            |chip| chip.key.as_str(),
            |chip| explicit_id(&chip.id).unwrap(),
            true,
        );
        for (next_index, previous_id) in assignments {
            assign_chip_id(document, next[next_index].location, &previous_id);
            next[next_index].id = Some(previous_id.clone());
            used.insert(previous_id);
        }
    }
    // When a chip is the only remaining candidate for this date+label, keep
    // it through edits to adjacent prose.  Multiple equal dates deliberately
    // do not use this relaxed fallback.
    let assignments = unambiguous_assignments(
        previous
            .iter()
            .filter(|chip| explicit_id(&chip.id).is_some_and(|id| !used.contains(id))),
        next.iter()
            .enumerate()
            .filter(|(_, chip)| generated_or_missing(&chip.id)),
        |chip| chip.relaxed_key.as_str(),
        |chip| explicit_id(&chip.id).unwrap(),
        false,
    );
    for (next_index, previous_id) in assignments {
        assign_chip_id(document, next[next_index].location, &previous_id);
        next[next_index].id = Some(previous_id.clone());
        used.insert(previous_id);
    }
    for chip in next
        .iter_mut()
        .filter(|chip| generated_or_missing(&chip.id))
    {
        let id = fresh_uuid(used);
        assign_chip_id(document, chip.location, &id);
        chip.id = Some(id);
    }
}

/// Return matches only when a key occurs exactly once on both sides.  This is
/// what makes repeated identical checklist text/chips conservative.
fn unambiguous_assignments<'a, T, P, N, K, I>(
    previous: P,
    next: N,
    key: K,
    id: I,
    allow_ordered_duplicates: bool,
) -> Vec<(usize, String)>
where
    P: Iterator<Item = &'a T>,
    N: Iterator<Item = (usize, &'a T)>,
    K: Fn(&T) -> &str,
    I: Fn(&T) -> &str,
    T: 'a,
{
    let previous: Vec<&T> = previous.collect();
    let next: Vec<(usize, &T)> = next.collect();
    let mut old_by_key: BTreeMap<&str, Vec<&T>> = BTreeMap::new();
    let mut new_by_key: BTreeMap<&str, Vec<(usize, &T)>> = BTreeMap::new();
    for value in previous {
        old_by_key.entry(key(value)).or_default().push(value);
    }
    for (index, value) in next {
        new_by_key
            .entry(key(value))
            .or_default()
            .push((index, value));
    }
    old_by_key
        .into_iter()
        .filter_map(|(match_key, old)| {
            let new = new_by_key.get(match_key)?;
            if old.len() == 1 && new.len() == 1 {
                Some(vec![(new[0].0, id(old[0]).to_owned())])
            } else if allow_ordered_duplicates && old.len() == new.len() {
                Some(
                    old.iter()
                        .zip(new.iter())
                        .map(|(old, new)| (new.0, id(old).to_owned()))
                        .collect(),
                )
            } else {
                None
            }
        })
        .flatten()
        .collect()
}

fn same_document_except_identities(previous: &Value, next: &Value) -> bool {
    let mut previous = previous.clone();
    let mut next = next.clone();
    strip_identities(&mut previous);
    strip_identities(&mut next);
    previous == next
}

fn strip_identities(value: &mut Value) {
    match value {
        Value::Array(values) => values.iter_mut().for_each(strip_identities),
        Value::Object(values) => {
            values.remove("id");
            values.remove("chipId");
            values.values_mut().for_each(strip_identities);
        }
        _ => {}
    }
}

fn explicit_id(id: &Option<String>) -> Option<&str> {
    id.as_deref().filter(|value| !is_generated_id(value))
}

fn generated_or_missing(id: &Option<String>) -> bool {
    id.as_deref().is_none_or(is_generated_id)
}

fn fresh_uuid(used: &mut BTreeSet<String>) -> String {
    loop {
        let id = Uuid::new_v4().to_string();
        if used.insert(id.clone()) {
            return id;
        }
    }
}

fn assign_item_id(document: &mut Value, location: ItemLocation, id: &str) {
    if let Some(item) = document
        .get_mut("blocks")
        .and_then(Value::as_array_mut)
        .and_then(|blocks| blocks.get_mut(location.block))
        .and_then(|block| block.get_mut("items"))
        .and_then(Value::as_array_mut)
        .and_then(|items| items.get_mut(location.item))
        .and_then(Value::as_object_mut)
    {
        item.insert("id".to_owned(), Value::String(id.to_owned()));
    }
}

fn assign_chip_id(document: &mut Value, location: ChipLocation, id: &str) {
    let Some(block) = document
        .get_mut("blocks")
        .and_then(Value::as_array_mut)
        .and_then(|blocks| blocks.get_mut(location.block))
    else {
        return;
    };
    let run = if let Some(item_index) = location.item {
        block
            .get_mut("items")
            .and_then(Value::as_array_mut)
            .and_then(|items| items.get_mut(item_index))
            .and_then(|item| item.get_mut("runs"))
            .and_then(Value::as_array_mut)
            .and_then(|runs| runs.get_mut(location.run))
    } else {
        block
            .get_mut("runs")
            .and_then(Value::as_array_mut)
            .and_then(|runs| runs.get_mut(location.run))
    };
    if let Some(run) = run.and_then(Value::as_object_mut) {
        let attrs = run
            .entry("attrs".to_owned())
            .or_insert_with(|| Value::Object(Default::default()));
        if let Some(attrs) = attrs.as_object_mut() {
            attrs.insert("chipId".to_owned(), Value::String(id.to_owned()));
        }
    }
}

/// Matches only the location-shaped ids produced by the current/older Slint
/// serializers.  Other `slint-*` strings remain legitimate explicit ids.
fn is_generated_id(id: &str) -> bool {
    let Some(rest) = id.strip_prefix("slint-") else {
        return false;
    };
    let parts: Vec<&str> = rest.split('-').collect();
    match parts.as_slice() {
        ["checklist", line] | ["list", line] => line.parse::<usize>().is_ok(),
        ["date", "line", line, run] | ["date", "list", line, run] | ["date", "item", line, run] => {
            line.parse::<usize>().is_ok() && run.parse::<usize>().is_ok()
        }
        // The early single-block serializer emitted slint-date-<run>-<date>.
        ["date", run, date] => run.parse::<usize>().is_ok() && !date.is_empty(),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn json(value: Value) -> String {
        value.to_string()
    }
    fn chip(id: &str, date: &str) -> Value {
        serde_json::json!({"type":"dateChip","attrs":{"chipId":id,"date":date,"label":date}})
    }
    fn text(text: &str) -> Value {
        serde_json::json!({"t":text})
    }
    fn task(id: &str, text_value: &str, checked: bool) -> Value {
        serde_json::json!({"id":id,"text":text_value,"checked":checked,"runs":[text(text_value)]})
    }
    fn doc(blocks: Vec<Value>) -> String {
        json(serde_json::json!({"blocks":blocks,"plain":""}))
    }

    #[test]
    fn preserves_chip_ids_when_lines_are_inserted_reordered_and_a_neighbour_is_removed() {
        let previous = doc(vec![
            serde_json::json!({"type":"text","runs":[text("Alpha "),chip("chip-alpha", "2026-10-01"),text(" done")]}),
            serde_json::json!({"type":"text","runs":[text("Bravo "),chip("chip-bravo", "2026-10-01"),text(" then "),chip("chip-remove", "2026-10-02")]}),
        ]);
        let next = doc(vec![
            serde_json::json!({"type":"text","runs":[text("New line")]}),
            serde_json::json!({"type":"text","runs":[text("Bravo "),chip("slint-date-line-1-0", "2026-10-01"),text(" then ")]}),
            serde_json::json!({"type":"text","runs":[text("Alpha "),chip("slint-date-line-2-0", "2026-10-01"),text(" done")]}),
        ]);
        let result: Value =
            serde_json::from_str(&reconcile_identities(&previous, &next).unwrap()).unwrap();
        assert_eq!(
            result["blocks"][1]["runs"][1]["attrs"]["chipId"],
            "chip-bravo"
        );
        assert_eq!(
            result["blocks"][2]["runs"][1]["attrs"]["chipId"],
            "chip-alpha"
        );
    }

    #[test]
    fn preserves_checklist_item_ids_through_reorder_insertion_and_dismissal() {
        let previous = doc(vec![
            serde_json::json!({"type":"checklist","items":[task("task-milk", "Buy milk", false)]}),
            serde_json::json!({"type":"checklist","items":[task("task-tax", "File taxes", false)]}),
        ]);
        let next = doc(vec![
            serde_json::json!({"type":"checklist","items":[task("slint-checklist-0", "New task", false)]}),
            // Marking it dismissed/completed is not part of its identity.
            serde_json::json!({"type":"checklist","items":[task("slint-checklist-1", "File taxes", true)]}),
            serde_json::json!({"type":"checklist","items":[task("slint-checklist-2", "Buy milk", false)]}),
        ]);
        let result: Value =
            serde_json::from_str(&reconcile_identities(&previous, &next).unwrap()).unwrap();
        assert_eq!(result["blocks"][1]["items"][0]["id"], "task-tax");
        assert_eq!(result["blocks"][2]["items"][0]["id"], "task-milk");
        let new_id = result["blocks"][0]["items"][0]["id"].as_str().unwrap();
        assert!(Uuid::parse_str(new_id).is_ok());
    }

    #[test]
    fn leaves_explicit_next_ids_alone_and_replaces_location_ids_with_uuids() {
        let previous = doc(vec![]);
        let next = doc(vec![
            serde_json::json!({"type":"checklist","items":[task("user-selected-id", "Keep this", false),task("slint-checklist-1", "New", false)]}),
            serde_json::json!({"type":"text","runs":[chip("slint-date-line-1-0", "2026-10-01")]}),
        ]);
        let result: Value =
            serde_json::from_str(&reconcile_identities(&previous, &next).unwrap()).unwrap();
        assert_eq!(result["blocks"][0]["items"][0]["id"], "user-selected-id");
        assert!(Uuid::parse_str(result["blocks"][0]["items"][1]["id"].as_str().unwrap()).is_ok());
        assert!(Uuid::parse_str(
            result["blocks"][1]["runs"][0]["attrs"]["chipId"]
                .as_str()
                .unwrap()
        )
        .is_ok());
    }

    #[test]
    fn changed_equal_duplicates_are_not_guessed() {
        let previous = doc(vec![serde_json::json!({"type":"checklist","items":[
            task("old-a", "Same", false), task("old-b", "Same", false)
        ]})]);
        // One indistinguishable task was removed.  Retaining either old id
        // would guess which task survived, so the remaining task is new.
        let next = doc(vec![serde_json::json!({"type":"checklist","items":[
            task("slint-checklist-0", "Same", false)
        ]})]);
        let result: Value =
            serde_json::from_str(&reconcile_identities(&previous, &next).unwrap()).unwrap();
        for index in 0..1 {
            let id = result["blocks"][0]["items"][index]["id"].as_str().unwrap();
            assert!(Uuid::parse_str(id).is_ok());
            assert_ne!(id, "old-a");
            assert_ne!(id, "old-b");
        }
    }

    #[test]
    fn unchanged_duplicate_chips_keep_ids_across_autosave_regeneration() {
        let previous = doc(vec![
            serde_json::json!({"type":"text","runs":[chip("first", "2026-10-01")]}),
            serde_json::json!({"type":"text","runs":[chip("second", "2026-10-01")]}),
        ]);
        let next = doc(vec![
            serde_json::json!({"type":"text","runs":[chip("slint-date-line-0-0", "2026-10-01")]}),
            serde_json::json!({"type":"text","runs":[chip("slint-date-line-1-0", "2026-10-01")]}),
        ]);
        let result: Value =
            serde_json::from_str(&reconcile_identities(&previous, &next).unwrap()).unwrap();
        assert_eq!(result["blocks"][0]["runs"][0]["attrs"]["chipId"], "first");
        assert_eq!(result["blocks"][1]["runs"][0]["attrs"]["chipId"], "second");
    }

    #[test]
    fn generated_id_detector_does_not_claim_arbitrary_explicit_slint_ids() {
        assert!(is_generated_id("slint-date-item-9-2"));
        assert!(is_generated_id("slint-checklist-42"));
        assert!(!is_generated_id("slint-date-imported"));
    }
}
