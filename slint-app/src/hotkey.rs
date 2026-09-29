//! Windows global quick-create shortcut support.
//!
//! Win32 delivers a process-wide `WM_HOTKEY` to the thread that calls
//! `RegisterHotKey`. That thread only writes to an MPSC channel; the Slint
//! event-loop timer in `main.rs` consumes it and performs all UI work.

/// The documented global shortcut for opening a new sticky.
pub const DEFAULT_SHORTCUT: &str = "Ctrl+Alt+N";

#[cfg(windows)]
mod platform {
    use std::{
        sync::mpsc::{self, Receiver},
        thread,
    };
    use windows_sys::Win32::{
        Foundation::HWND,
        UI::{
            Input::KeyboardAndMouse::{
                RegisterHotKey, UnregisterHotKey, MOD_ALT, MOD_CONTROL, MOD_NOREPEAT, VK_N,
            },
            WindowsAndMessaging::{GetMessageW, MSG, WM_HOTKEY},
        },
    };

    const QUICK_CREATE_ID: i32 = 0x534E;

    pub struct QuickCreate {
        receiver: Receiver<()>,
    }

    impl QuickCreate {
        pub fn register_default() -> anyhow::Result<Self> {
            let (ready_tx, ready_rx) = mpsc::sync_channel(1);
            let (command_tx, receiver) = mpsc::channel();
            thread::Builder::new()
                .name("sticky-notes-hotkey".into())
                .spawn(move || {
                    let registered = unsafe {
                        RegisterHotKey(
                            std::ptr::null_mut::<core::ffi::c_void>() as HWND,
                            QUICK_CREATE_ID,
                            MOD_CONTROL | MOD_ALT | MOD_NOREPEAT,
                            u32::from(VK_N),
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
    pub fn register_default() -> anyhow::Result<Self> {
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
}
