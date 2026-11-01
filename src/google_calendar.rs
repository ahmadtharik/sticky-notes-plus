//! Google sign-in and Calendar API calls for the one-way reminder push.
//!
//! Everything here uses what Windows already ships: Windows.Web.Http for
//! HTTPS, BCrypt for PKCE randomness/hashing, DPAPI for the stored refresh
//! token and the shell for opening the browser. Requests go through
//! [`Transport`] so the sync logic is tested without a network.
use crate::calendar_sync::{self, CalendarConfig, SyncOp};
use anyhow::{anyhow, Context};
use rusqlite::{Connection, OptionalExtension};
use serde_json::{json, Value};
use std::cell::RefCell;
use std::time::{Duration, Instant};

pub const SCOPE: &str = "https://www.googleapis.com/auth/calendar.app.created";
pub const CALENDAR_NAME: &str = "Sticky Notes";
const AUTH_URL: &str = "https://accounts.google.com/o/oauth2/v2/auth";
const TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
const REVOKE_URL: &str = "https://oauth2.googleapis.com/revoke";
const API: &str = "https://www.googleapis.com/calendar/v3";

/// Why a sync stopped. The worker maps these to retry/backoff and status text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SyncError {
    /// No network, timeout, 5xx or rate limit: retry later.
    Offline(String),
    /// Refresh token revoked or expired: the user must connect again.
    ReconnectNeeded,
    /// The app's calendar was deleted in Google.
    CalendarMissing,
    Other(String),
}

impl std::fmt::Display for SyncError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Offline(detail) => write!(f, "Offline — will retry ({detail})"),
            Self::ReconnectNeeded => f.write_str("Reconnect needed"),
            Self::CalendarMissing => f.write_str("The Sticky Notes calendar was removed"),
            Self::Other(detail) => f.write_str(detail),
        }
    }
}

impl std::error::Error for SyncError {}

pub struct HttpRequest {
    pub method: &'static str,
    pub url: String,
    pub bearer: Option<String>,
    /// Body and its media type.
    pub body: Option<(String, &'static str)>,
}

pub struct HttpResponse {
    pub status: u16,
    pub body: String,
}

pub trait Transport {
    /// Err only when no HTTP response arrived (offline, DNS, TLS, timeout).
    fn send(&self, request: HttpRequest) -> anyhow::Result<HttpResponse>;
}

// ---------------------------------------------------------------- encoding

pub fn percent_encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'%' if index + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[index + 1..index + 3]).ok();
                match hex.and_then(|hex| u8::from_str_radix(hex, 16).ok()) {
                    Some(byte) => {
                        out.push(byte);
                        index += 3;
                        continue;
                    }
                    None => out.push(b'%'),
                }
            }
            b'+' => out.push(b' '),
            byte => out.push(byte),
        }
        index += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

pub fn form(pairs: &[(&str, &str)]) -> String {
    pairs
        .iter()
        .map(|(key, value)| format!("{}={}", percent_encode(key), percent_encode(value)))
        .collect::<Vec<_>>()
        .join("&")
}

pub fn parse_query(query: &str) -> Vec<(String, String)> {
    query
        .split('&')
        .filter(|pair| !pair.is_empty())
        .map(|pair| {
            let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
            (percent_decode(key), percent_decode(value))
        })
        .collect()
}

pub fn base64url(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let value = (u32::from(chunk[0]) << 16)
            | (u32::from(*chunk.get(1).unwrap_or(&0)) << 8)
            | u32::from(*chunk.get(2).unwrap_or(&0));
        for position in 0..=chunk.len() {
            out.push(TABLE[((value >> (18 - 6 * position)) & 63) as usize] as char);
        }
    }
    out
}

fn base64url_decode(text: &str) -> anyhow::Result<Vec<u8>> {
    let mut out = Vec::with_capacity(text.len() * 3 / 4);
    let mut buffer = 0u32;
    let mut bits = 0;
    for byte in text.bytes() {
        let value = match byte {
            b'A'..=b'Z' => byte - b'A',
            b'a'..=b'z' => byte - b'a' + 26,
            b'0'..=b'9' => byte - b'0' + 52,
            b'-' => 62,
            b'_' => 63,
            _ => return Err(anyhow!("Invalid stored token encoding")),
        };
        buffer = (buffer << 6) | u32::from(value);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buffer >> bits) as u8);
        }
    }
    Ok(out)
}

// ------------------------------------------------------------ OAuth client

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OAuthClient {
    pub client_id: String,
    /// Google issues one for Desktop clients; it is not treated as secret.
    pub client_secret: String,
}

/// Built in with `STICKY_NOTES_GOOGLE_CLIENT_ID`/`_SECRET` at compile time,
/// or set at run time for development. `None` means sync is not configured.
pub fn oauth_client() -> Option<OAuthClient> {
    let read = |name: &str, built: Option<&'static str>| {
        std::env::var(name)
            .ok()
            .or_else(|| built.map(str::to_owned))
            .filter(|value| !value.trim().is_empty())
    };
    Some(OAuthClient {
        client_id: read(
            "STICKY_NOTES_GOOGLE_CLIENT_ID",
            option_env!("STICKY_NOTES_GOOGLE_CLIENT_ID"),
        )?,
        client_secret: read(
            "STICKY_NOTES_GOOGLE_CLIENT_SECRET",
            option_env!("STICKY_NOTES_GOOGLE_CLIENT_SECRET"),
        )
        .unwrap_or_default(),
    })
}

