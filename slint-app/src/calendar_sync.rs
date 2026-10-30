//! One-way push of timed reminders to a Google Calendar the app owns.
//!
//! This module decides what the calendar should contain and records what was
//! pushed; the network client only applies the planned operations. Google is
//! never read back into notes, and local reminders keep working without it.
use chrono::{Datelike, NaiveDate, NaiveDateTime, NaiveTime};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;

/// Alert lead times offered at connect time and in Settings.
pub const ALERT_CHOICES: [u32; 5] = [0, 10, 20, 30, 60];
const EVENT_MINUTES: i64 = 15;

/// Chosen when the account is first connected.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CalendarConfig {
    pub calendar_id: String,
    /// Whether reminders that existed before connecting are pushed too.
    pub sync_existing: bool,
    /// RFC 3339; with `sync_existing` off, older reminders stay local.
    pub connected_at: String,
    pub alert_minutes: u32,
    /// Off sends only the note title, never its text.
    pub include_note_text: bool,
}

impl CalendarConfig {
    pub fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(!self.calendar_id.is_empty(), "Missing calendar id");
        anyhow::ensure!(
            ALERT_CHOICES.contains(&self.alert_minutes),
            "Unsupported alert time"
        );
        chrono::DateTime::parse_from_rfc3339(&self.connected_at)?;
        Ok(())
    }
}

pub fn load_config(conn: &Connection) -> anyhow::Result<Option<CalendarConfig>> {
    let raw: Option<String> = conn
        .query_row(
            "SELECT value FROM meta WHERE key='google_calendar'",
            [],
            |row| row.get(0),
        )
        .optional()?;
    let Some(raw) = raw else { return Ok(None) };
    let config: CalendarConfig = serde_json::from_str(&raw)?;
    config.validate()?;
    Ok(Some(config))
}

pub fn save_config(conn: &Connection, config: &CalendarConfig) -> anyhow::Result<()> {
    config.validate()?;
    conn.execute("INSERT INTO meta(key,value) VALUES('google_calendar',?1) ON CONFLICT(key) DO UPDATE SET value=excluded.value", [serde_json::to_string(config)?])?;
    Ok(())
}

/// Disconnect forgets the calendar and every pushed event mapping. The events
/// themselves live in the app's own calendar, which the user can delete.
/// Runs inside the caller's transaction, which also forgets the token.
pub fn clear(conn: &Connection) -> anyhow::Result<()> {
    conn.execute("DELETE FROM meta WHERE key='google_calendar'", [])?;
    conn.execute("DELETE FROM calendar_events", [])?;
    Ok(())
}

/// What one reminder should look like in Google Calendar.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EventSpec {
    pub reminder_id: String,
    pub summary: String,
    pub description: String,
    /// The series anchor, not the next occurrence: completing a repeating
    /// reminder advances it locally but leaves the calendar series alone.
    pub start: NaiveDateTime,
    pub recurrence: String,
    pub alert_minutes: u32,
    /// The PC's IANA zone at push time; part of the fingerprint so events
    /// follow local wall time when the zone changes.
    pub time_zone: String,
    /// A one-off whose time has passed. Never created, and an already pushed
    /// one is left in place rather than updated or removed.
    pub is_past: bool,
}

impl EventSpec {
    /// Changes when anything Google shows changes, and only then.
    pub fn fingerprint(&self) -> String {
        json!([
            self.summary,
            self.description,
            self.start.format("%Y-%m-%dT%H:%M").to_string(),
            self.recurrence,
            self.alert_minutes,
            self.time_zone
        ])
        .to_string()
    }

    pub fn to_google(&self) -> Value {
        let time_zone = &self.time_zone;
        let end = self.start + chrono::Duration::minutes(EVENT_MINUTES);
        let mut event = json!({
            "summary": self.summary,
            "description": self.description,
            "start": {"dateTime": self.start.format("%Y-%m-%dT%H:%M:%S").to_string(), "timeZone": time_zone},
            "end": {"dateTime": end.format("%Y-%m-%dT%H:%M:%S").to_string(), "timeZone": time_zone},
            "reminders": {"useDefault": false, "overrides": [{"method": "popup", "minutes": self.alert_minutes}]},
            "extendedProperties": {"private": {"stickyReminderId": self.reminder_id}},
        });
        if let Some(rule) = rrule(&self.recurrence, self.start.date()) {
            event["recurrence"] = json!([rule]);
        }
        event
    }
}

