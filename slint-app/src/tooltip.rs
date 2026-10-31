//! Native tooltip for the note toolbar. A Slint overlay would be painted
//! beneath the RichEdit child window, so this is its own top-level window.
use windows_sys::Win32::{
    Foundation::{HWND, LPARAM, POINT},
    UI::{
        Controls::{
            TOOLTIPS_CLASSW, TTF_ABSOLUTE, TTF_TRACK, TTM_ADDTOOLW, TTM_TRACKACTIVATE,
            TTM_TRACKPOSITION, TTM_UPDATETIPTEXTW, TTS_ALWAYSTIP, TTS_NOPREFIX, TTTOOLINFOW,
        },
        WindowsAndMessaging::{
            CreateWindowExW, GetCursorPos, SendMessageW, ShowWindow, SW_HIDE, SW_SHOWNOACTIVATE,
            WS_EX_TOPMOST, WS_POPUP,
        },
    },
};

pub struct Tooltip {
    window: HWND,
    owner: HWND,
}

impl Tooltip {
    pub fn new(owner: HWND) -> Option<Self> {
        let window = unsafe {
            CreateWindowExW(
                WS_EX_TOPMOST,
                TOOLTIPS_CLASSW,
                std::ptr::null(),
                WS_POPUP | TTS_NOPREFIX | TTS_ALWAYSTIP,
                0,
                0,
                0,
                0,
                owner,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null(),
            )
        };
        if window.is_null() {
            return None;
        }
        let tooltip = Self { window, owner };
        let mut empty = [0u16];
        let info = tooltip.info(empty.as_mut_ptr());
        unsafe { SendMessageW(window, TTM_ADDTOOLW, 0, &info as *const _ as LPARAM) };
        Some(tooltip)
    }

    fn info(&self, text: *mut u16) -> TTTOOLINFOW {
        let mut info: TTTOOLINFOW = unsafe { std::mem::zeroed() };
        // The size without lpReserved is accepted by every comctl32 version.
        info.cbSize = (std::mem::size_of::<TTTOOLINFOW>() - std::mem::size_of::<usize>()) as u32;
        info.uFlags = TTF_TRACK | TTF_ABSOLUTE;
        info.hwnd = self.owner;
        info.lpszText = text;
        info
    }

    /// Shows the text just below the mouse pointer.
    pub fn show(&self, text: &str) {
        let mut wide: Vec<u16> = text.encode_utf16().chain(Some(0)).collect();
        let info = self.info(wide.as_mut_ptr());
        let mut point = POINT { x: 0, y: 0 };
        unsafe {
            GetCursorPos(&mut point);
            let info = &info as *const _ as LPARAM;
            SendMessageW(self.window, TTM_UPDATETIPTEXTW, 0, info);
            let position = ((point.y + 20) as u32 & 0xffff) << 16 | (point.x as u32 & 0xffff);
            SendMessageW(self.window, TTM_TRACKPOSITION, 0, position as LPARAM);
            SendMessageW(self.window, TTM_TRACKACTIVATE, 1, info);
            // Show without taking focus from the note, active or not.
            ShowWindow(self.window, SW_SHOWNOACTIVATE);
        }
    }

    pub fn hide(&self) {
        let mut empty = [0u16];
        let info = self.info(empty.as_mut_ptr());
        unsafe {
            SendMessageW(
                self.window,
                TTM_TRACKACTIVATE,
                0,
                &info as *const _ as LPARAM,
            );
            ShowWindow(self.window, SW_HIDE);
        }
    }
}
