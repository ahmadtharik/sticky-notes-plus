//! Bounded Win32 RichEdit host used by the Phase 2 editor architecture gate.
//!
//! This deliberately lives behind `cfg(windows)`: Slint remains the renderer
//! and the app has no WebView.  The control is a child of the public Win32
//! handle returned by `slint::Window::window_handle()`.

use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use std::{
    cell::{Cell, RefCell},
    collections::HashMap,
    mem::size_of,
    rc::{Rc, Weak},
    time::{Duration, Instant},
};
use windows::{
    core::{IUnknown, Interface},
    Win32::UI::Controls::RichEdit::{ITextDocument, ITextRange},
};
use windows_sys::Win32::{
    Foundation::{HWND, LPARAM, LRESULT, WPARAM},
    System::LibraryLoader::LoadLibraryW,
    UI::WindowsAndMessaging::{
        CallWindowProcW, CreateWindowExW, DestroyWindow, GetClientRect, GetWindowLongPtrW,
        MoveWindow, SendMessageW, SetWindowLongPtrW, ShowWindow, ES_AUTOVSCROLL, ES_MULTILINE,
        ES_NOHIDESEL, ES_WANTRETURN, GWLP_WNDPROC, GWL_STYLE, WM_CHAR, WM_CLEAR, WM_CUT,
        WM_GETTEXT, WM_GETTEXTLENGTH, WM_IME_COMPOSITION, WM_KEYUP, WM_KILLFOCUS, WM_PASTE,
        WM_SETTEXT, WM_UNDO, WNDPROC, WS_CHILD, WS_CLIPCHILDREN, WS_CLIPSIBLINGS, WS_VISIBLE,
        WS_VSCROLL,
    },
};

const EM_GETCHARFORMAT: u32 = 1082;
const EM_SETBKGNDCOLOR: u32 = 1091;
const EM_SETCHARFORMAT: u32 = 1092;
const EM_EXGETSEL: u32 = 1076;
const EM_EXSETSEL: u32 = 1079;
const EM_REDO: u32 = 1108;
const SCF_SELECTION: usize = 0x0001;
const CFM_BOLD: u32 = 0x0000_0001;
const CFM_ITALIC: u32 = 0x0000_0002;
const CFM_UNDERLINE: u32 = 0x0000_0004;
const CFM_STRIKEOUT: u32 = 0x0000_0008;
const SW_HIDE: i32 = 0;
const SW_SHOW: i32 = 5;

/// The subset of the note JSON editor state which RichEdit owns. Offsets are
/// UTF-8 bytes, matching the existing JSON schema rather than Win32's UTF-16
/// selection coordinates.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NativeDocument {
    pub text: String,
    pub bold: Vec<(usize, usize)>,
    pub italic: Vec<(usize, usize)>,
    pub underline: Vec<(usize, usize)>,
    pub strike: Vec<(usize, usize)>,
    pub chip_ids: Vec<String>,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct CharRange {
    cp_min: i32,
    cp_max: i32,
}

// CHARFORMAT2W is intentionally defined here because current windows-sys
// metadata does not expose the legacy RichEdit messages/struct. Its layout is
// the Win32 SDK CHARFORMAT2W layout (116 bytes on both 32 and 64-bit builds).
#[repr(C)]
#[derive(Clone, Copy)]
struct CharFormat2W {
    cb_size: u32,
    dw_mask: u32,
    dw_effects: u32,
    y_height: i32,
    y_offset: i32,
    cr_text_color: u32,
    b_char_set: u8,
    b_pitch_and_family: u8,
    sz_face_name: [u16; 32],
    w_weight: u16,
    s_spacing: u16,
    cr_back_color: u32,
    lcid: u32,
    dw_reserved: u32,
    s_style: u16,
    w_kerning: u16,
    b_underline_type: u8,
    b_animation: u8,
    b_rev_author: u8,
    b_reserved1: u8,
}

impl Default for CharFormat2W {
    fn default() -> Self {
        // This C-compatible POD has no invalid bit patterns.
        unsafe { std::mem::zeroed() }
    }
}

thread_local! {
    // A RichEdit control already uses GWLP_USERDATA internally. Keep our
    // callback lookup in a UI-thread-local map instead of corrupting it.
    static EDITORS: RefCell<HashMap<isize, *const Inner>> = RefCell::new(HashMap::new());
}

struct Inner {
    hwnd: HWND,
    original_proc: Cell<WNDPROC>,
    suppress_change: Cell<bool>,
    pending_change: Cell<bool>,
    change_timer: slint::Timer,
    paint_timer: slint::Timer,
    frame_timer: slint::Timer,
    last_frame: Cell<Option<Instant>>,
    repaint_needed: Cell<bool>,
    on_change: RefCell<Box<dyn FnMut(NativeDocument)>>,
    weak_self: RefCell<Weak<Inner>>,
    scale_factor: Cell<f32>,
    layout_bounds: Cell<Option<[f32; 4]>>,
    metrics: Cell<NativeMetrics>,
    history: RefCell<crate::editor_history::History<NativeDocument>>,
    chip_clicked: RefCell<Option<Box<dyn FnMut(i32, f32, f32)>>>,
    composing: Cell<bool>,
    handled_enter: Cell<bool>,
    palette: Cell<Option<u32>>,
    text_document: Option<ITextDocument>,
    tracked_chips: RefCell<Vec<TrackedNativeChip>>,
    last_edit_at: Cell<Option<usize>>,
}

struct TrackedNativeChip {
    range: ITextRange,
    id: String,
    date: String,
}

#[derive(Clone, Copy, Default, serde::Serialize)]
pub struct NativeMetrics {
    snapshots: u64,
    snapshot_micros: u128,
    callback_micros: u128,
    input_micros: u128,
    parent_frames: u64,
    child_paint_micros: u128,
}

impl Inner {
    fn schedule_change(&self) {
        if self.pending_change.replace(true) {
            return;
        }
        let weak = self.weak_self.borrow().clone();
        self.change_timer.start(
            slint::TimerMode::SingleShot,
            Duration::from_millis(120),
            move || {
                let Some(inner) = weak.upgrade() else {
                    return;
                };
                inner.emit_change();
            },
        );
    }

    fn emit_change(&self) {
        if self.composing.get() {
            self.pending_change.set(false);
            return;
        }
        self.pending_change.set(false);
        let started = Instant::now();
        let document = unsafe { document_from_control(self.hwnd) };
        self.history.borrow_mut().record(document.clone());
        let snapshot_micros = started.elapsed().as_micros();
        let started = Instant::now();
        (self.on_change.borrow_mut())(document);
        let mut metrics = self.metrics.get();
        metrics.snapshots += 1;
        metrics.snapshot_micros += snapshot_micros;
        metrics.callback_micros += started.elapsed().as_micros();
        self.metrics.set(metrics);
    }
}

/// Owns one child `RICHEDIT50W` control. It is !Send by construction and must
/// be created/dropped on Slint's UI thread.
pub struct NativeRichEditor {
    inner: Rc<Inner>,
    parent: HWND,
    added_parent_clip: bool,
}

impl NativeRichEditor {
    pub fn attach(
        window: &slint::Window,
        document: NativeDocument,
        on_change: impl FnMut(NativeDocument) + 'static,
    ) -> Result<Self, String> {
        // Slint creates transparent Winit hosts by default. GDI child controls
        // cannot provide alpha pixels for that backdrop, producing stale or
        // glass-tinted writing surfaces. Sticky paper is intentionally opaque.
        use i_slint_backend_winit::WinitWindowAccessor;
        window.with_winit_window(|host| host.set_transparent(false));
        let parent = parent_hwnd(window)?;
        // Winit 0.30 only applies its DWM transparent backdrop at creation;
        // set_transparent(false) updates its flags but does not undo that API
        // call. Explicitly disable the creation-time backdrop on our host.
        unsafe {
            use windows_sys::Win32::Graphics::Dwm::{
                DwmEnableBlurBehindWindow, DWM_BB_ENABLE, DWM_BLURBEHIND,
            };
            let backdrop = DWM_BLURBEHIND {
                dwFlags: DWM_BB_ENABLE,
                fEnable: 0,
                ..Default::default()
            };
            if DwmEnableBlurBehindWindow(parent, &backdrop) < 0 {
                return Err("Windows could not create an opaque native editor host".into());
            }
        }
        Self::attach_to_parent(parent, window.scale_factor(), document, on_change)
    }

