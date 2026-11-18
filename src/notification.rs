//! Versioned, bounded notification activation contract. No OS registration here.
use crate::db::reminders::SnoozeFor;
use chrono::{NaiveDate, NaiveTime};

pub const APP_ID: &str = "StickyNotesPlus.Native";
const PREFIX: &str = "sticky-notes-plus://reminder/v1/";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Open,
    Snooze(SnoozeFor),
    Done,
    Dismiss,
}

impl Action {
    fn name(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Snooze(SnoozeFor::TenMinutes) => "snooze",
            Self::Snooze(SnoozeFor::OneHour) => "snooze-hour",
            Self::Snooze(SnoozeFor::Tomorrow) => "snooze-tomorrow",
            Self::Done => "done",
            Self::Dismiss => "dismiss",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Activation {
    pub action: Action,
    pub reminder_id: String,
    pub occurrence: String,
}

impl Activation {
    pub fn uri(&self) -> String {
        let id: String = self
            .reminder_id
            .as_bytes()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        format!("{PREFIX}{}/{id}/{}", self.action.name(), self.occurrence)
    }

    pub fn parse(uri: &str) -> Option<Self> {
        if uri.len() > 4096 {
            return None;
        }
        let mut parts = uri.strip_prefix(PREFIX)?.split('/');
        let action = match parts.next()? {
            "open" => Action::Open,
            "snooze" => Action::Snooze(SnoozeFor::TenMinutes),
            "snooze-hour" => Action::Snooze(SnoozeFor::OneHour),
            "snooze-tomorrow" => Action::Snooze(SnoozeFor::Tomorrow),
            "done" => Action::Done,
            "dismiss" => Action::Dismiss,
            _ => return None,
        };
        let encoded = parts.next()?;
        if encoded.is_empty()
            || encoded.len() > 2048
            || encoded.len() % 2 != 0
            || !encoded
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return None;
        }
        let bytes: Option<Vec<_>> = (0..encoded.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&encoded[i..i + 2], 16).ok())
            .collect();
        let reminder_id = String::from_utf8(bytes?).ok()?;
        if reminder_id.chars().any(char::is_control) {
            return None;
        }
        let occurrence = parts.next()?;
        if parts.next().is_some() {
            return None;
        }
        let (date, time) = occurrence.split_once('T')?;
        if date.len() != 10
            || NaiveDate::parse_from_str(date, "%Y-%m-%d")
                .ok()?
                .format("%Y-%m-%d")
                .to_string()
                != date
        {
            return None;
        }
        if !time.is_empty()
            && (time.len() != 5
                || NaiveTime::parse_from_str(time, "%H:%M")
                    .ok()?
                    .format("%H:%M")
                    .to_string()
                    != time)
        {
            return None;
        }
        Some(Self {
            action,
            reminder_id,
            occurrence: occurrence.into(),
        })
    }
}

fn xml(value: &str) -> String {
    value
        .chars()
        .map(|character| match character {
            '&' => "&amp;".into(),
            '<' => "&lt;".into(),
            '>' => "&gt;".into(),
            '"' => "&quot;".into(),
            '\'' => "&apos;".into(),
            '\t' | '\n' | '\r' => character.to_string(),
            character if character < ' ' || matches!(character, '\u{fffe}' | '\u{ffff}') => {
                "�".into()
            }
            character => character.to_string(),
        })
        .collect()
}

/// `title` names the item; `detail`, its note or date, is the second line.
pub fn toast_xml(row: &crate::db::reminders::ReminderRow, title: &str, detail: &str) -> String {
    let activation = |action| {
        Activation {
            action,
            reminder_id: row.id.clone(),
            occurrence: format!("{}T{}", row.scheduled_date, row.due_time),
        }
        .uri()
    };
    let mut actions = String::new();
    for (action, label) in [
        (Action::Snooze(SnoozeFor::TenMinutes), "10 min"),
        (Action::Snooze(SnoozeFor::OneHour), "1 hour"),
        (Action::Snooze(SnoozeFor::Tomorrow), "Tomorrow"),
        (Action::Done, "Done"),
        (Action::Dismiss, "Dismiss"),
    ] {
        actions.push_str(&format!(
            "<action content=\"{label}\" activationType=\"protocol\" arguments=\"{}\"/>",
            xml(&activation(action))
        ));
    }
    format!("<toast scenario=\"reminder\" activationType=\"protocol\" launch=\"{}\"><visual><binding template=\"ToastGeneric\"><text>{}</text><text>{}</text></binding></visual><actions>{actions}</actions></toast>", xml(&activation(Action::Open)), xml(if title.is_empty() { "Sticky Notes+" } else { title }), xml(if detail.is_empty() { &row.label } else { detail }))
}

