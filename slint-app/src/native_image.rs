//! Bounded image transport for the existing RichEdit child, not another window.
//! Only app-generated thumbnails belong here; originals remain on disk.
use anyhow::{ensure, Result};
use image::GenericImageView;
use std::io::Cursor;

pub struct OleSession;
pub enum ClipboardContent {
    Text(Vec<u16>),
    Image(Vec<u8>),
}

pub fn dib_to_png(dib: &[u8]) -> Result<Vec<u8>> {
    ensure!(
        dib.len() >= 40 && dib.len() <= 64 * 1024 * 1024,
        "Clipboard image size is invalid"
    );
    let u32_at = |offset| u32::from_le_bytes(dib[offset..offset + 4].try_into().unwrap());
    let header = u32_at(0) as usize;
    ensure!(
        matches!(header, 40 | 108 | 124) && dib.len() >= header,
        "Unsupported clipboard bitmap header"
    );
    let width = u32_at(4) as i32;
    let height = u32_at(8) as i32;
    ensure!(
        width > 0
            && height != 0
            && height != i32::MIN
            && width as u64 * height.unsigned_abs() as u64 <= 8_000_000,
        "Clipboard image exceeds 8 megapixels"
    );
    let planes = u16::from_le_bytes(dib[12..14].try_into().unwrap());
    let depth = u16::from_le_bytes(dib[14..16].try_into().unwrap());
    let compression = u32_at(16);
    ensure!(
        planes == 1 && matches!(depth, 24 | 32) && matches!(compression, 0 | 3),
        "Unsupported clipboard bitmap encoding"
    );
    let masks = if header == 40 && compression == 3 {
        12
    } else {
        0
    };
    let offset = 14 + header + masks;
    ensure!(dib.len() >= header + masks, "Truncated clipboard bitmap");
    let mut bmp = Vec::with_capacity(dib.len() + 14);
    bmp.extend(b"BM");
    bmp.extend(((dib.len() + 14) as u32).to_le_bytes());
    bmp.extend([0; 4]);
    bmp.extend((offset as u32).to_le_bytes());
    bmp.extend(dib);
    let mut reader = image::io::Reader::with_format(Cursor::new(bmp), image::ImageFormat::Bmp);
    let mut limits = image::io::Limits::default();
    limits.max_alloc = Some(64 * 1024 * 1024);
    limits.max_image_width = Some(8192);
    limits.max_image_height = Some(8192);
    reader.limits(limits);
    let pixels = reader.decode()?;
    ensure!(
        pixels.width() == width as u32 && pixels.height() == height.unsigned_abs(),
        "Clipboard bitmap dimensions changed"
    );
    let mut png = Cursor::new(Vec::new());
    pixels.write_to(&mut png, image::ImageOutputFormat::Png)?;
    ensure!(
        png.get_ref().len() <= 20 * 1024 * 1024,
        "Clipboard image exceeds 20 MiB"
    );
    Ok(png.into_inner())
}