    fn attach_to_parent(
        parent: HWND,
        scale_factor: f32,
        document: NativeDocument,
        on_change: impl FnMut(NativeDocument) + 'static,
    ) -> Result<Self, String> {
        let module_name: Vec<u16> = "Msftedit.dll\0".encode_utf16().collect();
        // RichEdit is registered by Msftedit.dll. Loading this system module is
        // necessary before CreateWindowExW, and does not add a browser runtime.
        if unsafe { LoadLibraryW(module_name.as_ptr()) }.is_null() {
            return Err("Windows could not load Msftedit.dll".into());
        }
        let class_name: Vec<u16> = "RICHEDIT50W\0".encode_utf16().collect();
        let hwnd = unsafe {
            CreateWindowExW(
                0,
                class_name.as_ptr(),
                std::ptr::null(),
                WS_CHILD
                    | WS_CLIPSIBLINGS
                    | WS_VISIBLE
                    | WS_VSCROLL
                    | ES_MULTILINE as u32
                    | ES_AUTOVSCROLL as u32
                    | ES_WANTRETURN as u32
                    | ES_NOHIDESEL as u32,
                20,
                68,
                200,
                150,
                parent,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null(),
            )
        };
        if hwnd.is_null() {
            return Err("Windows could not create the RICHEDIT50W child".into());
        }
        let Some(text_document) = (unsafe { text_object_document(hwnd) }) else {
            unsafe { DestroyWindow(hwnd) };
            return Err("Windows RichEdit does not expose text ranges for date chips".into());
        };
        // Match the former editor's warm paper field instead of the default
        // pure white, while retaining system caret/selection accessibility.
        unsafe { SendMessageW(hwnd, EM_SETBKGNDCOLOR, 0, 0x00f8_f8_f8) };
        let inner = Rc::new(Inner {
            hwnd,
            original_proc: Cell::new(None),
            suppress_change: Cell::new(false),
            pending_change: Cell::new(false),
            change_timer: slint::Timer::default(),
            paint_timer: slint::Timer::default(),
            frame_timer: slint::Timer::default(),
            last_frame: Cell::new(None),
            repaint_needed: Cell::new(true),
            on_change: RefCell::new(Box::new(on_change)),
            weak_self: RefCell::new(Weak::new()),
            scale_factor: Cell::new(scale_factor),
            layout_bounds: Cell::new(None),
            metrics: Cell::new(NativeMetrics::default()),
            history: RefCell::new(crate::editor_history::History::new(
                document.clone(),
                document_weight,
            )),
            chip_clicked: RefCell::new(None),
            composing: Cell::new(false),
            handled_enter: Cell::new(false),
            palette: Cell::new(None),
            text_document: Some(text_document),
            tracked_chips: RefCell::new(Vec::new()),
            last_edit_at: Cell::new(None),
        });
        *inner.weak_self.borrow_mut() = Rc::downgrade(&inner);
        let ptr = Rc::as_ptr(&inner);
        let previous = unsafe {
            SetWindowLongPtrW(
                hwnd,
                GWLP_WNDPROC,
                rich_edit_proc as *const () as usize as isize,
            )
        };
        if previous == 0 {
            unsafe { DestroyWindow(hwnd) };
            return Err("Windows could not subclass the RichEdit child".into());
        }
        inner
            .original_proc
            .set(unsafe { std::mem::transmute(previous) });
        EDITORS.with(|editors| editors.borrow_mut().insert(hwnd as isize, ptr));
        // The software-rendered parent must not blit its canvas over a native
        // child. Preserve all other styles and undo only the bit we own.
        let parent_style = unsafe { GetWindowLongPtrW(parent, GWL_STYLE) };
        let added_parent_clip = parent_style as u32 & WS_CLIPCHILDREN == 0;
        if added_parent_clip {
            unsafe {
                SetWindowLongPtrW(parent, GWL_STYLE, parent_style | WS_CLIPCHILDREN as isize)
            };
            unsafe {
                use windows_sys::Win32::UI::WindowsAndMessaging::{
                    SetWindowPos, SWP_FRAMECHANGED, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE,
                    SWP_NOZORDER,
                };
                SetWindowPos(
                    parent,
                    std::ptr::null_mut(),
                    0,
                    0,
                    0,
                    0,
                    SWP_FRAMECHANGED | SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
                );
            }
        }
        let editor = Self {
            inner,
            parent,
            added_parent_clip,
        };
        editor.set_document(&document);
        editor.layout();
        Ok(editor)
    }

    pub fn set_document(&self, document: &NativeDocument) {
        self.inner.repaint_needed.set(true);
        // A programmatic state replacement follows a flush in every caller
        // that reads native text. Discard any timer from the previous native
        // state so it cannot later overwrite the newer Slint/date/list state.
        self.inner.change_timer.stop();
        self.inner.pending_change.set(false);
        self.inner.suppress_change.set(true);
        self.inner.history.borrow_mut().record(document.clone());
        unsafe { set_document(self.inner.hwnd, document) };
        self.inner.suppress_change.set(false);
    }

    /// Synchronize a debounced RichEdit notification now. This is used by the
    /// shared save/close path before it checks `dirty`, preventing keystrokes
    /// that have reached the child HWND but not the Slint model from being
    /// dropped during a close, explicit Save, or application quit.
    pub fn flush_pending_change(&self) {
        if self.inner.composing.get() {
            // Save/close must commit the current composition, not silently
            // drop it while normal autosave correctly waits for IME END.
            unsafe {
                use windows_sys::Win32::UI::Input::Ime::{
                    ImmGetContext, ImmNotifyIME, ImmReleaseContext, CPS_COMPLETE, NI_COMPOSITIONSTR,
                };
                let context = ImmGetContext(self.inner.hwnd);
                if !context.is_null() {
                    ImmNotifyIME(context, NI_COMPOSITIONSTR, CPS_COMPLETE, 0);
                    ImmReleaseContext(self.inner.hwnd, context);
                }
            }
            self.inner.composing.set(false);
            self.inner.pending_change.set(true);
        }
        if !self.inner.pending_change.replace(false) {
            return;
        }
        self.inner.change_timer.stop();
        self.inner.emit_change();
    }

    pub fn take_probe_metrics(&self) -> NativeMetrics {
        self.inner.metrics.replace(NativeMetrics::default())
    }

    pub fn history_action(&self, redo: bool) {
        unsafe { SendMessageW(self.inner.hwnd, if redo { EM_REDO } else { WM_UNDO }, 0, 0) };
    }

    pub fn on_chip_clicked(&self, callback: impl FnMut(i32, f32, f32) + 'static) {
        *self.inner.chip_clicked.borrow_mut() = Some(Box::new(callback));
    }

    pub fn set_palette(&self, value: &str) {
        self.inner.repaint_needed.set(true);
        let Ok(rgb) = u32::from_str_radix(value.trim_start_matches('#'), 16) else {
            return;
        };
        self.inner.palette.set(Some(rgb));
        unsafe { apply_palette(self.inner.hwnd, rgb) };
    }

    pub fn selection_byte_offsets(&self, source: &str) -> (i32, i32) {
        let range = unsafe { selection(self.inner.hwnd) };
        let projection = unsafe { control_projection(self.inner.hwnd) };
        debug_assert_eq!(source, projection.source);
        let convert = |offset: i32| projection.to_source(offset.max(0) as usize) as i32;
        (convert(range.cp_min), convert(range.cp_max))
    }

    pub fn restore_selection(&self, source: &str, start: usize, end: usize) {
        if start > source.len()
            || end > source.len()
            || !source.is_char_boundary(start)
            || !source.is_char_boundary(end)
        {
            return;
        }
        unsafe {
            let projection = control_projection(self.inner.hwnd);
            set_selection(
                self.inner.hwnd,
                CharRange {
                    cp_min: projection.to_control(start) as i32,
                    cp_max: projection.to_control(end) as i32,
                },
            );
            windows_sys::Win32::UI::Input::KeyboardAndMouse::SetFocus(self.inner.hwnd);
        }
    }

    #[cfg(test)]
    pub fn document(&self) -> NativeDocument {
        unsafe { document_from_control(self.inner.hwnd) }
    }

    /// Synthetic input is only used by the explicit isolated acceptance probe.
    /// It travels through the real child procedure and normal autosave callback.
    pub(crate) fn inject_probe_character(&self, character: char) {
        let started = Instant::now();
        assert!(character.is_ascii(), "probe input must be one UTF-16 unit");
        unsafe {
            set_selection(
                self.inner.hwnd,
                CharRange {
                    cp_min: -1,
                    cp_max: -1,
                },
            );
            SendMessageW(self.inner.hwnd, WM_CHAR, character as usize, 0);
        }
        let mut metrics = self.inner.metrics.get();
        metrics.input_micros += started.elapsed().as_micros();
        self.inner.metrics.set(metrics);
    }

    /// Toggles a format on the current mouse/keyboard selection and emits the
    /// resulting document. The editor retains selection when a Slint toolbar
    /// button temporarily owns focus.
    pub fn toggle_selection_style(&self, name: &str) -> Result<(), String> {
        let bit = style_bit(name).ok_or_else(|| "Unsupported text style.".to_owned())?;
        let hwnd = self.inner.hwnd;
        let selection = unsafe { selection(hwnd) };
        if selection.cp_min >= selection.cp_max {
            return Err("Select text before applying a style.".into());
        }
        let mut format = char_format();
        unsafe {
            SendMessageW(
                hwnd,
                EM_GETCHARFORMAT,
                SCF_SELECTION,
                (&mut format as *mut CharFormat2W) as LPARAM,
            );
        }
        // If every selected character has the effect, remove it; RichEdit
        // reports a cleared mask for a mixed selection, which correctly adds.
        let remove = format.dw_mask & bit != 0 && format.dw_effects & bit != 0;
        let mut update = char_format();
        update.dw_mask = bit;
        update.dw_effects = if remove { 0 } else { bit };
        unsafe {
            SendMessageW(
                hwnd,
                EM_SETCHARFORMAT,
                SCF_SELECTION,
                (&update as *const CharFormat2W) as LPARAM,
            );
            set_selection(hwnd, selection);
            windows_sys::Win32::UI::Input::KeyboardAndMouse::SetFocus(hwnd);
        }
        self.inner.schedule_change();
        Ok(())
    }

    pub fn layout(&self) {
        self.inner.repaint_needed.set(true);
        let mut rect = windows_sys::Win32::Foundation::RECT::default();
        if unsafe { GetClientRect(self.parent, &mut rect) } == 0 {
            return;
        }
        let width = (rect.right - rect.left).max(1);
        let height = (rect.bottom - rect.top).max(1);
        // The Slint layout values are logical px; the child HWND expects
        // physical pixels. Recompute using the current per-window scale so a
        // move across DPI displays does not shrink the editor's hit target.
        let scale = self.inner.scale_factor.get().max(1.0);
        let logical = |value: f32| (value * scale).round() as i32;
        let bounds = self.inner.layout_bounds.get().unwrap_or([
            23.0,
            70.0,
            width as f32 / scale - 46.0,
            height as f32 / scale - 154.0,
        ]);
        let x = logical(bounds[0]).clamp(0, width - 1);
        let y = logical(bounds[1]).clamp(0, height - 1);
        let editor_width = logical(bounds[2]).clamp(0, width - x);
        let editor_height = logical(bounds[3]).clamp(0, height - y);
        unsafe { MoveWindow(self.inner.hwnd, x, y, editor_width, editor_height, 1) };
    }