/// Runs `use_notifier` with the installed app's toast notifier. `None` when
/// this build has no notification identity: development and portable builds
/// never manufacture one in the user's registry or Start menu.
#[cfg(windows)]
fn with_notifier<T>(
    use_notifier: impl FnOnce(&windows::UI::Notifications::ToastNotifier) -> anyhow::Result<T>,
) -> anyhow::Result<Option<T>> {
    use anyhow::Context;
    use windows::{
        core::HSTRING,
        Win32::System::WinRT::{RoInitialize, RoUninitialize, RO_INIT_MULTITHREADED},
        UI::Notifications::ToastNotificationManager,
    };
    let marker = std::env::current_exe()?.with_file_name("notification-identity.txt");
    if !marker.is_file() || std::fs::read_to_string(marker)?.trim() != APP_ID {
        return Ok(None);
    }
    // An existing STA is valid too; only balance successful initialization.
    let initialized = unsafe { RoInitialize(RO_INIT_MULTITHREADED) };
    if let Err(error) = &initialized {
        if error.code().0 != 0x80010106_u32 as i32 {
            return Err(error.clone().into());
        }
    }
    struct Apartment(bool);
    impl Drop for Apartment {
        fn drop(&mut self) {
            if self.0 {
                unsafe {
                    RoUninitialize();
                }
            }
        }
    }
    let _apartment = Apartment(initialized.is_ok());
    let notifier = ToastNotificationManager::CreateToastNotifierWithId(&HSTRING::from(APP_ID))
        .context("Create installed toast notifier")?;
    use_notifier(&notifier).map(Some)
}

#[cfg(windows)]
fn document(xml: &str) -> anyhow::Result<windows::Data::Xml::Dom::XmlDocument> {
    let document = windows::Data::Xml::Dom::XmlDocument::new()?;
    document.LoadXml(&windows::core::HSTRING::from(xml))?;
    Ok(document)
}

#[cfg(windows)]
pub fn show(
    row: &crate::db::reminders::ReminderRow,
    title: &str,
    detail: &str,
) -> anyhow::Result<bool> {
    use anyhow::Context;
    use windows::UI::Notifications::{NotificationSetting, ToastNotification};
    let shown = with_notifier(|notifier| {
        match notifier.Setting() {
            Ok(NotificationSetting::Enabled) => (),
            Ok(_) => return Ok(false),
            // This query can be unavailable for an otherwise valid unpackaged
            // desktop identity. `Show` is the authoritative delivery attempt.
            Err(error) => eprintln!("Windows notification setting unavailable: {error}"),
        }
        let toast =
            ToastNotification::CreateToastNotification(&document(&toast_xml(row, title, detail))?)?;
        notifier
            .Show(&toast)
            .context("Submit installed reminder toast")?;
        Ok(true)
    })?;
    Ok(shown.unwrap_or(false))
}

/// A notification to hand to Windows ahead of time.
pub struct Planned {
    /// Short stable name; changes when the time or the text changes.
    pub tag: String,
    pub xml: String,
    pub at: chrono::DateTime<chrono::Utc>,
}

impl Planned {
    pub fn new(key: &str, xml: String, at: chrono::DateTime<chrono::Utc>) -> Self {
        // FNV-1a: stable across builds. Windows refuses a scheduled
        // notification id of 16 characters or more, so use 12.
        let mut hash: u64 = 0xcbf29ce484222325;
        for byte in key.bytes().chain([0]).chain(xml.bytes()) {
            hash = (hash ^ u64::from(byte)).wrapping_mul(0x100000001b3);
        }
        Self {
            tag: format!("{:012x}", hash >> 16),
            xml,
            at,
        }
    }
}

