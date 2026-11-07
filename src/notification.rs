//! Versioned, bounded notification activation contract. No OS registration here.
use chrono::{NaiveDate, NaiveTime};

pub const APP_ID: &str = "StickyNotesPlus.Native";
const PREFIX: &str = "sticky-notes-plus://reminder/v1/";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Open,
    Snooze,
    Done,
    Dismiss,
}

impl Action {
    fn name(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Snooze => "snooze",
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
            "snooze" => Action::Snooze,
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
        (Action::Snooze, "Snooze 10m"),
        (Action::Done, "Mark done"),
        (Action::Dismiss, "Dismiss"),
    ] {
        actions.push_str(&format!(
            "<action content=\"{label}\" activationType=\"protocol\" arguments=\"{}\"/>",
            xml(&activation(action))
        ));
    }
    format!("<toast scenario=\"reminder\" activationType=\"protocol\" launch=\"{}\"><visual><binding template=\"ToastGeneric\"><text>{}</text><text>{}</text></binding></visual><actions>{actions}</actions></toast>", xml(&activation(Action::Open)), xml(if title.is_empty() { "Sticky Notes+" } else { title }), xml(if detail.is_empty() { &row.label } else { detail }))
}

/// Installer-owned marker gates native delivery. Development/portable builds
/// never manufacture an identity in the user's registry or Start menu.
#[cfg(windows)]
pub fn show(
    row: &crate::db::reminders::ReminderRow,
    title: &str,
    detail: &str,
) -> anyhow::Result<bool> {
    use anyhow::Context;
    use windows::{
        core::HSTRING,
        Data::Xml::Dom::XmlDocument,
        Win32::System::WinRT::{RoInitialize, RoUninitialize, RO_INIT_MULTITHREADED},
        UI::Notifications::{NotificationSetting, ToastNotification, ToastNotificationManager},
    };
    let marker = std::env::current_exe()?.with_file_name("notification-identity.txt");
    if !marker.is_file() || std::fs::read_to_string(marker)?.trim() != APP_ID {
        return Ok(false);
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
    match notifier.Setting() {
        Ok(NotificationSetting::Enabled) => (),
        Ok(_) => return Ok(false),
        // This query can be unavailable for an otherwise valid unpackaged
        // desktop identity. `Show` is the authoritative delivery attempt.
        Err(error) => eprintln!("Windows notification setting unavailable: {error}"),
    }
    let document = XmlDocument::new()?;
    document.LoadXml(&HSTRING::from(toast_xml(row, title, detail)))?;
    let toast = ToastNotification::CreateToastNotification(&document)?;
    notifier
        .Show(&toast)
        .context("Submit installed reminder toast")?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn activation_roundtrips_legacy_and_unicode_ids() {
        for action in [Action::Open, Action::Snooze, Action::Done, Action::Dismiss] {
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
    fn toast_payload_has_three_protocol_buttons_and_occurrence_scoped_launch() {
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
        assert_eq!(payload.matches("<action content=").count(), 3);
        assert_eq!(payload.matches("activationType=\"protocol\"").count(), 4);
        assert_eq!(payload.matches("2026-10-02T09:00").count(), 4);
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
