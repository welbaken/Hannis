//! 接入口设置窗口(托盘 → "接入口设置…"):每个 Lua 接入口一行
//! 药丸形启停开关(与气泡里的"From …"胶囊同款视觉)+ 参数编辑
//! (如 MAA 的 log 路径、ComfyUI 的 IP 及端口)。
//! 保存 = 写回 config.json + 停旧线程重启启用的接入口。
//!
//! 实现说明:纯 Win32 手搭的模态窗口(父窗口禁用 + 自跑消息循环)。
//! 所有尺寸按父窗口 DPI 缩放;启停开关用 BS_OWNERDRAW 自绘圆角胶囊。

use super::{log_line, script_stops, spawn_script, App};
use dshpet::config::ScriptEntryConfig;
use std::sync::Mutex;
use windows::core::{PCWSTR, w};
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::UI::Controls::DRAWITEMSTRUCT;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::Input::KeyboardAndMouse::EnableWindow;
use windows::Win32::UI::WindowsAndMessaging::*;

/// 每个接入口在窗口里的布局快照(模态期间 cfg 不变)。脚本名/启停状态
/// 走 PILLS;这里只存参数行的 (键, 显示标签)。
struct Row {
    /// (参数键, 显示标签)
    keys: Vec<(String, String)>,
}

static LAYOUT: Mutex<Option<Vec<Row>>> = Mutex::new(None);
static RESULT: Mutex<Option<Vec<(bool, Vec<(String, serde_json::Value)>)>>> = Mutex::new(None);
/// 药丸开关的即时状态(点击即翻转,保存时才生效):Vec<(名称, 启用?)>
static PILLS: Mutex<Vec<(String, bool)>> = Mutex::new(Vec::new());
static FONT_REG: Mutex<isize> = Mutex::new(0); // HFONT as isize
static FONT_BOLD: Mutex<isize> = Mutex::new(0);

const ID_SAVE: usize = 1;
const ID_CANCEL: usize = 2;
const ID_PILL_BASE: usize = 2100;
const ID_HINT_BASE: usize = 2200;
const ID_EDIT_BASE: usize = 3000;
/// 每脚本最多渲染的参数行数(EDIT id = ID_EDIT_BASE + i*32 + j)
const MAX_ARGS_PER_ROW: usize = 32;

/// 品牌薄荷绿(与托盘图标同系),启用态药丸填充。
const PILL_ON_RGB: COLORREF = COLORREF(0x008EB53E); // BGR: RGB(62,181,142)
const PILL_OFF_RGB: COLORREF = COLORREF(0x00E9E6E6); // RGB(230,230,230)
const PILL_ON_TEXT: COLORREF = COLORREF(0x00FFFFFF);
const PILL_OFF_TEXT: COLORREF = COLORREF(0x008F8A8A);

fn set_layout(rows: Vec<Row>) {
    *LAYOUT.lock().unwrap() = Some(rows);
}
fn take_result() -> Option<Vec<(bool, Vec<(String, serde_json::Value)>)>> {
    RESULT.lock().unwrap().take()
}

/// 解析脚本内的参数声明(通用约定):
///   --[hannis:set] 键 | 标签 | 默认值
/// 多行可声明多个参数;标签/默认值可省略。任何未来新增的 Lua 接入口
/// 只要在脚本里写这几行注释,设置界面就会自动出现对应编辑框。
fn declared_settings(sc: &ScriptEntryConfig, exe_dir: &std::path::Path) -> Vec<(String, String, String)> {
    let path = dshpet::config::Config::resolve_script_path(exe_dir, &sc.file);
    let Ok(text) = std::fs::read_to_string(path) else { return vec![] };
    let mut out = vec![];
    for line in text.lines() {
        let t = line.trim_start();
        let Some(rest) = t.strip_prefix("--[hannis:set]") else { continue };
        let parts: Vec<&str> = rest.split('|').map(|x| x.trim()).collect();
        let Some(key) = parts.first().map(|x| x.to_string()) else { continue };
        if key.is_empty() {
            continue;
        }
        let label = parts
            .get(1)
            .map(|x| x.to_string())
            .filter(|x| !x.is_empty())
            .unwrap_or_else(|| key.clone());
        let default = parts.get(2).copied().unwrap_or("").to_string();
        out.push((key, label, default));
    }
    out
}

