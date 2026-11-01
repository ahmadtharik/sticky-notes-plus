//! Windows global quick-create shortcut support.
//!
//! Win32 delivers a process-wide `WM_HOTKEY` to the thread that calls
//! `RegisterHotKey`. That thread only writes to an MPSC channel; the Slint
//! event-loop timer in `main.rs` consumes it and performs all UI work.

/// The documented global shortcut for opening a new sticky.
pub const DEFAULT_SHORTCUT: &str = "Ctrl+Alt+N";

pub fn parse_shortcut(value: &str) -> anyhow::Result<(u32, u32)> {
    let parts: Vec<_> = value.split('+').map(str::trim).collect();
    anyhow::ensure!(
        parts.len() >= 2,
        "Use Ctrl or Alt plus a letter, digit or F1–F24"
    );
    let mut modifiers = 0;
    for part in &parts[..parts.len() - 1] {
        let flag = match part.to_ascii_lowercase().as_str() {
            "alt" => 1,
            "ctrl" => 2,
            "shift" => 4,
            _ => anyhow::bail!("Unsupported shortcut modifier"),
        };
        anyhow::ensure!(modifiers & flag == 0, "Repeated shortcut modifier");
        modifiers |= flag;
    }
    anyhow::ensure!(modifiers & 3 != 0, "Shortcut must include Ctrl or Alt");
    let key = parts.last().unwrap().to_ascii_uppercase();
    let key = if key.len() == 1 && key.as_bytes()[0].is_ascii_alphanumeric() {
        u32::from(key.as_bytes()[0])
    } else if let Some(number) = key
        .strip_prefix('F')
        .and_then(|value| value.parse::<u32>().ok())
        .filter(|number| (1..=24).contains(number))
    {
        0x70 + number - 1
    } else {
        anyhow::bail!("Use a letter, digit or F1–F24")
    };
    Ok((modifiers, key))
}

#[cfg(windows)]
mod platform {
    use std::{
        sync::mpsc::{self, Receiver},
        thread,
    };
    use windows_sys::Win32::{
        Foundation::HWND,
        UI::{
            Input::KeyboardAndMouse::{RegisterHotKey, UnregisterHotKey, MOD_NOREPEAT},
            WindowsAndMessaging::{GetMessageW, MSG, WM_HOTKEY},
        },
    };

    const QUICK_CREATE_ID: i32 = 0x534E;

    pub struct QuickCreate {
        receiver: Receiver<()>,
    }

    impl QuickCreate {
        pub fn register(shortcut: &str) -> anyhow::Result<Self> {
            let (modifiers, key) = super::parse_shortcut(shortcut)?;
            let (ready_tx, ready_rx) = mpsc::sync_channel(1);
            let (command_tx, receiver) = mpsc::channel();
            thread::Builder::new()
                .name("sticky-notes-hotkey".into())
                .spawn(move || {
                    let registered = unsafe {
                        RegisterHotKey(
                            std::ptr::null_mut::<core::ffi::c_void>() as HWND,
                            QUICK_CREATE_ID,
                            modifiers | MOD_NOREPEAT,
                            key,
                        )
                    };
                    if registered == 0 {
                        let _ = ready_tx.send(Err(std::io::Error::last_os_error()));
                        return;
                    }
                    let _ = ready_tx.send(Ok(()));

                    let mut message: MSG = unsafe { std::mem::zeroed() };
                    while unsafe {
                        GetMessageW(
                            &mut message,
                            std::ptr::null_mut::<core::ffi::c_void>() as HWND,
                            0,
                            0,
                        )
                    } > 0
                    {
                        if message.message == WM_HOTKEY
                            && message.wParam == QUICK_CREATE_ID as usize
                        {
                            // MOD_NOREPEAT suppresses held-key repeats. The
                            // timer in main.rs performs the actual UI action.
                            let _ = command_tx.send(());
                        }
                    }
                    unsafe {
                        UnregisterHotKey(
                            std::ptr::null_mut::<core::ffi::c_void>() as HWND,
                            QUICK_CREATE_ID,
                        );
                    }
                })?;

            match ready_rx.recv() {
                Ok(Ok(())) => Ok(Self { receiver }),
                Ok(Err(error)) => Err(error.into()),
                Err(_) => {
                    anyhow::bail!("global-hotkey registration thread exited before reporting")
                }
            }
        }

        pub fn poll_quick_create(&self) -> bool {
            self.receiver.try_recv().is_ok()
        }
    }
}

#[cfg(windows)]
pub use platform::QuickCreate;

#[cfg(not(windows))]
pub struct QuickCreate;

#[cfg(not(windows))]
impl QuickCreate {
    pub fn register(shortcut: &str) -> anyhow::Result<Self> {
        parse_shortcut(shortcut)?;
        Ok(Self)
    }
    pub fn poll_quick_create(&self) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::DEFAULT_SHORTCUT;

    #[test]
    fn documents_the_default_shortcut() {
        assert_eq!(DEFAULT_SHORTCUT, "Ctrl+Alt+N");
    }
    #[test]
    fn shortcuts_validate_modifiers_and_supported_keys() {
        assert_eq!(super::parse_shortcut("Ctrl+Alt+N").unwrap(), (3, 78));
        assert_eq!(
            super::parse_shortcut(" alt + shift + f24 ").unwrap(),
            (5, 0x87)
        );
        for invalid in [
            "N",
            "Shift+N",
            "Ctrl+Ctrl+N",
            "Win+N",
            "Ctrl+",
            "Ctrl+F25",
            "Ctrl+日本語",
        ] {
            assert!(super::parse_shortcut(invalid).is_err(), "{invalid}");
        }
    }
}