    /// Use the actual Slint editor rectangle, expressed in logical pixels.
    /// Cache it so the existing geometry poll does not repaint an unchanged
    /// native child every time it checks note positions.
    pub fn set_layout_bounds(&self, bounds: [f32; 4]) {
        if bounds.iter().any(|value| !value.is_finite())
            || bounds[2] < 0.0
            || bounds[3] < 0.0
            || self.inner.layout_bounds.get() == Some(bounds)
        {
            return;
        }
        self.inner.layout_bounds.set(Some(bounds));
        self.layout();
    }

    pub fn set_visible(&self, visible: bool) {
        self.inner.repaint_needed.set(visible);
        unsafe { ShowWindow(self.inner.hwnd, if visible { SW_SHOW } else { SW_HIDE }) };
        unsafe {
            use windows_sys::Win32::Graphics::Gdi::{
                RedrawWindow, RDW_ALLCHILDREN, RDW_ERASE, RDW_INVALIDATE,
            };
            RedrawWindow(
                self.parent,
                std::ptr::null(),
                std::ptr::null_mut(),
                RDW_INVALIDATE | RDW_ERASE | RDW_ALLCHILDREN,
            );
        }
    }

    pub fn client_size(&self) -> [f32; 2] {
        let mut rect = windows_sys::Win32::Foundation::RECT::default();
        unsafe { GetClientRect(self.parent, &mut rect) };
        let scale = self.inner.scale_factor.get().max(1.0);
        [
            (rect.right - rect.left).max(1) as f32 / scale,
            (rect.bottom - rect.top).max(1) as f32 / scale,
        ]
    }

    /// The software backend presents a full-window bitmap directly. Repaint
    /// the child after that present, rather than racing it or invalidating the
    /// parent again (which would create a perpetual redraw loop).
    pub fn repaint_after_parent(&self, after_draw: impl Fn() + 'static) {
        let mut metrics = self.inner.metrics.get();
        metrics.parent_frames += 1;
        self.inner.metrics.set(metrics);
        let needed = self.inner.repaint_needed.replace(false);
        let foreground =
            unsafe { windows_sys::Win32::UI::WindowsAndMessaging::GetForegroundWindow() };
        if foreground != self.parent && !needed {
            return;
        }
        let weak = Rc::downgrade(&self.inner);
        self.inner.paint_timer.start(
            slint::TimerMode::SingleShot,
            Duration::from_millis(1),
            move || {
                after_draw();
                if let Some(inner) = weak.upgrade() {
                    unsafe {
                        use windows_sys::Win32::Graphics::Gdi::{
                            RedrawWindow, RDW_ERASE, RDW_INVALIDATE, RDW_UPDATENOW,
                        };
                        if GetWindowLongPtrW(inner.hwnd, GWL_STYLE) as u32 & WS_VISIBLE != 0 {
                            let started = Instant::now();
                            RedrawWindow(
                                inner.hwnd,
                                std::ptr::null(),
                                std::ptr::null_mut(),
                                RDW_INVALIDATE | RDW_ERASE | RDW_UPDATENOW,
                            );
                            let mut metrics = inner.metrics.get();
                            metrics.child_paint_micros += started.elapsed().as_micros();
                            inner.metrics.set(metrics);
                        }
                    }
                }
            },
        );
    }

    /// Slint 1.9 requests animation frames as fast as the Windows software
    /// backend can consume them (there is no GPU/vsync pacing). Use the public
    /// event filter to pace parent frames, retaining model dirtiness and a
    /// guaranteed deferred Winit request. Native typing itself is not delayed.
    pub fn allow_parent_frame(&self, request_redraw: impl Fn() + 'static) -> bool {
        // Slint's animation driver wakes other note windows as well. They do
        // not need foreground-rate canvas animation while another note is
        // being edited. Native control input/painting stays independent.
        let foreground =
            unsafe { windows_sys::Win32::UI::WindowsAndMessaging::GetForegroundWindow() };
        // The Slint canvas contains only note chrome around the native child.
        // Thirty frames/second is ample for that chrome; RichEdit text input
        // and caret painting bypass this filter entirely.
        let interval = Duration::from_millis(if foreground == self.parent { 32 } else { 200 });
        if let Some(last) = self.inner.last_frame.get() {
            let elapsed = last.elapsed();
            if elapsed < interval {
                if !self.inner.frame_timer.running() {
                    self.inner.frame_timer.start(
                        slint::TimerMode::SingleShot,
                        interval - elapsed,
                        request_redraw,
                    );
                }
                return false;
            }
        }
        self.inner.last_frame.set(Some(Instant::now()));
        true
    }

    pub fn update_scale_factor(&self, scale_factor: f64) {
        self.inner.scale_factor.set(scale_factor as f32);
        self.layout();
    }
}

impl Drop for NativeRichEditor {
    fn drop(&mut self) {
        EDITORS.with(|editors| {
            editors.borrow_mut().remove(&(self.inner.hwnd as isize));
        });
        unsafe {
            SetWindowLongPtrW(
                self.inner.hwnd,
                GWLP_WNDPROC,
                self.inner
                    .original_proc
                    .get()
                    .map_or(0, |p| p as usize as isize),
            );
            DestroyWindow(self.inner.hwnd);
            if self.added_parent_clip {
                let style = GetWindowLongPtrW(self.parent, GWL_STYLE);
                SetWindowLongPtrW(self.parent, GWL_STYLE, style & !(WS_CLIPCHILDREN as isize));
            }
        }
    }
}

unsafe extern "system" fn rich_edit_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    let pointer = EDITORS.with(|editors| editors.borrow().get(&(hwnd as isize)).copied());
    let Some(pointer) = pointer else {
        return 0;
    };
    let original = unsafe { (*pointer).original_proc.get() };
    let inner = &*pointer;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        WM_IME_ENDCOMPOSITION, WM_IME_STARTCOMPOSITION,
    };
    if message == WM_IME_STARTCOMPOSITION {
        inner.composing.set(true);
    }
    if message == WM_IME_ENDCOMPOSITION {
        inner.composing.set(false);
        inner.schedule_change();
    }
    use windows_sys::Win32::UI::{
        Input::KeyboardAndMouse::{GetKeyState, VK_CONTROL, VK_SHIFT},
        WindowsAndMessaging::WM_KEYDOWN,
    };
    let control = GetKeyState(VK_CONTROL as i32) < 0;
    let undo = message == WM_UNDO || (message == WM_KEYDOWN && control && wparam == b'Z' as usize);
    let redo = message == EM_REDO
        || (message == WM_KEYDOWN && control && wparam == b'Y' as usize)
        || (message == WM_KEYDOWN && undo && GetKeyState(VK_SHIFT as i32) < 0);
    if undo || redo {
        inner.change_timer.stop();
        inner.pending_change.set(false);
        let document = document_from_control(hwnd);
        inner.history.borrow_mut().record(document);
        let restored = if redo {
            inner.history.borrow_mut().redo()
        } else {
            inner.history.borrow_mut().undo()
        };
        if let Some(document) = restored {
            inner.suppress_change.set(true);
            set_document(hwnd, &document);
            set_selection(
                hwnd,
                CharRange {
                    cp_min: -1,
                    cp_max: -1,
                },
            );
            inner.suppress_change.set(false);
            inner.emit_change();
        }
        return 0;
    }
    if message == WM_CHAR && matches!(wparam, 8 | 25 | 26) {
        return 0;
    }
    if message == WM_KEYUP && wparam == 13 {
        inner.handled_enter.set(false);
    }
    if message == WM_CHAR && wparam == 13 && inner.handled_enter.get() {
        return 0;
    }
    if matches!(message, WM_CHAR | WM_KEYDOWN)
        && wparam == 13
        && !inner.composing.get()
        && GetKeyState(VK_SHIFT as i32) >= 0
    {
        let document = document_from_control(hwnd);
        let range = selection(hwnd);
        let projection = control_projection(hwnd);
        let start = projection.to_source(range.cp_min.max(0) as usize);
        let end = projection.to_source(range.cp_max.max(0) as usize);
        if let Some(result) = crate::list_command(&document.text, "enter", start, end) {
            // RichEdit can process Return during key-down, before WM_CHAR.
            // Handle the physical key once and swallow its translated char.
            inner.handled_enter.set(message == WM_KEYDOWN);
            let mut formats = crate::FormatRanges {
                bold: document.bold.clone(),
                italic: document.italic.clone(),
                underline: document.underline.clone(),
                strike: document.strike.clone(),
            };
            crate::remap_formats_after_replacements(
                &document.text,
                &result.replacements,
                &mut formats,
            );
            let next = NativeDocument {
                text: result.source,
                bold: formats.bold,
                italic: formats.italic,
                underline: formats.underline,
                strike: formats.strike,
                chip_ids: document.chip_ids.clone(),
            };
            inner.history.borrow_mut().record(document);
            inner.history.borrow_mut().record(next.clone());
            inner.suppress_change.set(true);
            set_document(hwnd, &next);
            let caret = control_projection(hwnd).to_control(result.caret) as i32;
            set_selection(
                hwnd,
                CharRange {
                    cp_min: caret,
                    cp_max: caret,
                },
            );
            inner.suppress_change.set(false);
            inner.schedule_change();
            return 0;
        }
    }
    use windows_sys::Win32::UI::{
        Input::KeyboardAndMouse::{VK_BACK, VK_DELETE},
        WindowsAndMessaging::WM_LBUTTONUP,
    };
    let is_delete = message == WM_KEYDOWN && wparam == VK_DELETE as usize;
    let backwards = message == WM_KEYDOWN && wparam == VK_BACK as usize;
    if matches!(message, WM_CHAR | WM_PASTE | WM_CUT | WM_CLEAR) || is_delete || backwards {
        let projection = control_projection(hwnd);
        let mut range = selection(hwnd);
        inner.last_edit_at.set(Some(range.cp_min.max(0) as usize));
        for chip in &projection.chips {
            let start = chip.control_start as i32;
            let end = chip.control_end as i32;
            if range.cp_min != range.cp_max && range.cp_min < end && range.cp_max > start {
                range.cp_min = range.cp_min.min(start);
                range.cp_max = range.cp_max.max(end);
            } else if range.cp_min == range.cp_max {
                let point = range.cp_min;
                if (backwards && point > start && point <= end)
                    || (is_delete && point >= start && point < end)
                {
                    range = CharRange {
                        cp_min: start,
                        cp_max: end,
                    };
                } else if point > start && point < end {
                    range = CharRange {
                        cp_min: end,
                        cp_max: end,
                    };
                }
            }
        }
        set_selection(hwnd, range);
        let mut reset = char_format();
        reset.dw_mask = 0x100 | 0x10 | 0x0400_0000;
        reset.dw_effects = 0x0400_0000;
        SendMessageW(
            hwnd,
            EM_SETCHARFORMAT,
            SCF_SELECTION,
            (&reset as *const CharFormat2W) as LPARAM,
        );
        if (is_delete || backwards) && range.cp_min != range.cp_max {
            CallWindowProcW(original, hwnd, WM_CLEAR, 0, 0);
            inner.schedule_change();
            return 0;
        }
    }
    if message == 194 {
        inner
            .last_edit_at
            .set(Some(selection(hwnd).cp_min.max(0) as usize));
    }
    let result = unsafe { CallWindowProcW(original, hwnd, message, wparam, lparam) };
    if message == WM_LBUTTONUP {
        let projection = control_projection(hwnd);
        let point = selection(hwnd).cp_min.max(0) as usize;
        if let Some((index, chip)) = projection
            .chips
            .iter()
            .enumerate()
            .find(|(_, chip)| point >= chip.hidden_end && point < chip.control_end)
        {
            set_selection(
                hwnd,
                CharRange {
                    cp_min: chip.control_start as i32,
                    cp_max: chip.control_end as i32,
                },
            );
            let bounds = inner.layout_bounds.get().unwrap_or([0.0; 4]);
            let scale = inner.scale_factor.get().max(1.0);
            let x = (lparam as u32 & 0xffff) as i16 as f32 / scale + bounds[0];
            let y = ((lparam as u32 >> 16) & 0xffff) as i16 as f32 / scale + bounds[1];
            if let Some(callback) = inner.chip_clicked.borrow_mut().as_mut() {
                callback(index as i32, x, y);
            }
        }
    }
    if is_delete
        || backwards
        || matches!(
            message,
            WM_CHAR
                | WM_PASTE
                | WM_CUT
                | WM_CLEAR
                | WM_UNDO
                | EM_REDO
                | WM_IME_COMPOSITION
                | WM_KEYUP
                | WM_KILLFOCUS
        )
    {
        let inner = unsafe { &*pointer };
        if !inner.suppress_change.get() && !inner.composing.get() {
            // RichEdit sends several messages per edit (and IME composition
            // sends transient text). Snapshot once after input settles rather
            // than walking every character during each key-up.
            inner.schedule_change();
        }
    }
    result
}