pub unsafe fn clipboard_content(
    hwnd: windows_sys::Win32::Foundation::HWND,
) -> Result<ClipboardContent> {
    use windows_sys::Win32::System::{
        DataExchange::{
            CloseClipboard, GetClipboardData, IsClipboardFormatAvailable, OpenClipboard,
        },
        Memory::{GlobalLock, GlobalSize, GlobalUnlock},
    };
    ensure!(
        OpenClipboard(hwnd) != 0,
        "Clipboard is busy; try paste again"
    );
    struct Close;
    impl Drop for Close {
        fn drop(&mut self) {
            unsafe {
                CloseClipboard();
            }
        }
    }
    let _close = Close;
    // Prefer text when an application advertises both text and bitmap data.
    let format = if IsClipboardFormatAvailable(13) != 0 {
        13
    } else if IsClipboardFormatAvailable(17) != 0 {
        17
    } else if IsClipboardFormatAvailable(8) != 0 {
        8
    } else {
        anyhow::bail!("Clipboard has no supported text or bitmap");
    };
    let handle = GetClipboardData(format);
    ensure!(!handle.is_null(), "Clipboard data unavailable");
    let size = GlobalSize(handle);
    ensure!(
        size > 0 && size <= 64 * 1024 * 1024,
        "Clipboard data exceeds the size limit"
    );
    let pointer = GlobalLock(handle);
    ensure!(!pointer.is_null(), "Clipboard data unavailable");
    let bytes = std::slice::from_raw_parts(pointer as *const u8, size).to_vec();
    GlobalUnlock(handle);
    if format == 13 {
        let mut text: Vec<u16> = bytes
            .chunks_exact(2)
            .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
            .take_while(|unit| *unit != 0)
            .collect();
        text.push(0);
        Ok(ClipboardContent::Text(text))
    } else {
        Ok(ClipboardContent::Image(dib_to_png(&bytes)?))
    }
}
impl OleSession {
    pub fn new() -> Result<Self> {
        unsafe {
            windows::Win32::System::Ole::OleInitialize(None)?;
        }
        Ok(Self)
    }
}
impl Drop for OleSession {
    fn drop(&mut self) {
        unsafe {
            windows::Win32::System::Ole::OleUninitialize();
        }
    }
}
use windows::core::{OutRef, Ref, BOOL, GUID};
use windows::Win32::{
    Foundation::HGLOBAL,
    System::{
        Com::{
            IDataObject,
            StructuredStorage::{
                CreateILockBytesOnHGlobal, IStorage, StgCreateDocfileOnILockBytes,
            },
            STGM_CREATE, STGM_READWRITE, STGM_SHARE_EXCLUSIVE,
        },
        Ole::{IOleInPlaceFrame, IOleInPlaceUIWindow, IOleObject, DROPEFFECT, OLEINPLACEFRAMEINFO},
        SystemServices::{MODIFIERKEYS_FLAGS, RECO_FLAGS},
    },
    UI::{
        Controls::RichEdit::{
            IRichEditOleCallback, IRichEditOleCallback_Impl, CHARRANGE,
            RICH_EDIT_GET_CONTEXT_MENU_SEL_TYPE,
        },
        WindowsAndMessaging::HMENU,
    },
};

fn unsupported() -> windows::core::Error {
    windows::core::Error::from_hresult(windows::core::HRESULT(0x80004001u32 as i32))
}
pub unsafe fn object_positions(hwnd: windows_sys::Win32::Foundation::HWND) -> Vec<usize> {
    use windows::core::Interface;
    use windows::Win32::UI::Controls::RichEdit::{
        IRichEditOle, REOBJECT, REO_GETOBJ_NO_INTERFACES,
    };
    let mut pointer: *mut std::ffi::c_void = std::ptr::null_mut();
    windows_sys::Win32::UI::WindowsAndMessaging::SendMessageW(
        hwnd,
        1084,
        0,
        (&mut pointer as *mut *mut std::ffi::c_void) as isize,
    );
    if pointer.is_null() {
        return Vec::new();
    }
    let Ok(objects) = windows::core::IUnknown::from_raw(pointer).cast::<IRichEditOle>() else {
        return Vec::new();
    };
    let mut positions = Vec::new();
    for index in 0..objects.GetObjectCount() {
        let mut object = REOBJECT {
            cbStruct: std::mem::size_of::<REOBJECT>() as u32,
            ..Default::default()
        };
        if objects
            .GetObject(index, &mut object, REO_GETOBJ_NO_INTERFACES)
            .is_ok()
            && object.cp >= 0
        {
            positions.push(object.cp as usize);
        }
    }
    positions
}

