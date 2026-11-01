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
        WM_GETTEXTLENGTH, WM_IME_COMPOSITION, WM_KEYUP, WM_KILLFOCUS, WM_PASTE, WM_SETTEXT,
        WM_UNDO, WNDPROC, WS_CHILD, WS_CLIPCHILDREN, WS_CLIPSIBLINGS, WS_VISIBLE, WS_VSCROLL,
    },
};

const EM_GETCHARFORMAT: u32 = 1082;
const EM_SETBKGNDCOLOR: u32 = 1091;
const EM_SETCHARFORMAT: u32 = 1092;
const EM_EXGETSEL: u32 = 1076;
const EM_EXSETSEL: u32 = 1079;
const EM_REDO: u32 = 1108;
const EM_REPLACESEL: u32 = 194;
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

type Handler<F> = RefCell<Option<Box<F>>>;
type ImageLoader = dyn Fn(&str) -> Result<Vec<u8>, String>;

/// Presentation-only formatting laid over checklist lines; removed before the
/// document is read back so it never reaches saved styles.
#[derive(Clone, Copy)]
enum TaskStyle {
    Body { struck: bool },
    Marker,
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
    chip_clicked: Handler<dyn FnMut(i32, f32, f32)>,
    checklist_clicked: Handler<dyn FnMut(i32, bool)>,
    mouse_down: Cell<Option<(i16, i16)>>,
    composing: Cell<bool>,
    handled_enter: Cell<bool>,
    palette: Cell<Option<u32>>,
    text_document: Option<ITextDocument>,
    tracked_chips: RefCell<Vec<TrackedNativeChip>>,
    task_styles: RefCell<Vec<(ITextRange, TaskStyle)>>,
    last_edit_at: Cell<Option<usize>>,
    image_loader: Handler<ImageLoader>,
    images: RefCell<HashMap<String, Vec<u8>>>,
    image_errors: RefCell<Vec<String>>,
    image_paste: Handler<dyn FnMut(Result<Vec<u8>, String>)>,
    focus_toolbar: RefCell<Option<Box<dyn FnMut()>>>,
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
    fn dispatch_image_paste(&self, result: Result<Vec<u8>, String>) {
        // The callback rebuilds the control and re-enters its window procedure.
        let callback = self.image_paste.borrow_mut().take();
        if let Some(mut callback) = callback {
            callback(result);
            if self.image_paste.borrow().is_none() {
                *self.image_paste.borrow_mut() = Some(callback);
            }
        }
    }
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
    _ole_session: Option<crate::native_image::OleSession>,
}

impl NativeRichEditor {
    pub fn on_focus_toolbar(&self, callback: impl FnMut() + 'static) {
        *self.inner.focus_toolbar.borrow_mut() = Some(Box::new(callback));
    }

    pub fn focus(&self) {
        unsafe { windows_sys::Win32::UI::Input::KeyboardAndMouse::SetFocus(self.inner.hwnd) };
    }

    pub fn on_image_paste(&self, callback: impl FnMut(Result<Vec<u8>, String>) + 'static) {
        *self.inner.image_paste.borrow_mut() = Some(Box::new(callback));
    }
    pub fn set_image_loader(&self, loader: impl Fn(&str) -> Result<Vec<u8>, String> + 'static) {
        let document = unsafe { document_from_control(self.inner.hwnd) };
        *self.inner.image_loader.borrow_mut() = Some(Box::new(loader));
        self.inner.suppress_change.set(true);
        unsafe { set_document(self.inner.hwnd, &document) };
        self.inner.suppress_change.set(false);
    }
    pub fn image_errors(&self) -> Vec<String> {
        self.inner.image_errors.borrow().clone()
    }
    pub fn document_with_image(&self, id: &str) -> NativeDocument {
        self.flush_pending_change();
        let mut document = unsafe { document_from_control(self.inner.hwnd) };
        let projection = unsafe { control_projection(self.inner.hwnd) };
        let range = unsafe { selection(self.inner.hwnd) };
        let start = projection.to_source(range.cp_min.max(0) as usize);
        let end = projection.to_source(range.cp_max.max(0) as usize);
        let old = document.text.clone();
        document
            .text
            .replace_range(start..end, &crate::inline_images::marker(id));
        let mut formats = crate::FormatRanges {
            bold: document.bold,
            italic: document.italic,
            underline: document.underline,
            strike: document.strike,
        };
        crate::remap_formats_after_edit(&old, &document.text, &mut formats);
        for ranges in [
            &mut formats.bold,
            &mut formats.italic,
            &mut formats.underline,
            &mut formats.strike,
        ] {
            crate::inline_images::exclude_from_styles(&document.text, ranges);
        }
        document.bold = formats.bold;
        document.italic = formats.italic;
        document.underline = formats.underline;
        document.strike = formats.strike;
        document.chip_ids = projection
            .chips
            .iter()
            .filter(|chip| !chip.image && (chip.source_end <= start || chip.source_start >= end))
            .map(|chip| chip.id.clone())
            .collect();
        document
    }
    pub fn commit_image_document(&self, document: &NativeDocument, id: &str) {
        self.set_document(document);
        if let Some(image) = crate::inline_images::occurrences(&document.text)
            .into_iter()
            .find(|image| image.id == id)
        {
            self.restore_selection(&document.text, image.end, image.end);
        }
        self.inner.emit_change();
    }
    pub fn on_checklist_clicked(&self, callback: impl FnMut(i32, bool) + 'static) {
        *self.inner.checklist_clicked.borrow_mut() = Some(Box::new(callback));
    }
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
        unsafe { SendMessageW(hwnd, EM_SETBKGNDCOLOR, 0, 0x00f8f8f8) };
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
            checklist_clicked: RefCell::new(None),
            mouse_down: Cell::new(None),
            composing: Cell::new(false),
            handled_enter: Cell::new(false),
            palette: Cell::new(None),
            text_document: Some(text_document),
            tracked_chips: RefCell::new(Vec::new()),
            image_loader: RefCell::new(None),
            images: RefCell::new(HashMap::new()),
            image_errors: RefCell::new(Vec::new()),
            image_paste: RefCell::new(None),
            focus_toolbar: RefCell::new(None),
            task_styles: RefCell::new(Vec::new()),
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
        #[allow(clippy::missing_transmute_annotations)]
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
            _ole_session: crate::native_image::OleSession::new().ok(),
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