fn document_weight(document: &NativeDocument) -> usize {
    document.text.len()
        + (document.bold.len()
            + document.italic.len()
            + document.underline.len()
            + document.strike.len())
            * 16
}

fn parent_hwnd(window: &slint::Window) -> Result<HWND, String> {
    let slint_handle = window.window_handle();
    let handle = slint_handle
        .window_handle()
        .map_err(|_| "Slint has not created a native window handle yet".to_owned())?;
    match handle.as_raw() {
        RawWindowHandle::Win32(handle) => Ok(handle.hwnd.get() as HWND),
        _ => Err("The native RichEdit host is only available on Win32".into()),
    }
}

fn style_bit(name: &str) -> Option<u32> {
    match name {
        "bold" => Some(CFM_BOLD),
        "italic" => Some(CFM_ITALIC),
        "underline" => Some(CFM_UNDERLINE),
        "strike" => Some(CFM_STRIKEOUT),
        _ => None,
    }
}

fn char_format() -> CharFormat2W {
    CharFormat2W {
        cb_size: size_of::<CharFormat2W>() as u32,
        ..Default::default()
    }
}

unsafe fn selection(hwnd: HWND) -> CharRange {
    let mut range = CharRange {
        cp_min: 0,
        cp_max: 0,
    };
    SendMessageW(
        hwnd,
        EM_EXGETSEL,
        0,
        (&mut range as *mut CharRange) as LPARAM,
    );
    range
}

unsafe fn set_selection(hwnd: HWND, range: CharRange) {
    SendMessageW(hwnd, EM_EXSETSEL, 0, (&range as *const CharRange) as LPARAM);
}

unsafe fn set_document(hwnd: HWND, document: &NativeDocument) {
    let _redraw = RedrawBatch::new(hwnd);
    let projection = crate::native_projection::Projection::from_source_visible(
        &document.text,
        &document.chip_ids,
    );
    let control_text = projection.control.replace('\n', "\r\n");
    let mut wide: Vec<u16> = control_text.encode_utf16().collect();
    wide.push(0);
    SendMessageW(hwnd, WM_SETTEXT, 0, wide.as_ptr() as LPARAM);
    // WM_SETTEXT can inherit the previous insertion format. Replacements are
    // complete documents: clear owned effects before applying explicit ranges.
    set_selection(
        hwnd,
        CharRange {
            cp_min: 0,
            cp_max: -1,
        },
    );
    let mut reset = char_format();
    reset.dw_mask =
        CFM_BOLD | CFM_ITALIC | CFM_UNDERLINE | CFM_STRIKEOUT | 0x100 | 0x10 | 0x0400_0000;
    reset.dw_effects = 0x0400_0000; // automatic background, not an inherited chip fill
    SendMessageW(
        hwnd,
        EM_SETCHARFORMAT,
        SCF_SELECTION,
        (&reset as *const CharFormat2W) as LPARAM,
    );
    for (name, ranges) in [
        ("bold", &document.bold),
        ("italic", &document.italic),
        ("underline", &document.underline),
        ("strike", &document.strike),
    ] {
        let Some(bit) = style_bit(name) else { continue };
        for &(start, end) in ranges {
            if start >= end
                || end > document.text.len()
                || !document.text.is_char_boundary(start)
                || !document.text.is_char_boundary(end)
            {
                continue;
            }
            let range = CharRange {
                cp_min: projection.to_control(start) as i32,
                cp_max: projection.to_control(end) as i32,
            };
            set_selection(hwnd, range);
            let mut format = char_format();
            format.dw_mask = bit;
            format.dw_effects = bit;
            SendMessageW(
                hwnd,
                EM_SETCHARFORMAT,
                SCF_SELECTION,
                (&format as *const CharFormat2W) as LPARAM,
            );
        }
    }
    style_chips(hwnd, &projection);
    track_chips(hwnd, &projection);
    let palette = EDITORS
        .with(|editors| editors.borrow().get(&(hwnd as isize)).copied())
        .and_then(|inner| (*inner).palette.get());
    if let Some(rgb) = palette {
        apply_palette(hwnd, rgb);
    }
    set_selection(
        hwnd,
        CharRange {
            cp_min: 0,
            cp_max: 0,
        },
    );
}

unsafe fn track_chips(hwnd: HWND, projection: &crate::native_projection::Projection) {
    EDITORS.with(|editors| {
        let Some(pointer) = editors.borrow().get(&(hwnd as isize)).copied() else {
            return;
        };
        let inner = &*pointer;
        let Some(tom) = inner.text_document.as_ref() else {
            return;
        };
        let mut tracked = Vec::with_capacity(projection.chips.len());
        for chip in &projection.chips {
            let marker = &projection.source[chip.source_start..chip.source_end];
            let Some(date) = marker
                .strip_prefix("[[date:")
                .and_then(|value| value.strip_suffix("]]"))
            else {
                continue;
            };
            if let Ok(range) = tom.Range(chip.control_start as i32, chip.control_end as i32) {
                tracked.push(TrackedNativeChip {
                    range,
                    id: chip.id.clone(),
                    date: date.to_owned(),
                });
            }
        }
        *inner.tracked_chips.borrow_mut() = tracked;
        inner.last_edit_at.set(None);
    });
}

unsafe fn text_object_document(hwnd: HWND) -> Option<ITextDocument> {
    let mut pointer: *mut std::ffi::c_void = std::ptr::null_mut();
    if SendMessageW(
        hwnd,
        1084,
        0,
        (&mut pointer as *mut *mut std::ffi::c_void) as LPARAM,
    ) == 0
        || pointer.is_null()
    {
        return None;
    }
    // EM_GETOLEINTERFACE returns an AddRef-owned interface. The typed owner
    // releases it after QueryInterface; ITextDocument retains its own ref.
    let ole = IUnknown::from_raw(pointer);
    ole.cast().ok()
}

unsafe fn apply_palette(hwnd: HWND, rgb: u32) {
    let previous = selection(hwnd);
    let _redraw = RedrawBatch::new(hwnd);
    let colorref = ((rgb >> 16) & 255) | (rgb & 0xff00) | ((rgb & 255) << 16);
    SendMessageW(hwnd, EM_SETBKGNDCOLOR, 0, colorref as LPARAM);
    set_selection(
        hwnd,
        CharRange {
            cp_min: 0,
            cp_max: -1,
        },
    );
    let mut format = char_format();
    format.dw_mask = 0x4000_0000 | 0x2000_0000 | 0x8000_0000;
    format.cr_text_color = if rgb == 0x6f7078 {
        0x00f6f5f5
    } else {
        0x003d3432
    };
    format.y_height = 220;
    for (index, unit) in "Segoe UI".encode_utf16().enumerate() {
        format.sz_face_name[index] = unit;
    }
    SendMessageW(
        hwnd,
        EM_SETCHARFORMAT,
        SCF_SELECTION,
        (&format as *const CharFormat2W) as LPARAM,
    );
    style_chips(hwnd, &control_projection(hwnd));
    set_selection(hwnd, previous);
}