// RichEdit needs backing storage even for static bitmap pictures. Memory-only
// storage avoids temp files, document servers and additional windows.
#[windows_core::implement(IRichEditOleCallback)]
struct ImageStorage;
impl IRichEditOleCallback_Impl for ImageStorage_Impl {
    fn GetNewStorage(&self) -> windows::core::Result<IStorage> {
        unsafe {
            let bytes = CreateILockBytesOnHGlobal(None, true)?;
            StgCreateDocfileOnILockBytes(
                &bytes,
                STGM_CREATE | STGM_READWRITE | STGM_SHARE_EXCLUSIVE,
                0,
            )
        }
    }
    fn GetInPlaceContext(
        &self,
        _: OutRef<'_, IOleInPlaceFrame>,
        _: OutRef<'_, IOleInPlaceUIWindow>,
        _: *mut OLEINPLACEFRAMEINFO,
    ) -> windows::core::Result<()> {
        Err(unsupported())
    }
    fn ShowContainerUI(&self, _: BOOL) -> windows::core::Result<()> {
        Ok(())
    }
    fn QueryInsertObject(
        &self,
        _: *mut GUID,
        _: Ref<'_, IStorage>,
        _: i32,
    ) -> windows::core::Result<()> {
        Ok(())
    }
    fn DeleteObject(&self, _: Ref<'_, IOleObject>) -> windows::core::Result<()> {
        Ok(())
    }
    fn QueryAcceptData(
        &self,
        _: Ref<'_, IDataObject>,
        _: *mut u16,
        _: RECO_FLAGS,
        _: BOOL,
        _: HGLOBAL,
    ) -> windows::core::Result<()> {
        Err(unsupported())
    }
    fn ContextSensitiveHelp(&self, _: BOOL) -> windows::core::Result<()> {
        Err(unsupported())
    }
    fn GetClipboardData(
        &self,
        _: *mut CHARRANGE,
        _: u32,
        _: OutRef<'_, IDataObject>,
    ) -> windows::core::Result<()> {
        Err(unsupported())
    }
    fn GetDragDropEffect(
        &self,
        _: BOOL,
        _: MODIFIERKEYS_FLAGS,
        _: *mut DROPEFFECT,
    ) -> windows::core::Result<()> {
        Err(unsupported())
    }
    fn GetContextMenu(
        &self,
        _: RICH_EDIT_GET_CONTEXT_MENU_SEL_TYPE,
        _: Ref<'_, IOleObject>,
        _: *mut CHARRANGE,
        _: *mut HMENU,
    ) -> windows::core::Result<()> {
        Err(unsupported())
    }
}

#[cfg(test)]
pub fn thumbnail_rtf(png: &[u8]) -> Result<Vec<u8>> {
    thumbnail_rtf_width(png, 320)
}
#[cfg(test)]
pub fn thumbnail_rtf_width(png: &[u8], available_width: u32) -> Result<Vec<u8>> {
    thumbnail_rtf_paper(png, available_width, 0x00ff_ffff)
}
fn thumbnail_rtf_paper(png: &[u8], available_width: u32, paper: u32) -> Result<Vec<u8>> {
    ensure!(png.len() <= 1024 * 1024, "Thumbnail exceeds the size limit");
    let mut reader = image::io::Reader::with_format(Cursor::new(png), image::ImageFormat::Png);
    let mut limits = image::io::Limits::default();
    limits.max_image_width = Some(320);
    limits.max_image_height = Some(240);
    limits.max_alloc = Some(1024 * 1024);
    reader.limits(limits);
    let decoded = reader.decode()?;
    // Rasterize at the displayed logical size rather than asking the legacy
    // static OLE bitmap renderer to downsample a DIB during every paint.
    let target_width = decoded.width().min(available_width.max(1));
    let decoded = if target_width < decoded.width() {
        decoded.resize(target_width, 240, image::imageops::FilterType::Lanczos3)
    } else {
        decoded
    };
    let (width, height) = decoded.dimensions();
    ensure!(width > 0 && height > 0, "Empty thumbnail");
    // A fixed display size in twips preserves aspect ratio and is independent
    // of the PNG's optional DPI metadata. RichEdit handles monitor scaling.
    // Use a static DIB: this RichEdit version silently drops PNG-blip RTF.
    // Convert only the already bounded thumbnail, never the original image.
    let background = [(paper >> 16) as u8, (paper >> 8) as u8, paper as u8];
    let mut rgb = decoded.to_rgb8();
    for (out, input) in rgb.pixels_mut().zip(decoded.to_rgba8().pixels()) {
        for channel in 0..3 {
            let alpha = u32::from(input[3]);
            out[channel] = ((u32::from(input[channel]) * alpha
                + u32::from(background[channel]) * (255 - alpha)
                + 127)
                / 255) as u8;
        }
    }
    let mut bitmap = Cursor::new(Vec::new());
    rgb.write_to(&mut bitmap, image::ImageOutputFormat::Bmp)?;
    let dib = &bitmap.get_ref()[14..]; // omit the BMP file header
    let mut rtf = format!(
        "{{\\rtf1\\ansi{{\\pict\\dibitmap0\\wbmbitspixel24\\wbmplanes1\\wbmwidthbytes{}\\picw{width}\\pich{height}\\picwgoal{}\\pichgoal{} ",
        (width * 3 + 3) & !3,
        width.min(available_width.max(1)) * 15,
        (height * width.min(available_width.max(1)) / width).max(1) * 15
    )
    .into_bytes();
    const HEX: &[u8; 16] = b"0123456789abcdef";
    rtf.reserve(dib.len() * 2 + 2);
    for byte in dib {
        rtf.extend([HEX[(byte >> 4) as usize], HEX[(byte & 15) as usize]]);
    }
    rtf.extend(b"}}");
    Ok(rtf)
}