/// PKCE values for one sign-in attempt.
pub struct Pkce {
    pub verifier: String,
    pub challenge: String,
    pub state: String,
}

impl Pkce {
    pub fn from_random(
        verifier: &[u8; 32],
        state: &[u8; 16],
        sha256: impl Fn(&[u8]) -> [u8; 32],
    ) -> Self {
        let verifier = base64url(verifier);
        Self {
            challenge: base64url(&sha256(verifier.as_bytes())),
            verifier,
            state: base64url(state),
        }
    }
}

pub fn authorization_url(client: &OAuthClient, redirect_uri: &str, pkce: &Pkce) -> String {
    format!(
        "{AUTH_URL}?{}",
        form(&[
            ("client_id", &client.client_id),
            ("redirect_uri", redirect_uri),
            ("response_type", "code"),
            ("scope", SCOPE),
            ("code_challenge", &pkce.challenge),
            ("code_challenge_method", "S256"),
            ("state", &pkce.state),
            ("access_type", "offline"),
            ("prompt", "consent"),
        ])
    )
}

/// Reads the browser's redirect request line. `Ok(None)` is a request for
/// another path (such as /favicon.ico) that the listener should ignore.
pub fn parse_redirect(request_line: &str, expected_state: &str) -> anyhow::Result<Option<String>> {
    let target = request_line
        .split_whitespace()
        .nth(1)
        .ok_or_else(|| anyhow!("Malformed sign-in redirect"))?;
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    if path != "/" {
        return Ok(None);
    }
    let params = parse_query(query);
    let get = |name: &str| {
        params
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    };
    if let Some(error) = get("error") {
        return Err(anyhow!(if error == "access_denied" {
            "Google sign-in was cancelled".to_owned()
        } else {
            format!("Google sign-in failed: {error}")
        }));
    }
    anyhow::ensure!(
        get("state") == Some(expected_state),
        "Sign-in response did not match this request"
    );
    get("code")
        .map(|code| Some(code.to_owned()))
        .ok_or_else(|| anyhow!("Google did not return an authorization code"))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tokens {
    pub access_token: String,
    pub expires_in: u64,
    pub refresh_token: Option<String>,
}

fn token_error(response: &HttpResponse) -> SyncError {
    let error = serde_json::from_str::<Value>(&response.body)
        .ok()
        .and_then(|value| value["error"].as_str().map(str::to_owned))
        .unwrap_or_default();
    match (response.status, error.as_str()) {
        (_, "invalid_grant") | (401, _) => SyncError::ReconnectNeeded,
        (429 | 500..=599, _) => SyncError::Offline(format!("HTTP {}", response.status)),
        (status, _) => SyncError::Other(format!(
            "Google token request failed (HTTP {status} {error})"
        )),
    }
}

fn parse_tokens(response: &HttpResponse) -> Result<Tokens, SyncError> {
    if response.status != 200 {
        return Err(token_error(response));
    }
    let value: Value = serde_json::from_str(&response.body)
        .map_err(|error| SyncError::Other(format!("Unreadable token response: {error}")))?;
    Ok(Tokens {
        access_token: value["access_token"]
            .as_str()
            .ok_or_else(|| SyncError::Other("Token response had no access token".into()))?
            .to_owned(),
        expires_in: value["expires_in"].as_u64().unwrap_or(3600),
        refresh_token: value["refresh_token"].as_str().map(str::to_owned),
    })
}

fn send(transport: &dyn Transport, request: HttpRequest) -> Result<HttpResponse, SyncError> {
    transport
        .send(request)
        .map_err(|error| SyncError::Offline(error.to_string()))
}

pub fn exchange_code(
    transport: &dyn Transport,
    client: &OAuthClient,
    code: &str,
    verifier: &str,
    redirect_uri: &str,
) -> Result<Tokens, SyncError> {
    let body = form(&[
        ("code", code),
        ("client_id", &client.client_id),
        ("client_secret", &client.client_secret),
        ("redirect_uri", redirect_uri),
        ("grant_type", "authorization_code"),
        ("code_verifier", verifier),
    ]);
    parse_tokens(&send(
        transport,
        HttpRequest {
            method: "POST",
            url: TOKEN_URL.into(),
            bearer: None,
            body: Some((body, "application/x-www-form-urlencoded")),
        },
    )?)
}

// ------------------------------------------------------------- Calendar API

/// An authorized Calendar client. Access tokens live only in memory and are
/// refreshed on expiry or a 401.
pub struct CalendarApi<'a> {
    transport: &'a dyn Transport,
    client: OAuthClient,
    refresh_token: String,
    access: RefCell<Option<(String, Instant)>>,
}

