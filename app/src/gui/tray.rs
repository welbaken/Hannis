//! Tray icon (Shell_NotifyIcon): prefers the `icon.png`-derived HICON,
//! falls back to a runtime-drawn circle when no PNG is available.
//! Right-click menu: 退出.

use super::window::{instance, WM_APP_TRAY};
use windows::core::w;
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::HGDIOBJ;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::UI::Shell::*;
use windows::Win32::UI::WindowsAndMessaging::*;

pub const MENU_QUIT: usize = 1001;

pub struct Tray {
    hwnd: HWND,
    added: bool,
    icon: Option<HICON>,
}

fn fallback_icon() -> HICON {
    // 16x16 BGRA circle in pet-green with darker ring
    let mut px = [0u8; 16 * 16 * 4];
    for y in 0..16i32 {
        for x in 0..16i32 {
            let dx = x as f32 - 7.5;
            let dy = y as f32 - 7.5;
            let d = (dx * dx + dy * dy).sqrt();
            let i = ((y * 16 + x) * 4) as usize;
            if d <= 7.0 {
                if d > 6.0 {
                    px[i] = 90;
                    px[i + 1] = 60;
                    px[i + 2] = 40;
                } else {
                    px[i] = 200;
                    px[i + 1] = 230;
                    px[i + 2] = 120;
                }
                px[i + 3] = 255;
            }
        }
    }
    unsafe {
        let hbm_color = CreateBitmap(16, 16, 1, 32, Some(px.as_ptr() as *const _));
        let mask = [0u8; 16 * 2]; // monochrome, all 0 -> fully opaque
        let hbm_mask = CreateBitmap(16, 16, 1, 1, Some(mask.as_ptr() as *const _));
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

impl Tray {
    pub fn new(hwnd: HWND, icon: Option<HICON>) -> Tray {
        Tray { hwnd, added: false, icon }
    }

    pub fn add(&mut self) {
        let icon = self.icon.unwrap_or_else(fallback_icon);
        let tip: Vec<u16> = "Hannis".encode_utf16().chain(Some(0)).collect();
        let mut nid = NOTIFYICONDATAW {
            cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
            hWnd: self.hwnd,
            uID: 1,
            uFlags: NIF_MESSAGE | NIF_ICON | NIF_TIP,
            uCallbackMessage: WM_APP_TRAY,
            hIcon: icon,
            szTip: [0u16; 128],
            ..Default::default()
        };
        for (i, c) in tip.iter().take(127).enumerate() {
            nid.szTip[i] = *c;
        }
        unsafe {
            let _ = Shell_NotifyIconW(NIM_ADD, &nid);
        }
        self.added = true;
    }

    pub fn remove(&mut self) {
        if !self.added {
            return;
        }
        let nid = NOTIFYICONDATAW {
            cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
            hWnd: self.hwnd,
            uID: 1,
            ..Default::default()
        };
        unsafe {
            let _ = Shell_NotifyIconW(NIM_DELETE, &nid);
        }
        self.added = false;
    }

        /// Show the right-click menu at the cursor; returns the chosen id.
    /// The text-display switcher was intentionally omitted (屏蔽): the app
    /// renders the phone bubble by default.
    pub fn show_menu(&self) -> Option<usize> {
        unsafe {
            let menu = CreatePopupMenu().unwrap_or(HMENU::default());
            if menu.is_invalid() {
                return None;
            }
            let _ = AppendMenuW(menu, MF_STRING, MENU_QUIT, w!("退出"));
            let mut pt = POINT::default();
            let _ = GetCursorPos(&mut pt);
            let _ = SetForegroundWindow(self.hwnd);
            let cmd = TrackPopupMenu(
                menu,
                TPM_RIGHTBUTTON | TPM_RETURNCMD | TPM_NONOTIFY,
                pt.x,
                pt.y,
                0,
                self.hwnd,
                None,
            );
            let _ = DestroyMenu(menu);
            if !cmd.as_bool() {
                None
            } else {
                Some(cmd.0 as usize)
            }
        }
    }
}

impl Drop for Tray {
    fn drop(&mut self) {
        self.remove();
    }
}

#[allow(dead_code)]
fn _unused() {
    let _ = instance();
}