unsafe fn control_projection(hwnd: HWND) -> crate::native_projection::Projection {
    let length = SendMessageW(hwnd, WM_GETTEXTLENGTH, 0, 0).max(0) as usize;
    let mut buffer = vec![0u16; length + 1];
    SendMessageW(
        hwnd,
        WM_GETTEXT,
        buffer.len(),
        buffer.as_mut_ptr() as LPARAM,
    );
    let end = buffer
        .iter()
        .position(|&unit| unit == 0)
        .unwrap_or(buffer.len());
    let raw = String::from_utf16_lossy(&buffer[..end]).replace("\r\n", "\n");
    let tracked = EDITORS.with(|editors| {
        let Some(pointer) = editors.borrow().get(&(hwnd as isize)).copied() else {
            return Vec::new();
        };
        let inner = &*pointer;
        inner
            .tracked_chips
            .borrow()
            .iter()
            .filter_map(|chip| {
                let start = chip.range.GetStart().ok()?;
                let end = chip.range.GetEnd().ok()?;
                (start >= 0 && end > start).then(|| crate::native_projection::TrackedChip {
                    control_start: start as usize,
                    control_end: end as usize,
                    id: chip.id.clone(),
                    date: chip.date.clone(),
                })
            })
            .collect()
    });
    let edit_at = EDITORS.with(|editors| {
        editors
            .borrow()
            .get(&(hwnd as isize))
            .map(|pointer| (*(*pointer)).last_edit_at.get())
            .flatten()
    });
    crate::native_projection::Projection::from_visible_control_with_edit(&raw, &tracked, edit_at)
}

unsafe fn document_from_control(hwnd: HWND) -> NativeDocument {
    let projection = control_projection(hwnd);
    let mut document = NativeDocument {
        text: projection.source.clone(),
        chip_ids: projection.ids(),
        ..Default::default()
    };
    let spans: Vec<_> = document
        .text
        .char_indices()
        .filter_map(|(start, character)| {
            if projection
                .chips
                .iter()
                .any(|chip| start >= chip.source_start && start < chip.source_end)
            {
                return None;
            }
            let end = start + character.len_utf8();
            Some((
                start,
                end,
                projection.to_control(start),
                projection.to_control(end),
            ))
        })
        .collect();
    // Query homogeneous ranges as a whole, splitting only mixed ranges. A
    // long plain note now needs one format query rather than one per glyph.
    // Preserve the native selection once for the entire snapshot.
    let previous = selection(hwnd);
    let tom = EDITORS
        .with(|editors| editors.borrow().get(&(hwnd as isize)).copied())
        .and_then(|inner| (*inner).text_document.clone());
    let _redraw = tom.is_none().then(|| RedrawBatch::new(hwnd));
    collect_range_effects(hwnd, tom.as_ref(), &spans, &mut document);
    // A TOM query can fail even when the interface exists. The message-based
    // fallback moves the selection; restore it without disturbing a successful
    // TOM-only snapshot's caret.
    let current = selection(hwnd);
    if current.cp_min != previous.cp_min || current.cp_max != previous.cp_max {
        set_selection(hwnd, previous);
    }
    if projection.repaired {
        let start = projection.to_source(previous.cp_min.max(0) as usize);
        let end = projection.to_source(previous.cp_max.max(0) as usize);
        set_document(hwnd, &document);
        let rendered = crate::native_projection::Projection::from_source_visible(
            &document.text,
            &document.chip_ids,
        );
        set_selection(
            hwnd,
            CharRange {
                cp_min: rendered.to_control(start) as i32,
                cp_max: rendered.to_control(end) as i32,
            },
        );
    }
    document
}

unsafe fn style_chips(hwnd: HWND, projection: &crate::native_projection::Projection) {
    for chip in &projection.chips {
        set_selection(
            hwnd,
            CharRange {
                cp_min: chip.hidden_end as i32,
                cp_max: chip.control_end as i32,
            },
        );
        let mut label = char_format();
        label.dw_mask = 0x4000_0000 | 0x0400_0000 | CFM_BOLD | 0x100;
        label.dw_effects = CFM_BOLD;
        label.cr_text_color = 0x006f4b2b;
        label.cr_back_color = 0x00ffe8dc;
        SendMessageW(
            hwnd,
            EM_SETCHARFORMAT,
            SCF_SELECTION,
            (&label as *const CharFormat2W) as LPARAM,
        );
    }
}

/// Selecting format-query ranges must not paint every intermediate selection.
/// Preserve hidden child state: WM_SETREDRAW(true) would otherwise show an
/// editor intentionally hidden beneath a date/menu overlay.
struct RedrawBatch {
    hwnd: HWND,
    enabled: bool,
}

impl RedrawBatch {
    unsafe fn new(hwnd: HWND) -> Self {
        use windows_sys::Win32::UI::WindowsAndMessaging::{
            GetWindowLongPtrW, GWL_STYLE, WM_SETREDRAW,
        };
        let enabled = GetWindowLongPtrW(hwnd, GWL_STYLE) as u32 & WS_VISIBLE != 0;
        if enabled {
            SendMessageW(hwnd, WM_SETREDRAW, 0, 0);
        }
        Self { hwnd, enabled }
    }
}

impl Drop for RedrawBatch {
    fn drop(&mut self) {
        if self.enabled {
            unsafe {
                use windows_sys::Win32::Graphics::Gdi::{
                    RedrawWindow, RDW_ALLCHILDREN, RDW_ERASE, RDW_FRAME, RDW_INVALIDATE,
                };
                SendMessageW(
                    self.hwnd,
                    windows_sys::Win32::UI::WindowsAndMessaging::WM_SETREDRAW,
                    1,
                    0,
                );
                RedrawWindow(
                    self.hwnd,
                    std::ptr::null(),
                    std::ptr::null_mut(),
                    RDW_ALLCHILDREN | RDW_ERASE | RDW_FRAME | RDW_INVALIDATE,
                );
            }
        }
    }
}

unsafe fn collect_range_effects(
    hwnd: HWND,
    tom: Option<&ITextDocument>,
    spans: &[(usize, usize, usize, usize)],
    document: &mut NativeDocument,
) {
    let (Some(first), Some(last)) = (spans.first(), spans.last()) else {
        return;
    };
    let mut format = char_format();
    let queried = tom
        .and_then(|doc| {
            let font = doc
                .Range(first.2 as i32, last.3 as i32)
                .ok()?
                .GetFont()
                .ok()?;
            let values = [
                font.GetBold().ok()?,
                font.GetItalic().ok()?,
                font.GetUnderline().ok()?,
                font.GetStrikeThrough().ok()?,
            ];
            for (value, bit) in
                values
                    .into_iter()
                    .zip([CFM_BOLD, CFM_ITALIC, CFM_UNDERLINE, CFM_STRIKEOUT])
            {
                if value != -9999999 {
                    format.dw_mask |= bit;
                    if value != 0 {
                        format.dw_effects |= bit;
                    }
                }
            }
            Some(())
        })
        .is_some();
    if !queried {
        set_selection(
            hwnd,
            CharRange {
                cp_min: first.2 as i32,
                cp_max: last.3 as i32,
            },
        );
        SendMessageW(
            hwnd,
            EM_GETCHARFORMAT,
            SCF_SELECTION,
            (&mut format as *mut CharFormat2W) as LPARAM,
        );
    }
    let supported = CFM_BOLD | CFM_ITALIC | CFM_UNDERLINE | CFM_STRIKEOUT;
    if spans.len() > 1 && format.dw_mask & supported != supported {
        let middle = spans.len() / 2;
        collect_range_effects(hwnd, tom, &spans[..middle], document);
        collect_range_effects(hwnd, tom, &spans[middle..], document);
        return;
    }
    // RichEdit can retain effect bits which are not valid for the queried
    // selection. `dwMask` identifies the effects actually supplied.
    let effects = format.dw_effects & format.dw_mask;
    push_effect_range(&mut document.bold, effects & CFM_BOLD != 0, first.0, last.1);
    push_effect_range(
        &mut document.italic,
        effects & CFM_ITALIC != 0,
        first.0,
        last.1,
    );
    push_effect_range(
        &mut document.underline,
        effects & CFM_UNDERLINE != 0,
        first.0,
        last.1,
    );
    push_effect_range(
        &mut document.strike,
        effects & CFM_STRIKEOUT != 0,
        first.0,
        last.1,
    );
}

fn push_effect_range(ranges: &mut Vec<(usize, usize)>, enabled: bool, start: usize, end: usize) {
    if !enabled {
        return;
    }
    if let Some((_, previous_end)) = ranges.last_mut() {
        if *previous_end == start {
            *previous_end = end;
            return;
        }
    }
    ranges.push((start, end));
}

#[cfg(test)]
fn source_offset_to_control_utf16(source: &str, byte_offset: usize) -> usize {
    source[..byte_offset].chars().map(char::len_utf16).sum()
}

#[cfg(test)]
fn control_utf16_to_source_offset(source: &str, target: usize) -> usize {
    let mut position = 0;
    for (byte, character) in source.char_indices() {
        let width = character.len_utf16();
        if target < position + width {
            return byte;
        }
        position += width;
    }
    source.len()
}

#[cfg(test)]
mod tests {
    use super::{source_offset_to_control_utf16, NativeDocument, NativeRichEditor};
    use std::{cell::RefCell, rc::Rc};
    use windows_sys::Win32::{
        Foundation::HWND,
        UI::WindowsAndMessaging::{CreateWindowExW, DestroyWindow, SendMessageW, WS_OVERLAPPED},
    };