impl<'a> CalendarApi<'a> {
    pub fn new(transport: &'a dyn Transport, client: OAuthClient, refresh_token: String) -> Self {
        Self {
            transport,
            client,
            refresh_token,
            access: RefCell::new(None),
        }
    }

    fn refresh(&self) -> Result<String, SyncError> {
        let body = form(&[
            ("client_id", &self.client.client_id),
            ("client_secret", &self.client.client_secret),
            ("refresh_token", &self.refresh_token),
            ("grant_type", "refresh_token"),
        ]);
        let tokens = parse_tokens(&send(
            self.transport,
            HttpRequest {
                method: "POST",
                url: TOKEN_URL.into(),
                bearer: None,
                body: Some((body, "application/x-www-form-urlencoded")),
            },
        )?)?;
        // Refresh a minute early so a request never races the expiry.
        let lifetime = Duration::from_secs(tokens.expires_in.saturating_sub(60));
        *self.access.borrow_mut() = Some((tokens.access_token.clone(), Instant::now() + lifetime));
        Ok(tokens.access_token)
    }

    fn token(&self) -> Result<String, SyncError> {
        match &*self.access.borrow() {
            Some((token, until)) if Instant::now() < *until => return Ok(token.clone()),
            _ => (),
        }
        self.refresh()
    }

    /// One API call, retried once with a fresh token after a 401.
    fn call(
        &self,
        method: &'static str,
        url: &str,
        body: Option<&Value>,
    ) -> Result<HttpResponse, SyncError> {
        let request = |token: String| HttpRequest {
            method,
            url: url.to_owned(),
            bearer: Some(token),
            body: body.map(|value| (value.to_string(), "application/json")),
        };
        let response = send(self.transport, request(self.token()?))?;
        let response = if response.status == 401 {
            send(self.transport, request(self.refresh()?))?
        } else {
            response
        };
        match response.status {
            401 => Err(SyncError::ReconnectNeeded),
            429 | 500..=599 => Err(SyncError::Offline(format!("HTTP {}", response.status))),
            403 if response.body.contains("RateLimitExceeded")
                || response.body.contains("rateLimitExceeded") =>
            {
                Err(SyncError::Offline("rate limited".into()))
            }
            _ => Ok(response),
        }
    }

    fn id_from(response: HttpResponse, what: &str) -> Result<String, SyncError> {
        if !(200..300).contains(&response.status) {
            return Err(SyncError::Other(format!(
                "Could not {what} (HTTP {})",
                response.status
            )));
        }
        serde_json::from_str::<Value>(&response.body)
            .ok()
            .and_then(|value| value["id"].as_str().map(str::to_owned))
            .ok_or_else(|| SyncError::Other(format!("Could not {what}: no id in response")))
    }

    pub fn create_calendar(&self, time_zone: &str) -> Result<String, SyncError> {
        let body = json!({"summary": CALENDAR_NAME, "timeZone": time_zone});
        Self::id_from(
            self.call("POST", &format!("{API}/calendars"), Some(&body))?,
            "create the calendar",
        )
    }

    pub fn delete_calendar(&self, calendar_id: &str) -> Result<(), SyncError> {
        let response = self.call(
            "DELETE",
            &format!("{API}/calendars/{}", percent_encode(calendar_id)),
            None,
        )?;
        match response.status {
            200..=299 | 404 | 410 => Ok(()),
            status => Err(SyncError::Other(format!(
                "Could not delete the calendar (HTTP {status})"
            ))),
        }
    }

    pub fn insert_event(&self, calendar_id: &str, event: &Value) -> Result<String, SyncError> {
        let response = self.call(
            "POST",
            &format!("{API}/calendars/{}/events", percent_encode(calendar_id)),
            Some(event),
        )?;
        if response.status == 404 {
            return Err(SyncError::CalendarMissing);
        }
        Self::id_from(response, "add the event")
    }

    /// Full replace (PUT), so removing a repeat rule also reaches Google.
    /// `Ok(None)`: the event was deleted in Google and should be re-added.
    pub fn update_event(
        &self,
        calendar_id: &str,
        event_id: &str,
        event: &Value,
    ) -> Result<Option<String>, SyncError> {
        let response = self.call(
            "PUT",
            &format!(
                "{API}/calendars/{}/events/{}",
                percent_encode(calendar_id),
                percent_encode(event_id)
            ),
            Some(event),
        )?;
        if matches!(response.status, 404 | 410) {
            return Ok(None);
        }
        Self::id_from(response, "update the event").map(Some)
    }

    pub fn delete_event(&self, calendar_id: &str, event_id: &str) -> Result<(), SyncError> {
        let response = self.call(
            "DELETE",
            &format!(
                "{API}/calendars/{}/events/{}",
                percent_encode(calendar_id),
                percent_encode(event_id)
            ),
            None,
        )?;
        match response.status {
            200..=299 | 404 | 410 => Ok(()),
            status => Err(SyncError::Other(format!(
                "Could not remove the event (HTTP {status})"
            ))),
        }
    }