/// Mirrors the local schedule: monthly reminders on the 29th-31st fall on the
/// month's last day when it is shorter, as `next_recurrence` clamps them.
fn rrule(recurrence: &str, anchor: NaiveDate) -> Option<String> {
    Some(match recurrence {
        "daily" => "RRULE:FREQ=DAILY".into(),
        "weekly" => "RRULE:FREQ=WEEKLY".into(),
        "monthly" if anchor.day() <= 28 => "RRULE:FREQ=MONTHLY".into(),
        "monthly" => {
            let days = (28..=anchor.day())
                .map(|day| day.to_string())
                .collect::<Vec<_>>()
                .join(",");
            format!("RRULE:FREQ=MONTHLY;BYMONTHDAY={days};BYSETPOS=-1")
        }
        _ => return None,
    })
}

/// Pending reminders with a time, on notes that are not deleted. Date-only
/// reminders stay local.
pub fn desired_events(
    conn: &Connection,
    config: &CalendarConfig,
    time_zone: &str,
    now: NaiveDateTime,
) -> anyhow::Result<Vec<EventSpec>> {
    let mut statement = conn.prepare(
        "SELECT r.id,r.due_date,r.due_time,r.recurrence,r.created_at,n.title,n.content
         FROM reminders r JOIN notes n ON n.id=r.note_id
         WHERE r.status='pending' AND r.due_time<>'' AND n.deleted_at IS NULL
         ORDER BY r.id",
    )?;
    let rows = statement.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, String>(3)?,
            row.get::<_, String>(4)?,
            row.get::<_, Option<String>>(5)?,
            row.get::<_, String>(6)?,
        ))
    })?;
    let connected_at = chrono::DateTime::parse_from_rfc3339(&config.connected_at)?;
    let mut events = Vec::new();
    for row in rows {
        let (id, date, time, recurrence, created_at, title, content) = row?;
        if !config.sync_existing
            && chrono::DateTime::parse_from_rfc3339(&created_at).is_ok_and(|at| at < connected_at)
        {
            continue;
        }
        let (Ok(date), Ok(time)) = (
            NaiveDate::parse_from_str(&date, "%Y-%m-%d"),
            NaiveTime::parse_from_str(&time, "%H:%M"),
        ) else {
            continue;
        };
        // The event already shows the time, so date-chip labels are dropped.
        let text = without_date_labels(&content, &crate::plain_from_content(&content));
        let summary = text
            .lines()
            .find(|line| !line.trim().is_empty())
            .map(|line| line.trim().chars().take(60).collect::<String>())
            .or_else(|| title.map(|title| title.trim().to_owned()))
            .filter(|title| !title.is_empty())
            .unwrap_or_else(|| "Untitled note".into());
        let description: String = if config.include_note_text {
            text.chars().take(2000).collect()
        } else {
            String::new()
        };
        let start = date.and_time(time);
        events.push(EventSpec {
            reminder_id: id,
            summary,
            description,
            start,
            is_past: recurrence == "none" && start <= now,
            recurrence,
            alert_minutes: config.alert_minutes,
            time_zone: time_zone.to_owned(),
        });
    }
    Ok(events)
}

/// The note text without its date-chip labels ("Dentist Oct 28 · 21:00" →
/// "Dentist"). Labels are found in the content JSON and removed once each
/// from the plain text, so ordinary words that match are untouched.
fn without_date_labels(content: &str, plain: &str) -> String {
    fn labels(value: &Value, out: &mut Vec<String>) {
        match value {
            Value::Object(map) => {
                if map.get("type").and_then(Value::as_str) == Some("dateChip") {
                    if let Some(label) = value.pointer("/attrs/label").and_then(Value::as_str) {
                        out.push(label.to_owned());
                    }
                }
                map.values().for_each(|child| labels(child, out));
            }
            Value::Array(items) => items.iter().for_each(|child| labels(child, out)),
            _ => (),
        }
    }
    let mut found = Vec::new();
    if let Ok(value) = serde_json::from_str::<Value>(content) {
        labels(&value, &mut found);
    }
    let mut text = plain.to_owned();
    for label in found.iter().filter(|label| !label.is_empty()) {
        text = text.replacen(label.as_str(), "", 1);
    }
    text.lines()
        .map(|line| line.split_whitespace().collect::<Vec<_>>().join(" "))
        .collect::<Vec<_>>()
        .join(
            "
",
        )
        .trim()
        .to_owned()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PushedEvent {
    pub reminder_id: String,
    pub event_id: String,
    pub fingerprint: String,
}

pub fn pushed_events(conn: &Connection) -> rusqlite::Result<Vec<PushedEvent>> {
    let mut statement = conn.prepare(
        "SELECT reminder_id,event_id,fingerprint FROM calendar_events ORDER BY reminder_id",
    )?;
    let rows = statement.query_map([], |row| {
        Ok(PushedEvent {
            reminder_id: row.get(0)?,
            event_id: row.get(1)?,
            fingerprint: row.get(2)?,
        })
    })?;
    rows.collect()
}

pub fn record_pushed(
    conn: &Connection,
    reminder_id: &str,
    event_id: &str,
    fingerprint: &str,
) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO calendar_events(reminder_id,event_id,fingerprint) VALUES(?1,?2,?3)
         ON CONFLICT(reminder_id) DO UPDATE SET event_id=excluded.event_id,fingerprint=excluded.fingerprint",
        params![reminder_id, event_id, fingerprint],
    )?;
    Ok(())
}

