//! Date-chip extraction and reminder persistence.
//!
//! Date chips are part of the note's structured JSON. The reminder table is a
//! small projection of that content: editing a note reconciles the projection
//! while preserving a reminder's status when its stable chip identity remains.

use chrono::{DateTime, Datelike, Local, Months, NaiveDate, NaiveDateTime, NaiveTime, Utc};
use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;
use serde_json::Value;
use std::collections::HashSet;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DateChipOccurrence {
    pub chip_id: String,
    pub due_date: String,
    pub due_time: String,
    pub recurrence: String,
    pub label: String,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ReminderRow {
    pub id: String,
    pub note_id: String,
    pub chip_id: String,
    pub due_date: String,
    pub due_time: String,
    pub scheduled_date: String,
    pub recurrence: String,
    pub snoozed_until: Option<String>,
    pub notified_for: Option<String>,
    pub label: String,
    pub status: String,
    pub created_at: String,
    pub modified_at: String,
}

fn date_label(value: &str) -> String {
    NaiveDate::parse_from_str(value, "%Y-%m-%d")
        .map(|date| date.format("%b %-d").to_string())
        .unwrap_or_else(|_| value.to_owned())
}

fn chip_from_run(run: &Value, fallback_id: String) -> Option<DateChipOccurrence> {
    if run.get("type").and_then(Value::as_str) != Some("dateChip") {
        return None;
    }
    let attrs = run.get("attrs").unwrap_or(run);
    let date = attrs
        .get("date")
        .or_else(|| attrs.get("due"))
        .and_then(Value::as_str)?;
    NaiveDate::parse_from_str(date, "%Y-%m-%d").ok()?;
    let due_time = attrs.get("time").and_then(Value::as_str).unwrap_or("");
    if !due_time.is_empty()
        && (due_time.len() != 5 || NaiveTime::parse_from_str(due_time, "%H:%M").is_err())
    {
        return None;
    }
    let recurrence = attrs
        .get("recurrence")
        .and_then(Value::as_str)
        .filter(|value| matches!(*value, "daily" | "weekly" | "monthly"))
        .unwrap_or("none");
    let chip_id = attrs
        .get("chipId")
        .or_else(|| attrs.get("id"))
        .and_then(Value::as_str)
        .filter(|id| !id.trim().is_empty())
        .map(str::to_owned)
        .unwrap_or(fallback_id);
    let label = attrs
        .get("label")
        .and_then(Value::as_str)
        .filter(|label| !label.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| date_label(date));
    Some(DateChipOccurrence {
        chip_id,
        due_date: date.to_owned(),
        due_time: due_time.to_owned(),
        recurrence: recurrence.to_owned(),
        label,
    })
}

/// Extract active date-chip runs from normal text blocks and checklist items.
/// Completed checklist items retain their chips in note content, but do not
/// project reminders. Unchecking an item projects a fresh pending reminder.
/// Explicit `attrs.chipId`/`attrs.id` values are preferred. Older or hand
/// authored content without an id receives a deterministic location key.
pub fn extract_date_chips(content: &str) -> Vec<DateChipOccurrence> {
    chips_with_lines(content)
        .into_iter()
        .map(|(chip, _)| chip)
        .collect()
}

/// The text on the line of the note where this date chip sits, without its
/// date label, or `None` when the chip is gone or its line has no other text.
pub fn chip_line(content: &str, chip_id: &str) -> Option<String> {
    chips_with_lines(content)
        .into_iter()
        .find(|(chip, _)| chip.chip_id == chip_id)
        .map(|(_, line)| line)
        .filter(|line| !line.is_empty())
}

fn chips_with_lines(content: &str) -> Vec<(DateChipOccurrence, String)> {
    let Ok(value) = serde_json::from_str::<Value>(content) else {
        return Vec::new();
    };
    let Some(blocks) = value.get("blocks").and_then(Value::as_array) else {
        return Vec::new();
    };
    let mut result = Vec::new();
    let mut seen = HashSet::new();
    for (block_index, block) in blocks.iter().enumerate() {
        if let Some(runs) = block.get("runs").and_then(Value::as_array) {
            push_chips(
                runs,
                &format!("block-{block_index}"),
                &mut seen,
                &mut result,
            );
        }
        if let Some(items) = block.get("items").and_then(Value::as_array) {
            for (item_index, item) in items.iter().enumerate() {
                if block.get("type").and_then(Value::as_str) == Some("checklist")
                    && item.get("checked").and_then(Value::as_bool) == Some(true)
                {
                    continue;
                }
                if let Some(runs) = item.get("runs").and_then(Value::as_array) {
                    let prefix = format!("block-{block_index}-item-{item_index}");
                    push_chips(runs, &prefix, &mut seen, &mut result);
                }
            }
        }
    }
    result
}

/// Adds the chips in one run list, each with the text of its own line. New
/// content stores one line per block or item; older content may hold several
/// lines in one run list, separated by newlines inside text runs.
fn push_chips(
    runs: &[Value],
    prefix: &str,
    seen: &mut HashSet<String>,
    result: &mut Vec<(DateChipOccurrence, String)>,
) {
    let mut lines = vec![String::new()];
    let mut chips = Vec::new();
    // Where the current line's text after its latest chip begins, if any.
    let mut after_chip: Option<usize> = None;
    for (run_index, run) in runs.iter().enumerate() {
        let fallback = format!("{prefix}-run-{run_index}");
        if let Some(chip) = unique_chip(chip_from_run(run, fallback), seen) {
            let line = lines.last_mut().unwrap();
            // "Swim [Mon] and [Wed]": a joining word between two dates is
            // not part of what either reminder is about.
            if let Some(start) = after_chip {
                if is_joiner(&line[start..]) {
                    line.truncate(start);
                }
            }
            after_chip = Some(line.len());
            chips.push((chip, lines.len() - 1));
        } else if let Some(text) = run
            .get("t")
            .or_else(|| run.get("text"))
            .and_then(Value::as_str)
        {
            let mut parts = text.split('\n');
            if let Some(first) = parts.next() {
                lines.last_mut().unwrap().push_str(first);
            }
            for part in parts {
                lines.push(part.to_owned());
                after_chip = None;
            }
        }
    }
    for (chip, line) in chips {
        let text = lines[line].split_whitespace().collect::<Vec<_>>().join(" ");
        result.push((chip, text));
    }
}

fn is_joiner(text: &str) -> bool {
    let text = text.trim().to_lowercase();
    matches!(
        text.as_str(),
        "and" | "or" | "&" | "+" | "/" | "then" | "and then"
    )
}

fn unique_chip(
    mut chip: Option<DateChipOccurrence>,
    seen: &mut HashSet<String>,
) -> Option<DateChipOccurrence> {
    let chip = chip.as_mut()?;
    if !seen.insert(chip.chip_id.clone()) {
        let base = chip.chip_id.clone();
        let mut suffix = 2;
        while !seen.insert(format!("{base}#{suffix}")) {
            suffix += 1;
        }
        chip.chip_id = format!("{base}#{suffix}");
    }
    chip.clone().into()
}

fn now() -> String {
    chrono::Utc::now().to_rfc3339()
}

fn reminder_id(note_id: &str, chip_id: &str) -> String {
    format!("reminder:{note_id}:{chip_id}")
}

/// Reconcile a reminder projection using the caller's transaction.
///
/// Keeping this free of transaction management lets note writes include the
/// note row, FTS entry, and projection in one all-or-nothing operation.
pub(crate) fn reconcile_note_in_transaction(
    conn: &Connection,
    note_id: &str,
    content: &str,
) -> rusqlite::Result<()> {
    let occurrences = extract_date_chips(content);
    let timestamp = now();
    for occurrence in &occurrences {
        conn.execute(
            "INSERT INTO reminders
                (id,note_id,chip_id,due_date,due_time,scheduled_date,recurrence,label,status,created_at,modified_at)
             VALUES (?1,?2,?3,?4,?5,?4,?6,?7,'pending',?8,?8)
             ON CONFLICT(note_id,chip_id) DO UPDATE SET
                due_date=excluded.due_date,
                due_time=excluded.due_time,
                recurrence=excluded.recurrence,
                scheduled_date=CASE WHEN reminders.due_date <> excluded.due_date OR reminders.due_time <> excluded.due_time OR reminders.recurrence <> excluded.recurrence
                                    THEN excluded.scheduled_date ELSE reminders.scheduled_date END,
                label=excluded.label,
                modified_at=excluded.modified_at,
                status=CASE WHEN reminders.due_date <> excluded.due_date OR reminders.due_time <> excluded.due_time OR reminders.recurrence <> excluded.recurrence
                            THEN 'pending' ELSE reminders.status END,
                snoozed_until=CASE WHEN reminders.due_date <> excluded.due_date OR reminders.due_time <> excluded.due_time OR reminders.recurrence <> excluded.recurrence
                                   THEN NULL ELSE reminders.snoozed_until END,
                notified_for=CASE WHEN reminders.due_date <> excluded.due_date OR reminders.due_time <> excluded.due_time OR reminders.recurrence <> excluded.recurrence
                                  THEN NULL ELSE reminders.notified_for END",
            params![
                reminder_id(note_id, &occurrence.chip_id),
                note_id,
                occurrence.chip_id,
                occurrence.due_date,
                occurrence.due_time,
                occurrence.recurrence,
                occurrence.label,
                timestamp,
            ],
        )?;
    }
    let keep: Vec<&str> = occurrences
        .iter()
        .map(|item| item.chip_id.as_str())
        .collect();
    if keep.is_empty() {
        conn.execute("DELETE FROM reminders WHERE note_id=?1", [note_id])?;
    } else {
        let placeholders = std::iter::repeat_n("?", keep.len())
            .collect::<Vec<_>>()
            .join(",");
        let sql =
            format!("DELETE FROM reminders WHERE note_id=?1 AND chip_id NOT IN ({placeholders})");
        let mut values: Vec<&dyn rusqlite::ToSql> = Vec::with_capacity(keep.len() + 1);
        values.push(&note_id);
        values.extend(keep.iter().map(|id| id as &dyn rusqlite::ToSql));
        conn.execute(&sql, values.as_slice())?;
    }
    Ok(())
}

#[cfg(test)]
pub fn reconcile_note(conn: &Connection, note_id: &str, content: &str) -> rusqlite::Result<()> {
    let tx = conn.unchecked_transaction()?;
    reconcile_note_in_transaction(&tx, note_id, content)?;
    tx.commit()
}

pub fn remove_for_note(conn: &Connection, note_id: &str) -> rusqlite::Result<()> {
    conn.execute("DELETE FROM reminders WHERE note_id=?1", [note_id])?;
    Ok(())
}

fn row(r: &rusqlite::Row<'_>) -> rusqlite::Result<ReminderRow> {
    Ok(ReminderRow {
        id: r.get(0)?,
        note_id: r.get(1)?,
        chip_id: r.get(2)?,
        due_date: r.get(3)?,
        due_time: r.get(4)?,
        scheduled_date: r.get(5)?,
        recurrence: r.get(6)?,
        snoozed_until: r.get(7)?,
        notified_for: r.get(8)?,
        label: r.get(9)?,
        status: r.get(10)?,
        created_at: r.get(11)?,
        modified_at: r.get(12)?,
    })
}

pub fn list_for_note(conn: &Connection, note_id: &str) -> rusqlite::Result<Vec<ReminderRow>> {
    let mut stmt = conn.prepare(
        "SELECT id,note_id,chip_id,due_date,due_time,scheduled_date,recurrence,snoozed_until,notified_for,label,status,created_at,modified_at
         FROM reminders WHERE note_id=?1 ORDER BY scheduled_date,chip_id",
    )?;
    let result = stmt.query_map([note_id], row)?.collect();
    result
}

/// Return pending reminders whose local due date/time has arrived. Snooze is
/// stored as a UTC instant so daylight-saving changes cannot shorten it.
pub fn due_at(conn: &Connection, as_of: DateTime<Local>) -> rusqlite::Result<Vec<ReminderRow>> {
    let date = as_of.format("%Y-%m-%d").to_string();
    let time = as_of.format("%H:%M").to_string();
    let utc = as_of.with_timezone(&Utc).to_rfc3339();
    let mut stmt = conn.prepare(
        "SELECT id,note_id,chip_id,due_date,due_time,scheduled_date,recurrence,snoozed_until,notified_for,label,status,created_at,modified_at
         FROM reminders WHERE status='pending'
         AND (scheduled_date<?1 OR (scheduled_date=?1 AND due_time<=?2))
         AND (snoozed_until IS NULL OR snoozed_until<=?3)
         ORDER BY scheduled_date,due_time,note_id,chip_id",
    )?;
    let result = stmt.query_map(params![date, time, utc], row)?.collect();
    result
}

/// Date-only compatibility for existing deterministic tests and callers.
#[cfg(test)]
pub fn due_on(conn: &Connection, as_of: NaiveDate) -> rusqlite::Result<Vec<ReminderRow>> {
    use chrono::TimeZone;
    let end = as_of.and_hms_opt(23, 59, 0).unwrap();
    let local = Local
        .from_local_datetime(&end)
        .earliest()
        .ok_or_else(|| rusqlite::Error::InvalidParameterName("invalid local test date".into()))?;
    due_at(conn, local)
}

fn occurrence_key(row: &ReminderRow) -> String {
    format!("{}T{}", row.scheduled_date, row.due_time)
}

pub fn unannounced_due_at(
    conn: &Connection,
    as_of: DateTime<Local>,
) -> rusqlite::Result<Vec<ReminderRow>> {
    roll_elapsed_repeats(conn, as_of)?;
    Ok(due_at(conn, as_of)?
        .into_iter()
        .filter(|row| row.notified_for.as_deref() != Some(occurrence_key(row).as_str()))
        .collect())
}

/// A repeat normally moves on when the user marks it done or dismisses it.
/// One that was ignored (its notification timed out or was closed) would
/// otherwise keep its old date and never notify again, so once a later
/// occurrence has arrived it moves to the latest one and becomes announceable.
/// Snoozed reminders keep their snooze.
fn roll_elapsed_repeats(conn: &Connection, as_of: DateTime<Local>) -> rusqlite::Result<()> {
    let mut stmt = conn.prepare(
        "SELECT id,due_date,scheduled_date,due_time,recurrence FROM reminders
         WHERE status='pending' AND recurrence<>'none' AND scheduled_date<?1
         AND (snoozed_until IS NULL OR snoozed_until<=?2)",
    )?;
    let rows = stmt
        .query_map(
            params![
                as_of.format("%Y-%m-%d").to_string(),
                as_of.with_timezone(&Utc).to_rfc3339()
            ],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                ))
            },
        )?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for (id, anchor, current, due_time, cadence) in rows {
        let (Ok(anchor), Ok(mut date)) = (
            NaiveDate::parse_from_str(&anchor, "%Y-%m-%d"),
            NaiveDate::parse_from_str(&current, "%Y-%m-%d"),
        ) else {
            continue;
        };
        let time = if due_time.is_empty() {
            NaiveTime::MIN
        } else {
            match NaiveTime::parse_from_str(&due_time, "%H:%M") {
                Ok(time) => time,
                Err(_) => continue,
            }
        };
        // Step to the latest occurrence that has already arrived.
        while let Some(next) = next_recurrence(anchor, date, time, &cadence, date.and_time(time))
            .filter(|next| next.and_time(time) <= as_of.naive_local())
        {
            date = next;
        }
        if date.format("%Y-%m-%d").to_string() != current {
            conn.execute(
                "UPDATE reminders SET scheduled_date=?2,snoozed_until=NULL,notified_for=NULL,modified_at=?3
                 WHERE id=?1 AND status='pending' AND scheduled_date=?4",
                params![id, date.format("%Y-%m-%d").to_string(), now(), current],
            )?;
        }
    }
    Ok(())
}