    /// Best effort: a failure still lets the local disconnect proceed.
    pub fn revoke(&self) -> Result<(), SyncError> {
        let response = send(
            self.transport,
            HttpRequest {
                method: "POST",
                url: REVOKE_URL.into(),
                bearer: None,
                body: Some((
                    form(&[("token", &self.refresh_token)]),
                    "application/x-www-form-urlencoded",
                )),
            },
        )?;
        match response.status {
            200 | 400 => Ok(()),
            status => Err(SyncError::Other(format!(
                "Could not revoke access (HTTP {status})"
            ))),
        }
    }
}

// ------------------------------------------------------------------- sync

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct SyncSummary {
    pub created: usize,
    pub updated: usize,
    pub deleted: usize,
    pub calendar_recreated: bool,
}

/// One pass: plan from the database, apply each operation, record each
/// success immediately so an interruption never repeats finished work.
/// The database lock is held only for reads and writes, never during a call.
pub fn sync_once(
    db: &std::sync::Mutex<crate::db::Db>,
    api: &CalendarApi<'_>,
    time_zone: &str,
    now: chrono::NaiveDateTime,
) -> Result<SyncSummary, SyncError> {
    let mut summary = SyncSummary::default();
    for attempt in 0..2 {
        let lock = || Ok::<_, SyncError>(crate::db::lock(db));
        let (config, operations) = {
            let guard = lock()?;
            let config = calendar_sync::load_config(&guard.conn)
                .map_err(|error| SyncError::Other(error.to_string()))?
                .ok_or_else(|| SyncError::Other("Google Calendar is not connected".into()))?;
            let desired = calendar_sync::desired_events(&guard.conn, &config, time_zone, now)
                .map_err(|error| SyncError::Other(error.to_string()))?;
            let pushed = calendar_sync::pushed_events(&guard.conn)
                .map_err(|error| SyncError::Other(error.to_string()))?;
            (config, calendar_sync::plan(desired, pushed))
        };
        match apply(db, api, &config, operations, &mut summary) {
            Err(SyncError::CalendarMissing) if attempt == 0 => {
                let calendar_id = api.create_calendar(time_zone)?;
                let guard = lock()?;
                let transaction = guard
                    .conn
                    .unchecked_transaction()
                    .map_err(|error| SyncError::Other(error.to_string()))?;
                calendar_sync::save_config(
                    &transaction,
                    &CalendarConfig {
                        calendar_id,
                        ..config
                    },
                )
                .map_err(|error| SyncError::Other(error.to_string()))?;
                transaction
                    .execute("DELETE FROM calendar_events", [])
                    .map_err(|error| SyncError::Other(error.to_string()))?;
                transaction
                    .commit()
                    .map_err(|error| SyncError::Other(error.to_string()))?;
                summary.calendar_recreated = true;
            }
            result => return result.map(|()| summary),
        }
    }
    Err(SyncError::CalendarMissing)
}

fn apply(
    db: &std::sync::Mutex<crate::db::Db>,
    api: &CalendarApi<'_>,
    config: &CalendarConfig,
    operations: Vec<SyncOp>,
    summary: &mut SyncSummary,
) -> Result<(), SyncError> {
    let record =
        |result: rusqlite::Result<()>| result.map_err(|error| SyncError::Other(error.to_string()));
    let lock = || Ok::<_, SyncError>(crate::db::lock(db));
    for operation in operations {
        match operation {
            SyncOp::Create(spec) => {
                let event_id = api.insert_event(&config.calendar_id, &spec.to_google())?;
                record(calendar_sync::record_pushed(
                    &lock()?.conn,
                    &spec.reminder_id,
                    &event_id,
                    &spec.fingerprint(),
                ))?;
                summary.created += 1;
            }
            SyncOp::Update { event_id, spec } => {
                let event = spec.to_google();
                let event_id = match api.update_event(&config.calendar_id, &event_id, &event)? {
                    Some(event_id) => event_id,
                    None => api.insert_event(&config.calendar_id, &event)?,
                };
                record(calendar_sync::record_pushed(
                    &lock()?.conn,
                    &spec.reminder_id,
                    &event_id,
                    &spec.fingerprint(),
                ))?;
                summary.updated += 1;
            }
            SyncOp::Delete {
                reminder_id,
                event_id,
            } => {
                api.delete_event(&config.calendar_id, &event_id)?;
                record(calendar_sync::forget_pushed(&lock()?.conn, &reminder_id))?;
                summary.deleted += 1;
            }
        }
    }
    Ok(())
}

// --------------------------------------------------------- token storage

#[cfg(windows)]
/// The refresh token is stored DPAPI-encrypted for the current Windows user.
pub fn save_refresh_token(conn: &Connection, token: &str) -> anyhow::Result<()> {
    let sealed = base64url(&windows_impl::protect(token.as_bytes())?);
    conn.execute(
        "INSERT INTO meta(key,value) VALUES('google_refresh_token',?1) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
        [sealed],
    )?;
    Ok(())
}

#[cfg(windows)]
pub fn load_refresh_token(conn: &Connection) -> anyhow::Result<Option<String>> {
    let sealed: Option<String> = conn
        .query_row(
            "SELECT value FROM meta WHERE key='google_refresh_token'",
            [],
            |row| row.get(0),
        )
        .optional()?;
    let Some(sealed) = sealed else {
        return Ok(None);
    };
    let plain = windows_impl::unprotect(&base64url_decode(&sealed)?)?;
    Ok(Some(
        String::from_utf8(plain).context("Stored token is not text")?,
    ))
}