    #[cfg(feature = "dev-tools")]
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
        let value = crate::paper_hex(value);
        self.inner.repaint_needed.set(true);
        let Ok(rgb) = u32::from_str_radix(value.trim_start_matches('#'), 16) else {
            return;
        };
        let changed = self.inner.palette.replace(Some(rgb)) != Some(rgb);
        if changed && !self.inner.images.borrow().is_empty() {
            self.flush_pending_change();
            let document = unsafe { document_from_control(self.inner.hwnd) };
            let selection = unsafe { selection(self.inner.hwnd) };
            unsafe {
                set_document(self.inner.hwnd, &document);
                set_selection(self.inner.hwnd, selection);
            }
        } else {
            unsafe { apply_palette(self.inner.hwnd, rgb) };
        }
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
    #[cfg(feature = "dev-tools")]
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
        unsafe { clear_task_styles(hwnd) };
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
    pub fn set_layout_bounds(&self, bounds: [f32; 4]) -> bool {
        if bounds.iter().any(|value| !value.is_finite())
            || bounds[2] < 0.0
            || bounds[3] < 0.0
            || self.inner.layout_bounds.get() == Some(bounds)
        {
            return false;
        }
        let width_changed = self
            .inner
            .layout_bounds
            .get()
            .is_none_or(|previous| previous[2] != bounds[2]);
        let restore = if width_changed && !self.inner.images.borrow().is_empty() {
            self.flush_pending_change();
            Some(unsafe {
                (
                    document_from_control(self.inner.hwnd),
                    selection(self.inner.hwnd),
                )
            })
        } else {
            None
        };
        self.inner.layout_bounds.set(Some(bounds));
        self.layout();
        if let Some((document, range)) = restore {
            self.inner.suppress_change.set(true);
            unsafe {
                set_document(self.inner.hwnd, &document);
                set_selection(self.inner.hwnd, range);
            }
            self.inner.suppress_change.set(false);
        }
        true
    }