    struct TestParent(HWND, [u8; 256]);
    // Production owns all controls on one UI thread. Keep focus/IME replay
    // tests sequential rather than exercising multiple Windows GUI queues.
    static NATIVE_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    fn native_test_guard() -> std::sync::MutexGuard<'static, ()> {
        NATIVE_TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    unsafe fn visible_text(hwnd: HWND) -> String {
        #[repr(C)]
        struct GetTextEx {
            cb: u32,
            flags: u32,
            codepage: u32,
            default_char: *const u8,
            used_default: *mut i32,
        }
        let mut buffer = vec![0u16; 2048];
        let request = GetTextEx {
            cb: (buffer.len() * 2) as u32,
            flags: 8,
            codepage: 1200,
            default_char: std::ptr::null(),
            used_default: std::ptr::null_mut(),
        };
        SendMessageW(
            hwnd,
            1118,
            &request as *const GetTextEx as usize,
            buffer.as_mut_ptr() as isize,
        );
        String::from_utf16_lossy(&buffer[..buffer.iter().position(|unit| *unit == 0).unwrap()])
    }

    #[test]
    fn native_inline_dates_render_labels_keep_duplicate_ids_and_delete_atomically() {
        let _guard = native_test_guard();
        let parent = TestParent::new();
        let source = "A [[date:2026-09-27T17:30]] B [[date:2026-09-27T17:30]]";
        let ids = vec![
            uuid::Uuid::new_v4().to_string(),
            uuid::Uuid::new_v4().to_string(),
        ];
        let original = NativeDocument {
            text: source.into(),
            chip_ids: ids.clone(),
            ..Default::default()
        };
        let editor =
            NativeRichEditor::attach_to_parent(parent.0, 1.0, original.clone(), |_| {}).unwrap();
        let visible = unsafe { visible_text(editor.inner.hwnd) };
        assert!(
            !visible.contains("[["),
            "hidden metadata must never render: {visible}"
        );
        let raw = unsafe { super::control_projection(editor.inner.hwnd).control };
        assert!(!raw.contains("[[chip:"));
        assert!(!raw.contains(&ids[0]));
        assert!(!raw.contains(&ids[1]));
        assert!(visible.contains("Sep 27 · 17:30"));
        assert_eq!(editor.document(), original);
        editor.restore_selection(source, 0, 0);
        unsafe {
            SendMessageW(editor.inner.hwnd, super::WM_CHAR, 'X' as usize, 0);
        }
        editor.flush_pending_change();
        let changed = editor.document();
        assert_eq!(changed.chip_ids, ids);
        let second = crate::date_chip_occurrences(&changed.text)[1].clone();
        editor.restore_selection(&changed.text, second.end, second.end);
        unsafe {
            SendMessageW(
                editor.inner.hwnd,
                windows_sys::Win32::UI::WindowsAndMessaging::WM_KEYDOWN,
                windows_sys::Win32::UI::Input::KeyboardAndMouse::VK_BACK as usize,
                0,
            );
        }
        editor.flush_pending_change();
        let deleted = editor.document();
        let mut expected = changed.text.clone();
        expected.replace_range(second.start..second.end, "");
        assert_eq!(deleted.text, expected);
        assert_eq!(deleted.chip_ids, vec![ids[0].clone()]);
        unsafe {
            SendMessageW(editor.inner.hwnd, super::WM_UNDO, 0, 0);
        }
        assert_eq!(editor.document(), changed);
    }

    #[test]
    fn tom_chip_range_tracks_text_inserted_before_it() {
        let _guard = native_test_guard();
        let parent = TestParent::new();
        let editor = NativeRichEditor::attach_to_parent(
            parent.0,
            1.0,
            NativeDocument {
                text: "A [[date:2026-09-27]] B".into(),
                ..Default::default()
            },
            |_| {},
        )
        .unwrap();
        unsafe {
            let chip = super::control_projection(editor.inner.hwnd).chips.remove(0);
            let tom = super::text_object_document(editor.inner.hwnd).unwrap();
            let tracked = tom
                .Range(chip.hidden_end as i32, chip.control_end as i32)
                .unwrap();
            let start = tracked.GetStart().unwrap();
            super::set_selection(
                editor.inner.hwnd,
                super::CharRange {
                    cp_min: 0,
                    cp_max: 0,
                },
            );
            SendMessageW(editor.inner.hwnd, super::WM_CHAR, 'X' as usize, 0);
            assert_eq!(tracked.GetStart().unwrap(), start + 1);
        }
    }

    #[test]
    fn native_checked_task_enter_continues_unchecked_exits_empty_and_undoes() {
        let _guard = native_test_guard();
        let parent = TestParent::new();
        let original = NativeDocument {
            text: "[x] finished".into(),
            ..Default::default()
        };
        let editor =
            NativeRichEditor::attach_to_parent(parent.0, 1.0, original.clone(), |_| {}).unwrap();
        editor.restore_selection(&original.text, original.text.len(), original.text.len());
        unsafe {
            SendMessageW(editor.inner.hwnd, super::WM_CHAR, 13, 0);
        }
        editor.flush_pending_change();
        assert_eq!(editor.document().text, "[x] finished\n[ ] ");
        unsafe {
            SendMessageW(editor.inner.hwnd, super::WM_CHAR, 13, 0);
        }
        editor.flush_pending_change();
        assert_eq!(editor.document().text, "[x] finished\n");
        unsafe {
            SendMessageW(editor.inner.hwnd, super::WM_UNDO, 0, 0);
        }
        assert_eq!(editor.document().text, "[x] finished\n[ ] ");
        unsafe {
            SendMessageW(editor.inner.hwnd, super::WM_UNDO, 0, 0);
        }
        assert_eq!(editor.document(), original);
    }

    #[test]
    fn native_physical_enter_sequence_continues_a_task_only_once() {
        let _guard = native_test_guard();
        let parent = TestParent::new();
        let original = NativeDocument {
            text: "[x] finished".into(),
            ..Default::default()
        };
        let editor =
            NativeRichEditor::attach_to_parent(parent.0, 1.0, original.clone(), |_| {}).unwrap();
        editor.restore_selection(&original.text, original.text.len(), original.text.len());
        unsafe {
            SendMessageW(
                editor.inner.hwnd,
                windows_sys::Win32::UI::WindowsAndMessaging::WM_KEYDOWN,
                13,
                0,
            );
            SendMessageW(editor.inner.hwnd, super::WM_CHAR, 13, 0);
            SendMessageW(editor.inner.hwnd, super::WM_KEYUP, 13, 0);
        }
        editor.flush_pending_change();
        assert_eq!(editor.document().text, "[x] finished\n[ ] ");
        unsafe {
            SendMessageW(
                editor.inner.hwnd,
                windows_sys::Win32::UI::WindowsAndMessaging::WM_KEYDOWN,
                13,
                0,
            );
            SendMessageW(editor.inner.hwnd, super::WM_CHAR, 13, 0);
            SendMessageW(editor.inner.hwnd, super::WM_KEYUP, 13, 0);
        }
        editor.flush_pending_change();
        assert_eq!(editor.document().text, "[x] finished\n");
    }

    #[test]
    fn ime_defers_notifications_but_explicit_save_drains_composition() {
        let _guard = native_test_guard();
        let parent = TestParent::new();
        let seen = Rc::new(RefCell::new(Vec::new()));
        let changes = seen.clone();
        let editor = NativeRichEditor::attach_to_parent(
            parent.0,
            1.0,
            NativeDocument::default(),
            move |doc| changes.borrow_mut().push(doc),
        )
        .unwrap();
        unsafe {
            SendMessageW(
                editor.inner.hwnd,
                windows_sys::Win32::UI::WindowsAndMessaging::WM_IME_STARTCOMPOSITION,
                0,
                0,
            );
            // Supply committed Unicode through RichEdit's replacement API.
            // A fake START followed by WM_CHAR is not a real IME commit and
            // RichEdit legitimately ignores it. This tests our notification
            // boundary, not an installed language's composition engine.
            let committed: Vec<_> = "界".encode_utf16().chain(Some(0)).collect();
            SendMessageW(
                editor.inner.hwnd,
                194,
                1,
                committed.as_ptr() as super::LPARAM,
            );
        }
        editor.inner.emit_change();
        assert!(seen.borrow().is_empty());
        editor.flush_pending_change();
        assert_eq!(seen.borrow().last().unwrap().text, "界");
        assert!(!editor.inner.composing.get());
    }

    #[test]
    fn native_raw_date_insertion_projects_without_stealing_existing_ids() {
        let _guard = native_test_guard();
        let parent = TestParent::new();
        let id = uuid::Uuid::new_v4().to_string();
        let original = NativeDocument {
            text: "[[date:2026-09-27]]".into(),
            chip_ids: vec![id.clone()],
            ..Default::default()
        };
        let editor = NativeRichEditor::attach_to_parent(parent.0, 1.0, original, |_| {}).unwrap();
        editor.restore_selection("[[date:2026-09-27]]", 0, 0);
        let paste: Vec<_> = "[[date:2026-09-27]] "
            .encode_utf16()
            .chain(Some(0))
            .collect();
        unsafe {
            SendMessageW(editor.inner.hwnd, 194, 1, paste.as_ptr() as super::LPARAM);
        }
        let doc = editor.document();
        assert_eq!(doc.text, "[[date:2026-09-27]] [[date:2026-09-27]]");
        assert_eq!(doc.chip_ids[1], id);
        assert_ne!(doc.chip_ids[0], id);
        assert_eq!(editor.document(), doc);
    }

