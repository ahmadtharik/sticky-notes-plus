//! Background thread that keeps the Google calendar in step with reminders.
//!
//! Every few seconds it diffs the local reminders against what was pushed
//! (a local query, no network). Only a non-empty plan, a time-zone change or
//! "Sync now" goes online, so every save path is covered without hooks.
//! Network failures back off; a revoked token waits for a reconnect.
use crate::calendar_sync::{self, CalendarConfig};
use crate::db::Db;
use crate::google_calendar::{self, CalendarApi, OAuthClient, SyncError, Transport};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const CHECK_EVERY: Duration = Duration::from_secs(5);
/// Retry waits after network/server failures: 1 min, 5 min, then 15 min.
const BACKOFF: [Duration; 3] = [
    Duration::from_secs(60),
    Duration::from_secs(300),
    Duration::from_secs(900),
];
const SIGN_IN_TIMEOUT: Duration = Duration::from_secs(300);

/// What Settings shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SyncStatus {
    /// Built without a Google client ID.
    NotConfigured,
    NotConnected,
    Connecting,
    Synced(chrono::DateTime<chrono::Local>),
    Syncing,
    Offline,
    ReconnectNeeded,
    Error(String),
}

impl SyncStatus {
    pub fn text(&self) -> String {
        match self {
            Self::NotConfigured => "Google Calendar is not set up in this build.".into(),
            Self::NotConnected => "Not connected.".into(),
            Self::Connecting => "Waiting for Google sign-in in your browser…".into(),
            Self::Synced(at) => format!("Synced at {}.", at.format("%-I:%M %p")),
            Self::Syncing => "Syncing…".into(),
            Self::Offline => "Offline — will retry.".into(),
            Self::ReconnectNeeded => "Google access ended. Connect again to resume.".into(),
            Self::Error(detail) => format!("Sync problem: {detail}"),
        }
    }
}

/// First-connect choices.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConnectOptions {
    pub sync_existing: bool,
    pub alert_minutes: u32,
    pub include_note_text: bool,
}

enum Command {
    SyncNow,
    Connect(ConnectOptions),
    /// Alert time or note-text option changed; the next check re-pushes.
    SettingsChanged,
    Disconnect {
        delete_calendar: bool,
    },
}

#[derive(Clone)]
pub struct CalendarWorker {
    commands: Sender<Command>,
}

impl CalendarWorker {
    /// `on_status` runs on the worker thread; forward it to the UI thread.
    pub fn start(db: Arc<Mutex<Db>>, on_status: impl Fn(SyncStatus) + Send + 'static) -> Self {
        let (commands, receiver) = mpsc::channel();
        std::thread::Builder::new()
            .name("calendar-sync".into())
            .spawn(move || run(db, receiver, on_status))
            .expect("spawn calendar sync thread");
        Self { commands }
    }

    pub fn sync_now(&self) {
        let _ = self.commands.send(Command::SyncNow);
    }

    pub fn connect(&self, options: ConnectOptions) {
        let _ = self.commands.send(Command::Connect(options));
    }

    pub fn settings_changed(&self) {
        let _ = self.commands.send(Command::SettingsChanged);
    }

    pub fn disconnect(&self, delete_calendar: bool) {
        let _ = self.commands.send(Command::Disconnect { delete_calendar });
    }
}

/// Retry scheduling, kept apart from I/O so it can be tested.
#[derive(Debug, Default)]
struct Backoff {
    failures: usize,
    retry_at: Option<Instant>,
}

impl Backoff {
    fn failed(&mut self, now: Instant) {
        let wait = BACKOFF[self.failures.min(BACKOFF.len() - 1)];
        self.failures += 1;
        self.retry_at = Some(now + wait);
    }

    fn succeeded(&mut self) {
        *self = Self::default();
    }

    fn ready(&self, now: Instant) -> bool {
        self.retry_at.is_none_or(|at| now >= at)
    }
}

struct State {
    db: Arc<Mutex<Db>>,
    client: Option<OAuthClient>,
    backoff: Backoff,
    reconnect_needed: bool,
    last_zone: Option<String>,
    status: Option<SyncStatus>,
}