/// 组装某脚本的参数编辑行:(key, 预填值)。
/// 优先级:已保存的 config.args > 脚本声明的默认值 > 文件名猜测兜底。
fn row_args(sc: &ScriptEntryConfig, exe_dir: &std::path::Path) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    if let Some(serde_json::Value::Object(m)) = &sc.args {
        for (k, v) in m {
            out.push((k.clone(), value_to_edit(v)));
        }
    }
    for (k, _label, default) in declared_settings(sc, exe_dir) {
        if !out.iter().any(|(ek, _)| ek == &k) {
            out.push((k, default));
        }
    }
    if out.is_empty() {
        // 无声明也无保存值时的最后兜底(按文件名猜)
        let f = sc.file.to_ascii_lowercase();
        if f.contains("maa") {
            out.push(("log".into(), String::new()));
        } else if f.contains("comfy") {
            out.push(("url".into(), String::new()));
        }
    }
    out.truncate(MAX_ARGS_PER_ROW);
    out
}

fn value_to_edit(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Number(n) => n.to_string(),
        serde_json::Value::Bool(b) => b.to_string(),
        _ => String::new(),
    }
}

/// EDIT 字符串 → JSON 值:数字/布尔自动识别;空串丢弃(脚本走默认值)。
fn edit_to_value(s: &str) -> Option<serde_json::Value> {
    let t = s.trim();
    if t.is_empty() {
        return None;
    }
    if t == "true" {
        return Some(serde_json::Value::Bool(true));
    }
    if t == "false" {
        return Some(serde_json::Value::Bool(false));
    }
    if let Ok(i) = t.parse::<i64>() {
        return Some(serde_json::Value::Number(i.into()));
    }
    if let Ok(f) = t.parse::<f64>() {
        if let Some(n) = serde_json::Number::from_f64(f) {
            return Some(serde_json::Value::Number(n));
        }
    }
    Some(serde_json::Value::String(t.to_string()))
}

unsafe fn make_font(s: f32, weight: i32) -> isize {
    let mut lf = LOGFONTW::default();
    lf.lfHeight = -((14.0 * s).round() as i32);
    lf.lfWeight = weight;
    let name: Vec<u16> = "Segoe UI".encode_utf16().collect();
    for (i, c) in name.iter().take(31).enumerate() {
        lf.lfFaceName[i] = *c;
    }
    CreateFontIndirectW(&lf).0 as isize
}