pub fn forget_refresh_token(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute("DELETE FROM meta WHERE key='google_refresh_token'", [])?;
    Ok(())
}

// ------------------------------------------------------- Windows plumbing

#[cfg(windows)]
pub use windows_impl::{local_time_zone, open_in_browser, random_bytes, sha256, WinRtTransport};

#[cfg(windows)]
mod windows_impl {
    use super::{HttpRequest, HttpResponse, Transport};
    use anyhow::{anyhow, Context};
    use windows::core::HSTRING;

    pub fn random_bytes<const N: usize>() -> anyhow::Result<[u8; N]> {
        use windows_sys::Win32::Security::Cryptography::{
            BCryptGenRandom, BCRYPT_USE_SYSTEM_PREFERRED_RNG,
        };
        let mut bytes = [0u8; N];
        let status = unsafe {
            BCryptGenRandom(
                std::ptr::null_mut(),
                bytes.as_mut_ptr(),
                N as u32,
                BCRYPT_USE_SYSTEM_PREFERRED_RNG,
            )
        };
        anyhow::ensure!(status == 0, "Secure random generation failed ({status:#x})");
        Ok(bytes)
    }

    pub fn sha256(input: &[u8]) -> [u8; 32] {
        use windows_sys::Win32::Security::Cryptography::{BCryptHash, BCRYPT_SHA256_ALG_HANDLE};
        let mut digest = [0u8; 32];
        let status = unsafe {
            BCryptHash(
                BCRYPT_SHA256_ALG_HANDLE,
                std::ptr::null(),
                0,
                input.as_ptr(),
                input.len() as u32,
                digest.as_mut_ptr(),
                32,
            )
        };
        assert_eq!(
            status, 0,
            "SHA-256 is always available on supported Windows"
        );
        digest
    }

    fn dpapi(input: &[u8], seal: bool) -> anyhow::Result<Vec<u8>> {
        use windows_sys::Win32::Foundation::LocalFree;
        use windows_sys::Win32::Security::Cryptography::{
            CryptProtectData, CryptUnprotectData, CRYPTPROTECT_UI_FORBIDDEN, CRYPT_INTEGER_BLOB,
        };
        let data_in = CRYPT_INTEGER_BLOB {
            cbData: input.len() as u32,
            pbData: input.as_ptr() as *mut u8,
        };
        let mut data_out = CRYPT_INTEGER_BLOB::default();
        let ok = unsafe {
            if seal {
                CryptProtectData(
                    &data_in,
                    std::ptr::null(),
                    std::ptr::null(),
                    std::ptr::null(),
                    std::ptr::null(),
                    CRYPTPROTECT_UI_FORBIDDEN,
                    &mut data_out,
                )
            } else {
                CryptUnprotectData(
                    &data_in,
                    std::ptr::null_mut(),
                    std::ptr::null(),
                    std::ptr::null(),
                    std::ptr::null(),
                    CRYPTPROTECT_UI_FORBIDDEN,
                    &mut data_out,
                )
            }
        };
        if ok == 0 {
            return Err(anyhow!(std::io::Error::last_os_error()).context(if seal {
                "Could not encrypt the Google token"
            } else {
                "Could not decrypt the Google token (signed in as another Windows user?)"
            }));
        }
        let bytes =
            unsafe { std::slice::from_raw_parts(data_out.pbData, data_out.cbData as usize) }
                .to_vec();
        unsafe { LocalFree(data_out.pbData as _) };
        Ok(bytes)
    }

    pub fn protect(input: &[u8]) -> anyhow::Result<Vec<u8>> {
        dpapi(input, true)
    }

    pub fn unprotect(input: &[u8]) -> anyhow::Result<Vec<u8>> {
        dpapi(input, false)
    }

    /// Opens the user's default browser; only ever called with our own
    /// accounts.google.com authorization URL.
    pub fn open_in_browser(url: &str) -> anyhow::Result<()> {
        use windows_sys::Win32::UI::Shell::ShellExecuteW;
        use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
        let wide = |text: &str| text.encode_utf16().chain(Some(0)).collect::<Vec<_>>();
        let (verb, target) = (wide("open"), wide(url));
        let result = unsafe {
            ShellExecuteW(
                std::ptr::null_mut(),
                verb.as_ptr(),
                target.as_ptr(),
                std::ptr::null(),
                std::ptr::null(),
                SW_SHOWNORMAL,
            )
        };
        anyhow::ensure!(result as usize > 32, "Could not open the browser");
        Ok(())
    }

    /// The PC's IANA zone (for example "America/Toronto"). Call on a thread
    /// with WinRT initialized.
    pub fn local_time_zone() -> anyhow::Result<String> {
        Ok(windows::Globalization::Calendar::new()?
            .GetTimeZone()?
            .to_string())
    }