pub fn forget_pushed(conn: &Connection, reminder_id: &str) -> rusqlite::Result<()> {
    conn.execute(
        "DELETE FROM calendar_events WHERE reminder_id=?1",
        [reminder_id],
    )?;
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SyncOp {
    Create(EventSpec),
    Update {
        event_id: String,
        spec: EventSpec,
    },
    Delete {
        reminder_id: String,
        event_id: String,
    },
}

/// Diff what the calendar should hold against what was pushed. Idempotent:
/// re-running after a partial failure only repeats the unfinished work.
pub fn plan(desired: Vec<EventSpec>, pushed: Vec<PushedEvent>) -> Vec<SyncOp> {
    let mut pushed: HashMap<String, PushedEvent> = pushed
        .into_iter()
        .map(|event| (event.reminder_id.clone(), event))
        .collect();
    let mut operations = Vec::new();
    for spec in desired {
        match pushed.remove(&spec.reminder_id) {
            None if spec.is_past => (),
            Some(_) if spec.is_past => (),
            None => operations.push(SyncOp::Create(spec)),
            Some(event) if event.fingerprint != spec.fingerprint() => {
                operations.push(SyncOp::Update {
                    event_id: event.event_id,
                    spec,
                })
            }
            Some(_) => (),
        }
    }
    let mut stale: Vec<_> = pushed.into_values().collect();
    stale.sort_by(|a, b| a.reminder_id.cmp(&b.reminder_id));
    operations.extend(stale.into_iter().map(|event| SyncOp::Delete {
        reminder_id: event.reminder_id,
        event_id: event.event_id,
    }));
    operations
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{notes, reminders, Db};

    #[allow(non_snake_case)]
    fn NOW() -> NaiveDateTime {
        NaiveDate::from_ymd_opt(2026, 10, 27)
            .unwrap()
            .and_hms_opt(12, 0, 0)
            .unwrap()
    }

    fn db() -> Db {
        let path =
            std::env::temp_dir().join(format!("slint-calendar-test-{}", uuid::Uuid::new_v4()));
        Db::open_in_dir(&path).unwrap()
    }

    fn config(sync_existing: bool, connected_at: &str) -> CalendarConfig {
        CalendarConfig {
            calendar_id: "cal".into(),
            sync_existing,
            connected_at: connected_at.into(),
            alert_minutes: 10,
            include_note_text: true,
        }
    }

    fn note(db: &Db, id: &str, chips: serde_json::Value) {
        let content = json!({"blocks":[{"type":"text","runs":[{"type":"text","text":"Dentist "}, {"type":"dateChip","attrs":chips}]}],"plain":"Dentist"}).to_string();
        notes::create(&db.conn, id, &content, "#FFF9B1").unwrap();
    }

    #[test]
    fn only_pending_timed_reminders_on_live_notes_are_pushed() {
        let db = db();
        note(
            &db,
            "timed",
            json!({"chipId":"a","date":"2026-11-03","time":"14:00"}),
        );
        note(&db, "dateonly", json!({"chipId":"b","date":"2026-11-03"}));
        note(
            &db,
            "deleted",
            json!({"chipId":"c","date":"2026-11-03","time":"09:00"}),
        );
        note(
            &db,
            "done",
            json!({"chipId":"d","date":"2026-11-03","time":"09:00"}),
        );
        notes::soft_delete(&db.conn, "deleted").unwrap();
        let done = reminders::list_for_note(&db.conn, "done").unwrap();
        reminders::set_status(&db.conn, &done[0].id, "completed").unwrap();

        let events = desired_events(
            &db.conn,
            &config(true, "2000-01-01T00:00:00Z"),
            "UTC",
            NOW(),
        )
        .unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].start.to_string(), "2026-11-03 14:00:00");
        assert_eq!(events[0].summary, "Dentist");
    }

    #[test]
    fn declining_initial_sync_keeps_reminders_from_before_connecting_local() {
        let db = db();
        note(
            &db,
            "old",
            json!({"chipId":"a","date":"2026-11-03","time":"14:00"}),
        );
        let future = (chrono::Utc::now() + chrono::Duration::days(1)).to_rfc3339();
        assert!(
            desired_events(&db.conn, &config(false, &future), "UTC", NOW())
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            desired_events(&db.conn, &config(true, &future), "UTC", NOW())
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn plan_creates_updates_deletes_and_is_idempotent() {
        let spec = |id: &str, hour: u32| EventSpec {
            reminder_id: id.into(),
            summary: "Note".into(),
            description: String::new(),
            start: NaiveDate::from_ymd_opt(2026, 11, 3)
                .unwrap()
                .and_hms_opt(hour, 0, 0)
                .unwrap(),
            recurrence: "none".into(),
            alert_minutes: 10,
            time_zone: "America/Toronto".into(),
            is_past: false,
        };
        let pushed = |id: &str, spec: &EventSpec| PushedEvent {
            reminder_id: id.into(),
            event_id: format!("e-{id}"),
            fingerprint: spec.fingerprint(),
        };
        let unchanged = spec("same", 9);
        let moved_before = spec("moved", 9);
        let operations = plan(
            vec![spec("new", 9), unchanged.clone(), spec("moved", 10)],
            vec![
                pushed("same", &unchanged),
                pushed("moved", &moved_before),
                pushed("gone", &unchanged),
            ],
        );
        assert_eq!(
            operations,
            vec![
                SyncOp::Create(spec("new", 9)),
                SyncOp::Update {
                    event_id: "e-moved".into(),
                    spec: spec("moved", 10)
                },
                SyncOp::Delete {
                    reminder_id: "gone".into(),
                    event_id: "e-gone".into()
                },
            ]
        );
        let settled = vec![
            pushed("new", &spec("new", 9)),
            pushed("same", &unchanged),
            pushed("moved", &spec("moved", 10)),
        ];
        assert!(plan(vec![spec("new", 9), unchanged, spec("moved", 10)], settled).is_empty());
    }

    #[test]
    fn changing_the_alert_time_updates_every_event() {
        let mut spec = EventSpec {
            reminder_id: "r".into(),
            summary: "Note".into(),
            description: String::new(),
            start: NaiveDate::from_ymd_opt(2026, 11, 3)
                .unwrap()
                .and_hms_opt(9, 0, 0)
                .unwrap(),
            recurrence: "none".into(),
            alert_minutes: 10,
            time_zone: "America/Toronto".into(),
            is_past: false,
        };
        let before = spec.fingerprint();
        spec.alert_minutes = 30;
        assert_ne!(before, spec.fingerprint());
        assert_eq!(spec.to_google()["reminders"]["overrides"][0]["minutes"], 30);
    }

    #[test]
    fn completing_a_repeating_occurrence_keeps_the_series_unchanged() {
        let db = db();
        note(
            &db,
            "daily",
            json!({"chipId":"a","date":"2026-10-01","time":"09:00","recurrence":"daily"}),
        );
        let config = config(true, "2000-01-01T00:00:00Z");
        let before = desired_events(&db.conn, &config, "UTC", NOW()).unwrap();
        let row = &reminders::list_for_note(&db.conn, "daily").unwrap()[0];
        reminders::set_status(&db.conn, &row.id, "completed").unwrap();
        assert_eq!(
            desired_events(&db.conn, &config, "UTC", NOW()).unwrap(),
            before
        );
        assert_eq!(before[0].to_google()["recurrence"][0], "RRULE:FREQ=DAILY");
    }

    #[test]
    fn google_event_shape_and_month_end_rules() {
        let spec = EventSpec {
            reminder_id: "r".into(),
            summary: "Rent".into(),
            description: "Pay rent".into(),
            start: NaiveDate::from_ymd_opt(2026, 1, 31)
                .unwrap()
                .and_hms_opt(23, 50, 0)
                .unwrap(),
            recurrence: "monthly".into(),
            alert_minutes: 0,
            time_zone: "America/Toronto".into(),
            is_past: false,
        };
        let event = spec.to_google();
        assert_eq!(event["start"]["dateTime"], "2026-01-31T23:50:00");
        assert_eq!(event["end"]["dateTime"], "2026-02-01T00:05:00");
        assert_eq!(event["start"]["timeZone"], "America/Toronto");
        assert_eq!(
            event["recurrence"][0],
            "RRULE:FREQ=MONTHLY;BYMONTHDAY=28,29,30,31;BYSETPOS=-1"
        );
        assert_eq!(
            event["extendedProperties"]["private"]["stickyReminderId"],
            "r"
        );
        assert_eq!(
            rrule("monthly", NaiveDate::from_ymd_opt(2026, 1, 15).unwrap()).unwrap(),
            "RRULE:FREQ=MONTHLY"
        );
        assert_eq!(
            rrule("none", NaiveDate::from_ymd_opt(2026, 1, 15).unwrap()),
            None
        );
    }

    #[test]
    fn config_roundtrip_validation_and_disconnect() {
        let db = db();
        assert_eq!(load_config(&db.conn).unwrap(), None);
        let mut value = config(false, "2026-10-27T12:00:00Z");
        save_config(&db.conn, &value).unwrap();
        record_pushed(&db.conn, "r", "e", "f").unwrap();
        assert_eq!(load_config(&db.conn).unwrap(), Some(value.clone()));
        value.alert_minutes = 7;
        assert!(save_config(&db.conn, &value).is_err());
        // Disconnect clears inside its own transaction with the token.
        let transaction = db.conn.unchecked_transaction().unwrap();
        clear(&transaction).unwrap();
        transaction.commit().unwrap();
        assert_eq!(load_config(&db.conn).unwrap(), None);
        assert!(pushed_events(&db.conn).unwrap().is_empty());
    }

    #[test]
    fn past_one_offs_are_never_created_and_pushed_ones_are_left_alone() {
        let db = db();
        note(
            &db,
            "past",
            json!({"chipId":"a","date":"2026-10-01","time":"09:00"}),
        );
        note(
            &db,
            "pastdaily",
            json!({"chipId":"b","date":"2026-10-01","time":"09:00","recurrence":"daily"}),
        );
        let events = desired_events(
            &db.conn,
            &config(true, "2000-01-01T00:00:00Z"),
            "UTC",
            NOW(),
        )
        .unwrap();
        let operations = plan(events.clone(), vec![]);
        assert_eq!(operations.len(), 1, "only the repeating series is created");
        let past = events.iter().find(|event| event.is_past).unwrap();
        let pushed = PushedEvent {
            reminder_id: past.reminder_id.clone(),
            event_id: "e".into(),
            fingerprint: "stale".into(),
        };
        let operations = plan(vec![past.clone()], vec![pushed]);
        assert!(
            operations.is_empty(),
            "a pushed past event is not updated or deleted"
        );
    }

    #[test]
    fn time_zone_change_and_title_only_option() {
        let db = db();
        note(
            &db,
            "n",
            json!({"chipId":"a","date":"2026-11-03","time":"14:00"}),
        );
        let mut settings = config(true, "2000-01-01T00:00:00Z");
        let toronto = desired_events(&db.conn, &settings, "America/Toronto", NOW()).unwrap();
        let london = desired_events(&db.conn, &settings, "Europe/London", NOW()).unwrap();
        assert_ne!(toronto[0].fingerprint(), london[0].fingerprint());
        assert_eq!(london[0].to_google()["start"]["timeZone"], "Europe/London");
        assert!(!toronto[0].description.is_empty());
        settings.include_note_text = false;
        let title_only = desired_events(&db.conn, &settings, "America/Toronto", NOW()).unwrap();
        assert_eq!(title_only[0].description, "");
        assert_eq!(title_only[0].summary, "Dentist");
    }

    #[test]
    fn event_title_and_text_leave_out_date_labels() {
        let content = json!({"blocks":[{"type":"text","runs":[
            {"t":"Dentist "},
            {"type":"dateChip","attrs":{"chipId":"a","date":"2026-11-03","time":"14:00","label":"Nov 3 · 14:00"}},
            {"t":" bring Nov 3 · 14:00 card"}]}],
            "plain":"Dentist Nov 3 · 14:00 bring Nov 3 · 14:00 card
second line"}).to_string();
        assert_eq!(
            without_date_labels(&content, &crate::plain_from_content(&content)),
            "Dentist bring Nov 3 · 14:00 card
second line"
        );
    }
}