fn run(db: Arc<Mutex<Db>>, receiver: Receiver<Command>, on_status: impl Fn(SyncStatus)) {
    // Windows.Web.Http's blocking waits need a multithreaded apartment.
    let _apartment = Apartment::multithreaded();
    let transport = match google_calendar::WinRtTransport::new() {
        Ok(transport) => transport,
        Err(error) => {
            on_status(SyncStatus::Error(format!(
                "Networking unavailable: {error}"
            )));
            return;
        }
    };
    let mut state = State {
        db,
        client: google_calendar::oauth_client(),
        backoff: Backoff::default(),
        reconnect_needed: false,
        last_zone: None,
        status: None,
    };
    let report = |state: &mut State, status: SyncStatus| {
        if state.status.as_ref() != Some(&status) {
            state.status = Some(status.clone());
            on_status(status);
        }
    };
    let initial = if state.client.is_none() {
        SyncStatus::NotConfigured
    } else if connected(&state.db).is_some() {
        SyncStatus::Syncing
    } else {
        SyncStatus::NotConnected
    };
    report(&mut state, initial);
    // Commands that arrived during a sign-in, kept in order.
    let mut queued = std::collections::VecDeque::new();
    loop {
        let mut forced = false;
        let command = match queued.pop_front() {
            Some(command) => Ok(command),
            None => receiver.recv_timeout(CHECK_EVERY),
        };
        match command {
            Ok(Command::SyncNow) => {
                forced = true;
                state.backoff.succeeded();
            }
            Ok(Command::SettingsChanged) => forced = true,
            Ok(Command::Connect(options)) => {
                report(&mut state, SyncStatus::Connecting);
                let status = match connect(&state, &transport, options) {
                    Ok(()) => {
                        state.reconnect_needed = false;
                        state.backoff.succeeded();
                        forced = true;
                        SyncStatus::Syncing
                    }
                    Err(error) => SyncStatus::Error(error.to_string()),
                };
                report(&mut state, status);
                // Connect clicks made while the browser sign-in was open would
                // each start another sign-in; that one already finished.
                while let Ok(command) = receiver.try_recv() {
                    if !matches!(command, Command::Connect(_)) {
                        queued.push_back(command);
                    }
                }
            }
            Ok(Command::Disconnect { delete_calendar }) => {
                let status = match disconnect(&state, &transport, delete_calendar) {
                    Ok(()) => SyncStatus::NotConnected,
                    Err(error) => SyncStatus::Error(error.to_string()),
                };
                state.reconnect_needed = false;
                state.backoff.succeeded();
                report(&mut state, status);
                continue;
            }
            Err(RecvTimeoutError::Timeout) => (),
            Err(RecvTimeoutError::Disconnected) => return,
        }
        if let Some(status) = check(&mut state, &transport, forced) {
            report(&mut state, status);
        }
    }
}

fn connected(db: &Mutex<Db>) -> Option<(CalendarConfig, String)> {
    let guard = crate::db::lock(db);
    let config = calendar_sync::load_config(&guard.conn).ok()??;
    let token = google_calendar::load_refresh_token(&guard.conn).ok()??;
    Some((config, token))
}

/// One periodic pass. `None` leaves the status unchanged.
fn check(state: &mut State, transport: &dyn Transport, forced: bool) -> Option<SyncStatus> {
    let client = state.client.clone()?;
    if state.reconnect_needed {
        return None;
    }
    let (config, refresh_token) = connected(&state.db)?;
    let now = Instant::now();
    if !forced && !state.backoff.ready(now) {
        return None;
    }
    let zone = google_calendar::local_time_zone().unwrap_or_else(|_| "UTC".into());
    let local_now = chrono::Local::now().naive_local();
    if !forced
        && state.last_zone.as_deref() == Some(zone.as_str())
        && !has_work(&state.db, &config, &zone, local_now)
    {
        return None;
    }
    let api = CalendarApi::new(transport, client, refresh_token);
    match google_calendar::sync_once(&state.db, &api, &zone, local_now) {
        Ok(_) => {
            state.backoff.succeeded();
            state.last_zone = Some(zone);
            Some(SyncStatus::Synced(chrono::Local::now()))
        }
        Err(SyncError::Offline(_)) => {
            state.backoff.failed(now);
            Some(SyncStatus::Offline)
        }
        Err(SyncError::ReconnectNeeded) => {
            state.reconnect_needed = true;
            Some(SyncStatus::ReconnectNeeded)
        }
        Err(error) => {
            state.backoff.failed(now);
            Some(SyncStatus::Error(error.to_string()))
        }
    }
}