    /// Blocking Windows.Web.Http client. Use from a background thread with
    /// WinRT initialized (multithreaded), never from the UI thread.
    pub struct WinRtTransport {
        client: windows::Web::Http::HttpClient,
    }

    impl WinRtTransport {
        pub fn new() -> anyhow::Result<Self> {
            use windows::Web::Http::Filters::{HttpBaseProtocolFilter, HttpCacheReadBehavior};
            let filter = HttpBaseProtocolFilter::new()?;
            filter.SetAllowUI(false)?;
            filter
                .CacheControl()?
                .SetReadBehavior(HttpCacheReadBehavior::NoCache)?;
            let client = windows::Web::Http::HttpClient::Create(&filter)?;
            // GitHub rejects requests without a User-Agent.
            client
                .DefaultRequestHeaders()?
                .UserAgent()?
                .TryParseAdd(&HSTRING::from("StickyNotesPlus"))?;
            Ok(Self { client })
        }

        /// Fetches a binary file, following redirects.
        pub fn download(&self, url: &str) -> anyhow::Result<Vec<u8>> {
            let uri = windows::Foundation::Uri::CreateUri(&HSTRING::from(url))?;
            let response = self
                .client
                .GetAsync(&uri)?
                .get()
                .context("No response from the server")?;
            let status = response.StatusCode()?.0;
            anyhow::ensure!(status == 200, "The server answered {status}");
            let buffer = response.Content()?.ReadAsBufferAsync()?.get()?;
            let mut bytes = vec![0u8; buffer.Length()? as usize];
            windows::Storage::Streams::DataReader::FromBuffer(&buffer)?.ReadBytes(&mut bytes)?;
            Ok(bytes)
        }
    }