    pub fn set_visible(&self, visible: bool) {
        self.inner.repaint_needed.set(visible);
        unsafe { ShowWindow(self.inner.hwnd, if visible { SW_SHOW } else { SW_HIDE }) };
        if visible {
            // Dismissing a Slint overlay must return keyboard input to the
            // native child, not leave an invisible popup scope owning focus.
            self.focus();
        }
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
    if message == windows_sys::Win32::UI::WindowsAndMessaging::WM_SETCURSOR
        && (lparam as u32 & 0xffff) == 1
        && pointer_over_checkbox(hwnd)
    {
        use windows_sys::Win32::UI::WindowsAndMessaging::{LoadCursorW, SetCursor, IDC_HAND};
        SetCursor(LoadCursorW(std::ptr::null_mut(), IDC_HAND));
        return 1;
    }
    if message == windows_sys::Win32::UI::WindowsAndMessaging::WM_LBUTTONDOWN {
        (*pointer).mouse_down.set(Some((
            (lparam as u32 & 0xffff) as i16,
            ((lparam as u32 >> 16) & 0xffff) as i16,
        )));
    }
    let inner = &*pointer;
    if message == windows_sys::Win32::UI::WindowsAndMessaging::WM_KEYDOWN
        && wparam == windows_sys::Win32::UI::Input::KeyboardAndMouse::VK_F6 as usize
        && !inner.composing.get()
    {
        // Native child input does not reach Slint's focus traversal. F6 moves
        // to the toolbar without turning a normal text Tab into navigation.
        let callback = inner.focus_toolbar.borrow_mut().take();
        if let Some(mut callback) = callback {
            callback();
            if inner.focus_toolbar.borrow().is_none() {
                *inner.focus_toolbar.borrow_mut() = Some(callback);
            }
            return 0;
        }
    }
    // RichEdit handles keyboard paste internally rather than sending WM_PASTE
    // back through our subclass. Route both standard shortcuts through the
    // owned import path and swallow the translated Ctrl+V character once.
    if inner.image_paste.borrow().is_some() && !inner.suppress_change.get() {
        use windows_sys::Win32::UI::Input::KeyboardAndMouse::{GetKeyState, VK_CONTROL, VK_SHIFT};
        if is_paste_shortcut(
            message,
            wparam,
            GetKeyState(VK_CONTROL as i32) < 0,
            GetKeyState(VK_SHIFT as i32) < 0,
        ) {
            return SendMessageW(hwnd, WM_PASTE, 0, 0);
        }
        if message == WM_CHAR && wparam == 22 {
            return 0;
        }
    }
    if message == WM_PASTE && inner.image_paste.borrow().is_some() && !inner.suppress_change.get() {
        match crate::native_image::clipboard_content(hwnd) {
            Ok(crate::native_image::ClipboardContent::Text(text)) => {
                // Route plain text through the normal replacement path. Do not
                // accept untracked OLE/RTF objects from another application.
                return SendMessageW(hwnd, EM_REPLACESEL, 1, text.as_ptr() as LPARAM);
            }
            result => {
                let result = result
                    .map(|content| match content {
                        crate::native_image::ClipboardContent::Image(bytes) => bytes,
                        _ => unreachable!(),
                    })
                    .map_err(|error| error.to_string());
                let weak = inner.weak_self.borrow().clone();
                slint::Timer::single_shot(Duration::ZERO, move || {
                    if let Some(inner) = weak.upgrade() {
                        inner.dispatch_image_paste(result);
                    }
                });
                return 0;
            }
        }
    }
    // Remove presentation-only effects before RichEdit changes text or starts
    // an IME composition, so insertion never inherits a completed-task strike.
    if !inner.suppress_change.get()
        && matches!(
            message,
            WM_CHAR
                | WM_PASTE
                | WM_CUT
                | WM_CLEAR
                | EM_REPLACESEL
                | windows_sys::Win32::UI::WindowsAndMessaging::WM_IME_STARTCOMPOSITION
        )
    {
        clear_task_styles(hwnd);
    }
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
    if matches!(
        message,
        WM_CHAR | WM_PASTE | WM_CUT | WM_CLEAR | EM_REPLACESEL
    ) || is_delete
        || backwards
    {
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
        if range.cp_min < range.cp_max {
            // Replacing selected chip text consumes that chip even if the
            // replacement happens to spell the same visible date label.
            inner.tracked_chips.borrow_mut().retain(|tracked| {
                let start = tracked.range.GetStart().unwrap_or(-1);
                let end = tracked.range.GetEnd().unwrap_or(-1);
                !(range.cp_min < end && range.cp_max > start)
            });
        }
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
    let result = unsafe { CallWindowProcW(original, hwnd, message, wparam, lparam) };
    if message == windows_sys::Win32::UI::WindowsAndMessaging::WM_PAINT {
        draw_empty_hint(hwnd, &*pointer);
    }
    if message == WM_LBUTTONUP {
        let projection = control_projection(hwnd);
        let point = selection(hwnd).cp_min.max(0) as usize;
        let mouse = (
            (lparam as u32 & 0xffff) as i16,
            ((lparam as u32 >> 16) & 0xffff) as i16,
        );
        let click = inner.mouse_down.take().is_some_and(|down| {
            let threshold = 4.0 * inner.scale_factor.get();
            f32::from(mouse.0.abs_diff(down.0)) <= threshold
                && f32::from(mouse.1.abs_diff(down.1)) <= threshold
        });
        if click && selection(hwnd).cp_min == selection(hwnd).cp_max {
            if let Some((line, checked)) = crate::checklist_presentation::target(
                &projection.source,
                projection.to_source(point),
            ) {
                if let Some(callback) = inner.checklist_clicked.borrow_mut().as_mut() {
                    callback(line, checked);
                }
                return result;
            }
        }
        if let Some((index, chip)) = projection
            .chips
            .iter()
            .filter(|chip| !chip.image)
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
                | EM_REPLACESEL
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

fn is_paste_shortcut(message: u32, key: usize, control: bool, shift: bool) -> bool {
    message == windows_sys::Win32::UI::WindowsAndMessaging::WM_KEYDOWN
        && ((control && key == b'V' as usize) || (shift && key == 0x2d))
}

fn document_weight(document: &NativeDocument) -> usize {
    document.text.len()
        + (document.bold.len()
            + document.italic.len()
            + document.underline.len()
            + document.strike.len())
            * 16
}

pub(crate) fn parent_hwnd(window: &slint::Window) -> Result<HWND, String> {
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
    struct ChangeGuard<'a>(&'a Cell<bool>, bool);
    impl Drop for ChangeGuard<'_> {
        fn drop(&mut self) {
            self.0.set(self.1);
        }
    }
    let pointer = EDITORS.with(|editors| editors.borrow().get(&(hwnd as isize)).copied());
    let _changes = pointer.map(|pointer| {
        let cell = &(*pointer).suppress_change;
        ChangeGuard(cell, cell.replace(true))
    });
    clear_task_styles(hwnd);
    let _redraw = RedrawBatch::new(hwnd);
    let projection = crate::native_projection::Projection::from_source_visible(
        &document.text,
        &document.chip_ids,
    );
    let control_text =
        crate::checklist_presentation::render(&projection.control).replace('\n', "\r\n");
    let mut wide: Vec<u16> = control_text.encode_utf16().collect();
    wide.push(0);
    SendMessageW(hwnd, WM_SETTEXT, 0, wide.as_ptr() as LPARAM);
    EDITORS.with(|editors| {
        let Some(pointer) = editors.borrow().get(&(hwnd as isize)).copied() else {
            return;
        };
        let inner = &*pointer;
        let mut retained = std::collections::HashSet::new();
        for state in inner
            .history
            .borrow()
            .retained()
            .chain(std::iter::once(document))
        {
            retained.extend(
                crate::inline_images::occurrences(&state.text)
                    .into_iter()
                    .map(|image| image.id),
            );
        }
        inner
            .images
            .borrow_mut()
            .retain(|id, _| retained.contains(id));
        inner.image_errors.borrow_mut().clear();
        for image in projection.chips.iter().filter(|chip| chip.image).rev() {
            if !inner.images.borrow().contains_key(&image.id) {
                if let Some(loader) = inner.image_loader.borrow().as_ref() {
                    match loader(&image.id) {
                        Ok(bytes) => {
                            inner.images.borrow_mut().insert(image.id.clone(), bytes);
                        }
                        Err(error) => {
                            inner.image_errors.borrow_mut().push(error);
                        }
                    }
                }
            }
            if let Some(bytes) = inner.images.borrow().get(&image.id) {
                set_selection(
                    hwnd,
                    CharRange {
                        cp_min: image.control_start as i32,
                        cp_max: image.control_end as i32,
                    },
                );
                let empty: [u16; 1] = [0];
                SendMessageW(hwnd, EM_REPLACESEL, 0, empty.as_ptr() as LPARAM);
                let width = inner
                    .layout_bounds
                    .get()
                    .map_or(320, |bounds| (bounds[2] - 12.0).max(1.0) as u32);
                if let Err(error) = crate::native_image::insert_thumbnail_on_paper(
                    hwnd,
                    bytes,
                    width,
                    inner.palette.get().unwrap_or(0x00ff_ffff),
                ) {
                    inner.image_errors.borrow_mut().push(error.to_string());
                    let placeholder: [u16; 2] = [0x25a1, 0];
                    SendMessageW(hwnd, EM_REPLACESEL, 0, placeholder.as_ptr() as LPARAM);
                }
            }
        }
    });
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
    style_completed_tasks(hwnd, document, &projection);
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
            let date = if chip.image {
                Some(format!("image:{}", chip.id))
            } else {
                marker
                    .strip_prefix("[[date:")
                    .and_then(|value| value.strip_suffix("]]"))
                    .map(str::to_owned)
            };
            let Some(date) = date else {
                continue;
            };
            if let Ok(range) = tom.Range(chip.control_start as i32, chip.control_end as i32) {
                tracked.push(TrackedNativeChip {
                    range,
                    id: chip.id.clone(),
                    date,
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
    clear_task_styles(hwnd);
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
    format.cr_text_color = if rgb == 0x6f7078 || rgb == 0x454545 {
        0x00f6f5f5
    } else {
        0x003d3432
    };
    format.y_height = 210;
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
    let _ = document_from_control(hwnd);
}

/// RichEdit has no placeholder of its own, so this paints one over an empty note.
unsafe fn draw_empty_hint(hwnd: HWND, inner: &Inner) {
    use windows_sys::Win32::{
        Foundation::RECT,
        Graphics::Gdi::{
            CreateFontW, DeleteObject, GetDC, ReleaseDC, SelectObject, SetBkMode, SetTextColor,
            TextOutW, TRANSPARENT,
        },
        UI::WindowsAndMessaging::GetWindowTextLengthW,
    };
    const EM_GETRECT: u32 = 0x00B2;
    if GetWindowTextLengthW(hwnd) != 0 {
        return;
    }
    let mut area = RECT {
        left: 0,
        top: 0,
        right: 0,
        bottom: 0,
    };
    SendMessageW(hwnd, EM_GETRECT, 0, (&mut area as *mut RECT) as LPARAM);
    let dc = GetDC(hwnd);
    if dc.is_null() {
        return;
    }
    let face: Vec<u16> = "Segoe UI\0".encode_utf16().collect();
    let height = -(14.0 * inner.scale_factor.get()).round() as i32;
    let font = CreateFontW(height, 0, 0, 0, 400, 0, 0, 0, 0, 0, 0, 0, 0, face.as_ptr());
    let previous = SelectObject(dc, font);
    SetBkMode(dc, TRANSPARENT as i32);
    // COLORREF is 0x00BBGGRR.
    SetTextColor(
        dc,
        if matches!(inner.palette.get(), Some(0x454545 | 0x6f7078)) {
            0x00c8c8c8
        } else {
            0x00707070
        },
    );
    let hint: Vec<u16> = "Write a note…".encode_utf16().collect();
    TextOutW(dc, area.left, area.top, hint.as_ptr(), hint.len() as i32);
    SelectObject(dc, previous);
    DeleteObject(font);
    ReleaseDC(hwnd, dc);
}

/// True when the mouse is over a checklist square, so it shows a hand cursor.
unsafe fn pointer_over_checkbox(hwnd: HWND) -> bool {
    use windows_sys::Win32::{Foundation::POINT, Graphics::Gdi::ScreenToClient};
    const EM_CHARFROMPOS: u32 = 0x00D7;
    let mut point = POINT { x: 0, y: 0 };
    if windows_sys::Win32::UI::WindowsAndMessaging::GetCursorPos(&mut point) == 0
        || ScreenToClient(hwnd, &mut point) == 0
    {
        return false;
    }
    let index = SendMessageW(hwnd, EM_CHARFROMPOS, 0, (&point as *const POINT) as LPARAM);
    if index < 0 {
        return false;
    }
    // The nearest character can be the marker even well left of or below it;
    // require the pointer to be over the marker's own box.
    let mut origin = POINT { x: 0, y: 0 };
    const EM_POSFROMCHAR: u32 = 0x00D6;
    SendMessageW(
        hwnd,
        EM_POSFROMCHAR,
        (&mut origin as *mut POINT) as WPARAM,
        index,
    );
    let mut after = POINT { x: 0, y: 0 };
    SendMessageW(
        hwnd,
        EM_POSFROMCHAR,
        (&mut after as *mut POINT) as WPARAM,
        index + 1,
    );
    if point.x < origin.x || (after.y == origin.y && point.x > after.x) {
        return false;
    }
    let projection = control_projection(hwnd);
    crate::checklist_presentation::target(&projection.source, projection.to_source(index as usize))
        .is_some()
}

unsafe fn control_projection(hwnd: HWND) -> crate::native_projection::Projection {
    let length = SendMessageW(hwnd, WM_GETTEXTLENGTH, 0, 0).max(0) as usize;
    let mut buffer = vec![0u16; length + 1];
    use windows::Win32::UI::Controls::RichEdit::{EM_GETTEXTEX, GETTEXTEX, GT_RAWTEXT};
    let request = GETTEXTEX {
        cb: (buffer.len() * 2) as u32,
        flags: GT_RAWTEXT,
        codepage: 1200,
        ..Default::default()
    };
    SendMessageW(
        hwnd,
        EM_GETTEXTEX,
        (&request as *const GETTEXTEX) as usize,
        buffer.as_mut_ptr() as LPARAM,
    );
    let end = buffer
        .iter()
        .position(|&unit| unit == 0)
        .unwrap_or(buffer.len());
    for position in crate::native_image::object_positions(hwnd) {
        if position < end {
            buffer[position] = 0xfffc;
        }
    }
    // Raw-text mode preserves U+FFFC for objects, rather than substituting
    // spaces, and excludes TOM's mandatory final paragraph.
    let raw_text = String::from_utf16_lossy(&buffer[..end]);
    let raw =
        crate::checklist_presentation::restore(&raw_text.replace("\r\n", "\n").replace('\r', "\n"));
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
            .and_then(|pointer| (*(*pointer)).last_edit_at.get())
    });
    let projection = crate::native_projection::Projection::from_visible_control_with_edit(
        &raw, &tracked, edit_at,
    );
    // TOM includes text inserted exactly at a range boundary. Once the edit
    // hint identifies the original label, narrow the live range immediately:
    // a later edit must not reassign its identity to an identical pasted label.
    EDITORS.with(|editors| {
        let Some(pointer) = editors.borrow().get(&(hwnd as isize)).copied() else {
            return;
        };
        let inner = &*pointer;
        inner.tracked_chips.borrow_mut().retain_mut(|chip| {
            let start = chip.range.GetStart().unwrap_or(-1).max(0) as usize;
            let Some(span) = projection
                .chips
                .iter()
                .filter(|span| span.id == chip.id)
                .min_by_key(|span| span.control_start.abs_diff(start))
            else {
                return false;
            };
            if chip.range.GetStart().ok() == Some(span.control_start as i32)
                && chip.range.GetEnd().ok() == Some(span.control_end as i32)
            {
                return true;
            }
            chip.range
                .SetRange(span.control_start as i32, span.control_end as i32)
                .is_ok()
        });
        inner.last_edit_at.set(None);
    });
    projection
}

unsafe fn document_from_control(hwnd: HWND) -> NativeDocument {
    clear_task_styles(hwnd);
    let projection = control_projection(hwnd);
    let mut document = NativeDocument {
        text: projection.source.clone(),
        chip_ids: projection.ids(),
        ..Default::default()
    };
    let spans = projection.plain_character_spans();
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
    style_completed_tasks(hwnd, &document, &projection);
    document
}

/// Presentation ranges retain their actual user strike state. They are removed
/// before snapshots/input and rebuilt afterwards, never serialized as styles.
unsafe fn clear_task_styles(hwnd: HWND) {
    let pointer = EDITORS.with(|editors| editors.borrow().get(&(hwnd as isize)).copied());
    let Some(pointer) = pointer else { return };
    let inner = &*pointer;
    let ranges = std::mem::take(&mut *inner.task_styles.borrow_mut());
    if ranges.is_empty() {
        return;
    }
    let previous = selection(hwnd);
    let suppress = inner.suppress_change.replace(true);
    let _redraw = RedrawBatch::new(hwnd);
    for (range, style) in ranges {
        let (Ok(start), Ok(end)) = (range.GetStart(), range.GetEnd()) else {
            continue;
        };
        set_selection(
            hwnd,
            CharRange {
                cp_min: start,
                cp_max: end,
            },
        );
        let mut format = char_format();
        format.dw_mask = CFM_STRIKEOUT | 0x4000_0000;
        match style {
            TaskStyle::Body { struck } => {
                format.dw_effects = if struck { CFM_STRIKEOUT } else { 0 };
            }
            TaskStyle::Marker => set_face(&mut format, "Segoe UI", 210),
        }
        format.cr_text_color = if matches!(inner.palette.get(), Some(0x454545 | 0x6f7078)) {
            0x00f6f5f5
        } else {
            0x003d3432
        };
        SendMessageW(
            hwnd,
            EM_SETCHARFORMAT,
            SCF_SELECTION,
            (&format as *const CharFormat2W) as LPARAM,
        );
    }
    set_selection(hwnd, previous);
    inner.suppress_change.set(suppress);
}

unsafe fn style_completed_tasks(
    hwnd: HWND,
    document: &NativeDocument,
    projection: &crate::native_projection::Projection,
) {
    let pointer = EDITORS.with(|editors| editors.borrow().get(&(hwnd as isize)).copied());
    let Some(pointer) = pointer else { return };
    let inner = &*pointer;
    let Some(tom) = inner.text_document.as_ref() else {
        return;
    };
    let bodies = crate::checklist_presentation::completed_bodies(&document.text);
    let markers = crate::checklist_presentation::markers(&document.text);
    if markers.is_empty() {
        return;
    }
    // Existing overlays must be removed by the caller before capturing styles.
    if !inner.task_styles.borrow().is_empty() {
        return;
    }
    let previous = selection(hwnd);
    let suppress = inner.suppress_change.replace(true);
    let _redraw = RedrawBatch::new(hwnd);
    for (start, end) in bodies {
        let mut boundaries = vec![start, end];
        for &(a, b) in &document.strike {
            boundaries.extend([a, b].into_iter().filter(|p| *p > start && *p < end));
        }
        boundaries.sort_unstable();
        boundaries.dedup();
        for bounds in boundaries.windows(2) {
            let a = projection.to_control(bounds[0]) as i32;
            let b = projection.to_control(bounds[1]) as i32;
            let Ok(range) = tom.Range(a, b) else { continue };
            let struck = document
                .strike
                .iter()
                .any(|&(s, e)| s <= bounds[0] && e >= bounds[1]);
            inner
                .task_styles
                .borrow_mut()
                .push((range, TaskStyle::Body { struck }));
            set_selection(
                hwnd,
                CharRange {
                    cp_min: a,
                    cp_max: b,
                },
            );
            let mut format = char_format();
            format.dw_mask = CFM_STRIKEOUT | 0x4000_0000;
            format.dw_effects = CFM_STRIKEOUT;
            format.cr_text_color = if matches!(inner.palette.get(), Some(0x454545 | 0x6f7078)) {
                0x00b8b8b8
            } else {
                0x00808080
            };
            SendMessageW(
                hwnd,
                EM_SETCHARFORMAT,
                SCF_SELECTION,
                (&format as *const CharFormat2W) as LPARAM,
            );
        }
    }
    // Larger, coloured squares read as clickable controls.
    let dark = matches!(inner.palette.get(), Some(0x454545 | 0x6f7078));
    for (offset, checked) in markers {
        let a = projection.to_control(offset) as i32;
        let Ok(range) = tom.Range(a, a + 1) else {
            continue;
        };
        inner
            .task_styles
            .borrow_mut()
            .push((range, TaskStyle::Marker));
        set_selection(
            hwnd,
            CharRange {
                cp_min: a,
                cp_max: a + 1,
            },
        );
        let mut format = char_format();
        format.dw_mask = CFM_STRIKEOUT | 0x4000_0000;
        set_face(&mut format, "Segoe UI Symbol", 270);
        // COLORREF is 0x00BBGGRR.
        format.cr_text_color = match (checked, dark) {
            (true, false) => 0x00c06700,
            (true, true) => 0x00ffc560,
            (false, false) => 0x00505050,
            (false, true) => 0x00d0d0d0,
        };
        SendMessageW(
            hwnd,
            EM_SETCHARFORMAT,
            SCF_SELECTION,
            (&format as *const CharFormat2W) as LPARAM,
        );
    }
    // Date labels retain their own foreground/background within a task.
    style_chips(hwnd, projection);
    set_selection(hwnd, previous);
    inner.suppress_change.set(suppress);
}

fn set_face(format: &mut CharFormat2W, face: &str, height: i32) {
    format.dw_mask |= 0x2000_0000 | 0x8000_0000;
    format.y_height = height;
    format.sz_face_name = [0; 32];
    for (index, unit) in face.encode_utf16().enumerate() {
        format.sz_face_name[index] = unit;
    }
}

unsafe fn style_chips(hwnd: HWND, projection: &crate::native_projection::Projection) {
    for chip in projection.chips.iter().filter(|chip| !chip.image) {
        set_selection(
            hwnd,
            CharRange {
                cp_min: chip.hidden_end as i32,
                cp_max: chip.control_end as i32,
            },
        );
        let mut label = char_format();
        label.dw_mask = 0x4000_0000 | 0x0400_0000 | CFM_BOLD | CFM_UNDERLINE | 0x100;
        label.dw_effects = CFM_UNDERLINE;
        label.cr_text_color = 0x00303132;
        label.cr_back_color = 0x00a8e3f1;
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
    #[test]
    fn image_paste_callback_can_rebuild_control_without_borrowing_its_handler() {
        let _guard = native_test_guard();
        let parent = TestParent::new();
        let editor =
            NativeRichEditor::attach_to_parent(parent.0, 1.0, NativeDocument::default(), |_| {})
                .unwrap();
        let inner = Rc::downgrade(&editor.inner);
        editor.on_image_paste(move |result| {
            assert!(result.is_ok());
            let inner = inner.upgrade().unwrap();
            assert!(inner.image_paste.borrow().is_none());
            unsafe {
                super::set_document(
                    inner.hwnd,
                    &NativeDocument {
                        text: "paste handled".into(),
                        ..Default::default()
                    },
                );
            }
        });
        editor.inner.dispatch_image_paste(Ok(Vec::new()));
        assert!(editor.inner.image_paste.borrow().is_some());
        assert_eq!(editor.document().text, "paste handled");
        editor.inner.dispatch_image_paste(Ok(Vec::new()));
        assert!(editor.inner.image_paste.borrow().is_some());
    }
    #[test]
    fn keyboard_paste_shortcuts_route_to_owned_import_once() {
        use windows_sys::Win32::UI::WindowsAndMessaging::{WM_CHAR, WM_KEYDOWN, WM_KEYUP};
        assert!(super::is_paste_shortcut(
            WM_KEYDOWN,
            b'V' as usize,
            true,
            false
        ));
        assert!(super::is_paste_shortcut(WM_KEYDOWN, 0x2d, false, true));
        assert!(!super::is_paste_shortcut(WM_CHAR, 22, true, false));
        assert!(!super::is_paste_shortcut(
            WM_KEYUP,
            b'V' as usize,
            true,
            false
        ));
        assert!(!super::is_paste_shortcut(
            WM_KEYDOWN,
            b'V' as usize,
            false,
            false
        ));
    }
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
    fn toolbar_navigation_preserves_selection_and_defers_during_composition() {
        let _guard = native_test_guard();
        let parent = TestParent::new();
        let document = NativeDocument {
            text: "Unicode café 😀".into(),
            ..Default::default()
        };
        let editor =
            NativeRichEditor::attach_to_parent(parent.0, 1.0, document.clone(), |_| {}).unwrap();
        editor.restore_selection(&document.text, 0, 7);
        let visits = Rc::new(std::cell::Cell::new(0));
        let observed = visits.clone();
        let weak = Rc::downgrade(&editor.inner);
        editor.on_focus_toolbar(move || {
            let inner = weak.upgrade().unwrap();
            assert!(inner.focus_toolbar.borrow().is_none());
            observed.set(observed.get() + 1);
        });
        let navigate = || unsafe {
            SendMessageW(
                editor.inner.hwnd,
                windows_sys::Win32::UI::WindowsAndMessaging::WM_KEYDOWN,
                windows_sys::Win32::UI::Input::KeyboardAndMouse::VK_F6 as usize,
                0,
            );
        };
        navigate();
        navigate();
        assert_eq!(visits.get(), 2);
        assert_eq!(editor.selection_byte_offsets(&document.text), (0, 7));
        editor.inner.composing.set(true);
        navigate();
        assert_eq!(visits.get(), 2);
        editor.inner.composing.set(false);
        assert_eq!(editor.document(), document);
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
    fn chip_identity_survives_a_boundary_paste_followed_by_another_edit() {
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
        editor.restore_selection(source, 0, 0);
        let paste: Vec<_> = " Sep 27 ".encode_utf16().chain(Some(0)).collect();
        unsafe {
            SendMessageW(editor.inner.hwnd, 194, 1, paste.as_ptr() as super::LPARAM);
        }
        let pasted = editor.document();
        assert_eq!(pasted.text, " Sep 27 [[date:2026-09-27]]");
        editor.restore_selection(&pasted.text, pasted.text.len(), pasted.text.len());
        unsafe {
            SendMessageW(editor.inner.hwnd, super::WM_CHAR, 'X' as usize, 0);
        }
        let edited = editor.document();
        assert_eq!(edited.text, " Sep 27 [[date:2026-09-27]]X");
        assert_eq!(edited.chip_ids, vec![id.clone()]);
        editor.history_action(false);
        assert_eq!(editor.document().text, source);
        assert_eq!(editor.document().chip_ids, vec![id]);
    }

    #[test]
    fn replacing_a_selected_chip_with_its_label_removes_the_chip_identity() {
        let _guard = native_test_guard();
        let parent = TestParent::new();
        let source = "[[date:2026-09-27]]";
        let editor = NativeRichEditor::attach_to_parent(
            parent.0,
            1.0,
            NativeDocument {
                text: source.into(),
                chip_ids: vec![uuid::Uuid::new_v4().to_string()],
                ..Default::default()
            },
            |_| {},
        )
        .unwrap();
        editor.restore_selection(source, 0, source.len());
        let paste: Vec<_> = " Sep 27 ".encode_utf16().chain(Some(0)).collect();
        unsafe {
            SendMessageW(
                editor.inner.hwnd,
                super::EM_REPLACESEL,
                1,
                paste.as_ptr() as super::LPARAM,
            );
        }
        let edited = editor.document();
        assert_eq!(edited.text, " Sep 27 ");
        assert!(edited.chip_ids.is_empty());
        editor.history_action(false);
        assert_eq!(editor.document().text, source);
        assert_eq!(editor.document().chip_ids.len(), 1);
    }

    #[test]
    fn consecutive_native_replacements_keep_the_original_chip_range() {
        let _guard = native_test_guard();
        let parent = TestParent::new();
        let id = uuid::Uuid::new_v4().to_string();
        let source = "[[date:2026-09-27]]";
        let saved = Rc::new(RefCell::new(Vec::new()));
        let changes = saved.clone();
        let editor = NativeRichEditor::attach_to_parent(
            parent.0,
            1.0,
            NativeDocument {
                text: source.into(),
                chip_ids: vec![id.clone()],
                ..Default::default()
            },
            move |document| changes.borrow_mut().push(document),
        )
        .unwrap();
        editor.restore_selection(source, 0, 0);
        let paste: Vec<_> = " Sep 27 ".encode_utf16().chain(Some(0)).collect();
        let suffix: Vec<_> = "Z".encode_utf16().chain(Some(0)).collect();
        unsafe {
            SendMessageW(
                editor.inner.hwnd,
                super::EM_REPLACESEL,
                1,
                paste.as_ptr() as super::LPARAM,
            );
            super::set_selection(
                editor.inner.hwnd,
                super::CharRange {
                    cp_min: -1,
                    cp_max: -1,
                },
            );
            SendMessageW(
                editor.inner.hwnd,
                super::EM_REPLACESEL,
                1,
                suffix.as_ptr() as super::LPARAM,
            );
        }
        let edited = editor.document();
        assert_eq!(edited.text, " Sep 27 [[date:2026-09-27]]Z");
        assert_eq!(edited.chip_ids, vec![id]);
        editor.flush_pending_change();
        assert_eq!(saved.borrow().last(), Some(&edited));
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
    fn inline_checkboxes_preserve_source_and_date_identity_after_replacement() {
        let _guard = native_test_guard();
        let parent = TestParent::new();
        let source = "[ ] café 😀 [[date:2026-10-03]]\n[x] Done";
        let id = uuid::Uuid::new_v4().to_string();
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
        assert_eq!(editor.document().text, source);
        assert_eq!(editor.document().chip_ids, vec![id.clone()]);
        let checked = source.replacen("[ ]", "[x]", 1);
        editor.set_document(&NativeDocument {
            text: checked.clone(),
            chip_ids: vec![id.clone()],
            ..Default::default()
        });
        assert_eq!(editor.document().text, checked);
        assert_eq!(editor.document().chip_ids, vec![id.clone()]);
        editor.history_action(false);
        assert_eq!(editor.document().text, source);
        assert_eq!(editor.document().chip_ids, vec![id]);
    }

    #[test]
    fn completed_task_presentation_never_changes_saved_user_styles() {
        let _guard = native_test_guard();
        let parent = TestParent::new();
        let source = "[x] café 😀 task\nOrdinary text";
        let task = source.find("task").unwrap();
        let expected = NativeDocument {
            text: source.into(),
            bold: vec![(4, 9)],
            strike: vec![(task, task + 4)],
            ..Default::default()
        };
        let editor =
            NativeRichEditor::attach_to_parent(parent.0, 1.0, expected.clone(), |_| {}).unwrap();
        for _ in 0..3 {
            let snapshot = editor.document();
            assert_eq!(snapshot.text, expected.text);
            assert_eq!(snapshot.bold, expected.bold);
            assert_eq!(snapshot.strike, expected.strike);
        }
        unsafe {
            super::set_selection(
                editor.inner.hwnd,
                super::CharRange {
                    cp_min: 4,
                    cp_max: 5,
                },
            );
            let mut format = super::char_format();
            SendMessageW(
                editor.inner.hwnd,
                super::EM_GETCHARFORMAT,
                super::SCF_SELECTION,
                (&mut format as *mut super::CharFormat2W) as super::LPARAM,
            );
            assert_ne!(format.dw_effects & super::CFM_STRIKEOUT, 0);
            assert_eq!(format.cr_text_color, 0x00808080);
        }
        let mut unchecked = expected.clone();
        unchecked.text = source.replacen("[x]", "[ ]", 1);
        editor.set_document(&unchecked);
        assert_eq!(editor.document().strike, expected.strike);
        editor.history_action(false);
        assert_eq!(editor.document().text, expected.text);
        assert_eq!(editor.document().strike, expected.strike);
        editor.restore_selection(source, source.len(), source.len());
        unsafe {
            SendMessageW(editor.inner.hwnd, super::WM_CHAR, '!' as usize, 0);
        }
        let edited = editor.document();
        assert_eq!(edited.text, format!("{source}!"));
        assert_eq!(edited.strike, expected.strike);
    }

    #[test]
    fn missing_inline_image_preserves_reference_and_remains_editable() {
        let _guard = native_test_guard();
        let parent = TestParent::new();
        let source = format!(
            "Before {} After",
            crate::inline_images::marker(&uuid::Uuid::new_v4().to_string())
        );
        let editor = NativeRichEditor::attach_to_parent(
            parent.0,
            1.0,
            NativeDocument {
                text: source.clone(),
                ..Default::default()
            },
            |_| {},
        )
        .unwrap();
        editor.set_image_loader(|_| Err("Missing thumbnail".into()));
        assert!(!editor.image_errors().is_empty());
        assert_eq!(editor.document().text, source);
        editor.restore_selection(&source, 0, 0);
        unsafe {
            SendMessageW(editor.inner.hwnd, super::WM_CHAR, 'X' as usize, 0);
        }
        assert_eq!(editor.document().text, format!("X{source}"));
    }

    #[test]
    fn inline_images_keep_unicode_styles_date_ids_delete_undo_and_resize() {
        let _guard = native_test_guard();
        let parent = TestParent::new();
        let image = uuid::Uuid::new_v4().to_string();
        let source = format!(
            "café 😀 [[date:2026-10-10]]\n{}\n[x] Done [[date:2026-10-11]]\n",
            crate::inline_images::marker(&image)
        );
        let ids = vec![
            uuid::Uuid::new_v4().to_string(),
            uuid::Uuid::new_v4().to_string(),
        ];
        let expected = NativeDocument {
            text: source.clone(),
            bold: vec![(0, 5)],
            chip_ids: ids.clone(),
            ..Default::default()
        };
        let editor =
            NativeRichEditor::attach_to_parent(parent.0, 1.0, expected.clone(), |_| {}).unwrap();
        let mut png = std::io::Cursor::new(Vec::new());
        image::RgbaImage::new(320, 240)
            .write_to(&mut png, image::ImageOutputFormat::Png)
            .unwrap();
        let png = png.into_inner();
        editor.set_image_loader(move |_| Ok(png.clone()));
        assert!(
            editor.image_errors().is_empty(),
            "{:?}",
            editor.image_errors()
        );
        for width in [300.0, 180.0, 400.0] {
            editor.set_layout_bounds([0.0, 0.0, width, 400.0]);
            let document = editor.document();
            assert_eq!(document.text, source);
            assert_eq!(document.chip_ids, ids);
            assert_eq!(document.bold, expected.bold);
            assert!(document.strike.is_empty());
        }
        let reference = crate::inline_images::occurrences(&source).remove(0);
        editor.restore_selection(&source, reference.start, reference.end);
        unsafe {
            SendMessageW(editor.inner.hwnd, super::WM_CLEAR, 0, 0);
        }
        assert!(!editor.document().text.contains("[[image:"));
        editor.history_action(false);
        assert_eq!(editor.document().text, source);
        assert_eq!(editor.document().chip_ids, ids);
        editor.restore_selection(&source, 0, 0);
        unsafe {
            SendMessageW(editor.inner.hwnd, super::WM_CHAR, 'X' as usize, 0);
        }
        assert_eq!(editor.document().text, format!("X{source}"));
        assert_eq!(editor.document().chip_ids, ids);
    }

    #[test]
    fn caret_import_reopens_with_placement_and_unified_undo_redo() {
        let _guard = native_test_guard();
        let directory =
            std::env::temp_dir().join(format!("sticky-native-image-{}", uuid::Uuid::new_v4()));
        let db = crate::db::Db::open_in_dir(&directory).unwrap();
        let source = "café 😀 then [[date:2026-10-10]]";
        let id = uuid::Uuid::new_v4().to_string();
        let initial = NativeDocument {
            text: source.into(),
            chip_ids: vec![id.clone()],
            bold: vec![(0, 5)],
            ..Default::default()
        };
        let initial_content = crate::native_projection::with_ids(
            &crate::content_for_editor_with_checklists_and_formats(
                source,
                false,
                &crate::FormatRanges {
                    bold: initial.bold.clone(),
                    ..Default::default()
                },
            ),
            &initial.chip_ids,
        );
        crate::db::notes::create(&db.conn, "note", &initial_content, "#FFF9B1").unwrap();
        let parent = TestParent::new();
        let editor =
            NativeRichEditor::attach_to_parent(parent.0, 1.0, initial.clone(), |_| {}).unwrap();
        editor.restore_selection(source, 5, 5);
        let mut png = std::io::Cursor::new(Vec::new());
        image::RgbImage::from_pixel(80, 60, image::Rgb([20, 80, 120]))
            .write_to(&mut png, image::ImageOutputFormat::Png)
            .unwrap();
        let prepared =
            crate::db::attachments::prepare_bytes(png.into_inner(), "Pasted image.png").unwrap();
        let image_id = prepared.item.id.clone();
        let candidate = editor.document_with_image(&image_id);
        assert_eq!(
            candidate.text,
            format!(
                "café{} 😀 then [[date:2026-10-10]]",
                crate::inline_images::marker(&image_id)
            )
        );
        assert_eq!(candidate.chip_ids, vec![id]);
        let formats = crate::FormatRanges {
            bold: candidate.bold.clone(),
            italic: candidate.italic.clone(),
            underline: candidate.underline.clone(),
            strike: candidate.strike.clone(),
        };
        let content = crate::native_projection::with_ids(
            &crate::content_for_editor_with_checklists_and_formats(
                &candidate.text,
                false,
                &formats,
            ),
            &candidate.chip_ids,
        );
        let mut content: serde_json::Value = serde_json::from_str(&content).unwrap();
        content["editor"]["inlineImages"] = true.into();
        crate::db::attachments::commit_prepared(
            &db.conn,
            "note",
            prepared,
            Some(&content.to_string()),
        )
        .unwrap();
        let thumbnail = crate::db::attachments::thumbnail_bytes(&db.conn, &image_id).unwrap();
        editor.set_image_loader(move |_| Ok(thumbnail.clone()));
        editor.commit_image_document(&candidate, &image_id);
        assert_eq!(editor.document(), candidate);
        editor.history_action(false);
        assert_eq!(editor.document(), initial);
        editor.history_action(true);
        assert_eq!(editor.document(), candidate);
        drop(editor);
        drop(db);
        let db = crate::db::Db::open_in_dir(&directory).unwrap();
        let note = crate::db::notes::get(&db.conn, "note").unwrap().unwrap();
        let (text, _, ranges) = crate::editor_state_from_content_with_checklists(&note.content);
        let formats = crate::parse_format_ranges(&ranges);
        let restored = NativeDocument {
            text,
            bold: formats.bold,
            italic: formats.italic,
            underline: formats.underline,
            strike: formats.strike,
            chip_ids: crate::native_projection::content_ids(&note.content),
        };
        let reopened = NativeRichEditor::attach_to_parent(parent.0, 1.0, restored, |_| {}).unwrap();
        let thumbnail = crate::db::attachments::thumbnail_bytes(&db.conn, &image_id).unwrap();
        reopened.set_image_loader(move |_| Ok(thumbnail.clone()));
        assert_eq!(reopened.document(), candidate);
        assert!(reopened.image_errors().is_empty());
        drop(reopened);
        drop(db);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn native_image_keeps_gradient_pixels_not_just_object_count() {
        use windows::core::Interface;
        use windows::Win32::{
            System::Com::{IDataObject, FORMATETC, TYMED_HGLOBAL},
            UI::Controls::RichEdit::{IRichEditOle, REOBJECT, REO_GETOBJ_POLEOBJ},
        };
        let _guard = native_test_guard();
        let parent = TestParent::new();
        let editor =
            NativeRichEditor::attach_to_parent(parent.0, 1.0, NativeDocument::default(), |_| {})
                .unwrap();
        let original = image::RgbImage::from_fn(320, 213, |x, y| {
            image::Rgb([x as u8, y as u8, (x + y) as u8])
        });
        let mut png = std::io::Cursor::new(Vec::new());
        original
            .write_to(&mut png, image::ImageOutputFormat::Png)
            .unwrap();
        unsafe {
            crate::native_image::insert_thumbnail(editor.inner.hwnd, png.get_ref()).unwrap();
            let mut pointer: *mut std::ffi::c_void = std::ptr::null_mut();
            SendMessageW(
                editor.inner.hwnd,
                1084,
                0,
                (&mut pointer as *mut *mut std::ffi::c_void) as super::LPARAM,
            );
            let ole = windows::core::IUnknown::from_raw(pointer)
                .cast::<IRichEditOle>()
                .unwrap();
            let mut object = REOBJECT {
                cbStruct: std::mem::size_of::<REOBJECT>() as u32,
                ..Default::default()
            };
            ole.GetObject(0, &mut object, REO_GETOBJ_POLEOBJ).unwrap();
            let object_interface = std::mem::ManuallyDrop::take(&mut object.poleobj).unwrap();
            let data = object_interface.cast::<IDataObject>().unwrap();
            let format = FORMATETC {
                cfFormat: 8,
                dwAspect: 1,
                lindex: -1,
                tymed: TYMED_HGLOBAL.0 as u32,
                ..Default::default()
            };
            let mut medium = data.GetData(&format).unwrap();
            let handle = medium.u.hGlobal.0;
            let pointer = windows_sys::Win32::System::Memory::GlobalLock(handle);
            let size = windows_sys::Win32::System::Memory::GlobalSize(handle);
            let bytes = std::slice::from_raw_parts(pointer as *const u8, size).to_vec();
            windows_sys::Win32::System::Memory::GlobalUnlock(handle);
            windows::Win32::System::Ole::ReleaseStgMedium(&mut medium);
            let png = crate::native_image::dib_to_png(&bytes).unwrap();
            let actual = image::load_from_memory_with_format(&png, image::ImageFormat::Png)
                .unwrap()
                .to_rgb8();
            assert_eq!(actual.dimensions(), original.dimensions());
            assert!(
                actual.as_raw() == original.as_raw(),
                "Native image pixels changed"
            );
        }
    }

    #[test]
    fn native_thumbnail_is_a_real_inline_object_in_the_existing_editor() {
        use windows::core::Interface;
        use windows::Win32::UI::Controls::RichEdit::IRichEditOle;
        let _guard = native_test_guard();
        unsafe {
            windows::Win32::System::Ole::OleInitialize(None).unwrap();
        }
        struct OleSession;
        impl Drop for OleSession {
            fn drop(&mut self) {
                unsafe {
                    windows::Win32::System::Ole::OleUninitialize();
                }
            }
        }
        let _ole = OleSession;
        let parent = TestParent::new();
        let editor = NativeRichEditor::attach_to_parent(
            parent.0,
            1.0,
            NativeDocument {
                text: "Before\nAfter".into(),
                ..Default::default()
            },
            |_| {},
        )
        .unwrap();
        editor.restore_selection("Before\nAfter", 7, 7);
        let mut bytes = std::io::Cursor::new(Vec::new());
        image::RgbaImage::new(160, 120)
            .write_to(&mut bytes, image::ImageOutputFormat::Png)
            .unwrap();
        unsafe {
            crate::native_image::insert_thumbnail(editor.inner.hwnd, bytes.get_ref()).unwrap();
            let mut pointer: *mut std::ffi::c_void = std::ptr::null_mut();
            assert_ne!(
                SendMessageW(
                    editor.inner.hwnd,
                    1084,
                    0,
                    (&mut pointer as *mut *mut std::ffi::c_void) as super::LPARAM
                ),
                0
            );
            assert!(!pointer.is_null());
            let ole = windows::core::IUnknown::from_raw(pointer)
                .cast::<IRichEditOle>()
                .unwrap();
            assert_eq!(ole.GetObjectCount(), 1);
        }
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
                assert_eq!(font.y_height, 210);
                assert_eq!(font.cr_text_color, 0x00f6f5f5);
                let previous =
                    SendMessageW(editor.inner.hwnd, super::EM_SETBKGNDCOLOR, 0, 0x00454545);
                assert_eq!(previous, 0x00454545);
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
        assert!(editor.set_layout_bounds([10.0, 10.0, 200.0, 140.0]));
        assert!(!editor.set_layout_bounds([10.0, 10.0, 200.0, 140.0]));
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

    #[test]
    fn scale_transitions_preserve_unicode_selection_styles_and_chip_identity() {
        let _guard = native_test_guard();
        let parent = TestParent::new();
        let document = NativeDocument {
            text: "café 😀 [[date:2026-10-06]] and [[date:2026-10-06]]".into(),
            bold: vec![(0, 5)],
            chip_ids: vec![
                uuid::Uuid::new_v4().to_string(),
                uuid::Uuid::new_v4().to_string(),
            ],
            ..Default::default()
        };
        let editor =
            NativeRichEditor::attach_to_parent(parent.0, 1.0, document.clone(), |_| {}).unwrap();
        editor.set_layout_bounds([10.0, 20.0, 180.0, 120.0]);
        editor.restore_selection(&document.text, 6, 10);
        for scale in [2.0, 1.5, 1.0, 2.0, 1.0] {
            editor.update_scale_factor(scale);
            assert_eq!(editor.document(), document);
            assert_eq!(editor.selection_byte_offsets(&document.text), (6, 10));
        }
        editor.set_visible(false);
        editor.set_visible(true);
        assert_eq!(editor.document(), document);
        assert_eq!(editor.selection_byte_offsets(&document.text), (6, 10));
        assert_eq!(
            unsafe { windows_sys::Win32::UI::Input::KeyboardAndMouse::GetFocus() },
            editor.inner.hwnd
        );
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