/// Claim only the occurrence actually shown. A snooze or date edit clears the
/// marker, and a new recurrence has a different scheduled key.
pub fn mark_announced(conn: &Connection, row: &ReminderRow) -> rusqlite::Result<bool> {
    let key = occurrence_key(row);
    Ok(conn.execute(
        "UPDATE reminders SET notified_for=?2,modified_at=?3
         WHERE id=?1 AND status='pending' AND scheduled_date=?4 AND due_time=?5
         AND (notified_for IS NULL OR notified_for<>?2)",
        params![row.id, key, now(), row.scheduled_date, row.due_time],
    )? == 1)
}

pub fn snooze(conn: &Connection, id: &str, until: DateTime<Utc>) -> rusqlite::Result<()> {
    let changed = conn.execute(
        "UPDATE reminders SET snoozed_until=?2,notified_for=NULL,modified_at=?3
         WHERE id=?1 AND status='pending'",
        params![id, until.to_rfc3339(), now()],
    )?;
    if changed == 0 {
        Err(rusqlite::Error::QueryReturnedNoRows)
    } else {
        Ok(())
    }
}

/// A notification is valid only for the pending occurrence that was announced.
/// Serialize validation and mutation so duplicate/stale toast clicks cannot
/// advance a recurring reminder twice or act on a deleted note.
pub fn activate(
    conn: &Connection,
    activation: &crate::notification::Activation,
    as_of: DateTime<Local>,
) -> rusqlite::Result<Option<String>> {
    use crate::notification::Action;
    let transaction = conn.unchecked_transaction()?;
    let note_id: Option<String> = transaction
        .query_row(
            "SELECT r.note_id FROM reminders r JOIN notes n ON n.id=r.note_id
         WHERE r.id=?1 AND n.deleted_at IS NULL AND r.status='pending'
         AND r.scheduled_date || 'T' || r.due_time=?2 AND r.notified_for=?2",
            params![activation.reminder_id, activation.occurrence],
            |row| row.get(0),
        )
        .optional()?;
    if note_id.is_some() {
        match activation.action {
            Action::Open => (),
            Action::Snooze => snooze(
                &transaction,
                &activation.reminder_id,
                as_of.with_timezone(&Utc) + chrono::Duration::minutes(10),
            )?,
            Action::Done => {
                set_status_at(&transaction, &activation.reminder_id, "completed", as_of)?
            }
            Action::Dismiss => {
                set_status_at(&transaction, &activation.reminder_id, "dismissed", as_of)?
            }
        }
    }
    transaction.commit()?;
    Ok(note_id)
}