pub fn open_settings(app: &mut App) {
    // ---- DPI:先按父窗口估一个初值建窗,创建后用窗口自身实际 DPI 复算 ----
    let s0 = {
        let d = unsafe { GetDpiForWindow(app.hwnd) } as f32 / 96.0;
        if d < 1.0 { 1.0 } else { d }
    };

    // ---- 布局快照 ----
    let exe_dir = app.exe_dir();
    let mut rows: Vec<Row> = Vec::new();
    let mut args_all: Vec<Vec<(String, String)>> = Vec::new();
    let mut pills: Vec<(String, bool)> = Vec::new();
    for (i, sc) in app.cfg.scripts.iter().enumerate() {
        let label =
            if sc.name.is_empty() { format!("脚本 {}", i + 1) } else { sc.name.clone() };
        let declared = declared_settings(sc, &exe_dir);
        let args = row_args(sc, &exe_dir);
        // 行标签优先用脚本声明的中文标签,否则退回参数键名
        let keys: Vec<(String, String)> = args
            .iter()
            .map(|(k, _)| {
                let lbl = declared
                    .iter()
                    .find(|(dk, _, _)| dk == k)
                    .map(|(_, l, _)| l.clone())
                    .unwrap_or_else(|| k.clone());
                (k.clone(), lbl)
            })
            .collect();
        args_all.push(args);
        pills.push((label.clone(), sc.enabled));
        rows.push(Row { keys });
    }
    if rows.is_empty() {
        log_line("[settings] no scripts configured");
        return;
    }
    set_layout(rows);
    *PILLS.lock().unwrap() = pills;
    *RESULT.lock().unwrap() = None;
    let (n_lines, n_groups) = {
        let g = LAYOUT.lock().unwrap();
        (
            g.as_ref().map(|rs| rs.iter().map(|r| (1 + r.keys.len()) as i32).sum()).unwrap_or(0),
            g.as_ref().map(|r| r.len()).unwrap_or(0) as i32,
        )
    };

    unsafe {
        let hinst = GetModuleHandleW(None).map(|h| HINSTANCE(h.0)).unwrap_or(HINSTANCE::default());
        let class = w!("HannisSettings");
        let wc = WNDCLASSW {
            lpfnWndProc: Some(settings_wndproc),
            hInstance: hinst,
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or(HCURSOR::default()),
            hbrBackground: GetSysColorBrush(COLOR_WINDOW),
            lpszClassName: class,
            ..Default::default()
        };
        RegisterClassW(&wc); // 已注册则忽略

        let m0 = calc_m(s0, n_lines, n_groups);
        let parent = app.hwnd;
        let _ = EnableWindow(parent, false);

        let hwnd = CreateWindowExW(
            WS_EX_DLGMODALFRAME,
            class,
            w!("接入口设置"),
            WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU,
            200,
            200,
            m0.client_w + 40,
            m0.client_h + 48,
            parent,
            HMENU::default(),
            hinst,
            None,
        )
        .unwrap_or(HWND::default());
        if hwnd.is_invalid() {
            let _ = EnableWindow(parent, true);
            return;
        }

        // 以本窗口实际 DPI 为准重算全部像素尺寸(PER_MONITOR_V2 下
        // 可能与父窗口不在同一显示器/缩放);字体同步重建。
        let dpi_self = GetDpiForWindow(hwnd);
        let s = if dpi_self > 0 { dpi_self as f32 / 96.0 } else { s0 };
        let s = if s < 1.0 { 1.0 } else { s };
        let m = calc_m(s, n_lines, n_groups);
        let _ = DeleteObject(HFONT(*FONT_REG.lock().unwrap() as *mut _));
        let _ = DeleteObject(HFONT(*FONT_BOLD.lock().unwrap() as *mut _));
        *FONT_REG.lock().unwrap() = make_font(s, 400);
        *FONT_BOLD.lock().unwrap() = make_font(s, 700);
        let font = HFONT(*FONT_REG.lock().unwrap() as *mut _);

        // 精确客户区(AdjustWindowRectEx 补边框标题栏)+ 居中到父窗口
        let style = WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU;
        let mut rc = RECT { left: 0, top: 0, right: m.client_w, bottom: m.client_h };
        let _ = AdjustWindowRectEx(&mut rc, style, false, WS_EX_DLGMODALFRAME);
        let (w_out, h_out) = (rc.right - rc.left, rc.bottom - rc.top);
        let mut pr = RECT::default();
        let _ = GetWindowRect(parent, &mut pr);
        let x = ((pr.left + pr.right) / 2 - w_out / 2).max(0);
        let y = ((pr.top + pr.bottom) / 2 - h_out / 2).max(0);
        let _ = SetWindowPos(hwnd, HWND::default(), x, y, w_out, h_out, SWP_NOZORDER | SWP_NOACTIVATE);

        // ---- 子控件 ----
        let mut y = m.margin;
        // 参数行的显示标签(声明了中文标签就用它)
        let rows_snapshot: Vec<Vec<(String, String)>> = LAYOUT
            .lock()
            .unwrap()
            .as_ref()
            .map(|rs| rs.iter().map(|r| r.keys.clone()).collect())
            .unwrap_or_default();
        for (i, sc_args) in args_all.iter().enumerate() {
            // 药丸形启停开关(owner-draw)+ 右侧状态提示
            let _ = create_control(hwnd, hinst, "BUTTON",
                WS_CHILD | WS_VISIBLE | WINDOW_STYLE(BS_PUSHBUTTON as u32) | WINDOW_STYLE(BS_OWNERDRAW as u32) | WS_TABSTOP,
                m.margin, y, m.pill_w, m.row_h, (ID_PILL_BASE + i) as i32, &[], font);
            let hint_txt: Vec<u16> = {
                let on = PILLS.lock().unwrap()[i].1;
                hint_text(on).encode_utf16().chain(Some(0)).collect()
            };
            create_control(hwnd, hinst, "STATIC", WS_CHILD | WS_VISIBLE,
                m.margin + m.pill_w + m.gap, y + (m.row_h - m.hint_h) / 2, (170.0 * s).round() as i32, m.hint_h,
                (ID_HINT_BASE + i) as i32, &hint_txt, font);
            y += m.row_h + m.gap;
            // 参数行:key 标签 + EDIT
            for (j, (_k, v)) in sc_args.iter().enumerate() {
                let lbl = rows_snapshot
                    .get(i)
                    .and_then(|r| r.get(j))
                    .map(|(_, l)| l.clone())
                    .unwrap_or_else(|| _k.clone());
                let kt: Vec<u16> = format!("{lbl}:").encode_utf16().chain(Some(0)).collect();
                create_control(hwnd, hinst, "STATIC", WS_CHILD | WS_VISIBLE,
                    m.margin + m.indent, y + m.edit_pad(), m.label_w, m.row_h - 2 * m.edit_pad(),
                    0, &kt, font);
                let et: Vec<u16> = v.encode_utf16().chain(Some(0)).collect();
                create_control(hwnd, hinst, "EDIT", WS_CHILD | WS_VISIBLE | WS_BORDER | WINDOW_STYLE(ES_AUTOHSCROLL as u32) | WS_TABSTOP,
                    m.margin + m.indent + m.label_w + m.gap, y + m.edit_pad(),
                    m.client_w - m.margin * 2 - m.indent - m.label_w - m.gap * 2, m.row_h - 2 * m.edit_pad(),
                    (ID_EDIT_BASE + i * MAX_ARGS_PER_ROW + j) as i32, &et, font);
                y += m.row_h + m.gap;
            }
            y += m.group_gap - m.gap;
        }
        // 底部按钮
        let btn_w = (110.0 * s).round() as i32;
        let btn_h = m.btn_h;
        let save_txt: Vec<u16> = "保存并应用".encode_utf16().chain(Some(0)).collect();
        create_control(hwnd, hinst, "BUTTON",
            WS_CHILD | WS_VISIBLE | WINDOW_STYLE(BS_PUSHBUTTON as u32) | WS_TABSTOP | WINDOW_STYLE(BS_DEFPUSHBUTTON as u32),
            m.client_w - m.margin - btn_w * 2 - m.gap, y, btn_w, btn_h, ID_SAVE as i32, &save_txt, font);
        let cancel_txt: Vec<u16> = "取消".encode_utf16().chain(Some(0)).collect();
        create_control(hwnd, hinst, "BUTTON",
            WS_CHILD | WS_VISIBLE | WINDOW_STYLE(BS_PUSHBUTTON as u32) | WS_TABSTOP,
            m.client_w - m.margin - btn_w, y, btn_w, btn_h, ID_CANCEL as i32, &cancel_txt, font);

        let _ = ShowWindow(hwnd, SW_SHOW);
        let _ = SetForegroundWindow(hwnd);

        // ---- 模态消息循环(父窗口已禁用) ----
        let mut msg = MSG::default();
        while IsWindow(hwnd).as_bool() && GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        let _ = EnableWindow(parent, true);
        let _ = SetForegroundWindow(parent);
    }

    // 清理字体
    unsafe {
        let fr = *FONT_REG.lock().unwrap();
        let fb = *FONT_BOLD.lock().unwrap();
        if fr != 0 {
            let _ = DeleteObject(HFONT(fr as *mut _));
        }
        if fb != 0 {
            let _ = DeleteObject(HFONT(fb as *mut _));
        }
        *FONT_REG.lock().unwrap() = 0;
        *FONT_BOLD.lock().unwrap() = 0;
    }

    // ---- 应用结果 ----
    if let Some(res) = take_result() {
        for (i, (enabled, kvs)) in res.iter().enumerate() {
            if let Some(sc) = app.cfg.scripts.get_mut(i) {
                sc.enabled = *enabled;
                let mut m = serde_json::Map::new();
                for (k, v) in kvs {
                    m.insert(k.clone(), v.clone());
                }
                sc.args = if m.is_empty() { None } else { Some(serde_json::Value::Object(m)) };
            }
        }
        let _ = app.cfg.save(&app.exe_dir().join("config.json"));
        // 全部重建:先停旧线程(令牌置 true),再启动启用的
        {
            let stops = script_stops().lock().unwrap();
            for t in stops.values() {
                t.store(true, std::sync::atomic::Ordering::Relaxed);
            }
        }
        script_stops().lock().unwrap().clear();
        for (i, sc) in app.cfg.scripts.iter().enumerate() {
            if sc.enabled {
                spawn_script(i, sc, &app.tx, &app.exe_dir());
            } else {
                log_line(&format!("[lua] scripts[{i}] disabled via settings"));
            }
        }
        log_line("[settings] applied endpoint settings");
    }
}

