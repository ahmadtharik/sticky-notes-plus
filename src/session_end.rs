//! Saves open notes when Windows signs out or shuts down. Windows warns each
//! top-level window and then ends the process, so typing still inside the
//! autosave delay would otherwise be lost. A hidden window of our own hears
//! the warning: the board's window may not exist yet after a background start,
//! and message-only windows aren't told about session end.
use std::cell::RefCell;
use windows_sys::Win32::{
    Foundation::{HWND, LPARAM, LRESULT, WPARAM},
    System::LibraryLoader::GetModuleHandleW,
    UI::WindowsAndMessaging::{
        CreateWindowExW, DefWindowProcW, RegisterClassW, WM_ENDSESSION, WM_QUERYENDSESSION,
        WNDCLASSW, WS_OVERLAPPED,
    },
};

thread_local! {
    /// Runs on the UI thread, which owns the window and dispatches its messages.
    static ON_SESSION_END: RefCell<Option<Box<dyn Fn()>>> = const { RefCell::new(None) };
}

/// Calls `save` when the session is about to end. Call once, on the UI thread.
pub fn watch(save: impl Fn() + 'static) -> anyhow::Result<()> {
    ON_SESSION_END.with(|slot| *slot.borrow_mut() = Some(Box::new(save)));
    let class: Vec<u16> = "StickyNotesPlusSessionEnd\0".encode_utf16().collect();
    let window = unsafe {
        let instance = GetModuleHandleW(std::ptr::null());
        let mut info: WNDCLASSW = std::mem::zeroed();
        info.lpfnWndProc = Some(window_proc);
        info.hInstance = instance;
        info.lpszClassName = class.as_ptr();
        RegisterClassW(&info);
        // Never shown: a hidden top-level window still gets session messages.
        CreateWindowExW(
            0,
            class.as_ptr(),
            std::ptr::null(),
            WS_OVERLAPPED,
            0,
            0,
            0,
            0,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            instance,
            std::ptr::null(),
        )
    };
    anyhow::ensure!(!window.is_null(), "Couldn't create the session-end window");
    Ok(())
}

fn save_now() {
    ON_SESSION_END.with(|slot| {
        // A message sent while the callback is already running is ignored.
        if let Ok(slot) = slot.try_borrow() {
            if let Some(save) = slot.as_ref() {
                save();
            }
        }
    });
}

unsafe extern "system" fn window_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match message {
        // Save as soon as Windows asks, while there's still time, and never
        // block the sign-out.
        WM_QUERYENDSESSION => {
            save_now();
            1
        }
        // Sent again once the session is really ending (wparam != 0).
        WM_ENDSESSION if wparam != 0 => {
            save_now();
            0
        }
        _ => DefWindowProcW(hwnd, message, wparam, lparam),
    }
}