pub fn set_status(conn: &Connection, reminder_id: &str, status: &str) -> rusqlite::Result<()> {
    set_status_at(conn, reminder_id, status, Local::now())
}

fn next_recurrence(
    anchor: NaiveDate,
    current: NaiveDate,
    time: NaiveTime,
    recurrence: &str,
    after: NaiveDateTime,
) -> Option<NaiveDate> {
    let mut date = current;
    let mut month = ((current.year() - anchor.year()) * 12 + current.month() as i32
        - anchor.month() as i32)
        .max(0) as u32;
    for _ in 0..100_000 {
        date = match recurrence {
            "daily" => date.checked_add_signed(chrono::Duration::days(1))?,
            "weekly" => date.checked_add_signed(chrono::Duration::days(7))?,
            "monthly" => {
                month = month.checked_add(1)?;
                anchor.checked_add_months(Months::new(month))?
            }
            _ => return None,
        };
        if date.and_time(time) > after {
            return Some(date);
        }
    }
    None
}

pub fn set_status_at(
    conn: &Connection,
    reminder_id: &str,
    status: &str,
    as_of: DateTime<Local>,
) -> rusqlite::Result<()> {
    if !matches!(status, "pending" | "dismissed" | "completed") {
        return Err(rusqlite::Error::InvalidParameterName(format!(
            "invalid reminder status: {status}"
        )));
    }
    let recurrence: Option<(String, String, String, String)> = conn
        .query_row(
            "SELECT due_date,scheduled_date,due_time,recurrence FROM reminders WHERE id=?1",
            [reminder_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .optional()?;
    let Some((anchor, current, due_time, cadence)) = recurrence else {
        return Err(rusqlite::Error::QueryReturnedNoRows);
    };
    let changed = if status != "pending" && cadence != "none" {
        let anchor = NaiveDate::parse_from_str(&anchor, "%Y-%m-%d")
            .map_err(|_| rusqlite::Error::InvalidQuery)?;
        let current = NaiveDate::parse_from_str(&current, "%Y-%m-%d")
            .map_err(|_| rusqlite::Error::InvalidQuery)?;
        let time = if due_time.is_empty() {
            NaiveTime::from_hms_opt(0, 0, 0).unwrap()
        } else {
            NaiveTime::parse_from_str(&due_time, "%H:%M")
                .map_err(|_| rusqlite::Error::InvalidQuery)?
        };
        let next = next_recurrence(anchor, current, time, &cadence, as_of.naive_local())
            .ok_or(rusqlite::Error::InvalidQuery)?;
        conn.execute(
            "UPDATE reminders SET scheduled_date=?2,status='pending',snoozed_until=NULL,notified_for=NULL,modified_at=?3 WHERE id=?1",
            params![reminder_id, next.format("%Y-%m-%d").to_string(), now()],
        )?
    } else {
        conn.execute(
            "UPDATE reminders SET status=?2,modified_at=?3 WHERE id=?1",
            params![reminder_id, status, now()],
        )?
    };
    if changed == 0 {
        Err(rusqlite::Error::QueryReturnedNoRows)
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{notes, Db};
    use chrono::TimeZone;

    fn db() -> Db {
        let p = std::env::temp_dir().join(format!("slint-reminders-test-{}", uuid::Uuid::new_v4()));
        Db::open_in_dir(&p).unwrap()
    }

    fn content(chips: &[(&str, &str)]) -> String {
        serde_json::json!({
            "blocks": [{"type":"text", "runs": chips.iter().map(|(id,date)| serde_json::json!({"type":"dateChip", "attrs":{"chipId":id,"date":date,"label":date}})).collect::<Vec<_>>() }],
            "plain": "dates"
        }).to_string()
    }

    #[test]
    fn ignored_repeats_notify_again_on_their_next_occurrence() {
        let d = db();
        let content = serde_json::json!({"blocks":[{"type":"text","runs":[{"type":"dateChip","attrs":{"chipId":"c","date":"2026-11-01","time":"09:00","recurrence":"daily"}}]}],"plain":"stretch"}).to_string();
        notes::create(&d.conn, "n", &content, "#FFF9B1").unwrap();
        let at = |day, hour| {
            Local
                .with_ymd_and_hms(2026, 11, day, hour, 0, 0)
                .single()
                .unwrap()
        };
        let announce = |when| {
            let due = unannounced_due_at(&d.conn, when).unwrap();
            for row in &due {
                mark_announced(&d.conn, row).unwrap();
            }
            due.into_iter()
                .map(|row| row.scheduled_date)
                .collect::<Vec<_>>()
        };
        assert_eq!(announce(at(1, 9)), ["2026-11-01"]);
        // Ignored: nothing more until the next occurrence arrives.
        assert!(announce(at(1, 20)).is_empty());
        assert!(announce(at(2, 8)).is_empty());
        assert_eq!(announce(at(2, 9)), ["2026-11-02"]);
        // Days the app missed collapse into one notification for the latest.
        assert_eq!(announce(at(5, 10)), ["2026-11-05"]);
        assert!(announce(at(5, 11)).is_empty());
        // A snooze past the next occurrence is kept; when it ends, the one
        // notification is for that day's occurrence.
        let id = due_at(&d.conn, at(5, 11)).unwrap().remove(0).id;
        snooze(&d.conn, &id, at(6, 12).with_timezone(&Utc)).unwrap();
        assert!(announce(at(6, 10)).is_empty());
        assert_eq!(announce(at(6, 12)), ["2026-11-06"]);
        assert!(announce(at(6, 13)).is_empty());
    }

    #[test]
    fn toast_actions_are_occurrence_scoped_and_duplicate_done_is_harmless() {
        use crate::notification::{Action, Activation};
        let d = db();
        let content = serde_json::json!({"blocks":[{"type":"text","runs":[{"type":"dateChip","attrs":{"chipId":"c","date":"2026-10-01","time":"09:00","recurrence":"daily"}}]}],"plain":"meeting"}).to_string();
        notes::create(&d.conn, "n", &content, "#FFF9B1").unwrap();
        let at = Local
            .with_ymd_and_hms(2026, 10, 1, 9, 0, 0)
            .single()
            .unwrap();
        let row = due_at(&d.conn, at).unwrap().remove(0);
        let mut action = Activation {
            action: Action::Done,
            reminder_id: row.id.clone(),
            occurrence: occurrence_key(&row),
        };
        assert_eq!(activate(&d.conn, &action, at).unwrap(), None); // Never announced.
        mark_announced(&d.conn, &row).unwrap();
        action.occurrence = "2026-09-30T09:00".into();
        assert_eq!(activate(&d.conn, &action, at).unwrap(), None);
        action.occurrence = occurrence_key(&row);
        assert_eq!(
            activate(&d.conn, &action, at).unwrap().as_deref(),
            Some("n")
        );
        assert_eq!(activate(&d.conn, &action, at).unwrap(), None);
        assert_eq!(
            list_for_note(&d.conn, "n").unwrap()[0].scheduled_date,
            "2026-10-02"
        );
    }

    #[test]
    fn toast_snooze_replay_and_deleted_note_activation_are_ignored() {
        use crate::notification::{Action, Activation};
        let d = db();
        notes::create(&d.conn, "n", &content(&[("c", "2026-10-01")]), "#FFF9B1").unwrap();
        let at = Local
            .with_ymd_and_hms(2026, 10, 1, 9, 0, 0)
            .single()
            .unwrap();
        let row = due_at(&d.conn, at).unwrap().remove(0);
        mark_announced(&d.conn, &row).unwrap();
        let action = Activation {
            action: Action::Snooze,
            reminder_id: row.id.clone(),
            occurrence: occurrence_key(&row),
        };
        assert_eq!(
            activate(&d.conn, &action, at).unwrap().as_deref(),
            Some("n")
        );
        assert_eq!(
            activate(&d.conn, &action, at + chrono::Duration::minutes(1)).unwrap(),
            None
        );
        assert_eq!(
            list_for_note(&d.conn, "n").unwrap()[0].snoozed_until,
            Some((at.with_timezone(&Utc) + chrono::Duration::minutes(10)).to_rfc3339())
        );
        notes::soft_delete(&d.conn, "n").unwrap();
        assert_eq!(activate(&d.conn, &action, at).unwrap(), None);
    }

    #[test]
    fn exact_time_and_snooze_follow_local_clock_and_survive_reopen() {
        let directory = std::env::temp_dir().join(format!(
            "slint-reminder-clock-test-{}",
            uuid::Uuid::new_v4()
        ));
        let d = Db::open_in_dir(&directory).unwrap();
        let timed = |time: &str| {
            serde_json::json!({
                "blocks": [{"type":"text","runs":[{"type":"dateChip","attrs":{
                    "chipId":"meeting","date":"2026-09-28","time":time
                }}]}], "plain":"meeting"
            })
            .to_string()
        };
        notes::create(&d.conn, "n", &timed("17:30"), "#FFF9B1").unwrap();
        let at = |hour, minute| {
            Local
                .with_ymd_and_hms(2026, 9, 28, hour, minute, 0)
                .single()
                .unwrap()
        };
        assert!(due_at(&d.conn, at(17, 29)).unwrap().is_empty());
        let reminder = due_at(&d.conn, at(17, 30)).unwrap().pop().unwrap();
        assert_eq!(reminder.due_time, "17:30");
        snooze(&d.conn, &reminder.id, at(17, 40).with_timezone(&Utc)).unwrap();
        assert!(due_at(&d.conn, at(17, 39)).unwrap().is_empty());
        drop(d);
        let reopened = Db::open_in_dir(&directory).unwrap();
        assert_eq!(due_at(&reopened.conn, at(17, 40)).unwrap().len(), 1);
        notes::update_content(&reopened.conn, "n", &timed("18:00")).unwrap();
        assert!(due_at(&reopened.conn, at(17, 59)).unwrap().is_empty());
        assert_eq!(
            due_at(&reopened.conn, at(18, 0)).unwrap()[0].snoozed_until,
            None
        );
    }

    #[test]
    fn invalid_time_never_projects_a_reminder() {
        let json = serde_json::json!({"blocks":[{"type":"text","runs":[
            {"type":"dateChip","attrs":{"date":"2026-09-28","time":"25:99"}}
        ]}]});
        assert!(extract_date_chips(&json.to_string()).is_empty());
    }

    #[test]
    fn monthly_recurrence_keeps_anchor_day_and_survives_unchanged_save() {
        let d = db();
        let content = serde_json::json!({"blocks":[{"type":"text","runs":[
            {"type":"dateChip","attrs":{"chipId":"monthly","date":"2026-01-31","time":"12:00","recurrence":"monthly"}}
        ]}],"plain":"monthly"}).to_string();
        notes::create(&d.conn, "n", &content, "#FFF9B1").unwrap();
        let id = list_for_note(&d.conn, "n").unwrap()[0].id.clone();
        let on = |month, day| {
            Local
                .with_ymd_and_hms(2026, month, day, 13, 0, 0)
                .single()
                .unwrap()
        };
        set_status_at(&d.conn, &id, "completed", on(1, 31)).unwrap();
        assert_eq!(
            list_for_note(&d.conn, "n").unwrap()[0].scheduled_date,
            "2026-02-28"
        );
        notes::update_content(&d.conn, "n", &content).unwrap();
        assert_eq!(
            list_for_note(&d.conn, "n").unwrap()[0].scheduled_date,
            "2026-02-28"
        );
        set_status_at(&d.conn, &id, "dismissed", on(3, 1)).unwrap();
        assert_eq!(
            list_for_note(&d.conn, "n").unwrap()[0].scheduled_date,
            "2026-03-31"
        );
        assert!(due_at(&d.conn, on(3, 1)).unwrap().is_empty());
    }

    #[test]
    fn recurring_completion_catches_up_without_replaying_old_alerts() {
        let d = db();
        let content = |cadence: &str| {
            serde_json::json!({"blocks":[{"type":"text","runs":[
            {"type":"dateChip","attrs":{"chipId":"repeat","date":"2026-09-01","time":"17:30","recurrence":cadence}}
        ]}],"plain":"repeat"}).to_string()
        };
        notes::create(&d.conn, "n", &content("daily"), "#FFF9B1").unwrap();
        let id = list_for_note(&d.conn, "n").unwrap()[0].id.clone();
        let now = Local
            .with_ymd_and_hms(2026, 9, 28, 18, 0, 0)
            .single()
            .unwrap();
        set_status_at(&d.conn, &id, "completed", now).unwrap();
        let after = list_for_note(&d.conn, "n").unwrap().pop().unwrap();
        assert_eq!(after.scheduled_date, "2026-09-29");
        assert_eq!(after.status, "pending");
        assert!(due_at(&d.conn, now).unwrap().is_empty());
        notes::update_content(&d.conn, "n", &content("weekly")).unwrap();
        let changed = list_for_note(&d.conn, "n").unwrap().pop().unwrap();
        assert_eq!(changed.scheduled_date, "2026-09-01");
        assert_eq!(changed.recurrence, "weekly");
    }

    #[test]
    fn announcement_is_once_per_occurrence_and_snooze_rearms_it() {
        let d = db();
        let content = serde_json::json!({"blocks":[{"type":"text","runs":[
            {"type":"dateChip","attrs":{"chipId":"once","date":"2026-09-28","time":"17:30"}}
        ]}],"plain":"once"})
        .to_string();
        notes::create(&d.conn, "n", &content, "#FFF9B1").unwrap();
        let at = |minute| {
            Local
                .with_ymd_and_hms(2026, 9, 28, 17, minute, 0)
                .single()
                .unwrap()
        };
        let due = unannounced_due_at(&d.conn, at(30)).unwrap();
        assert_eq!(due.len(), 1);
        assert!(mark_announced(&d.conn, &due[0]).unwrap());
        assert!(!mark_announced(&d.conn, &due[0]).unwrap());
        assert!(unannounced_due_at(&d.conn, at(30)).unwrap().is_empty());
        snooze(&d.conn, &due[0].id, at(40).with_timezone(&Utc)).unwrap();
        assert!(unannounced_due_at(&d.conn, at(39)).unwrap().is_empty());
        assert_eq!(unannounced_due_at(&d.conn, at(40)).unwrap().len(), 1);
    }

    #[test]
    fn extracts_normal_and_checklist_chips_with_stable_fallbacks() {
        let json = serde_json::json!({"blocks":[
            {"type":"text","runs":[{"type":"dateChip","attrs":{"chipId":"ship","date":"2026-09-18","label":"Ship it"}}]},
            {"type":"checklist","items":[{"runs":[{"type":"dateChip","attrs":{"date":"2026-09-19"}}]}]}
        ]});
        let chips = extract_date_chips(&json.to_string());
        assert_eq!(chips[0].chip_id, "ship");
        assert_eq!(chips[0].label, "Ship it");
        assert_eq!(chips[1].chip_id, "block-1-item-0-run-0");
        assert_eq!(chips[1].due_date, "2026-09-19");
    }

    #[test]
    fn reconcile_preserves_status_and_resets_when_due_date_changes() {
        let d = db();
        notes::create(
            &d.conn,
            "n1",
            &content(&[("ship", "2026-09-18")]),
            "#FFF9B1",
        )
        .unwrap();
        let id = list_for_note(&d.conn, "n1").unwrap().pop().unwrap().id;
        set_status(&d.conn, &id, "completed").unwrap();
        reconcile_note(&d.conn, "n1", &content(&[("ship", "2026-09-18")])).unwrap();
        assert_eq!(list_for_note(&d.conn, "n1").unwrap()[0].status, "completed");
        reconcile_note(&d.conn, "n1", &content(&[("ship", "2026-09-19")])).unwrap();
        assert_eq!(list_for_note(&d.conn, "n1").unwrap()[0].status, "pending");
    }

    #[test]
    fn due_query_is_pending_only_and_deterministically_ordered() {
        let d = db();
        notes::create(&d.conn, "b", &content(&[("late", "2026-09-17")]), "#FFF9B1").unwrap();
        notes::create(
            &d.conn,
            "a",
            &content(&[("early", "2026-09-16")]),
            "#FFF9B1",
        )
        .unwrap();
        let early = list_for_note(&d.conn, "a").unwrap().pop().unwrap().id;
        set_status(&d.conn, &early, "dismissed").unwrap();
        let due = due_on(&d.conn, NaiveDate::from_ymd_opt(2026, 9, 18).unwrap()).unwrap();
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].note_id, "b");
    }

    #[test]
    fn deleting_a_note_removes_its_reminders() {
        let d = db();
        notes::create(
            &d.conn,
            "n1",
            &content(&[("ship", "2026-09-18")]),
            "#FFF9B1",
        )
        .unwrap();
        notes::soft_delete(&d.conn, "n1").unwrap();
        assert!(list_for_note(&d.conn, "n1").unwrap().is_empty());
        notes::restore(&d.conn, "n1").unwrap();
        assert_eq!(list_for_note(&d.conn, "n1").unwrap().len(), 1);
    }

    #[test]
    fn completing_task_cancels_only_its_reminders_and_unchecking_rearms() {
        let d = db();
        let task_content = |checked| {
            serde_json::json!({
            "plain": "Buy milk and call home",
            "blocks": [
                {"type":"checklist", "items":[{"id":"task", "checked":checked,
                    "runs":[{"type":"dateChip", "attrs":{"chipId":"task-date", "date":"2026-09-26"}}]}]},
                {"type":"text", "runs":[{"type":"dateChip", "attrs":{"chipId":"note-date", "date":"2026-09-26"}}]}
            ]
        }).to_string()
        };
        notes::create(&d.conn, "task-note", &task_content(false), "#FFF9B1").unwrap();
        let as_of = NaiveDate::from_ymd_opt(2026, 9, 26).unwrap();
        assert_eq!(due_on(&d.conn, as_of).unwrap().len(), 2);
        let note_reminder = list_for_note(&d.conn, "task-note")
            .unwrap()
            .into_iter()
            .find(|row| row.chip_id == "note-date")
            .unwrap();
        set_status(&d.conn, &note_reminder.id, "dismissed").unwrap();

        notes::update_content(&d.conn, "task-note", &task_content(true)).unwrap();
        assert!(due_on(&d.conn, as_of).unwrap().is_empty());
        let remaining = list_for_note(&d.conn, "task-note").unwrap();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].status, "dismissed");
        assert!(notes::get(&d.conn, "task-note")
            .unwrap()
            .unwrap()
            .content
            .contains("task-date"));

        notes::update_content(&d.conn, "task-note", &task_content(false)).unwrap();
        let due = due_on(&d.conn, as_of).unwrap();
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].chip_id, "task-date");
    }
}