/// 一个缩放系数下的全部布局像素值(由窗口实际 DPI 计算得出)。
struct M {
    row_h: i32,
    gap: i32,
    group_gap: i32,
    margin: i32,
    label_w: i32,
    client_w: i32,
    pill_w: i32,
    indent: i32,
    hint_h: i32,
    btn_h: i32,
    client_h: i32,
}

impl M {
    fn edit_pad(&self) -> i32 {
        (self.row_h as f32 * 0.17).round() as i32
    }
}

fn calc_m(s: f32, n_lines: i32, n_groups: i32) -> M {
    let r = |v: f32| (v * s).round() as i32;
    let row_h = r(36.0);
    let gap = r(10.0);
    let group_gap = r(18.0);
    let btn_h = r(34.0);
    // 与控件循环的累加严格一致:每行(row_h+gap),每脚本末尾再补
    // (group_gap-gap);内容底之后是按钮(btn_h)+ 底边距(margin)。
    let content_bottom = n_lines * (row_h + gap) + n_groups * (group_gap - gap);
    let client_h = content_bottom + btn_h + r(20.0);
    M {
        row_h,
        gap,
        group_gap,
        margin: r(20.0),
        label_w: r(130.0),
        client_w: r(600.0),
        pill_w: r(190.0),
        indent: r(14.0),
        hint_h: r(18.0),
        btn_h,
        client_h,
    }
}