    impl Transport for WinRtTransport {
        fn send(&self, request: HttpRequest) -> anyhow::Result<HttpResponse> {
            use windows::Foundation::Uri;
            use windows::Storage::Streams::UnicodeEncoding;
            use windows::Web::Http::Headers::HttpCredentialsHeaderValue;
            use windows::Web::Http::{HttpMethod, HttpRequestMessage, HttpStringContent};
            let method = match request.method {
                "GET" => HttpMethod::Get()?,
                "POST" => HttpMethod::Post()?,
                "PUT" => HttpMethod::Put()?,
                "PATCH" => HttpMethod::Patch()?,
                "DELETE" => HttpMethod::Delete()?,
                other => return Err(anyhow!("Unsupported HTTP method {other}")),
            };
            let message = HttpRequestMessage::Create(
                &method,
                &Uri::CreateUri(&HSTRING::from(request.url.as_str()))?,
            )?;
            if let Some(token) = &request.bearer {
                message.Headers()?.SetAuthorization(
                    &HttpCredentialsHeaderValue::CreateFromSchemeWithToken(
                        &HSTRING::from("Bearer"),
                        &HSTRING::from(token.as_str()),
                    )?,
                )?;
            }
            if let Some((body, media_type)) = &request.body {
                message.SetContent(
                    &HttpStringContent::CreateFromStringWithEncodingAndMediaType(
                        &HSTRING::from(body.as_str()),
                        UnicodeEncoding::Utf8,
                        &HSTRING::from(*media_type),
                    )?,
                )?;
            }
            let response = self
                .client
                .SendRequestAsync(&message)?
                .get()
                .context("No response from the server")?;
            let status = response.StatusCode()?.0 as u16;
            let body = response.Content()?.ReadAsStringAsync()?.get()?.to_string();
            Ok(HttpResponse { status, body })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{notes, Db};
    use std::sync::Mutex;

    /// Scripted Google: records every request and answers from a queue.
    struct FakeGoogle {
        requests: RefCell<Vec<(String, String, Option<String>)>>,
        responses: RefCell<std::collections::VecDeque<(u16, String)>>,
    }

    impl FakeGoogle {
        fn new(responses: &[(u16, &str)]) -> Self {
            Self {
                requests: RefCell::new(Vec::new()),
                responses: RefCell::new(
                    responses
                        .iter()
                        .map(|(status, body)| (*status, body.to_string()))
                        .collect(),
                ),
            }
        }
        fn calls(&self) -> Vec<String> {
            self.requests
                .borrow()
                .iter()
                .map(|(method, url, _)| {
                    format!(
                        "{method} {}",
                        url.replace(API, "").replace(TOKEN_URL, "token")
                    )
                })
                .collect()
        }
    }

    impl Transport for FakeGoogle {
        fn send(&self, request: HttpRequest) -> anyhow::Result<HttpResponse> {
            self.requests.borrow_mut().push((
                request.method.into(),
                request.url,
                request.body.map(|body| body.0),
            ));
            let (status, body) = self
                .responses
                .borrow_mut()
                .pop_front()
                .ok_or_else(|| anyhow!("offline"))?;
            Ok(HttpResponse { status, body })
        }
    }

    const TOKEN: &str = r#"{"access_token":"a1","expires_in":3600}"#;

    fn client() -> OAuthClient {
        OAuthClient {
            client_id: "id.apps.googleusercontent.com".into(),
            client_secret: "s".into(),
        }
    }

    fn connected_db() -> Mutex<Db> {
        let path = std::env::temp_dir().join(format!("slint-google-test-{}", uuid::Uuid::new_v4()));
        let db = Db::open_in_dir(&path).unwrap();
        calendar_sync::save_config(
            &db.conn,
            &CalendarConfig {
                calendar_id: "cal@group".into(),
                sync_existing: true,
                connected_at: "2000-01-01T00:00:00Z".into(),
                alert_minutes: 10,
                include_note_text: true,
            },
        )
        .unwrap();
        let content = json!({"blocks":[{"type":"text","runs":[{"type":"text","text":"Dentist "},{"type":"dateChip","attrs":{"chipId":"a","date":"2026-11-03","time":"14:00"}}]}],"plain":"Dentist"}).to_string();
        notes::create(&db.conn, "n", &content, "#FFF9B1").unwrap();
        Mutex::new(db)
    }

    fn now() -> chrono::NaiveDateTime {
        chrono::NaiveDate::from_ymd_opt(2026, 10, 27)
            .unwrap()
            .and_hms_opt(12, 0, 0)
            .unwrap()
    }

    #[test]
    fn encodings_match_their_standards() {
        // RFC 4648 test vectors, unpadded URL-safe alphabet.
        for (input, expected) in [
            ("", ""),
            ("f", "Zg"),
            ("fo", "Zm8"),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg"),
            ("fooba", "Zm9vYmE"),
            ("foobar", "Zm9vYmFy"),
        ] {
            assert_eq!(base64url(input.as_bytes()), expected);
            assert_eq!(base64url_decode(expected).unwrap(), input.as_bytes());
        }
        assert_eq!(base64url(&[0xfb, 0xff]), "-_8");
        assert_eq!(percent_encode("a b&c=d/é~"), "a%20b%26c%3Dd%2F%C3%A9~");
        assert_eq!(
            parse_query("code=4%2F0Ab&state=x+y&e"),
            vec![
                ("code".into(), "4/0Ab".into()),
                ("state".into(), "x y".into()),
                ("e".into(), "".into())
            ]
        );
    }

    #[test]
    fn pkce_uses_rfc7636_s256() {
        // RFC 7636 appendix B.
        let verifier: [u8; 32] = [
            116, 24, 223, 180, 151, 153, 224, 37, 79, 250, 96, 125, 216, 173, 187, 186, 22, 212,
            37, 77, 105, 214, 191, 240, 91, 88, 5, 88, 83, 132, 141, 121,
        ];
        let pkce = Pkce::from_random(&verifier, &[0; 16], sha256);
        assert_eq!(pkce.verifier, "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk");
        assert_eq!(
            pkce.challenge,
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
        let url = authorization_url(&client(), "http://127.0.0.1:5000", &pkce);
        assert!(
            url.contains("scope=https%3A%2F%2Fwww.googleapis.com%2Fauth%2Fcalendar.app.created")
        );
        assert!(url.contains("code_challenge_method=S256") && url.contains("access_type=offline"));
    }

    #[test]
    fn redirect_parsing_checks_state_and_reports_cancel() {
        assert_eq!(
            parse_redirect("GET /?state=s&code=c HTTP/1.1", "s").unwrap(),
            Some("c".into())
        );
        assert_eq!(
            parse_redirect("GET /favicon.ico HTTP/1.1", "s").unwrap(),
            None
        );
        assert!(parse_redirect("GET /?state=other&code=c HTTP/1.1", "s").is_err());
        let cancelled =
            parse_redirect("GET /?error=access_denied&state=s HTTP/1.1", "s").unwrap_err();
        assert_eq!(cancelled.to_string(), "Google sign-in was cancelled");
    }

    #[test]
    fn code_exchange_returns_refresh_token_and_maps_errors() {
        let google = FakeGoogle::new(&[
            (
                200,
                r#"{"access_token":"a","expires_in":3599,"refresh_token":"r"}"#,
            ),
            (400, r#"{"error":"invalid_grant"}"#),
        ]);
        let tokens = exchange_code(&google, &client(), "c", "v", "http://127.0.0.1:1").unwrap();
        assert_eq!(tokens.refresh_token.as_deref(), Some("r"));
        let body = google.requests.borrow()[0].2.clone().unwrap();
        assert!(body.contains("grant_type=authorization_code") && body.contains("code_verifier=v"));
        assert_eq!(
            exchange_code(&google, &client(), "c", "v", "x"),
            Err(SyncError::ReconnectNeeded)
        );
    }

    #[test]
    fn sync_pushes_then_is_quiet_then_deletes_completed() {
        let db = connected_db();
        let google = FakeGoogle::new(&[(200, TOKEN), (200, r#"{"id":"ev1"}"#)]);
        let api = CalendarApi::new(&google, client(), "r".into());
        let summary = sync_once(&db, &api, "America/Toronto", now()).unwrap();
        assert_eq!(summary.created, 1);
        assert_eq!(
            google.calls(),
            vec!["POST token", "POST /calendars/cal%40group/events"]
        );
        let event: Value =
            serde_json::from_str(google.requests.borrow()[1].2.as_ref().unwrap()).unwrap();
        assert_eq!(event["start"]["timeZone"], "America/Toronto");
        assert_eq!(event["reminders"]["overrides"][0]["minutes"], 10);

        assert_eq!(
            sync_once(&db, &api, "America/Toronto", now()).unwrap(),
            SyncSummary::default()
        );
        assert_eq!(google.calls().len(), 2, "nothing changed, nothing sent");

        let id = crate::db::reminders::list_for_note(&db.lock().unwrap().conn, "n").unwrap()[0]
            .id
            .clone();
        crate::db::reminders::set_status(&db.lock().unwrap().conn, &id, "completed").unwrap();
        google
            .responses
            .borrow_mut()
            .push_back((204, String::new()));
        assert_eq!(
            sync_once(&db, &api, "America/Toronto", now())
                .unwrap()
                .deleted,
            1
        );
        assert_eq!(
            google.calls().last().unwrap(),
            "DELETE /calendars/cal%40group/events/ev1"
        );
        assert!(calendar_sync::pushed_events(&db.lock().unwrap().conn)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn expired_token_is_refreshed_once_and_revoked_access_needs_reconnect() {
        let db = connected_db();
        let google = FakeGoogle::new(&[
            (200, TOKEN),
            (401, "{}"),
            (200, TOKEN),
            (200, r#"{"id":"ev1"}"#),
        ]);
        let api = CalendarApi::new(&google, client(), "r".into());
        assert_eq!(sync_once(&db, &api, "UTC", now()).unwrap().created, 1);
        assert_eq!(
            google.calls(),
            vec![
                "POST token",
                "POST /calendars/cal%40group/events",
                "POST token",
                "POST /calendars/cal%40group/events"
            ]
        );

        let db = connected_db();
        let google = FakeGoogle::new(&[(400, r#"{"error":"invalid_grant"}"#)]);
        let api = CalendarApi::new(&google, client(), "r".into());
        assert_eq!(
            sync_once(&db, &api, "UTC", now()),
            Err(SyncError::ReconnectNeeded)
        );
    }

    #[test]
    fn deleted_calendar_is_recreated_and_offline_keeps_work_pending() {
        let db = connected_db();
        let google = FakeGoogle::new(&[
            (200, TOKEN),
            (404, "{}"),
            (200, r#"{"id":"new-cal"}"#),
            (200, r#"{"id":"ev2"}"#),
        ]);
        let api = CalendarApi::new(&google, client(), "r".into());
        let summary = sync_once(&db, &api, "UTC", now()).unwrap();
        assert!(summary.calendar_recreated);
        assert_eq!(summary.created, 1);
        assert_eq!(google.calls()[2], "POST /calendars");
        assert_eq!(google.calls()[3], "POST /calendars/new-cal/events");
        let config = calendar_sync::load_config(&db.lock().unwrap().conn)
            .unwrap()
            .unwrap();
        assert_eq!(config.calendar_id, "new-cal");

        let db = connected_db();
        let google = FakeGoogle::new(&[(200, TOKEN), (503, "")]);
        let api = CalendarApi::new(&google, client(), "r".into());
        assert!(matches!(
            sync_once(&db, &api, "UTC", now()),
            Err(SyncError::Offline(_))
        ));
        assert!(calendar_sync::pushed_events(&db.lock().unwrap().conn)
            .unwrap()
            .is_empty());
        let offline = FakeGoogle::new(&[]);
        let api = CalendarApi::new(&offline, client(), "r".into());
        assert!(matches!(
            sync_once(&db, &api, "UTC", now()),
            Err(SyncError::Offline(_))
        ));
    }

    #[test]
    fn event_deleted_in_google_is_re_added_on_update() {
        let db = connected_db();
        {
            let conn = &db.lock().unwrap().conn;
            let id = &crate::db::reminders::list_for_note(conn, "n").unwrap()[0].id;
            calendar_sync::record_pushed(conn, id, "old", "stale").unwrap();
        }
        let google = FakeGoogle::new(&[(200, TOKEN), (404, "{}"), (200, r#"{"id":"fresh"}"#)]);
        let api = CalendarApi::new(&google, client(), "r".into());
        assert_eq!(sync_once(&db, &api, "UTC", now()).unwrap().updated, 1);
        assert_eq!(google.calls()[1], "PUT /calendars/cal%40group/events/old");
        assert_eq!(
            calendar_sync::pushed_events(&db.lock().unwrap().conn).unwrap()[0].event_id,
            "fresh"
        );
    }

    #[cfg(windows)]
    #[test]
    fn refresh_token_is_stored_encrypted_and_round_trips() {
        let db = connected_db();
        let conn = &db.lock().unwrap().conn;
        save_refresh_token(conn, "1//secret-refresh").unwrap();
        let raw: String = conn
            .query_row(
                "SELECT value FROM meta WHERE key='google_refresh_token'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(!raw.contains("secret"));
        assert_eq!(
            load_refresh_token(conn).unwrap().as_deref(),
            Some("1//secret-refresh")
        );
        forget_refresh_token(conn).unwrap();
        assert_eq!(load_refresh_token(conn).unwrap(), None);
    }
}