#[cfg(test)]
pub unsafe fn insert_thumbnail(
    hwnd: windows_sys::Win32::Foundation::HWND,
    png: &[u8],
) -> Result<()> {
    insert_thumbnail_width(hwnd, png, 320)
}
#[cfg(test)]
pub unsafe fn insert_thumbnail_width(
    hwnd: windows_sys::Win32::Foundation::HWND,
    png: &[u8],
    available_width: u32,
) -> Result<()> {
    insert_thumbnail_on_paper(hwnd, png, available_width, 0x00ff_ffff)
}
pub unsafe fn insert_thumbnail_on_paper(
    hwnd: windows_sys::Win32::Foundation::HWND,
    png: &[u8],
    available_width: u32,
    paper: u32,
) -> Result<()> {
    use windows::core::Interface;
    use windows::Win32::UI::Controls::RichEdit::{EDITSTREAM, EM_STREAMIN, SFF_SELECTION, SF_RTF};
    use windows_sys::Win32::UI::WindowsAndMessaging::SendMessageW;
    let rtf = thumbnail_rtf_paper(png, available_width, paper)?;
    let mut raw: *mut std::ffi::c_void = std::ptr::null_mut();
    ensure!(
        SendMessageW(
            hwnd,
            1084,
            0,
            (&mut raw as *mut *mut std::ffi::c_void) as isize
        ) != 0
            && !raw.is_null(),
        "Native image interface unavailable"
    );
    let objects = windows::core::IUnknown::from_raw(raw)
        .cast::<windows::Win32::UI::Controls::RichEdit::IRichEditOle>()?;
    let before = objects.GetObjectCount();
    let mut selection = CHARRANGE::default();
    SendMessageW(hwnd, 1076, 0, (&mut selection as *mut CHARRANGE) as isize);
    ensure!(
        selection.cpMin == selection.cpMax,
        "Image insertion requires a caret"
    );
    let callback: IRichEditOleCallback = ImageStorage.into();
    let installed = SendMessageW(
        hwnd,
        windows::Win32::UI::Controls::RichEdit::EM_SETOLECALLBACK,
        0,
        callback.as_raw() as isize,
    );
    ensure!(installed != 0, "Native image storage unavailable");
    struct Detach(windows_sys::Win32::Foundation::HWND);
    impl Drop for Detach {
        fn drop(&mut self) {
            unsafe {
                SendMessageW(
                    self.0,
                    windows::Win32::UI::Controls::RichEdit::EM_SETOLECALLBACK,
                    0,
                    0,
                );
            }
        }
    }
    let _detach = Detach(hwnd);
    struct Stream<'a> {
        bytes: &'a [u8],
        offset: usize,
    }
    unsafe extern "system" fn read(
        cookie: usize,
        buffer: *mut u8,
        count: i32,
        read: *mut i32,
    ) -> u32 {
        if buffer.is_null() || read.is_null() || count < 0 {
            return 1;
        }
        let stream = &mut *(cookie as *mut Stream<'_>);
        let size = (count as usize).min(stream.bytes.len() - stream.offset);
        std::ptr::copy_nonoverlapping(stream.bytes.as_ptr().add(stream.offset), buffer, size);
        stream.offset += size;
        *read = size as i32;
        0
    }
    let mut source = Stream {
        bytes: &rtf,
        offset: 0,
    };
    let mut stream = EDITSTREAM {
        dwCookie: (&mut source as *mut Stream<'_>) as usize,
        dwError: 0,
        pfnCallback: Some(read),
    };
    SendMessageW(
        hwnd,
        EM_STREAMIN,
        (SF_RTF | SFF_SELECTION) as usize,
        (&mut stream as *mut EDITSTREAM) as isize,
    );
    let error = stream.dwError;
    ensure!(error == 0, "Native image stream failed: {error}");
    ensure!(
        source.offset == rtf.len(),
        "Native image stream was incomplete"
    );
    ensure!(
        objects.GetObjectCount() == before + 1,
        "Native image object was not inserted"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn clipboard_bitmap_roundtrips_and_rejects_oversized_or_malformed_headers() {
        let mut bmp = Cursor::new(Vec::new());
        image::RgbImage::from_pixel(30, 20, image::Rgb([12, 34, 56]))
            .write_to(&mut bmp, image::ImageOutputFormat::Bmp)
            .unwrap();
        let dib = &bmp.get_ref()[14..];
        let png = dib_to_png(dib).unwrap();
        let image = image::load_from_memory_with_format(&png, image::ImageFormat::Png)
            .unwrap()
            .to_rgb8();
        assert_eq!(image.dimensions(), (30, 20));
        assert_eq!(image.get_pixel(0, 0).0, [12, 34, 56]);
        assert!(dib_to_png(&dib[..20]).is_err());
        let mut huge = dib.to_vec();
        huge[4..8].copy_from_slice(&10000u32.to_le_bytes());
        huge[8..12].copy_from_slice(&10000u32.to_le_bytes());
        assert!(dib_to_png(&huge).is_err());
        let mut invalid = dib.to_vec();
        invalid[16..20].copy_from_slice(&5u32.to_le_bytes());
        assert!(dib_to_png(&invalid).is_err());
    }
    #[test]
    fn transparent_pixels_composite_on_note_paper_and_resize_before_transport() {
        let mut bytes = Cursor::new(Vec::new());
        image::RgbaImage::from_pixel(80, 60, image::Rgba([240, 100, 20, 0]))
            .write_to(&mut bytes, image::ImageOutputFormat::Png)
            .unwrap();
        let rtf =
            String::from_utf8(thumbnail_rtf_paper(bytes.get_ref(), 40, 0x102030).unwrap()).unwrap();
        assert!(rtf.contains("\\picw40\\pich30\\picwgoal600\\pichgoal450"));
        let hex = rtf.rsplit_once(' ').unwrap().1.trim_end_matches('}');
        let dib: Vec<u8> = hex
            .as_bytes()
            .chunks_exact(2)
            .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
            .collect();
        let image = image::load_from_memory(&dib_to_png(&dib).unwrap())
            .unwrap()
            .to_rgb8();
        assert_eq!(image.dimensions(), (40, 30));
        assert_eq!(image.get_pixel(0, 0).0, [16, 32, 48]);
    }

    #[test]
    fn bounds_and_malformed_inputs_are_rejected() {
        assert!(thumbnail_rtf(b"not png").is_err());
        assert!(thumbnail_rtf(&vec![0; 1024 * 1024 + 1]).is_err());
        let mut bytes = Cursor::new(Vec::new());
        image::RgbaImage::new(321, 1)
            .write_to(&mut bytes, image::ImageOutputFormat::Png)
            .unwrap();
        assert!(thumbnail_rtf(bytes.get_ref()).is_err());
    }
    #[test]
    fn transport_is_bounded_and_preserves_display_aspect_ratio() {
        let mut bytes = Cursor::new(Vec::new());
        image::RgbaImage::new(160, 120)
            .write_to(&mut bytes, image::ImageOutputFormat::Png)
            .unwrap();
        let rtf = String::from_utf8(thumbnail_rtf(bytes.get_ref()).unwrap()).unwrap();
        assert!(rtf.contains("\\picwgoal2400\\pichgoal1800"));
        assert!(rtf.len() < 2 * 160 * 120 * 3 + 256);
    }
}
