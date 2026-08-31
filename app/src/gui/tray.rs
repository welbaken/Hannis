//! Tray icon (Shell_NotifyIcon): prefers the `icon.png`-derived HICON,
//! falls back to a runtime-drawn circle when no PNG is available.
//! Right-click menu: 回避模式 / 自动收起 / 多源堆叠显示 / 鼠标穿透 / 开机自启 (checkable) + 接入口 / 退出.

use super::window::{instance, WM_APP_TRAY};
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::HGDIOBJ;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::UI::Shell::*;
use windows::Win32::UI::WindowsAndMessaging::*;

pub const MENU_QUIT: usize = 1001;
/// Toggle 回避模式 (avoid mode): pet runs from the cursor and returns home.
pub const MENU_AVOID_TOGGLE: usize = 1002;
/// Toggle 自动收起 (auto-hide): idle/offline 太久就把宠物收到任务栏后。
pub const MENU_AUTOHIDE_TOGGLE: usize = 1003;
/// 打开"接入口设置"窗口(启停 / 参数如 log 位置、IP 及端口)。
pub const MENU_ENDPOINTS: usize = 1004;
/// Toggle 开机自启:同步 config.autostart 与 HKCU Run 注册表项。
pub const MENU_AUTOSTART_TOGGLE: usize = 1005;
/// Toggle 多源堆叠显示(bubble.stack):Working/Thinking 时每个有活动
/// 会话的接入口一张级联卡,前排卡显示流式内容。
pub const MENU_STACK_TOGGLE: usize = 1006;
/// Toggle 鼠标穿透(WS_EX_TRANSPARENT):所有鼠标操作穿透到下层界面,
/// 悬浮在宠物上时宠物近乎透明(可透视下层)。
pub const MENU_CLICKTHROUGH_TOGGLE: usize = 1007;
/// Toggle 提示音(SoundConfig.enabled):done/failed/attention 状态音。
pub const MENU_SOUND_TOGGLE: usize = 1008;
/// 接入口子菜单里第 i 个脚本的启停项 id = MENU_SCRIPT_BASE + i。
pub const MENU_SCRIPT_BASE: usize = 1100;

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
    /// `avoid_enabled` / `auto_hide_enabled` / `autostart_enabled` /
    /// `stack_enabled` / `click_through_enabled` / `sound_enabled` control the
    /// checkmarks so the current states are visible before the user picks anything.
    /// `scripts`: (启用?, 显示名) 列表,渲染"接入口"子菜单的启停项。
    pub fn show_menu(
        &self,
        avoid_enabled: bool,
        auto_hide_enabled: bool,
        autostart_enabled: bool,
        stack_enabled: bool,
        click_through_enabled: bool,
        sound_enabled: bool,
        scripts: &[(bool, String)],
    ) -> Option<usize> {
        unsafe {
            let menu = CreatePopupMenu().unwrap_or(HMENU::default());
            if menu.is_invalid() {
                return None;
            }
            let avoid_flags: MENU_ITEM_FLAGS =
                if avoid_enabled { MF_STRING | MF_CHECKED } else { MF_STRING };
            let _ = AppendMenuW(menu, avoid_flags, MENU_AVOID_TOGGLE, w!("回避模式"));
            let hide_flags: MENU_ITEM_FLAGS =
                if auto_hide_enabled { MF_STRING | MF_CHECKED } else { MF_STRING };
            let _ = AppendMenuW(menu, hide_flags, MENU_AUTOHIDE_TOGGLE, w!("自动收起"));
            let stack_flags: MENU_ITEM_FLAGS =
                if stack_enabled { MF_STRING | MF_CHECKED } else { MF_STRING };
            let _ = AppendMenuW(menu, stack_flags, MENU_STACK_TOGGLE, w!("多源堆叠显示"));
            let ct_flags: MENU_ITEM_FLAGS =
                if click_through_enabled { MF_STRING | MF_CHECKED } else { MF_STRING };
            let _ = AppendMenuW(menu, ct_flags, MENU_CLICKTHROUGH_TOGGLE, w!("鼠标穿透"));
            let snd_flags: MENU_ITEM_FLAGS =
                if sound_enabled { MF_STRING | MF_CHECKED } else { MF_STRING };
            let _ = AppendMenuW(menu, snd_flags, MENU_SOUND_TOGGLE, w!("提示音"));
            let auto_flags: MENU_ITEM_FLAGS =
                if autostart_enabled { MF_STRING | MF_CHECKED } else { MF_STRING };
            let _ = AppendMenuW(menu, auto_flags, MENU_AUTOSTART_TOGGLE, w!("开机自启"));
            // 接入口子菜单:每脚本一个勾选项(点击=启停切换)
            if !scripts.is_empty() {
                let sub = CreatePopupMenu().unwrap_or(HMENU::default());
                for (i, (on, label)) in scripts.iter().enumerate() {
                    let flags: MENU_ITEM_FLAGS =
                        if *on { MF_STRING | MF_CHECKED } else { MF_STRING };
                    let wide: Vec<u16> = label.encode_utf16().chain(Some(0)).collect();
                    let _ = AppendMenuW(sub, flags, (MENU_SCRIPT_BASE + i) as usize, PCWSTR(wide.as_ptr()));
                }
                let _ = AppendMenuW(menu, MF_POPUP, sub.0 as usize, w!("接入口"));
            }
            let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
            let _ = AppendMenuW(menu, MF_STRING, MENU_ENDPOINTS, w!("接入口设置…"));
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