/// Makes Windows' schedule match `planned`: notifications no longer wanted are
/// taken off and missing ones added, so they appear even when the app isn't
/// running. Returns which entries are scheduled, or `None` without an
/// installed identity.
#[cfg(windows)]
pub fn sync_schedule(planned: &[Planned]) -> anyhow::Result<Option<Vec<bool>>> {
    use windows::{
        core::HSTRING, Foundation::DateTime, UI::Notifications::ScheduledToastNotification,
    };
    with_notifier(|notifier| {
        let mut present = std::collections::HashSet::new();
        for toast in notifier.GetScheduledToastNotifications()? {
            let id = toast.Id()?.to_string();
            if planned.iter().any(|plan| plan.tag == id) {
                present.insert(id);
            } else {
                notifier.RemoveFromSchedule(&toast)?;
            }
        }
        // Windows refuses a time that has already passed.
        let soonest = chrono::Utc::now() + chrono::Duration::seconds(5);
        let mut scheduled = Vec::with_capacity(planned.len());
        for plan in planned {
            if present.contains(&plan.tag) {
                scheduled.push(true);
                continue;
            }
            if plan.at < soonest {
                scheduled.push(false);
                continue;
            }
            // 100-nanosecond ticks since 1601-01-01 UTC.
            let ticks = (plan.at.timestamp() + 11_644_473_600) * 10_000_000
                + i64::from(plan.at.timestamp_subsec_nanos() / 100);
            let toast = ScheduledToastNotification::CreateScheduledToastNotification(
                &document(&plan.xml)?,
                DateTime {
                    UniversalTime: ticks,
                },
            )?;
            toast.SetId(&HSTRING::from(&plan.tag))?;
            toast.SetTag(&HSTRING::from(&plan.tag))?;
            toast.SetGroup(&HSTRING::from("reminders"))?;
            notifier.AddToSchedule(&toast)?;
            scheduled.push(true);
        }
        Ok(scheduled)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn activation_roundtrips_legacy_and_unicode_ids() {
        for action in [
            Action::Open,
            Action::Snooze(SnoozeFor::TenMinutes),
            Action::Snooze(SnoozeFor::OneHour),
            Action::Snooze(SnoozeFor::Tomorrow),
            Action::Done,
            Action::Dismiss,
        ] {
            for occurrence in ["2026-10-01T", "2026-10-01T17:30"] {
                let value = Activation {
                    action,
                    reminder_id: "note:chip/😀&日本語".into(),
                    occurrence: occurrence.into(),
                };
                assert_eq!(Activation::parse(&value.uri()), Some(value));
            }
        }
    }
    #[test]
    fn activation_rejects_ambiguous_or_malformed_arguments() {
        for tail in [
            "delete/61/2026-10-01T",
            "done/6/2026-10-01T",
            "done/zz/2026-10-01T",
            "done/00/2026-10-01T",
            "done/ff/2026-10-01T",
            "done/61/2026-02-30T",
            "done/61/2026-10-01T24:00",
            "done/61/2026-10-01T/extra",
            "done/61/2026-10-01T?x=1",
        ] {
            assert!(
                Activation::parse(&format!("{PREFIX}{tail}")).is_none(),
                "{tail}"
            );
        }
        assert!(Activation::parse("https://example.com").is_none());
        assert!(Activation::parse(&"x".repeat(5000)).is_none());
    }
    #[test]
    fn xml_escapes_note_text_and_invalid_xml_controls() {
        assert_eq!(xml("<&\"'\0😀"), "&lt;&amp;&quot;&apos;�😀");
    }
    #[test]
    fn toast_payload_has_five_protocol_buttons_and_occurrence_scoped_launch() {
        let row = crate::db::reminders::ReminderRow {
            id: "n:c".into(),
            note_id: "n".into(),
            chip_id: "c".into(),
            due_date: "2026-10-01".into(),
            due_time: "09:00".into(),
            scheduled_date: "2026-10-02".into(),
            recurrence: "daily".into(),
            snoozed_until: None,
            notified_for: None,
            label: "<meeting & café>".into(),
            status: "pending".into(),
            created_at: String::new(),
            modified_at: String::new(),
        };
        let payload = toast_xml(&row, "Note <title>", "");
        assert_eq!(payload.matches("<action content=").count(), 5);
        assert_eq!(payload.matches("activationType=\"protocol\"").count(), 6);
        assert_eq!(payload.matches("2026-10-02T09:00").count(), 6);
        assert!(payload.contains("Note &lt;title&gt;"));
        assert!(payload.contains("&lt;meeting &amp; café&gt;"));
        assert_eq!(
            include_str!("../packaging/notification-identity.txt").trim(),
            APP_ID
        );
        let installer = include_str!("../packaging/installer.wxs");
        assert!(installer.contains(&format!("Value=\"{APP_ID}\"")));
        assert!(installer.contains("--activate &quot;%1&quot;"));
        assert!(
            !installer.contains("ShortcutProperty Key=\"System.AppUserModel.ToastActivatorCLSID\"")
        );
        assert!(!installer.contains("Name=\"CustomActivator\""));
    }
}