fn hint_text(on: bool) -> &'static str {
    if on {
        "已启用 · 点击停用"
    } else {
        "已停用 · 点击启用"
    }
}

#[allow(clippy::too_many_arguments)]
unsafe fn create_control(
    parent: HWND,
    hinst: HINSTANCE,
    class: &str,
    style: WINDOW_STYLE,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    id: i32,
    text: &[u16],
    font: HFONT,
) -> HWND {
    let cls: Vec<u16> = class.encode_utf16().chain(Some(0)).collect();
    let empty: Vec<u16> = vec![0];
    let t: &[u16] = if text.is_empty() { &empty } else { text };
    let hwnd = CreateWindowExW(
        WINDOW_EX_STYLE::default(),
        PCWSTR(cls.as_ptr()),
        PCWSTR(t.as_ptr()),
        style,
        x,
        y,
        w.max(1),
        h.max(1),
        parent,
        HMENU::default(),
        hinst,
        None,
    )
    .unwrap_or(HWND::default());
    if !hwnd.is_invalid() {
        SendMessageW(hwnd, WM_SETFONT, WPARAM(font.0 as usize), LPARAM(1));
        if id != 0 {
            let _ = SetWindowLongPtrW(hwnd, GWLP_ID, id as isize);
        }
    }
    hwnd
}

/// 自绘药丸:圆角矩形填充(启用=品牌薄荷绿/停用=浅灰),白色粗体/灰色文字。
unsafe fn draw_pill(dis: &DRAWITEMSTRUCT) {
    let idx = dis.CtlID as usize;
    let state = {
        let p = PILLS.lock().unwrap();
        p.get(idx - ID_PILL_BASE).map(|x| x.1).unwrap_or(false)
    };
    let (fill, txtc, bold) = if state {
        (PILL_ON_RGB, PILL_ON_TEXT, true)
    } else {
        (PILL_OFF_RGB, PILL_OFF_TEXT, false)
    };
    let hdc = dis.hDC;
    // 先擦底(按钮区域透出窗口背景)
    FillRect(hdc, &dis.rcItem, GetSysColorBrush(COLOR_WINDOW));
    // 全圆角胶囊
    let r = dis.rcItem;
    let rad = (r.bottom - r.top) / 2;
    let hpen = CreatePen(PEN_STYLE(PS_NULL.0), 0, COLORREF(0));
    let hold = SelectObject(hdc, hpen);
    let hbr = CreateSolidBrush(fill);
    let holdb = SelectObject(hdc, hbr);
    let _ = RoundRect(hdc, r.left, r.top, r.right, r.bottom, rad, rad);
    let _ = SelectObject(hdc, hold);
    let _ = SelectObject(hdc, holdb);
    let _ = DeleteObject(hpen);
    let _ = DeleteObject(hbr);
    // 文字居中
    let _ = SetBkMode(hdc, TRANSPARENT);
    let _ = SetTextColor(hdc, txtc);
    let f = if bold { *FONT_BOLD.lock().unwrap() } else { *FONT_REG.lock().unwrap() };
    if f != 0 {
        SelectObject(hdc, HFONT(f as *mut _));
    }
    let name = {
        let p = PILLS.lock().unwrap();
        p.get(idx - ID_PILL_BASE).map(|x| x.0.clone()).unwrap_or_default()
    };
    let mut wide: Vec<u16> = name.encode_utf16().collect();
    let mut rc = dis.rcItem;
    let _ = DrawTextW(hdc, &mut wide, &mut rc, DT_CENTER | DT_VCENTER | DT_SINGLELINE | DT_END_ELLIPSIS);
}