fn has_work(
    db: &Mutex<Db>,
    config: &CalendarConfig,
    zone: &str,
    now: chrono::NaiveDateTime,
) -> bool {
    let guard = crate::db::lock(db);
    let (Ok(desired), Ok(pushed)) = (
        calendar_sync::desired_events(&guard.conn, config, zone, now),
        calendar_sync::pushed_events(&guard.conn),
    ) else {
        return false;
    };
    !calendar_sync::plan(desired, pushed).is_empty()
}

/// Browser sign-in with PKCE and a loopback redirect, then create the app's
/// calendar and store the encrypted refresh token and choices.
fn connect(
    state: &State,
    transport: &dyn Transport,
    options: ConnectOptions,
) -> anyhow::Result<()> {
    use std::io::{Read, Write};
    let client = state
        .client
        .clone()
        .ok_or_else(|| anyhow::anyhow!("Google Calendar is not set up in this build"))?;
    let pkce = google_calendar::Pkce::from_random(
        &google_calendar::random_bytes::<32>()?,
        &google_calendar::random_bytes::<16>()?,
        google_calendar::sha256,
    );
    let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
    listener.set_nonblocking(true)?;
    let redirect_uri = format!("http://127.0.0.1:{}", listener.local_addr()?.port());
    google_calendar::open_in_browser(&google_calendar::authorization_url(
        &client,
        &redirect_uri,
        &pkce,
    ))?;
    let deadline = Instant::now() + SIGN_IN_TIMEOUT;
    let code = loop {
        anyhow::ensure!(Instant::now() < deadline, "Google sign-in timed out");
        let (mut stream, _) = match listener.accept() {
            Ok(connection) => connection,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(200));
                continue;
            }
            Err(error) => return Err(error.into()),
        };
        stream.set_nonblocking(false)?;
        stream.set_read_timeout(Some(Duration::from_secs(5)))?;
        let mut buffer = [0u8; 8192];
        let read = stream.read(&mut buffer).unwrap_or(0);
        let request = String::from_utf8_lossy(&buffer[..read]);
        let result =
            google_calendar::parse_redirect(request.lines().next().unwrap_or(""), &pkce.state);
        let (status, message) = match &result {
            Ok(Some(_)) => (
                "200 OK",
                "Sticky Notes+ is connected to Google Calendar. You can close this tab.",
            ),
            Ok(None) => ("404 Not Found", ""),
            Err(_) => (
                "200 OK",
                "Google sign-in did not complete. Return to Sticky Notes+ to try again.",
            ),
        };
        let page = format!("<!doctype html><meta charset=utf-8><title>Sticky Notes+</title><p style=\"font:16px system-ui;margin:40px\">{message}</p>");
        let _ = write!(stream, "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{page}", page.len());
        if let Some(code) = result? {
            break code;
        }
    };
    let tokens =
        google_calendar::exchange_code(transport, &client, &code, &pkce.verifier, &redirect_uri)?;
    let refresh_token = tokens.refresh_token.ok_or_else(|| {
        anyhow::anyhow!("Google did not grant offline access; try connecting again")
    })?;
    // Reconnecting after access ended keeps the existing calendar and what
    // was already pushed; only a first connect creates the calendar.
    let existing = {
        let guard = crate::db::lock(&state.db);
        calendar_sync::load_config(&guard.conn).ok().flatten()
    };
    let config = match existing {
        Some(config) => config,
        None => {
            let zone = google_calendar::local_time_zone().unwrap_or_else(|_| "UTC".into());
            let api = CalendarApi::new(transport, client, refresh_token.clone());
            let calendar_id = match api.create_calendar(&zone) {
                Ok(id) => id,
                Err(error) => {
                    // Nothing was saved, so don't leave Google holding a grant
                    // the app has already forgotten.
                    let _ = api.revoke();
                    return Err(error.into());
                }
            };
            CalendarConfig {
                calendar_id,
                sync_existing: options.sync_existing,
                connected_at: chrono::Utc::now().to_rfc3339(),
                alert_minutes: options.alert_minutes,
                include_note_text: options.include_note_text,
            }
        }
    };
    let guard = crate::db::lock(&state.db);
    let transaction = guard.conn.unchecked_transaction()?;
    calendar_sync::save_config(&transaction, &config)?;
    google_calendar::save_refresh_token(&transaction, &refresh_token)?;
    transaction.commit()?;
    Ok(())
}

