//! Icons from executables, DLLs and `.ico` files, as RGBA pixels for egui textures.
//!
//! Accepts the `DisplayIcon`-style "path,index" form used by uninstall entries and
//! shortcuts (a negative index is a resource id).

use std::path::Path;

use windows::Win32::Graphics::Gdi::{
    BI_RGB, BITMAP, BITMAPINFO, BITMAPINFOHEADER, CreateCompatibleDC, DIB_RGB_COLORS, DeleteDC, DeleteObject,
    GetDIBits, GetObjectW, HGDIOBJ,
};
use windows::Win32::UI::Shell::SHDefExtractIconW;
use windows::Win32::UI::WindowsAndMessaging::{DestroyIcon, GetIconInfo, HICON, ICONINFO};
use windows::core::HSTRING;

/// RGBA pixels, row-major, `size × size`.
#[derive(Debug, Clone)]
pub struct IconImage {
    pub size: usize,
    pub rgba: Vec<u8>,
}

/// Splits `"C:\x\app.exe",2` / `C:\x\app.exe,-101` / `"C:\x\app.exe,1"` into a path
/// and an icon index.
pub fn split_icon_location(spec: &str) -> (String, i32) {
    let mut spec = spec.trim();
    // Some programs quote the whole "path,index".
    if spec.len() > 1 && spec.starts_with('"') && spec.ends_with('"') && spec.matches('"').count() == 2 {
        spec = &spec[1..spec.len() - 1];
    }
    let (path, index) = match spec.rsplit_once(',') {
        Some((p, i)) if i.trim().parse::<i32>().is_ok() => (p, i.trim().parse().unwrap_or(0)),
        _ => (spec, 0),
    };
    (crate::paths::expand_env(path.trim().trim_matches('"')), index)
}

/// Extracts the icon at `index` from `path` at `size` pixels (e.g. 32 or 48).
pub fn extract(path: &Path, index: i32, size: u32) -> Option<IconImage> {
    let mut large = HICON::default();
    let hr = unsafe {
        SHDefExtractIconW(
            &HSTRING::from(path.as_os_str()),
            index,
            0,
            Some(&mut large),
            None,
            size & 0xFFFF,
        )
    };
    if hr.is_err() || large.is_invalid() {
        return None;
    }
    let image = unsafe { icon_to_rgba(large) };
    unsafe {
        let _ = DestroyIcon(large);
    }
    image
}

/// Convenience for "path,index" specs.
pub fn extract_spec(spec: &str, size: u32) -> Option<IconImage> {
    let (path, index) = split_icon_location(spec);
    if path.is_empty() {
        return None;
    }
    extract(Path::new(&path), index, size)
}

unsafe fn bitmap_pixels(
    hdc: windows::Win32::Graphics::Gdi::HDC,
    bmp: HGDIOBJ,
    w: i32,
    h: i32,
) -> Option<Vec<u8>> {
    let mut info = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: w,
            biHeight: -h, // top-down
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        ..Default::default()
    };
    let mut buf = vec![0u8; (w * h * 4) as usize];
    let lines = unsafe {
        GetDIBits(
            hdc,
            windows::Win32::Graphics::Gdi::HBITMAP(bmp.0),
            0,
            h as u32,
            Some(buf.as_mut_ptr().cast()),
            &mut info,
            DIB_RGB_COLORS,
        )
    };
    (lines == h).then_some(buf)
}