unsafe extern "system" fn settings_wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_COMMAND => {
            let id = wp.0 & 0xFFFF;
            match id as usize {
                ID_SAVE => {
                    collect_result(hwnd);
                    let _ = DestroyWindow(hwnd);
                    LRESULT(0)
                }
                ID_CANCEL => {
                    let _ = DestroyWindow(hwnd);
                    LRESULT(0)
                }
                c if (ID_PILL_BASE..ID_PILL_BASE + 512).contains(&c) => {
                    // 药丸点击:翻转本地状态(保存时才真正应用)
                    let i = c - ID_PILL_BASE;
                    {
                        let mut p = PILLS.lock().unwrap();
                        if let Some(e) = p.get_mut(i) {
                            e.1 = !e.1;
                        }
                    }
                    let hint = GetDlgItem(hwnd, (ID_HINT_BASE + i) as i32).unwrap_or(HWND::default());
                    if !hint.is_invalid() {
                        let t: Vec<u16> = hint_text(PILLS.lock().unwrap()[i].1)
                            .encode_utf16()
                            .chain(Some(0))
                            .collect();
                        let _ = SetWindowTextW(hint, PCWSTR(t.as_ptr()));
                        let _ = InvalidateRect(hint, None, true);
                    }
                    LRESULT(0)
                }
                _ => LRESULT(0),
            }
        }
        WM_DRAWITEM => {
            let dis = &*(lp.0 as *const DRAWITEMSTRUCT);
            if dis.CtlID as usize >= ID_PILL_BASE {
                draw_pill(dis);
                LRESULT(1)
            } else {
                DefWindowProcW(hwnd, msg, wp, lp)
            }
        }
        WM_CLOSE => {
            let _ = DestroyWindow(hwnd);
            LRESULT(0)
        }
        WM_DESTROY => LRESULT(0), // 注意:不能 PostQuitMessage —— 那会退出整个应用
        _ => DefWindowProcW(hwnd, msg, wp, lp),
    }
}

/// 回读所有药丸状态和 EDIT,写入 RESULT。
fn collect_result(_parent: HWND) {
    let g = LAYOUT.lock().unwrap();
    let Some(rows) = g.as_ref() else { return };
    let mut res: Vec<(bool, Vec<(String, serde_json::Value)>)> = Vec::new();
    let states: Vec<bool> = PILLS.lock().unwrap().iter().map(|x| x.1).collect();
    for (i, row) in rows.iter().enumerate() {
        let on = states.get(i).copied().unwrap_or(false);
        let mut kvs: Vec<(String, serde_json::Value)> = Vec::new();
        for (j, (k, _lbl)) in row.keys.iter().enumerate() {
            let text = unsafe { get_text(_parent, (ID_EDIT_BASE + i * MAX_ARGS_PER_ROW + j) as i32) };
            if let Some(v) = edit_to_value(&text) {
                kvs.push((k.clone(), v));
            }
        }
        res.push((on, kvs));
    }
    drop(g);
    *RESULT.lock().unwrap() = Some(res);
}

unsafe fn get_text(parent: HWND, id: i32) -> String {
    let h = GetDlgItem(parent, id).unwrap_or(HWND::default());
    if h.is_invalid() {
        return String::new();
    }
    let len = GetWindowTextLengthW(h).max(0) as usize;
    let mut buf = vec![0u16; len + 1];
    GetWindowTextW(h, &mut buf);
    let s: String = buf
        .iter()
        .take_while(|&&c| c != 0)
        .map(|&c| char::from_u32(c as u32).unwrap_or('\u{fffd}'))
        .collect();
    s
}
