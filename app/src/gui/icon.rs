//! App icon: load `icon.png` next to the exe and build a per-pixel-alpha
//! HICON for the tray / window. Falls back to `None` so callers can draw
//! their own fallback when the PNG is missing or unreadable.

use std::path::Path;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::UI::WindowsAndMessaging::*;

/// Load `icon.png` from `dir` and return a 32x32 alpha HICON.
/// `None` if the file is missing or cannot be decoded.
pub fn load_hicon(dir: &Path) -> Option<HICON> {
    let path = dir.join("icon.png");
    let rgba = image::open(&path).ok()?.to_rgba8();
    let icon = image::imageops::resize(&rgba, 32, 32, image::imageops::FilterType::Triangle);
    Some(hicon_from_rgba(&icon))
}

/// Build an alpha HICON from RGBA8 pixels (32bpp + all-zero 1bpp mask:
/// Windows Vista+ honors the alpha channel in CreateIconIndirect).
fn hicon_from_rgba(img: &image::RgbaImage) -> HICON {
    let (w, h) = (img.width() as i32, img.height() as i32);
    // CreateBitmap expects device-dependent 32bpp bits: BGRA on little-endian
    let mut bgra = Vec::with_capacity((w * h * 4) as usize);
    for p in img.pixels() {
        let [r, g, b, a] = p.0;
        bgra.extend_from_slice(&[b, g, r, a]);
    }
    unsafe {
        let hbm_color = CreateBitmap(w, h, 1, 32, Some(bgra.as_ptr() as *const _));
        // monochrome mask, all zeros -> the alpha channel decides
        let mask = vec![0u8; (((w + 7) / 8) * h) as usize];
        let hbm_mask = CreateBitmap(w, h, 1, 1, Some(mask.as_ptr() as *const _));
        let info = ICONINFO {
            fIcon: true.into(),
            xHotspot: 0,
            yHotspot: 0,
            hbmMask: hbm_mask,
            hbmColor: hbm_color,
        };
        let icon = CreateIconIndirect(&info).unwrap_or(HICON::default());
        let _ = DeleteObject(HGDIOBJ(hbm_color.0));
        let _ = DeleteObject(HGDIOBJ(hbm_mask.0));
        icon
    }
}