unsafe fn icon_to_rgba(icon: HICON) -> Option<IconImage> {
    let mut ii = ICONINFO::default();
    unsafe { GetIconInfo(icon, &mut ii) }.ok()?;
    let color = HGDIOBJ(ii.hbmColor.0);
    let mask = HGDIOBJ(ii.hbmMask.0);
    let result = (|| {
        if color.is_invalid() {
            return None; // monochrome icons: rare for programs, skipped
        }
        let mut bm = BITMAP::default();
        let got = unsafe { GetObjectW(color, size_of::<BITMAP>() as i32, Some((&raw mut bm).cast())) };
        if got == 0 || bm.bmWidth <= 0 || bm.bmWidth != bm.bmHeight.abs() {
            return None;
        }
        let (w, h) = (bm.bmWidth, bm.bmHeight.abs());
        let hdc = unsafe { CreateCompatibleDC(None) };
        let bgra = unsafe { bitmap_pixels(hdc, color, w, h) };
        // Older icons have no alpha channel; their transparency is in the AND mask.
        let mask_px = unsafe { bitmap_pixels(hdc, mask, w, h) };
        unsafe {
            let _ = DeleteDC(hdc);
        }
        let mut px = bgra?;
        let has_alpha = px.as_chunks::<4>().0.iter().any(|p| p[3] != 0);
        for (i, p) in px.as_chunks_mut::<4>().0.iter_mut().enumerate() {
            p.swap(0, 2); // BGRA → RGBA
            if !has_alpha {
                let opaque = mask_px.as_ref().is_none_or(|m| m[i * 4] == 0);
                p[3] = if opaque { 255 } else { 0 };
            }
        }
        Some(IconImage {
            size: w as usize,
            rgba: px,
        })
    })();
    unsafe {
        if !color.is_invalid() {
            let _ = DeleteObject(color);
        }
        if !mask.is_invalid() {
            let _ = DeleteObject(mask);
        }
    }
    result
}

/// Windows' own thumbnail of a file (from its thumbnail cache when available), fitted
/// into `size × size`. Fast even for very large pictures. Returns (width, height, RGBA).
pub fn thumbnail(path: &Path, size: u32) -> Option<(usize, usize, Vec<u8>)> {
    use windows::Win32::Foundation::SIZE;
    use windows::Win32::UI::Shell::{
        IShellItemImageFactory, SHCreateItemFromParsingName, SIIGBF_RESIZETOFIT,
    };
    crate::shell::ensure_com();
    let factory: IShellItemImageFactory =
        unsafe { SHCreateItemFromParsingName(&HSTRING::from(path.as_os_str()), None) }.ok()?;
    let side = size as i32;
    let bmp = unsafe { factory.GetImage(SIZE { cx: side, cy: side }, SIIGBF_RESIZETOFIT) }.ok()?;
    let obj = HGDIOBJ(bmp.0);
    let mut bm = BITMAP::default();
    let got = unsafe { GetObjectW(obj, size_of::<BITMAP>() as i32, Some((&raw mut bm).cast())) };
    let result = (got != 0 && bm.bmWidth > 0).then(|| {
        let (w, h) = (bm.bmWidth, bm.bmHeight.abs());
        let hdc = unsafe { CreateCompatibleDC(None) };
        let px = unsafe { bitmap_pixels(hdc, obj, w, h) };
        unsafe {
            let _ = DeleteDC(hdc);
        }
        px.map(|mut px| {
            for p in px.as_chunks_mut::<4>().0.iter_mut() {
                p.swap(0, 2); // BGRA → RGBA
                p[3] = 255; // photos are opaque; the alpha byte is not always set
            }
            (w as usize, h as usize, px)
        })
    });
    unsafe {
        let _ = DeleteObject(obj);
    }
    result.flatten()
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn icon_locations() {
        assert_eq!(
            split_icon_location(r#""C:\a b\app.exe",2"#),
            (r"C:\a b\app.exe".into(), 2)
        );
        assert_eq!(
            split_icon_location(r"C:\x\app.exe,-101"),
            (r"C:\x\app.exe".into(), -101)
        );
        assert_eq!(split_icon_location(r"C:\x\a,b.ico"), (r"C:\x\a,b.ico".into(), 0));
        assert_eq!(
            split_icon_location(r#""C:\x\app.exe,1""#),
            (r"C:\x\app.exe".into(), 1)
        );
    }

    #[test]
    fn extracts_a_system_icon() {
        let explorer = crate::paths::expand_env(r"%WINDIR%\explorer.exe");
        let img = extract(Path::new(&explorer), 0, 32).expect("explorer icon");
        assert_eq!(img.size, 32);
        assert_eq!(img.rgba.len(), 32 * 32 * 4);
        assert!(img.rgba.as_chunks::<4>().0.iter().any(|p| p[3] == 255));
        assert!(extract(Path::new(r"C:\does\not\exist.exe"), 0, 32).is_none());
    }
}
