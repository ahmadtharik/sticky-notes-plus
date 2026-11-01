//! Settings live in the existing transactional metadata store, not note content.
use rusqlite::{Connection, OptionalExtension};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Preferences {
    pub version: u32,
    pub dark_theme: bool,
    pub shortcut: String,
    pub start_with_windows: bool,
}
impl Default for Preferences {
    fn default() -> Self {
        Self {
            version: 1,
            dark_theme: false,
            shortcut: crate::hotkey::DEFAULT_SHORTCUT.into(),
            start_with_windows: false,
        }
    }
}
impl Preferences {
    pub fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(self.version == 1, "Unsupported settings version");
        crate::hotkey::parse_shortcut(&self.shortcut)?;
        Ok(())
    }
}
pub fn load(conn: &Connection) -> anyhow::Result<Preferences> {
    let raw: Option<String> = conn
        .query_row(
            "SELECT value FROM meta WHERE key='user_settings'",
            [],
            |row| row.get(0),
        )
        .optional()?;
    let value = match raw {
        Some(raw) => serde_json::from_str(&raw)?,
        None => Preferences::default(),
    };
    value.validate()?;
    Ok(value)
}

/// Board placement (x, y, width, height in physical pixels). Kept apart from
/// `Preferences` so frequent moves never touch startup registration.
pub fn load_board_geometry(conn: &Connection) -> anyhow::Result<Option<(f64, f64, f64, f64)>> {
    let raw: Option<String> = conn
        .query_row(
            "SELECT value FROM meta WHERE key='board_window'",
            [],
            |row| row.get(0),
        )
        .optional()?;
    Ok(match raw {
        Some(raw) => {
            let [x, y, width, height] = serde_json::from_str::<[f64; 4]>(&raw)?;
            Some((x, y, width, height))
        }
        None => None,
    })
}

pub fn save_board_geometry(
    conn: &Connection,
    geometry: (f64, f64, f64, f64),
) -> anyhow::Result<()> {
    let (x, y, width, height) = geometry;
    conn.execute("INSERT INTO meta(key,value) VALUES('board_window',?1) ON CONFLICT(key) DO UPDATE SET value=excluded.value", [serde_json::to_string(&[x, y, width, height])?])?;
    Ok(())
}

pub fn apply(
    conn: &Connection,
    previous: &Preferences,
    next: &Preferences,
    mut startup: impl FnMut(bool) -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    next.validate()?;
    let transaction = conn.unchecked_transaction()?;
    transaction.execute("INSERT INTO meta(key,value) VALUES('user_settings',?1) ON CONFLICT(key) DO UPDATE SET value=excluded.value", [serde_json::to_string(next)?])?;
    let changed = previous.start_with_windows != next.start_with_windows;
    if changed {
        startup(next.start_with_windows)?;
    }
    if let Err(error) = transaction.commit() {
        if changed {
            if let Err(restore) = startup(previous.start_with_windows) {
                anyhow::bail!(
                    "Settings save failed ({error}); startup rollback also failed ({restore})"
                );
            }
        }
        return Err(error.into());
    }
    Ok(())
}

