#![allow(clippy::field_reassign_with_default)]
use anyhow::Result;
use std::path::PathBuf;

#[cfg(windows)]
fn select_kind(owner: &slint::Window, export: bool) -> Result<Option<PathBuf>> {
    use i_slint_backend_winit::WinitWindowAccessor;
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    use windows_sys::Win32::UI::Controls::Dialogs::*;
    let hwnd = owner
        .with_winit_window(|host| match host.window_handle().ok().map(|h| h.as_raw()) {
            Some(RawWindowHandle::Win32(h)) => h.hwnd.get() as windows_sys::Win32::Foundation::HWND,
            _ => std::ptr::null_mut(),
        })
        .unwrap_or(std::ptr::null_mut());
    anyhow::ensure!(!hwnd.is_null(), "Image picker needs an open note window");
    let filter: Vec<u16> = (if export {
        "Sticky Notes+ export\0*.json\0\0"
    } else {
        "Images (PNG, JPEG)\0*.png;*.jpg;*.jpeg\0\0"
    })
    .encode_utf16()
    .collect();
    let title: Vec<u16> = (if export {
        "Import from export\0"
    } else {
        "Add image\0"
    })
    .encode_utf16()
    .collect();
    let mut path = vec![0u16; 32768];
    let mut dialog = OPENFILENAMEW::default();
    dialog.lStructSize = std::mem::size_of::<OPENFILENAMEW>() as u32;
    dialog.hwndOwner = hwnd;
    dialog.lpstrFilter = filter.as_ptr();
    dialog.lpstrTitle = title.as_ptr();
    dialog.lpstrFile = path.as_mut_ptr();
    dialog.nMaxFile = path.len() as u32;
    dialog.Flags = OFN_FILEMUSTEXIST
        | OFN_PATHMUSTEXIST
        | OFN_NOCHANGEDIR
        | OFN_DONTADDTORECENT
        | OFN_EXPLORER;
    if unsafe { GetOpenFileNameW(&mut dialog) } == 0 {
        let error = unsafe { CommDlgExtendedError() };
        anyhow::ensure!(error == 0, "Image picker failed ({error})");
        return Ok(None);
    }
    let end = path.iter().position(|c| *c == 0).unwrap_or(path.len());
    use std::os::windows::ffi::OsStringExt;
    Ok(Some(std::ffi::OsString::from_wide(&path[..end]).into()))
}

#[cfg(not(windows))]
fn select_kind(_: &slint::Window, _: bool) -> Result<Option<PathBuf>> {
    anyhow::bail!("File selection is currently available on Windows")
}

/// Asks where to save a notes export, offering `suggested` as the file name.
#[cfg(windows)]
pub fn select_export(owner: &slint::Window, suggested: &str) -> Result<Option<PathBuf>> {
    use i_slint_backend_winit::WinitWindowAccessor;
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    use windows_sys::Win32::UI::Controls::Dialogs::*;
    let hwnd = owner
        .with_winit_window(|host| match host.window_handle().ok().map(|h| h.as_raw()) {
            Some(RawWindowHandle::Win32(h)) => h.hwnd.get() as windows_sys::Win32::Foundation::HWND,
            _ => std::ptr::null_mut(),
        })
        .unwrap_or(std::ptr::null_mut());
    let filter: Vec<u16> = "JSON file\0*.json\0\0".encode_utf16().collect();
    let title: Vec<u16> = "Export notes\0".encode_utf16().collect();
    let extension: Vec<u16> = "json\0".encode_utf16().collect();
    let mut path = vec![0u16; 32768];
    for (slot, unit) in path.iter_mut().zip(suggested.encode_utf16()) {
        *slot = unit;
    }
    let mut dialog = OPENFILENAMEW::default();
    dialog.lStructSize = std::mem::size_of::<OPENFILENAMEW>() as u32;
    dialog.hwndOwner = hwnd;
    dialog.lpstrFilter = filter.as_ptr();
    dialog.lpstrTitle = title.as_ptr();
    dialog.lpstrDefExt = extension.as_ptr();
    dialog.lpstrFile = path.as_mut_ptr();
    dialog.nMaxFile = path.len() as u32;
    dialog.Flags = OFN_OVERWRITEPROMPT
        | OFN_PATHMUSTEXIST
        | OFN_NOCHANGEDIR
        | OFN_DONTADDTORECENT
        | OFN_EXPLORER;
    if unsafe { GetSaveFileNameW(&mut dialog) } == 0 {
        let error = unsafe { CommDlgExtendedError() };
        anyhow::ensure!(error == 0, "Save dialog failed ({error})");
        return Ok(None);
    }
    let end = path.iter().position(|c| *c == 0).unwrap_or(path.len());
    use std::os::windows::ffi::OsStringExt;
    Ok(Some(std::ffi::OsString::from_wide(&path[..end]).into()))
}

pub fn select(owner: &slint::Window) -> Result<Option<PathBuf>> {
    select_kind(owner, false)
}
pub fn select_export_file(owner: &slint::Window) -> Result<Option<PathBuf>> {
    select_kind(owner, true)
}