/// Revoke and forget locally even when Google is unreachable, so the user is
/// never stuck connected.
fn disconnect(
    state: &State,
    transport: &dyn Transport,
    delete_calendar: bool,
) -> anyhow::Result<()> {
    let mut problem = None;
    if let (Some(client), Some((config, token))) = (state.client.clone(), connected(&state.db)) {
        let api = CalendarApi::new(transport, client, token);
        if delete_calendar {
            if let Err(error) = api.delete_calendar(&config.calendar_id) {
                problem = Some(format!(
                    "The calendar could not be deleted ({error}); remove it in Google Calendar."
                ));
            }
        }
        if let Err(error) = api.revoke() {
            problem.get_or_insert(format!(
                "Disconnected here, but Google couldn't be told ({error}). Remove Sticky Notes Plus at myaccount.google.com/permissions."
            ));
        }
    }
    let guard = crate::db::lock(&state.db);
    let transaction = guard.conn.unchecked_transaction()?;
    calendar_sync::clear(&transaction)?;
    google_calendar::forget_refresh_token(&transaction)?;
    transaction.commit()?;
    match problem {
        Some(problem) => Err(anyhow::anyhow!(problem)),
        None => Ok(()),
    }
}

/// Settings writes that change what Google should show.
pub fn update_options(
    db: &Mutex<Db>,
    alert_minutes: u32,
    include_note_text: bool,
) -> anyhow::Result<()> {
    let guard = db
        .lock()
        .map_err(|_| anyhow::anyhow!("Database lock unavailable"))?;
    let mut config = calendar_sync::load_config(&guard.conn)?
        .ok_or_else(|| anyhow::anyhow!("Google Calendar is not connected"))?;
    config.alert_minutes = alert_minutes;
    config.include_note_text = include_note_text;
    calendar_sync::save_config(&guard.conn, &config)
}

pub(crate) struct Apartment(bool);

impl Apartment {
    pub(crate) fn multithreaded() -> Self {
        use windows::Win32::System::WinRT::{RoInitialize, RO_INIT_MULTITHREADED};
        Self(unsafe { RoInitialize(RO_INIT_MULTITHREADED) }.is_ok())
    }
}

impl Drop for Apartment {
    fn drop(&mut self) {
        if self.0 {
            unsafe { windows::Win32::System::WinRT::RoUninitialize() };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_waits_one_five_then_fifteen_minutes_and_resets() {
        let start = Instant::now();
        let mut backoff = Backoff::default();
        assert!(backoff.ready(start));
        for (failure, minutes) in [(1, 1), (2, 5), (3, 15), (4, 15)] {
            backoff.failed(start);
            assert_eq!(backoff.failures, failure);
            assert!(!backoff.ready(start + Duration::from_secs(minutes * 60 - 1)));
            assert!(backoff.ready(start + Duration::from_secs(minutes * 60)));
        }
        backoff.succeeded();
        assert!(backoff.ready(start));
    }

    #[test]
    fn status_text_is_plain() {
        assert_eq!(SyncStatus::Offline.text(), "Offline — will retry.");
        assert_eq!(SyncStatus::NotConnected.text(), "Not connected.");
        let at = chrono::Local
            .with_ymd_and_hms(2026, 10, 27, 14, 5, 0)
            .unwrap();
        assert_eq!(SyncStatus::Synced(at).text(), "Synced at 2:05 PM.");
    }

    use chrono::TimeZone;
}