#[cfg(windows)]
pub fn set_windows_startup(enabled: bool) -> anyhow::Result<()> {
    use windows_sys::Win32::System::Registry::*;
    let wide = |text: &str| text.encode_utf16().chain(Some(0)).collect::<Vec<_>>();
    let path = wide("Software\\Microsoft\\Windows\\CurrentVersion\\Run");
    let name = wide("StickyNotesPlus");
    let mut key = std::ptr::null_mut();
    let status = unsafe {
        if enabled {
            RegCreateKeyExW(
                HKEY_CURRENT_USER,
                path.as_ptr(),
                0,
                std::ptr::null(),
                REG_OPTION_NON_VOLATILE,
                KEY_SET_VALUE,
                std::ptr::null(),
                &mut key,
                std::ptr::null_mut(),
            )
        } else {
            RegOpenKeyExW(HKEY_CURRENT_USER, path.as_ptr(), 0, KEY_SET_VALUE, &mut key)
        }
    };
    if !enabled && status == 2 {
        return Ok(());
    }
    if status != 0 {
        return Err(std::io::Error::from_raw_os_error(status as i32).into());
    }
    struct RegistryKey(HKEY);
    impl Drop for RegistryKey {
        fn drop(&mut self) {
            unsafe {
                RegCloseKey(self.0);
            }
        }
    }
    let key = RegistryKey(key);
    let status = if enabled {
        let exe = std::env::current_exe()?;
        let exe = exe
            .to_str()
            .ok_or_else(|| anyhow::anyhow!("Executable path is not valid Unicode"))?;
        let command = wide(&format!("\"{exe}\" --background"));
        unsafe {
            RegSetValueExW(
                key.0,
                name.as_ptr(),
                0,
                REG_SZ,
                command.as_ptr().cast(),
                (command.len() * 2) as u32,
            )
        }
    } else {
        unsafe { RegDeleteValueW(key.0, name.as_ptr()) }
    };
    if status != 0 && !(status == 2 && !enabled) {
        return Err(std::io::Error::from_raw_os_error(status as i32).into());
    }
    Ok(())
}
#[cfg(not(windows))]
pub fn set_windows_startup(enabled: bool) -> anyhow::Result<()> {
    anyhow::ensure!(!enabled, "Windows startup is unavailable on this platform");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE TABLE meta(key TEXT PRIMARY KEY,value TEXT NOT NULL)")
            .unwrap();
        conn
    }
    #[test]
    fn safe_defaults_and_settings_roundtrip() {
        let conn = db();
        let previous = load(&conn).unwrap();
        assert!(!previous.start_with_windows);
        let next = Preferences {
            dark_theme: true,
            shortcut: "Ctrl+Alt+K".into(),
            ..previous.clone()
        };
        apply(&conn, &previous, &next, |_| {
            panic!("No startup mutation expected")
        })
        .unwrap();
        assert_eq!(load(&conn).unwrap(), next);
    }
    #[test]
    fn board_geometry_roundtrips_without_touching_preferences() {
        let conn = db();
        assert_eq!(load_board_geometry(&conn).unwrap(), None);
        save_board_geometry(&conn, (-1200.0, 40.0, 420.0, 640.0)).unwrap();
        save_board_geometry(&conn, (-1180.0, 60.0, 400.0, 620.0)).unwrap();
        assert_eq!(
            load_board_geometry(&conn).unwrap(),
            Some((-1180.0, 60.0, 400.0, 620.0))
        );
        assert_eq!(load(&conn).unwrap(), Preferences::default());
    }
    #[test]
    fn failed_startup_registration_rolls_back_settings() {
        let conn = db();
        let previous = Preferences::default();
        let next = Preferences {
            start_with_windows: true,
            ..previous.clone()
        };
        assert!(apply(&conn, &previous, &next, |_| anyhow::bail!(
            "registration denied"
        ))
        .is_err());
        assert_eq!(load(&conn).unwrap(), previous);
    }
    #[test]
    fn failed_commit_compensates_startup_registration() {
        let conn = db();
        conn.execute_batch("PRAGMA foreign_keys=ON; CREATE TABLE parent(id INTEGER PRIMARY KEY); CREATE TABLE child(parent_id INTEGER REFERENCES parent(id) DEFERRABLE INITIALLY DEFERRED)").unwrap();
        let previous = Preferences::default();
        let next = Preferences {
            start_with_windows: true,
            ..previous.clone()
        };
        let mut calls = Vec::new();
        let result = apply(&conn, &previous, &next, |enabled| {
            calls.push(enabled);
            if enabled {
                conn.execute("INSERT INTO child VALUES(99)", [])?;
            }
            Ok(())
        });
        assert!(result.is_err());
        assert_eq!(calls, vec![true, false]);
        assert_eq!(load(&conn).unwrap(), previous);
    }
    #[test]
    fn invalid_shortcuts_and_failed_database_writes_never_register_startup() {
        let conn = db();
        let previous = Preferences::default();
        let mut next = Preferences {
            start_with_windows: true,
            shortcut: "Shift+N".into(),
            ..previous.clone()
        };
        assert!(apply(&conn, &previous, &next, |_| panic!(
            "Invalid settings must not register startup"
        ))
        .is_err());
        next.shortcut = "Ctrl+Alt+N".into();
        conn.execute_batch("CREATE TRIGGER deny_settings BEFORE INSERT ON meta BEGIN SELECT RAISE(ABORT,'save denied'); END").unwrap();
        assert!(apply(&conn, &previous, &next, |_| panic!(
            "Failed DB write must not register startup"
        ))
        .is_err());
        assert_eq!(load(&conn).unwrap(), previous);
    }
    #[test]
    fn disk_reopen_restores_preferences_without_touching_notes() {
        let directory =
            std::env::temp_dir().join(format!("sticky-settings-test-{}", uuid::Uuid::new_v4()));
        let db = crate::db::Db::open_in_dir(&directory).unwrap();
        crate::db::notes::create(&db.conn, "n", crate::EMPTY_CONTENT, "#FFF9B1").unwrap();
        let before = crate::db::notes::get(&db.conn, "n").unwrap().unwrap();
        let previous = Preferences::default();
        let next = Preferences {
            dark_theme: true,
            shortcut: "Ctrl+Alt+F8".into(),
            start_with_windows: true,
            ..previous.clone()
        };
        let mut calls = Vec::new();
        apply(&db.conn, &previous, &next, |enabled| {
            calls.push(enabled);
            Ok(())
        })
        .unwrap();
        assert_eq!(calls, vec![true]);
        drop(db);
        let db = crate::db::Db::open_in_dir(&directory).unwrap();
        assert_eq!(load(&db.conn).unwrap(), next);
        assert_eq!(
            crate::db::notes::get(&db.conn, "n").unwrap().unwrap(),
            before
        );
        apply(&db.conn, &next, &next, |_| {
            panic!("Unchanged startup must not be rewritten")
        })
        .unwrap();
    }
    #[test]
    fn corrupt_future_and_invalid_settings_are_not_silently_replaced() {
        for raw in [
            "invalid",
            r#"{"version":2}"#,
            r#"{"shortcut":"Shift+N"}"#,
            r#"{"unknown":true}"#,
        ] {
            let conn = db();
            conn.execute("INSERT INTO meta VALUES('user_settings',?1)", [raw])
                .unwrap();
            assert!(load(&conn).is_err());
            assert_eq!(
                conn.query_row("SELECT value FROM meta", [], |row| row.get::<_, String>(0))
                    .unwrap(),
                raw
            );
        }
    }
}
