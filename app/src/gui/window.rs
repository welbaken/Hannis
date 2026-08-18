//! Layered always-on-top window (WS_EX_LAYERED|TOPMOST|TOOLWINDOW) and its
//! message handling: drag (move state), Ctrl+wheel zoom, tray callback.

use super::App;
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::HBRUSH;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Input::KeyboardAndMouse::{GetKeyState, VK_CONTROL};
use windows::Win32::UI::WindowsAndMessaging::*;

pub const WM_APP_TRAY: u32 = WM_APP + 10;

thread_local! {
    static APP_PTR: std::cell::RefCell<Option<*mut App>> = const { std::cell::RefCell::new(None) };
}

pub fn with_app<T>(f: impl FnOnce(&mut App) -> T) -> Option<T> {
    APP_PTR.with(|p| {
        let ptr = *p.borrow();
        match ptr {
            Some(ptr) => Some(unsafe { f(&mut *ptr) }),
            None => None,
        }
    })
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_CREATE => {
            let _ = (wparam, lparam);
            LRESULT(0)
        }
        WM_ERASEBKGND => LRESULT(1),
        WM_TIMER => {
            with_app(|a| a.timer_tick());
            LRESULT(0)
        }
        WM_LBUTTONDOWN => {
            with_app(|a| {
                a.on_lbutton_down();
            });
            LRESULT(0)
        }
        WM_MOUSEMOVE => {
            with_app(|a| {
                a.on_mouse_move();
            });
            LRESULT(0)
        }
        WM_LBUTTONUP => {
            with_app(|a| {
                a.on_lbutton_up();
            });
            LRESULT(0)
        }
        WM_MOUSEWHEEL => {
            // always consume the wheel over the pet so it never reaches the
            // app below; Ctrl+wheel adjusts the pet size
            let delta = ((wparam.0 >> 16) as u16 as i16) as i32;
            let ctrl = unsafe { (GetKeyState(VK_CONTROL.0 as i32) as u16) & 0x8000 != 0 };
            if ctrl {
                with_app(|a| {
                    a.on_zoom(delta);
                });
            }
            LRESULT(0)
        }
        WM_MOUSEHWHEEL => LRESULT(0),
        WM_RBUTTONUP => {
            with_app(|a| {
                a.show_tray_menu();
            });
            LRESULT(0)
        }
        WM_APP_TRAY => {
            with_app(|a| {
                a.on_tray(wparam, lparam);
            });
            LRESULT(0)
        }
        WM_DESTROY => {
            unsafe { PostQuitMessage(0) };
            LRESULT(0)
        }
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

pub fn create_main_window(app: *mut App, w: i32, h: i32, icon: HICON) -> HWND {
    APP_PTR.with(|p| *p.borrow_mut() = Some(app));
    unsafe {
        let hinst = GetModuleHandleW(None)
            .map(|h| HINSTANCE(h.0))
            .unwrap_or(HINSTANCE::default());
        let class = w!("hannis");
        let wc = WNDCLASSW {
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(wndproc),
            cbClsExtra: 0,
            cbWndExtra: 0,
            hInstance: hinst,
            hIcon: icon,
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or(HCURSOR::default()),
            hbrBackground: HBRUSH::default(),
            lpszMenuName: PCWSTR::null(),
            lpszClassName: class,
        };
        if RegisterClassW(&wc) == 0 {
            // class may already exist
        }
        let hwnd = CreateWindowExW(
            WS_EX_LAYERED | WS_EX_TOPMOST | WS_EX_TOOLWINDOW,
            class,
            w!("Hannis"),
            WS_POPUP,
            100,
            100,
            w.max(1),
            h.max(1),
            HWND::default(),
            HMENU::default(),
            hinst,
            None,
        )
        .unwrap_or(HWND::default());
        hwnd
    }
}

pub fn set_window_rect(hwnd: HWND, x: i32, y: i32, w: i32, h: i32) {
    unsafe {
        let _ = SetWindowPos(hwnd, HWND_TOPMOST, x, y, w, h, SWP_NOACTIVATE | SWP_SHOWWINDOW);
    }
}

pub fn show(hwnd: HWND) {
    unsafe {
        let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
    }
}

pub fn window_rect(hwnd: HWND) -> (i32, i32, i32, i32) {
    unsafe {
        let mut r = RECT::default();
        let _ = GetWindowRect(hwnd, &mut r);
        (r.left, r.top, r.right - r.left, r.bottom - r.top)
    }
}

/// GetModuleHandleW wrapper for resource/instance use.
pub fn instance() -> HINSTANCE {
    unsafe { GetModuleHandleW(None).map(|h| HINSTANCE(h.0)).unwrap_or(HINSTANCE::default()) }
}

pub fn cursor_pos() -> (i32, i32) {
    unsafe {
        let mut p = POINT::default();
        let _ = GetCursorPos(&mut p);
        (p.x, p.y)
    }
}