    #[test]
    fn pasted_visible_date_text_does_not_steal_an_existing_chip_id() {
        let _guard = native_test_guard();
        let parent = TestParent::new();
        let id = uuid::Uuid::new_v4().to_string();
        let source = "[[date:2026-09-27]]";
        let editor = NativeRichEditor::attach_to_parent(
            parent.0,
            1.0,
            NativeDocument {
                text: source.into(),
                chip_ids: vec![id.clone()],
                ..Default::default()
            },
            |_| {},
        )
        .unwrap();
        editor.restore_selection(source, source.len(), source.len());
        let paste: Vec<_> = " Sep 27 ".encode_utf16().chain(Some(0)).collect();
        unsafe {
            SendMessageW(editor.inner.hwnd, 194, 1, paste.as_ptr() as super::LPARAM);
        }
        let doc = editor.document();
        assert_eq!(doc.text, "[[date:2026-09-27]] Sep 27 ");
        assert_eq!(doc.chip_ids, vec![id.clone()]);
        editor.set_document(&NativeDocument {
            text: source.into(),
            chip_ids: doc.chip_ids,
            ..Default::default()
        });
        editor.restore_selection(source, 0, 0);
        unsafe {
            SendMessageW(editor.inner.hwnd, 194, 1, paste.as_ptr() as super::LPARAM);
        }
        let before = editor.document();
        assert_eq!(before.text, " Sep 27 [[date:2026-09-27]]");
        assert_eq!(before.chip_ids, vec![id]);
    }

    #[test]
    fn native_palette_and_font_survive_replacement_and_undo() {
        let _guard = native_test_guard();
        let parent = TestParent::new();
        let editor = NativeRichEditor::attach_to_parent(
            parent.0,
            1.0,
            NativeDocument {
                text: "original".into(),
                ..Default::default()
            },
            |_| {},
        )
        .unwrap();
        editor.set_palette("#6F7078");
        assert!(
            editor.inner.text_document.is_some(),
            "Windows RichEdit must supply TOM for the measured path"
        );
        editor.set_document(&NativeDocument {
            text: "replacement".into(),
            ..Default::default()
        });
        for undo in [false, true] {
            if undo {
                editor.history_action(false);
            }
            unsafe {
                super::set_selection(
                    editor.inner.hwnd,
                    super::CharRange {
                        cp_min: 0,
                        cp_max: -1,
                    },
                );
                let mut font = super::char_format();
                SendMessageW(
                    editor.inner.hwnd,
                    super::EM_GETCHARFORMAT,
                    super::SCF_SELECTION,
                    (&mut font as *mut super::CharFormat2W) as super::LPARAM,
                );
                assert_eq!(font.y_height, 220);
                assert_eq!(font.cr_text_color, 0x00f6f5f5);
                let previous =
                    SendMessageW(editor.inner.hwnd, super::EM_SETBKGNDCOLOR, 0, 0x0078706f);
                assert_eq!(previous, 0x0078706f);
            }
        }
    }

    #[test]
    fn native_zero_bounds_clear_a_previous_large_rectangle_and_recover() {
        let _guard = native_test_guard();
        let parent = TestParent::new();
        let editor =
            NativeRichEditor::attach_to_parent(parent.0, 1.0, NativeDocument::default(), |_| {})
                .unwrap();
        editor.set_layout_bounds([10.0, 10.0, 200.0, 140.0]);
        editor.set_layout_bounds([10.0, 10.0, 0.0, 0.0]);
        let mut rect = windows_sys::Win32::Foundation::RECT::default();
        unsafe {
            windows_sys::Win32::UI::WindowsAndMessaging::GetWindowRect(
                editor.inner.hwnd,
                &mut rect,
            );
        }
        assert_eq!((rect.right - rect.left, rect.bottom - rect.top), (0, 0));
        editor.set_layout_bounds([10.0, 10.0, 120.0, 60.0]);
        unsafe {
            windows_sys::Win32::UI::WindowsAndMessaging::GetWindowRect(
                editor.inner.hwnd,
                &mut rect,
            );
        }
        assert_eq!((rect.right - rect.left, rect.bottom - rect.top), (120, 60));
    }

    #[test]
    fn native_geometry_tracks_logical_bounds_at_multiple_scale_factors() {
        let _guard = native_test_guard();
        let parent = TestParent::new();
        let editor =
            NativeRichEditor::attach_to_parent(parent.0, 1.0, NativeDocument::default(), |_| {})
                .unwrap();
        editor.set_layout_bounds([10.0, 20.0, 180.0, 120.0]);
        for scale in [1.0, 1.5, 2.0] {
            editor.update_scale_factor(scale);
            let mut rect = windows_sys::Win32::Foundation::RECT::default();
            unsafe {
                super::GetClientRect(editor.inner.hwnd, &mut rect);
            }
            // The child has a native vertical scrollbar, so client width
            // excludes it. Verify the outer rectangle's scaled dimensions.
            unsafe {
                windows_sys::Win32::UI::WindowsAndMessaging::GetWindowRect(
                    editor.inner.hwnd,
                    &mut rect,
                );
            }
            assert_eq!(rect.right - rect.left, (180.0 * scale) as i32);
            assert_eq!(rect.bottom - rect.top, (120.0 * scale) as i32);
        }
    }

    impl TestParent {
        fn new() -> Self {
            // Win32 keyboard state is per-thread. Isolate replay tests from
            // physical modifiers held by the user while the suite is running.
            let mut keyboard = [0u8; 256];
            unsafe {
                windows_sys::Win32::UI::Input::KeyboardAndMouse::GetKeyboardState(
                    keyboard.as_mut_ptr(),
                );
                windows_sys::Win32::UI::Input::KeyboardAndMouse::SetKeyboardState(
                    [0u8; 256].as_ptr(),
                );
            }
            let class: Vec<u16> = "STATIC\0".encode_utf16().collect();
            let hwnd = unsafe {
                CreateWindowExW(
                    0,
                    class.as_ptr(),
                    std::ptr::null(),
                    WS_OVERLAPPED,
                    0,
                    0,
                    640,
                    480,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null(),
                )
            };
            assert!(!hwnd.is_null());
            Self(hwnd, keyboard)
        }
    }

    impl Drop for TestParent {
        fn drop(&mut self) {
            unsafe {
                DestroyWindow(self.0);
                windows_sys::Win32::UI::Input::KeyboardAndMouse::SetKeyboardState(self.1.as_ptr());
            }
        }
    }

    struct TestDatabase(std::path::PathBuf);

    impl TestDatabase {
        fn new() -> Self {
            Self(std::env::temp_dir().join(format!("sticky-rich-test-{}", uuid::Uuid::new_v4())))
        }
    }

    impl Drop for TestDatabase {
        fn drop(&mut self) {
            // Only this test's uniquely named database files, never user data.
            for name in [
                "sticky-notes.db",
                "sticky-notes.db-wal",
                "sticky-notes.db-shm",
            ] {
                let _ = std::fs::remove_file(self.0.join(name));
            }
            let _ = std::fs::remove_dir(&self.0);
        }
    }

    fn content_from_document(document: &NativeDocument) -> String {
        crate::native_projection::with_ids(
            &crate::content_for_editor_with_checklists_and_formats(
                &document.text,
                false,
                &crate::FormatRanges {
                    bold: document.bold.clone(),
                    italic: document.italic.clone(),
                    underline: document.underline.clone(),
                    strike: document.strike.clone(),
                },
            ),
            &document.chip_ids,
        )
    }

    fn document_from_content(content: &str) -> NativeDocument {
        let (text, _, formats) = crate::editor_state_from_content_with_checklists(content);
        let formats = crate::parse_format_ranges(&formats);
        NativeDocument {
            text,
            bold: formats.bold,
            italic: formats.italic,
            underline: formats.underline,
            strike: formats.strike,
            chip_ids: crate::native_projection::content_ids(content),
        }
    }

    #[test]
    fn two_native_notes_survive_database_reopen_without_cross_talk() {
        let _guard = native_test_guard();
        let path = TestDatabase::new();
        let parent_a = TestParent::new();
        let parent_b = TestParent::new();
        let original = NativeDocument {
            text: "First 😀\n[ ] Call café [[date:2026-09-27]]".into(),
            bold: vec![(0, 5)],
            italic: vec![(6, 10)],
            underline: vec![(17, 22)],
            strike: vec![(14, 16)],
            chip_ids: vec![uuid::Uuid::new_v4().to_string()],
        };
        let other = NativeDocument {
            text: "Independent second note".into(),
            ..Default::default()
        };
        {
            let db = crate::db::Db::open_in_dir(&path.0).unwrap();
            let first =
                NativeRichEditor::attach_to_parent(parent_a.0, 1.0, original.clone(), |_| {})
                    .unwrap();
            let second =
                NativeRichEditor::attach_to_parent(parent_b.0, 1.0, other.clone(), |_| {}).unwrap();
            crate::db::notes::create(
                &db.conn,
                "a",
                &content_from_document(&first.document()),
                "#FFF9B1",
            )
            .unwrap();
            crate::db::notes::create(
                &db.conn,
                "b",
                &content_from_document(&second.document()),
                "#D9F7BE",
            )
            .unwrap();
            unsafe {
                super::set_selection(
                    first.inner.hwnd,
                    super::CharRange {
                        cp_min: 0,
                        cp_max: 5,
                    },
                );
            }
            first.toggle_selection_style("bold").unwrap();
            crate::db::notes::save_content_and_color(
                &db.conn,
                "a",
                &content_from_document(&first.document()),
                Some("#FFD7E5"),
            )
            .unwrap();
            assert_eq!(second.document(), other);
        }
        // Close both controls and the SQLite connection before reconstructing
        // state using the same serializer/rehydration path as the application.
        let db = crate::db::Db::open_in_dir(&path.0).unwrap();
        let row = crate::db::notes::get(&db.conn, "a").unwrap().unwrap();
        assert_eq!(row.color, "#FFD7E5");
        let mut expected = original;
        expected.bold.clear();
        let restored = NativeRichEditor::attach_to_parent(
            parent_a.0,
            1.0,
            document_from_content(&row.content),
            |_| {},
        )
        .unwrap();
        assert_eq!(restored.document(), expected);
        let row_b = crate::db::notes::get(&db.conn, "b").unwrap().unwrap();
        let restored_b = NativeRichEditor::attach_to_parent(
            parent_b.0,
            1.0,
            document_from_content(&row_b.content),
            |_| {},
        )
        .unwrap();
        assert_eq!(restored_b.document(), other);
        assert_eq!(
            crate::db::notes::list(&db.conn, false, false, Some("café"))
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            crate::db::reminders::list_for_note(&db.conn, "a")
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn native_typing_undo_redo_flushes_the_current_document() {
        let _guard = native_test_guard();
        let parent = TestParent::new();
        let observed = Rc::new(RefCell::new(Vec::new()));
        let changes = observed.clone();
        let editor = NativeRichEditor::attach_to_parent(
            parent.0,
            1.0,
            NativeDocument {
                text: "draft".into(),
                ..Default::default()
            },
            move |document| changes.borrow_mut().push(document),
        )
        .unwrap();
        unsafe {
            super::set_selection(
                editor.inner.hwnd,
                super::CharRange {
                    cp_min: 5,
                    cp_max: 5,
                },
            );
            SendMessageW(editor.inner.hwnd, super::WM_CHAR, '!' as usize, 0);
        }
        editor.flush_pending_change();
        assert_eq!(observed.borrow().last().unwrap().text, "draft!");
        unsafe {
            SendMessageW(editor.inner.hwnd, super::WM_UNDO, 0, 0);
        }
        editor.flush_pending_change();
        assert_eq!(observed.borrow().last().unwrap().text, "draft");
        unsafe {
            SendMessageW(editor.inner.hwnd, super::EM_REDO, 0, 0);
        }
        editor.flush_pending_change();
        assert_eq!(observed.borrow().last().unwrap().text, "draft!");
    }

    #[test]
    fn replacing_native_document_does_not_inherit_previous_styles() {
        let _guard = native_test_guard();
        let parent = TestParent::new();
        let editor = NativeRichEditor::attach_to_parent(
            parent.0,
            1.0,
            NativeDocument {
                text: "styled".into(),
                bold: vec![(0, 6)],
                italic: vec![(0, 6)],
                underline: vec![(0, 6)],
                strike: vec![(0, 6)],
                chip_ids: vec![],
            },
            |_| {},
        )
        .unwrap();
        editor.restore_selection("styled", 3, 3);
        let replacement = NativeDocument {
            text: "plain replacement".into(),
            ..Default::default()
        };
        editor.set_document(&replacement);
        assert_eq!(editor.document(), replacement);
    }

    #[test]
    fn native_snapshot_and_replacement_do_not_show_a_hidden_overlay_child() {
        let _guard = native_test_guard();
        use windows_sys::Win32::UI::WindowsAndMessaging::{GetWindowLongPtrW, GWL_STYLE};
        let parent = TestParent::new();
        let document = NativeDocument {
            text: "menu context".into(),
            ..Default::default()
        };
        let editor =
            NativeRichEditor::attach_to_parent(parent.0, 1.0, document.clone(), |_| {}).unwrap();
        editor.set_visible(false);
        assert_eq!(editor.document(), document);
        editor.set_document(&document);
        let style = unsafe { GetWindowLongPtrW(editor.inner.hwnd, GWL_STYLE) } as u32;
        assert_eq!(style & super::WS_VISIBLE, 0);
    }

    #[test]
    fn utf8_offsets_become_win32_utf16_offsets_without_splitting_unicode() {
        let source = "A😀\nélan";
        assert_eq!(source_offset_to_control_utf16(source, 0), 0);
        assert_eq!(source_offset_to_control_utf16(source, 1), 1);
        assert_eq!(source_offset_to_control_utf16(source, 5), 3);
        assert_eq!(source_offset_to_control_utf16(source, 6), 4);
        assert_eq!(source_offset_to_control_utf16(source, source.len()), 8);
        for byte in [0, 1, 5, 6, 8, source.len()] {
            assert_eq!(
                super::control_utf16_to_source_offset(
                    source,
                    source_offset_to_control_utf16(source, byte)
                ),
                byte
            );
        }
        assert_eq!(super::control_utf16_to_source_offset(source, 2), 1);
    }

    #[test]
    fn native_list_action_uses_native_caret_and_restores_it_after_replacement() {
        let _guard = native_test_guard();
        let parent = TestParent::new();
        let source = "heading 😀\n[ ] second line";
        let editor = NativeRichEditor::attach_to_parent(
            parent.0,
            1.0,
            NativeDocument {
                text: source.into(),
                ..Default::default()
            },
            |_| {},
        )
        .unwrap();
        editor.restore_selection(source, source.len(), source.len());
        let (anchor, cursor) = editor.selection_byte_offsets(source);
        assert_eq!((anchor, cursor), (source.len() as i32, source.len() as i32));
        let result =
            crate::list_command(source, "checklist", anchor as usize, cursor as usize).unwrap();
        assert_eq!(result.source, "heading 😀\nsecond line");
        editor.set_document(&NativeDocument {
            text: result.source.clone(),
            ..Default::default()
        });
        editor.restore_selection(&result.source, result.caret, result.caret);
        assert_eq!(
            editor.selection_byte_offsets(&result.source),
            (result.caret as i32, result.caret as i32)
        );
        assert_eq!(editor.document().text, result.source);
    }

    // This is deliberately a real Win32 control test, rather than a mock of
    // the serializer. It exercises Msftedit loading, child creation,
    // subclassing, UTF-16 conversion, mixed-format retrieval and toolbar
    // formatting on a hidden parent HWND. It runs on the current UI thread and
    // avoids a Slint event loop so `cargo test` can execute it deterministically.
    #[test]
    fn win32_rich_edit_round_trips_unicode_multiline_and_mixed_styles() {
        let _guard = native_test_guard();
        let class_name: Vec<u16> = "STATIC\0".encode_utf16().collect();
        let parent: HWND = unsafe {
            CreateWindowExW(
                0,
                class_name.as_ptr(),
                std::ptr::null(),
                WS_OVERLAPPED,
                0,
                0,
                320,
                240,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null(),
            )
        };
        assert!(
            !parent.is_null(),
            "Win32 did not create the hidden test parent"
        );
        let source = "plain 😀\nélan";
        let emoji_end = "plain 😀".len();
        let accent_start = source.find('é').unwrap();
        let editor = NativeRichEditor::attach_to_parent(
            parent,
            1.0,
            NativeDocument {
                text: source.into(),
                bold: vec![(0, 5)],
                italic: vec![(6, emoji_end)],
                underline: vec![(accent_start, source.len())],
                strike: vec![(accent_start, accent_start + 'é'.len_utf8())],
                chip_ids: vec![],
            },
            |_| {},
        )
        .expect("Msftedit child should be constructible on supported Windows");
        let restored = editor.document();
        assert_eq!(restored.text, source);
        assert_eq!(restored.bold, vec![(0, 5)]);
        assert_eq!(restored.italic, vec![(6, emoji_end)]);
        assert_eq!(restored.underline, vec![(accent_start, source.len())]);
        assert_eq!(
            restored.strike,
            vec![(accent_start, accent_start + 'é'.len_utf8())]
        );
        // Formatting while the RichEdit subclass is active used to expose the
        // re-entrant RefCell hazard. This must simply schedule its debounced
        // notification and leave the native mixed style intact.
        unsafe {
            super::set_selection(
                editor.inner.hwnd,
                super::CharRange {
                    cp_min: 0,
                    cp_max: 5,
                },
            )
        };
        editor.toggle_selection_style("bold").unwrap();
        assert!(editor.document().bold.is_empty());
        let selection = unsafe { super::selection(editor.inner.hwnd) };
        assert_eq!(selection.cp_min, 0);
        assert_eq!(selection.cp_max, 5);
        // Verify actual selected characters, not just symmetric load/save
        // coordinates: paragraph breaks count once inside RichEdit.
        editor.restore_selection(source, accent_start, accent_start + 'é'.len_utf8());
        unsafe {
            SendMessageW(editor.inner.hwnd, super::WM_CHAR, 'X' as usize, 0);
        }
        assert_eq!(editor.document().text, source.replace('é', "X"));
        drop(editor);
        unsafe { DestroyWindow(parent) };
    }

    #[test]
    fn pending_native_input_is_emitted_synchronously_before_a_save() {
        let _guard = native_test_guard();
        let class_name: Vec<u16> = "STATIC\0".encode_utf16().collect();
        let parent = unsafe {
            CreateWindowExW(
                0,
                class_name.as_ptr(),
                std::ptr::null(),
                WS_OVERLAPPED,
                0,
                0,
                320,
                240,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null(),
            )
        };
        assert!(!parent.is_null());
        let observed = Rc::new(RefCell::new(Vec::<NativeDocument>::new()));
        let observed_for_callback = observed.clone();
        let editor = NativeRichEditor::attach_to_parent(
            parent,
            1.0,
            NativeDocument {
                text: "draft".into(),
                ..Default::default()
            },
            move |document| observed_for_callback.borrow_mut().push(document),
        )
        .unwrap();
        // This enters the real control's input procedure and schedules its
        // debounce. No Slint event loop is pumped before the explicit flush.
        unsafe {
            super::set_selection(
                editor.inner.hwnd,
                super::CharRange {
                    cp_min: 5,
                    cp_max: 5,
                },
            );
            SendMessageW(editor.inner.hwnd, super::WM_CHAR, '!' as usize, 0);
        }
        assert!(observed.borrow().is_empty());
        editor.flush_pending_change();
        assert_eq!(observed.borrow().last().unwrap().text, "draft!");
        drop(editor);
        unsafe { DestroyWindow(parent) };
    }
}
