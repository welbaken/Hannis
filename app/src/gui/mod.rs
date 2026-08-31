//! Windows GUI. Compiled only on Windows; empty module elsewhere.

#![cfg(target_os = "windows")]

pub mod bubble;
pub mod icon;
pub mod render;
pub mod tray;
pub mod window;

use dshpet::anim::{load_animation, load_loop_animation, Animation, Frame, Player};
use dshpet::bubble_stack;
use dshpet::bubble_text;
use dshpet::config::Config;
use dshpet::connectors::stop_flag;
use dshpet::state::{Mode, PetState, Snapshot, StateEvent};
pub mod settings;
pub mod sound;
use dshpet::config::ScriptEntryConfig;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::PathBuf;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::{
    GetMonitorInfoW, MonitorFromWindow, MONITOR_DEFAULTTONEAREST, MONITORINFO,
};
use windows::Win32::System::Diagnostics::ToolHelp::*;
use windows::Win32::System::Registry::*;
use windows::Win32::System::Threading::{CreateMutexW, OpenProcess, TerminateProcess, PROCESS_TERMINATE};
use windows::Win32::UI::HiDpi::*;
use windows::Win32::UI::Input::KeyboardAndMouse::{ReleaseCapture, SetCapture};
use windows::Win32::UI::WindowsAndMessaging::*;

/// Debug log: the GUI has no console (windows_subsystem), so all notable
/// transitions and any panic payload are appended to `<exe_dir>/hannis.log`
/// to make crashes reproducible.
static LOG: Mutex<Option<std::fs::File>> = Mutex::new(None);

/// 进程内存采样(诊断用):工作集 / 私有字节,单位 MB。
/// 非 Windows(头less 调试)下返回 0。
fn proc_ws_mb() -> u64 {
    get_proc_mem().map(|m| m.0 / (1024 * 1024)).unwrap_or(0)
}
fn proc_priv_mb() -> u64 {
    get_proc_mem().map(|m| m.1 / (1024 * 1024)).unwrap_or(0)
}

#[cfg(target_os = "windows")]
fn get_proc_mem() -> Option<(u64, u64)> {
    use windows::Win32::System::ProcessStatus::K32GetProcessMemoryInfo;
    unsafe {
        let mut pmc = windows::Win32::System::ProcessStatus::PROCESS_MEMORY_COUNTERS::default();
        let cb = std::mem::size_of::<windows::Win32::System::ProcessStatus::PROCESS_MEMORY_COUNTERS>() as u32;
        let h = windows::Win32::System::Threading::GetCurrentProcess();
        if K32GetProcessMemoryInfo(h, &mut pmc, cb).as_bool() {
            Some((pmc.WorkingSetSize as u64, pmc.PagefileUsage as u64))
        } else {
            None
        }
    }
}

#[cfg(not(target_os = "windows"))]
fn get_proc_mem() -> Option<(u64, u64)> {
    None
}

/// 每脚本的停止令牌(接入口启停:置 true 让该脚本线程在下一次
/// 可中断等待处退出)。托盘切换/设置窗口保存时重建。
static SCRIPT_STOPS: OnceLock<Mutex<HashMap<u16, Arc<AtomicBool>>>> = OnceLock::new();

fn script_stops() -> &'static Mutex<HashMap<u16, Arc<AtomicBool>>> {
    SCRIPT_STOPS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// 启动一个 Lua 接入口线程(独立 stop 令牌,便于单独启停)。
fn spawn_script(i: usize, sc: &ScriptEntryConfig, tx: &Sender<StateEvent>, exe_dir: &std::path::Path) {
    if sc.file.trim().is_empty() {
        log_line(&format!("[lua] scripts[{i}] has empty file, skipped"));
        return;
    }
    let mut sc = sc.clone();
    // 相对路径按 exe 目录解析(不能依赖进程 CWD:快捷方式启动时
    // CWD 常是 System32,之前会静默加载失败 → 该源无反应)
    sc.file = Config::resolve_script_path(exe_dir, &sc.file).display().to_string();
    let tok = stop_flag();
    script_stops().lock().unwrap().insert(i as u16, tok.clone());
    log_line(&format!("[lua] scripts[{i}] spawn '{}' -> {}", sc.name, sc.file));
    dshpet::connectors::lua::make(i as u16, sc, Some(exe_dir.join("hannis.log"))).spawn(tx.clone(), tok);
}

pub(crate) fn log_line(msg: &str) {    if let Ok(mut g) = LOG.lock() {
        if let Some(f) = g.as_mut() {
            use std::io::Write;
            let _ = writeln!(f, "{} {msg}", now_ms());
            let _ = f.flush();
        }
    }
}

pub(crate) fn init_log(exe_dir: &PathBuf) {
    *LOG.lock().unwrap() = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(exe_dir.join("hannis.log"))
        .ok();
    std::panic::set_hook(Box::new(|info| {
        log_line(&format!("PANIC: {info}"));
    }));
}

const TIMER_RENDER: usize = 1;
const TIMER_MS: u32 = 15;
/// Typewriter lag cap: when the live stream grows faster than the reveal
/// speed, jump the cursor so the visible window stays at most this many
/// chars behind the newest content.
const TYPE_LAG_CHARS: usize = 240;
/// Horizontal padding around the pet so the top-left phone bubble has a
/// transparent gutter that doesn't crowd the character's face. Also used
/// as the pet's draw-x, so the pet sits flush against the right side of
/// the window while the left side is empty for the bubble.
const WINDOW_EXTRA_W: u32 = 180;
/// Right-edge gap from the screen edge. Kept small so the whole window
/// (and the pet) shifts visibly to the right of where a bottom-right
/// 80-px anchor would place it.
const RIGHT_MARGIN: i32 = 10;
/// 拖拽/恢复/回避归位的可见底线(px):本体大部分可以拖出屏幕,但至少保留
/// 这么多像素可见——保证总能再抓回来(完全离屏会丢窗口,残留实例还会挡住重启)。
const DRAG_SLIVER_PX: i32 = 48;

#[derive(Default)]
pub(crate) struct LoadSlot {
    ready: Option<(Mode, Arc<Animation>, Option<Arc<Animation>>)>,
    error: Option<(Mode, String)>,
}

pub struct App {
    pub cfg: Config,
    pub resource_dir: PathBuf,
    pub pet: PetState,
    pub rx: Receiver<StateEvent>,
    /// Event sender: used to emit `Tick` so the pending-request TTL reaper
    /// actually runs (nothing else drives it).
    pub tx: Sender<StateEvent>,
    pub stop: Arc<AtomicBool>,
    pub load_slot: Arc<Mutex<LoadSlot>>,
    pub loading: Option<String>,
    pub anim: Option<Arc<Animation>>,
    pub player: Option<Player>,
    /// Preloaded separate loop animation (`<state>_loop.sheet.*`), if any.
    pub loop_anim: Option<Arc<Animation>>,
    pub loop_switched: bool,
    pub mode: Mode,
    pub base_mode: Mode,
    pub pending: Option<Mode>,
    pub comp: render::Compositor,
    pub bubble: bubble::Bubble,
    /// Structured bubble content (header row + divider + stream).
    pub bubble_text: bubble_text::BubbleText,
    /// Typewriter reveal cursor: (session_id, stream kind, chars revealed).
    pub reveal: Option<(String, u8, f32)>,
    /// 轮流显示: session whose message the bubble currently shows, and the
    /// last time (now_ms) that message changed. A pick whose message stays
    /// unchanged for bubble.rotate_ms is handed to another session with content.
    pub bubble_pick: Option<String>,
    pub bubble_stale_since: Option<u64>,
    pub last_interaction: Instant,
    pub fade_alpha: f32,
    pub dragging: bool,
    pub drag_win: (i32, i32),
    pub drag_cursor: (i32, i32),
    pub hwnd: HWND,
    pub tray: Option<tray::Tray>,
    pub cf_from: Option<Frame>,
    pub cf_t: f32,
    pub last_tick: Instant,
    /// 内存日志采样时刻(每 30s 写一行 [mem])。
    pub last_mem_log: Instant,
    /// True when a fullscreen app on the same monitor is covering the pet;
    /// the window is hidden via ShowWindow and not interacted with until the
    /// fullscreen app exits.
    pub hidden: bool,
    pub last_fs_check: Instant,
    /// 回避模式 (avoid mode): the pet's resting spot, recorded the moment a
    /// dodge begins, so it can glide back once the cursor leaves the area.
    pub avoid_home: Option<(i32, i32)>,
    /// True while the pet is currently displaced (dodging) by the cursor.
    pub avoid_offscreen: bool,
    /// Non-transparent pet bbox in window-local coords (x_off, y_off, w, h),
    /// cached from the loaded animation's first frame so the avoid trigger
    /// hugs the pet's visible body instead of the whole layered window rect.
    /// None = not scanned yet -> the full window rect is used as a fallback.
    pub avoid_box: Option<(i32, i32, i32, i32)>,
    /// Dirty-flag rendering: set when compose-relevant state changed since
    /// the last present (mode/animation switch, bubble text, zoom...).
    /// The 15 ms timer early-outs without recompositing when nothing moved —
    /// idle CPU drops to near zero instead of redrawing at 66 fps.
    pub dirty_request: bool,
    /// (animation name, frame index) of the last composed frame; None =
    /// nothing composed yet. Compared each tick to skip redundant redraws.
    pub composed: Option<(String, usize)>,
    /// Fade alpha of the last composed frame (fade transitions redraw).
    pub composed_fade: f32,
    /// 自动收起(auto-hide):idle/offline 连续持续的起点(None=不在计时)。
    pub collapse_idle_since: Option<Instant>,
    /// 已处于收起状态(下移到任务栏后 + 更透明 + 鼠标穿透)。
    pub collapsed: bool,
    /// 收起前的窗口位置,退出收起时恢复。
    pub collapse_home: Option<(i32, i32)>,
    /// 悬停计时起点(now_ms);None=光标不在宠物窗口上。
    pub collapse_hover_since: Option<u64>,
    /// 收起时从缓冲区底部裁掉的像素行数(被任务栏盖住的身体部分留透明)。
    pub collapse_clip: i32,
    /// 收起/恢复的滑动目标位置;None=静止。按下→目标用 move_toward 逐帧滑动。
    pub collapse_anim: Option<(i32, i32)>,
    /// 气泡淡入系数(0..1):出现时 200ms 内从 0 → 1(配合 8px 滑入)。
    pub bubble_fade: f32,
    /// 鼠标穿透悬浮的气泡变暗系数(0.05..1,与本体同步 lerp):光标悬在
    /// 宠物上时气泡随本体一起压到 hover_opacity(透视下层);平时恒 1.0,
    /// 气泡的免渐隐可读性不变。
    pub bubble_hover_dim: f32,
    /// 上一次 compose 时的 bubble_hover_dim(变化超阈值才重绘)。
    pub composed_bubble_dim: f32,
    /// 自动收起恢复后:回避模式先挂起,直到光标首次离开宠物(避免"叫回来
    /// 立刻又被躲开/被回避拉扯回不了原位")。
    pub avoid_arm_after_leave: bool,
    /// 多源堆叠(bubble.stack):每源出现/消失滞后计时(源 id → 状态)。
    pub stack_state: BTreeMap<u16, bubble_stack::StackState>,
    /// 多源堆叠卡池(源 id → (卡 widget, 出现时刻 now_ms)):按源池化保住
    /// layout 早退(内容与几何都没变时零开销),每卡出现时 200ms 淡入。
    pub stack_pool: BTreeMap<u16, (bubble::Bubble, u64)>,
    /// 多源堆叠缓存:timer_tick 算出的当前卡列表(后→前,末位 = 前排卡),
    /// compose 按此顺序级联绘制。
    pub stack_cache: Vec<bubble_stack::StackCard>,
    /// 提示音:上一拍的快照模式,用于检测"进入 Attention"沿(播放 attention 音)。
    pub sound_prev_mode: Mode,
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

pub fn run() {
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
    // single instance: a second launch exits — UNLESS the existing instance
    // has no window (crashed/hung, or its window got lost off-screen). In
    // that case we kill the stale process and take over, so the pet can
    // always be reopened after a mishap instead of being bricked by a
    // zombie holding the mutex forever.
    unsafe {
        let _h = CreateMutexW(None, false, w!("hannis-single-instance")).unwrap_or(HANDLE::default());
        if GetLastError() == ERROR_ALREADY_EXISTS {
            let existing = FindWindowW(w!("hannis"), PCWSTR::null()).unwrap_or(HWND::default());
            if existing.0.is_null() {
                eprintln!("[gui] stale instance without a window - cleaning up");
                kill_stale_instances();
            } else {
                // The other instance has a window: only block when that
                // window is actually reachable on the virtual screen. A
                // window parked entirely off-screen (lost by a drag /
                // multi-monitor quirk) is as good as gone and would brick
                // every relaunch, so we kill the stale process instead.
                let mut r = RECT::default();
                // 失败(窗口恰在检查间隙销毁)时 r 保持全零 → offscreen →
                // 走 stale 清理,与本分支语义一致,可安全忽略返回值
                let _ = GetWindowRect(existing, &mut r);
                let vx = GetSystemMetrics(SM_XVIRTUALSCREEN);
                let vy = GetSystemMetrics(SM_YVIRTUALSCREEN);
                let vw = GetSystemMetrics(SM_CXVIRTUALSCREEN);
                let vh = GetSystemMetrics(SM_CYVIRTUALSCREEN);
                let onscreen = r.right > vx && r.left < vx + vw && r.bottom > vy && r.top < vy + vh;
                if onscreen {
                    eprintln!("hannis already running");
                    std::process::exit(0);
                } else {
                    eprintln!("[gui] existing instance window is off-screen - cleaning up");
                    kill_stale_instances();
                }
            }
        }
    }
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()))
        .unwrap_or_else(|| PathBuf::from("."));
    init_log(&exe_dir);
    log_line("=== hannis start ===");
    let cfg = Config::load(&exe_dir.join("config.json"));
    if !exe_dir.join("config.json").exists() {
        let _ = cfg.save(&exe_dir.join("config.json"));
    }
    let exe_path = std::env::current_exe().unwrap_or_else(|_| exe_dir.join("hannis.exe"));
    apply_autostart(&cfg, &exe_path);
    let resource_dir = exe_dir.join("resource");
    let icon = icon::load_hicon(&exe_dir);

    let (tx, rx) = channel::<StateEvent>();
    let stop = stop_flag();
    // 全部来源都是 Lua 脚本(DSH/Hermes/MAA/ComfyUI/自定义)
    for (i, sc) in cfg.scripts.iter().enumerate() {
        if !sc.enabled {
            log_line(&format!("[lua] scripts[{i}] disabled in config, skipped"));
            continue;
        }
        spawn_script(i, sc, &tx, &exe_dir);
    }

    let done_ms = cfg.windows.done_sec * 1000;
    let fail_ms = cfg.windows.fail_sec * 1000;
    let celebrate_ms = cfg.windows.celebrate_sec * 1000;
    let font_scale = cfg.bubble.font_scale;
    let scale = cfg.display.scale.clamp(0.25, 2.0);
    let w = ((800.0 + WINDOW_EXTRA_W as f32) * scale) as i32;
    let h = (800.0 * scale) as i32;
    let (sx, sy) = screen_size();
    let mut x = (sx - w - RIGHT_MARGIN).max(0);
    let mut y = (sy - h - 80).max(0);
    if let (Some(px), Some(py)) = (cfg.window_pos.x, cfg.window_pos.y) {
        // restore the pet's last dragged spot, clamped to the virtual
        // screen so a monitor layout change can never park it unreachable.
        // 与拖拽同一规则:本体大部分可拖出屏外(至少保留 DRAG_SLIVER_PX 可见),
        // 所以上次拖到边缘外(如 80% 出屏)的位置重启后也原样恢复。
        unsafe {
            let dpi = GetDpiForSystem().max(96) as f32 / 96.0;
            let pet_w = (800.0 * scale) as u32;
            let (lo, hi) = pet_drag_x_bounds(pet_w, bubble_overhang_px(pet_w, dpi));
            x = px.clamp(lo, hi);
            let (lo, hi) = pet_drag_y_bounds(h);
            y = py.clamp(lo.min(hi), lo.max(hi));
        }
        log_line(&format!("[gui] restore window pos ({x}, {y})"));
    }

    let bubble_theme = dshpet::config::BubbleTheme::resolve(&cfg.bubble.theme);
    let acrylic_on = cfg.bubble.theme.acrylic; // cfg 之后被移入 App,先取出来
    let mut app = Box::new(App {
        cfg,
        resource_dir,
        pet: PetState::new(done_ms, fail_ms),
        rx,
        tx,
        stop,
        load_slot: Arc::new(Mutex::new(LoadSlot::default())),
        loading: None,
        anim: None,
        player: None,
        loop_anim: None,
        loop_switched: false,
        mode: Mode::Idle,
        base_mode: Mode::Idle,
        pending: None,
        comp: render::Compositor::new(HWND::default(), 1.0),
        bubble: bubble::Bubble::default(),
        bubble_text: bubble_text::BubbleText::default(),
        reveal: None,
        bubble_pick: None,
        bubble_stale_since: None,
        last_interaction: Instant::now(),
        fade_alpha: 1.0,
        dragging: false,
        drag_win: (0, 0),
        drag_cursor: (0, 0),
        hwnd: HWND::default(),
        tray: None,
        cf_from: None,
        cf_t: 1.0,
        last_tick: Instant::now(),
        hidden: false,
        last_fs_check: Instant::now(),
        last_mem_log: Instant::now(),
        avoid_home: None,
        avoid_offscreen: false,
        avoid_box: None,
        dirty_request: true, // first tick must present once
        composed: None,
        composed_fade: 1.0,
        collapse_idle_since: None,
        collapsed: false,
        collapse_home: None,
        collapse_hover_since: None,
        collapse_clip: 0,
        collapse_anim: None,
        bubble_fade: 0.0,
        bubble_hover_dim: 1.0,
        composed_bubble_dim: 1.0,
        avoid_arm_after_leave: false,
        stack_state: BTreeMap::new(),
        stack_pool: BTreeMap::new(),
        stack_cache: Vec::new(),
        sound_prev_mode: Mode::Idle,
    });
    app.pet.set_celebrate_ms(celebrate_ms);
    app.bubble.theme = bubble_theme;
    let ptr = &mut *app as *mut App;
    let hwnd = window::create_main_window(ptr, w, h, icon.unwrap_or(HICON::default()));
    app.hwnd = hwnd;
    app.comp = render::Compositor::new(hwnd, font_scale);
    // 上一帧快照只在亚克力需要反混合时保留(省 ~3MB 常驻)
    app.comp.keep_last_frame = acrylic_on;
    // 鼠标穿透(配置开启则启动即生效;窗口收不到鼠标消息,悬浮检测走轮询)
    if app.cfg.click_through.enabled {
        window::set_click_through(hwnd, true);
        log_line("[click-through] enabled from config at startup");
    }
    window::set_window_rect(hwnd, x, y, w, h);
    window::show(hwnd);
    // 注意:不再对整窗调用 DWM accent(SetWindowCompositionAttribute):
    // (1) 系统是分层窗口,Win11 上该 API 要么静默失效,要么把 80% 白色渐变
    //     刷满整窗(透明背景变白不透明);(2) 亚克力由气泡的软件实现
    //     (截屏→反混合→模糊→着色)提供,只作用于卡片区域。
    unsafe {
        let _ = SetTimer(hwnd, TIMER_RENDER, TIMER_MS, None);
    }
    let mut tray = tray::Tray::new(hwnd, icon);
    tray.add();
    app.tray = Some(tray);
    app.spawn_load("idle");

    let mut msg = MSG::default();
    unsafe {
        while GetMessageW(&mut msg, None, 0, 0).0 != 0 {
            let _ = TranslateMessage(&msg);
            let _ = DispatchMessageW(&msg);
        }
    }
    // shutdown
    app.stop.store(true, Ordering::Relaxed);
}

fn screen_size() -> (i32, i32) {
    unsafe {
        (
            GetSystemMetrics(SM_CXSCREEN),
            GetSystemMetrics(SM_CYSCREEN),
        )
    }
}

/// Distance from a point to the nearest point on an axis-aligned rect.
fn point_rect_dist(px: f32, py: f32, rx: f32, ry: f32, rw: f32, rh: f32) -> f32 {
    let nx = px.clamp(rx, rx + rw);
    let ny = py.clamp(ry, ry + rh);
    let dx = px - nx;
    let dy = py - ny;
    (dx * dx + dy * dy).sqrt()
}

/// Move `cur` toward `goal` at `speed` px/s over a `dt_ms` tick. Returns the
/// new position and whether the goal was actually reached (within a pixel).
fn move_toward(cur: (i32, i32), goal: (i32, i32), speed: f32, dt_ms: u64) -> ((i32, i32), bool) {
    let step = speed.max(0.0) * (dt_ms as f32 / 1000.0);
    let dx = goal.0 as f32 - cur.0 as f32;
    let dy = goal.1 as f32 - cur.1 as f32;
    let dist = (dx * dx + dy * dy).sqrt();
    if dist < 0.5 || step >= dist {
        (goal, true)
    } else {
        let nx = (cur.0 as f32 + dx / dist * step).round() as i32;
        let ny = (cur.1 as f32 + dy / dist * step).round() as i32;
        ((nx, ny), false)
    }
}

/// 气泡左缘相对本体左缘的偏移(物理 px):右缘锚定在 动图宽×DPI 的
/// BUBBLE_RIGHT_FRACTION(= 设计宽×DPI 的 25%)。动图宽必须乘 DPI:
/// 帧按 display.scale 缩放(不含 DPI),气泡宽按 DPI 缩放,两者混算会把
/// 比例锚点拉离原位——scale=1.0 时右缘必须恒为 200×DPI,否则 DPI>100%
/// 下气泡会明显偏左。
fn bubble_left_rel(pet_w: u32, dpi: f32) -> f32 {
    (pet_w as f32) * dpi * bubble::BUBBLE_RIGHT_FRACTION
        - bubble::scaled(bubble::MAX_BUBBLE_W, dpi) as f32
}

/// 气泡左缘越过本体左缘时本体需向右让位的距离(px):`pet_offset_x` 与
/// 启动恢复(`run`)共用同一公式。
fn bubble_overhang_px(pet_w: u32, dpi: f32) -> i32 {
    (-bubble_left_rel(pet_w, dpi)).max(0.0).round() as i32
}

/// 本体拖拽/恢复的横向边界(物理 px):本体(窗口 x + 让位偏移)大部分可拖出
/// 虚拟屏,但至少保留 `DRAG_SLIVER_PX` 可见(窗口的左右留白 EXTRA 与让位
/// 允许悬出屏外)。原先把本体完整钳在屏内,用户没法把宠物拖到屏幕边缘外。
fn pet_drag_x_bounds(pet_w: u32, overhang: i32) -> (i32, i32) {
    let vx = unsafe { GetSystemMetrics(SM_XVIRTUALSCREEN) };
    let vw = unsafe { GetSystemMetrics(SM_CXVIRTUALSCREEN) };
    let s = DRAG_SLIVER_PX;
    let lo = vx + s - overhang - pet_w as i32;
    let hi = vx + vw - s - overhang;
    (lo.min(hi), lo.max(hi))
}

/// 纵向边界(物理 px):窗口至少保留 `DRAG_SLIVER_PX` 在虚拟屏内(其余可拖出)。
fn pet_drag_y_bounds(h: i32) -> (i32, i32) {
    unsafe {
        let vy = GetSystemMetrics(SM_YVIRTUALSCREEN);
        let vh = GetSystemMetrics(SM_CYVIRTUALSCREEN);
        let s = DRAG_SLIVER_PX;
        (vy + s - h, vy + vh - s)
    }
}

impl App {
    fn window_size(&self) -> (i32, i32, i32, i32) {
        window::window_rect(self.hwnd)
    }

    fn spawn_load(&mut self, state: &str) {
        if self.loading.as_deref() == Some(state) {
            return;
        }
        self.loading = Some(state.to_string());
        let dir = self.resource_dir.clone();
        let scale = self.cfg.display.scale.clamp(0.25, 2.0);
        let frame_ms = self.cfg.display.frame_ms;
        let slot = self.load_slot.clone();
        let mode = self.mode;
        let state = state.to_string();
        std::thread::Builder::new()
            .name(format!("anim-{state}"))
            .spawn(move || {
            // decoding must never take the whole app down (panic=abort):
            // catch any panic in the load path and report it as an error
            let slot2 = slot.clone();
            let state2 = state.clone();
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
                match load_animation(&dir, &state2, scale, frame_ms) {
                    Ok(a) => {
                        let a = Arc::new(a);
                        // preload the separate loop animation while we are at it
                        let loop_anim = load_loop_animation(&dir, &state2, scale, frame_ms)
                            .map(Arc::new);
                        if loop_anim.is_some() {
                            log_line(&format!("[anim] loop file found for {state2}"));
                        }
                        slot2.lock().unwrap().ready = Some((mode, a, loop_anim));
                    }
                    Err(e) => {
                        let msg = e.to_string();
                        log_line(&format!("[anim] load {state2}: {msg}"));
                        slot2.lock().unwrap().error = Some((mode, msg));
                    }
                }
            }));
            if let Err(p) = result {
                log_line(&format!("[anim] load {state} PANICKED: {p:?}"));
                slot.lock().unwrap().error =
                    Some((mode, format!("load {state} panicked")));
            }
        })
        .ok();
    }

    pub fn timer_tick(&mut self) {
        let now = Instant::now();
        let dt = now.duration_since(self.last_tick).as_millis() as u64;
        self.last_tick = now;
        // 内存采样(每 30s 一行,诊断"内存多了很多"用):工作集/私有字节
        if now.duration_since(self.last_mem_log) >= Duration::from_secs(30) {
            self.last_mem_log = now;
            log_line(&format!("[mem] ws={}MB priv={}MB", proc_ws_mb(), proc_priv_mb()));
        }
        // drive the pending-request TTL reaper (approvals/questions older
        // than TTL_APPROVAL_MS get dropped); without this the 30-min safety
        // net never runs and a stale pending keeps attention forever
        let _ = self.tx.send(StateEvent::Tick);

        // 0) fullscreen detection: poll every ~500ms so the pet steps out
        //    of the way when a game / video goes fullscreen on the same
        //    monitor, and reappears when it exits.
        if now.duration_since(self.last_fs_check) >= Duration::from_millis(500) {
            self.last_fs_check = now;
            let fs = self.check_fullscreen();
            if fs.is_some() != self.hidden {
                self.hidden = fs.is_some();
                unsafe {
                    if self.hidden {
                        let _ = ShowWindow(self.hwnd, SW_HIDE);
                    } else {
                        let _ = ShowWindow(self.hwnd, SW_SHOWNOACTIVATE);
                    }
                }
                let detail = fs
                    .map(|c| format!("fullscreen detected: window class '{c}'"))
                    .unwrap_or_else(|| "fullscreen cleared".to_string());
                log_line(&format!("[gui] {} ({detail})", if self.hidden { "hidden" } else { "shown" }));
            }
        }

        // 1) crossfade progress
        if self.cf_from.is_some() {
            self.cf_t += dt as f32 / 200.0;
            if self.cf_t >= 1.0 {
                self.cf_from = None;
            }
        }

        // 2) drain connector events
        self.pet.now_ms = now_ms();
        while let Ok(ev) = self.rx.try_recv() {
            self.pet.apply(ev);
        }
        let snap = self.pet.snapshot();
        self.base_mode = snap.mode;

        // 2b) 提示音:done/failed 用状态机的沿标记;attention 用模式进入沿。
        //     托盘「提示音」关掉后不播(配置写回 config.json)。沿标记无论
        //     开关都要消费掉:否则关闭期间积压的 done/fail 标志会在重新
        //     开启后补响一声旧事件。
        let (done_snd, fail_snd) = self.pet.consume_sounds();
        if self.cfg.sound.enabled {
            if done_snd {
                sound::play(&self.resource_dir, "done");
            }
            if fail_snd {
                sound::play(&self.resource_dir, "failed");
            }
            if snap.mode == Mode::Attention && self.sound_prev_mode != Mode::Attention {
                sound::play(&self.resource_dir, "attention");
            }
        }
        self.sound_prev_mode = snap.mode;

        // 3) effective mode (drag overlay: move animation only for idle)
        let effective = self.effective_mode();
        if effective != self.mode {
            self.switch_mode(effective);
        }

        // 4) consume loaded animation
        let ready = {
            let mut slot = self.load_slot.lock().unwrap();
            slot.ready.take()
        };
        if let Some((m, a, la)) = ready {
            let target = self.pending.unwrap_or(self.mode);
            if m == target {
                self.cf_from = self.current_frame_clone();
                self.cf_t = 0.0;
                self.anim = Some(a.clone());
                self.dirty_request = true;
                self.refresh_avoid_box(); // bbox for the 回避模式 trigger
                self.loop_anim = la;
                self.loop_switched = false;
                self.loading = None;
                self.pending = None;
                if self.mode != Mode::Offline {
                    self.player = Some(Player::new(
                        &a,
                        self.mode.loops_full(),
                        self.cfg.display.tail_ms,
                        self.cfg.display.tail_frames,
                    ));
                }
                eprintln!("[gui] anim ready: {} (mode {:?})", a.name, self.mode);
                log_line(&format!("[gui] anim ready: {} (mode {:?})", a.name, self.mode));
            } else {
                // stale result (mode changed while decoding): allow a fresh
                // request for the asset we still need
                self.loading = None;
            }
        }
        // load errors: clear the loading flag and fall back to idle so the
        // pet never stays invisible
        let load_err = {
            let mut slot = self.load_slot.lock().unwrap();
            slot.error.take()
        };
        if let Some((m, e)) = load_err {
            eprintln!("[gui] anim load failed for {m:?}: {e}");
            log_line(&format!("[gui] anim load failed for {m:?}: {e}"));
            if self.loading.is_some() {
                self.loading = None;
            }
            if self.anim.is_none() {
                self.pending = Some(Mode::Offline);
                self.dirty_request = true;
                self.spawn_load("idle");
            }
        }

        // make sure a pending target always has a load in flight
        if let Some(p) = self.pending {
            if self.loading.is_none() {
                let need = if p == Mode::Offline { "idle" } else { p.asset() };
                let have = self.anim.as_ref().map(|a| a.name.as_str());
                if have != Some(need) {
                    self.spawn_load(need);
                }
            }
        }

        // 5) advance playback (keep showing the old animation while a new
        //    one is loading - never blank the window on mode switches)
        if let Some(a) = &self.anim {
            let asset = self.mode.asset();
            if a.name == asset && self.mode != Mode::Offline {
                if self.player.is_none() {
                    self.player = Some(Player::new(
                        a,
                        self.mode.loops_full(),
                        self.cfg.display.tail_ms,
                        self.cfg.display.tail_frames,
                    ));
                }
                if let Some(p) = &mut self.player {
                    p.advance(a, dt);
                }
                // initial full pass done: switch to the separate loop
                // animation if one was preloaded (hard cut: the loop's first
                // frame is authored to continue the action's last frame)
                if !self.loop_switched {
                    let pass_done = self.player.as_ref().map(|p| p.full_passes > 0).unwrap_or(false);
                    if pass_done {
                        self.loop_switched = true;
                        if let Some(la) = self.loop_anim.clone() {
                            eprintln!("[gui] switching to loop anim {}", la.name);
                            log_line(&format!("[gui] switching to loop anim {}", la.name));
                            self.anim = Some(la.clone());
                            self.refresh_avoid_box(); // new bbox for 回避模式
                            self.player = Some(Player::new(
                                &la,
                                true,
                                self.cfg.display.tail_ms,
                                self.cfg.display.tail_frames,
                            ));
                        }
                    }
                }
            }
        } else if self.mode != Mode::Offline && self.pending.is_none() && self.loading.is_none() {
            self.spawn_load(self.mode.asset());
        }

        // 6) text: hidden while resting or fully disconnected. The phone
        //    bubble shows structured content (header row + divider + stream)
        //    from bubble_text::BubbleText, using the wide per-line window
        //    (text.max_chars).
        // 气泡淡入:出现时 200ms 从 0→1(内容为空则立即归零)
        let stack_shown = self.cfg.bubble.stack && !self.stack_cache.is_empty();
        let fade_target = if self.bubble.visible() || stack_shown { 1.0 } else { 0.0 };
        let fade_k = ((dt as f32) / 200.0).min(1.0);
        self.bubble_fade += (fade_target - self.bubble_fade) * fade_k;
        if self.bubble_fade > 0.999 {
            self.bubble_fade = 1.0;
        }
        if fade_target == 0.0 {
            self.bubble_fade = 0.0;
        }
        let sel = self.pet.select_bubble_source();
        let type_cps = self.cfg.bubble.type_cps;
        let max_chars = self.cfg.text.max_chars;
        let tick_now = now_ms();
        if matches!(effective, Mode::Idle | Mode::Offline | Mode::Move) {
            self.clear_stack();
            self.bubble_pick = None;
            self.bubble_stale_since = None;
            self.reveal = None;
            // The Bubble widget keeps its own laid-out text copy (`visible()`
            // and `draw()` read it): resetting only the App-level
            // `bubble_text` here left the last message (e.g. "任务完成啦")
            // on screen forever once the pet returned to idle. layout() with
            // an empty title clears the widget (and reports whether anything
            // actually changed, so the repaint only fires on a real change).
            let cleared = self.bubble.layout(bubble_text::BubbleText::default(), 0, 0, &self.comp);
            self.bubble_text = bubble_text::BubbleText::default();
            if cleared {
                self.dirty_request = true;
            }
        } else if matches!(effective, Mode::Thinking | Mode::Working) {
            if self.cfg.bubble.stack {
                // 多源堆叠路径:每个有活动会话的接入口一张级联卡
                self.update_stack_bubble(&snap, effective, tick_now, dt, type_cps, max_chars);
            } else {
                self.clear_stack();
                // 轮流显示: keep the current session while its message keeps
                // updating; once it has been static for rotate_ms and
                // another session has content, hand the bubble over to it.
                let stale = self
                    .bubble_stale_since
                    .map(|at| tick_now.saturating_sub(at) >= self.cfg.bubble.rotate_ms)
                    .unwrap_or(false);
                let pick = bubble_text::rotate_pick(&snap, sel, effective, self.bubble_pick.as_deref(), stale);
                if pick != self.bubble_pick {
                    self.bubble_pick = pick.clone();
                    self.bubble_stale_since = Some(tick_now);
                }
                let prefer = self.bubble_pick.as_deref();
                let pos = if type_cps > 0 {
                    // typewriter: reveal the live stream char by char
                    let stream = bubble_text::live_stream_pinned(&snap, sel, effective, prefer);
                    let same = match (&self.reveal, &stream) {
                        (Some((sid, kind, _)), Some(s)) => *sid == s.session_id && *kind == s.kind,
                        (None, None) => true,
                        _ => false,
                    };
                    if !same {
                        // new session / new stream (e.g. next turn): start over
                        self.reveal = stream.as_ref().map(|s| (s.session_id.clone(), s.kind, 0.0));
                    }
                    let p = match (&mut self.reveal, &stream) {
                        (Some((_, _, pos)), Some(s)) => {
                            *pos += type_cps as f32 * dt as f32 / 1000.0;
                            let p = (*pos as usize).min(s.len);
                            // keep up with a fast stream: never lag more than one
                            // and a half visible windows behind the newest chars
                            if s.len.saturating_sub(p) > TYPE_LAG_CHARS {
                                *pos = (s.len - TYPE_LAG_CHARS) as f32;
                            }
                            Some(p)
                        }
                        _ => None,
                    };
                    p
                } else {
                    self.reveal = None;
                    None
                };
                let text = bubble_text::bubble_text_pinned(&snap, sel, prefer, pos, max_chars);
                // a changed message resets the staleness timer
                if text != self.bubble_text {
                    self.bubble_stale_since = Some(tick_now);
                    self.bubble_text = text;
                    self.dirty_request = true;
                }
                // 每帧按当前 pet 尺寸重排:宠物缩小后旧的大气泡会把下段顶出
                // 窗口底部被截断;layout 在文本与几何都未变时直接返回(零开销)
                let pet_w = self.pet_size().0;
                let pet_h = self.pet_size().1;
                if self.bubble.layout(self.bubble_text.clone(), pet_w, pet_h, &self.comp) {
                    self.dirty_request = true;
                }
            }
        } else {
            self.clear_stack();
            self.reveal = None;
            self.bubble_pick = None;
            self.bubble_stale_since = None;
            let text = bubble_text::bubble_text_pinned(&snap, sel, None, None, max_chars);
            if text != self.bubble_text {
                self.bubble_text = text;
                self.dirty_request = true;
            }
            let pet_w = self.pet_size().0;
            let pet_h = self.pet_size().1;
            if self.bubble.layout(self.bubble_text.clone(), pet_w, pet_h, &self.comp) {
                self.dirty_request = true;
            }
        }

        // 7) fade
        self.update_fade(dt, &snap);

        // 7b) 回避模式: scurry away from the cursor, glide back home once the
        //     cursor leaves the pet's area. Moves the window only.
        self.update_avoid(dt);

        // 7c) 自动收起(auto-hide):idle/offline 太久 → 下移到任务栏后、
        //     更透明、鼠标穿透;有新消息或悬停超时 → 恢复。
        self.update_collapse(dt);

        // 8) compose + present (skip while hidden to save CPU during long
        //    fullscreen sessions; the DIB keeps the last frame for the
        //    SW_SHOW transition). Dirty-flag rendering: the 15ms timer only
        //    recomposites when something actually changed — an animation
        //    frame boundary, a cross-fade in flight, bubble text, the fade
        //    alpha, or the window size — otherwise the previous frame stays
        //    up and the tick early-outs (idle CPU ≈ 0 instead of 66 fps).
        if !self.hidden {
            let mut dirty = self.dirty_request;
            self.dirty_request = false;
            if self.cf_from.is_some() {
                dirty = true;
            }
            let anim_key = self
                .anim
                .as_ref()
                .map(|a| a.name.clone())
                .zip(self.player.as_ref().map(|p| p.idx));
            if anim_key != self.composed {
                dirty = true;
            }
            if (self.fade_alpha - self.composed_fade).abs() > 0.002 {
                dirty = true;
            }
            // 气泡悬浮变暗系数变化也要重绘(穿透悬浮的淡入/恢复)
            if (self.bubble_hover_dim - self.composed_bubble_dim).abs() > 0.002 {
                dirty = true;
            }
            let (_, _, w, h) = self.window_size();
            if w as u32 != self.comp.win_w || h as u32 != self.comp.win_h {
                dirty = true;
            }
            if dirty {
                self.compose();
                self.composed = self
                    .anim
                    .as_ref()
                    .map(|a| a.name.clone())
                    .zip(self.player.as_ref().map(|p| p.idx));
                self.composed_fade = self.fade_alpha;
                self.composed_bubble_dim = self.bubble_hover_dim;
            }
        }
    }

    fn pet_size(&self) -> (u32, u32) {
        if let Some(a) = &self.anim {
            let f = a.frame(0);
            (f.w, f.h)
        } else {
            let s = self.cfg.display.scale.clamp(0.25, 2.0);
            ((800.0 * s) as u32, (800.0 * s) as u32)
        }
    }

    fn current_frame_clone(&self) -> Option<Frame> {
        let a = self.anim.as_ref()?;
        let idx = self.player.as_ref().map(|p| p.idx).unwrap_or(0);
        Some(render::frame_clone(a.frame(idx)))
    }

    /// Returns Some(class name) when the FOREGROUND window on the pet's
    /// monitor currently covers the full monitor (true fullscreen /
    /// borderless fullscreen), None otherwise.
    ///
    /// Only the foreground window is inspected, NOT every top-level window:
    /// the shell's desktop windows ("Progman" desktop background, "WorkerW"
    /// desktop-icons host) are visible windows that also span the whole
    /// monitor, so an EnumWindows scan would mistake the plain desktop for
    /// fullscreen and hide the pet at startup — exactly the "pet invisible
    /// right after launch while the tray icon works" bug (the log shows
    /// "hidden (fullscreen detected)" ~500ms after start).
    fn check_fullscreen(&self) -> Option<String> {
        unsafe {
            let fg = GetForegroundWindow();
            if fg.0.is_null() || fg == self.hwnd {
                return None;
            }
            // The desktop background / desktop-icon windows are visible and
            // cover the whole monitor; never treat them as fullscreen.
            if fg == GetShellWindow() || fg == GetDesktopWindow() {
                return None;
            }
            let mut cls = [0u16; 128];
            let n = GetClassNameW(fg, &mut cls);
            let class = String::from_utf16_lossy(&cls[..n.max(0) as usize]);
            if class == "Progman" || class == "WorkerW" {
                return None;
            }
            // The fullscreen app must be on the same monitor as the pet;
            // fullscreen on a different monitor doesn't hide us.
            let fg_mon = MonitorFromWindow(fg, MONITOR_DEFAULTTONEAREST);
            let pet_mon = MonitorFromWindow(self.hwnd, MONITOR_DEFAULTTONEAREST);
            if fg_mon != pet_mon {
                return None;
            }
            let mut r = RECT::default();
            let _ = GetWindowRect(fg, &mut r);
            let mut mi = MONITORINFO {
                cbSize: std::mem::size_of::<MONITORINFO>() as u32,
                ..Default::default()
            };
            let _ = GetMonitorInfoW(pet_mon, &mut mi);
            // A fullscreen window covers the entire monitor (rcMonitor, not
            // rcWork). One-sided comparisons tolerate the few extra pixels
            // of invisible resize borders on borderless windows.
            const TOL: i32 = 10;
            if r.left <= mi.rcMonitor.left + TOL
                && r.top <= mi.rcMonitor.top + TOL
                && r.right >= mi.rcMonitor.right - TOL
                && r.bottom >= mi.rcMonitor.bottom - TOL
            {
                // Careful: on Win10/11 a plain MAXIMIZED window's rect also
                // covers the whole monitor (it extends behind the taskbar
                // and past the screen edge via invisible DWM resize
                // borders), so the rect test alone would treat every
                // maximized app as fullscreen — switching to a maximized
                // program via the taskbar thumbnail / Alt+Tab would hide
                // the pet. Distinguish by chrome: a window that still has
                // its caption / title bar is a normal app (possibly
                // maximized) and must NOT hide the pet; genuine fullscreen
                // games and players drop WS_CAPTION.
                let style = GetWindowLongPtrW(fg, GWL_STYLE) as u32;
                if style & WS_CAPTION.0 == 0 {
                    return Some(class);
                }
            }
            None
        }
    }

    fn switch_mode(&mut self, m: Mode) {
        if self.mode == m {
            return;
        }
        eprintln!("[gui] mode: {:?} -> {:?}", self.mode, m);
        log_line(&format!("[gui] mode: {:?} -> {:?}", self.mode, m));
        self.mode = m;
        self.dirty_request = true;
        // state change cancels the fade-out and restarts the countdown
        self.fade_alpha = 1.0;
        self.last_interaction = Instant::now();
        self.cf_from = None;
        self.cf_t = 0.0;
        let asset = if m == Mode::Offline { "idle" } else { m.asset() };
        if let Some(a) = &self.anim {
            if a.name == asset {
                // already showing the right animation
                self.pending = None;
                self.loading = None;
                self.player = if m == Mode::Offline {
                    None
                } else {
                    Some(Player::new(
                        a,
                        m.loops_full(),
                        self.cfg.display.tail_ms,
                        self.cfg.display.tail_frames,
                    ))
                };
                return;
            }
        }
        // keep the current animation on screen until the new one decodes
        self.pending = Some(m);
        self.loop_anim = None;
        self.loop_switched = false;
        self.spawn_load(asset);
    }

    fn update_fade(&mut self, dt: u64, _snap: &Snapshot) {
        let disabled = self
            .cfg
            .fade
            .fade_disabled_states
            .iter()
            .any(|s| s.as_str() == self.mode.asset() || s.as_str() == format!("{:?}", self.mode).to_lowercase());
        let idle_for = self.last_interaction.elapsed().as_secs();
        let hover = self.cfg.click_through.enabled
            && !self.hidden
            && !self.collapsed
            && self.cursor_over_pet();
        let mut target = if disabled || idle_for < self.cfg.fade.fade_after_sec {
            1.0
        } else {
            self.cfg.fade.fade_target.clamp(0.0, 1.0)
        };
        // 鼠标穿透悬浮反馈:光标悬在宠物上时把不透明度压到 hover_opacity
        // (默认 0.1 = 透明度 90%,透视看到下层界面),移开后恢复。穿透状态
        // 下窗口收不到鼠标消息,悬浮检测只能轮询光标位置;收起状态有自己的
        // 悬停唤回逻辑,不参与;全屏隐藏时窗口不可见,同样跳过。
        let hover_dim = self.cfg.click_through.hover_opacity.clamp(0.05, 1.0);
        if hover {
            target = target.min(hover_dim);
        }
        if self.collapsed {
            // 自动收起:在渐隐基础上进一步变透明(同一套 lerp 平滑过渡)
            target *= self.cfg.auto_hide.opacity.clamp(0.05, 1.0);
        }
        let fade_ms = self.cfg.fade.fade_ms.max(50) as f32;
        let k = ((dt as f32) * 4.0 / fade_ms).min(1.0);
        self.fade_alpha += (target - self.fade_alpha) * k;
        if (self.fade_alpha - target).abs() < 0.005 {
            self.fade_alpha = target;
        }
        // 气泡跟随悬浮变暗(单源气泡与堆叠卡统一生效):与本体同一速度
        // lerp;不悬浮时回到 1.0,平时的免渐隐可读性不变
        let dim_target = if hover { hover_dim } else { 1.0 };
        self.bubble_hover_dim += (dim_target - self.bubble_hover_dim) * k;
        if (self.bubble_hover_dim - dim_target).abs() < 0.005 {
            self.bubble_hover_dim = dim_target;
        }
    }

    /// 光标当前是否悬在宠物的可见本体上(屏幕坐标;本体的非透明 bbox,
    /// 气泡留白不算)。鼠标穿透开启时窗口收不到鼠标消息,悬浮检测全靠它。
    fn cursor_over_pet(&self) -> bool {
        let (cx, cy) = window::cursor_pos();
        let (wx, wy, _, _) = self.window_size();
        let (fx, fy, fw, fh) = self.pet_visual_rect();
        cx >= wx + fx && cx < wx + fx + fw.max(1) && cy >= wy + fy && cy < wy + fy + fh.max(1)
    }

    /// 回避模式 (avoid mode). While enabled the pet dodges the cursor and,
    /// once the cursor leaves its home area (`distance * hysteresis`), glides
    /// back to the exact spot it was at when the dodge began.
    ///
    /// Trigger distance is measured against the pet's *home* visual rect — the
    /// non-transparent bbox cached in `avoid_box`, NOT the whole layered window
    /// (which also spans the transparent bubble gutter and clear sprite
    /// margins). The visual rect costs nothing per tick (it is cached on
    /// loading). Dodging against the home rect, never the current dodged rect,
    /// prevents re-triggering and edge oscillation: the pet stays clear while
    /// the cursor hovers near its home and returns the moment the cursor
    /// withdraws.
    fn update_avoid(&mut self, dt: u64) {
        let avoid = self.cfg.avoid.clone();
        let (x, y, w, h) = self.window_size();
        let (cx, cy) = window::cursor_pos();
        let (fx, fy, fw, fh) = self.pet_visual_rect();
        // The pet's visible-body rect anchored at any window position.
        let anchor = |ax: i32, ay: i32| (ax + fx, ay + fy, fw.max(1), fh.max(1));
        let trigger = avoid.distance.max(1.0);
        // 收起/恢复滑动期间回避不参与(否则"悬停叫回"会被回避搅乱);
        // 恢复完成后先挂起,等光标首次离开宠物再武装(避免立刻被躲开)。
        if self.avoid_arm_after_leave {
            let home = self.avoid_home.unwrap_or((x, y));
            let (hrx, hry, hw, hh) = anchor(home.0, home.1);
            let hdist = point_rect_dist(
                cx as f32, cy as f32, hrx as f32, hry as f32, hw as f32, hh as f32,
            );
            if hdist > trigger * avoid.hysteresis.max(1.0) {
                self.avoid_arm_after_leave = false;
            }
        }
        // 收起/滑动中,或光标尚未离开(恢复后的挂起期):回避不参与
        let active = avoid.enabled
            && !self.hidden
            && !self.dragging
            && !self.collapsed
            && self.collapse_anim.is_none()
            && !self.avoid_arm_after_leave;

        // Disabled (tray toggle / hidden / being dragged): stop dodging and
        // glide anything still displaced back to its remembered home.
        if !active {
            if let Some(home) = self.avoid_home {
                self.avoid_offscreen = false;
                let (goal_x, goal_y) = self.clamp_inside_screen(home.0, home.1, h);
                let ((nx, ny), done) = move_toward((x, y), (goal_x, goal_y), avoid.return_speed, dt);
                if done {
                    self.avoid_home = None;
                }
                if nx != x || ny != y {
                    window::set_window_rect(self.hwnd, nx, ny, w, h);
                }
            }
            return;
        }

        let home = self.avoid_home.unwrap_or((x, y));
        let (hrx, hry, hw, hh) = anchor(home.0, home.1);
        let hdist = point_rect_dist(
            cx as f32,
            cy as f32,
            hrx as f32,
            hry as f32,
            hw as f32,
            hh as f32,
        );

        let should_dodge = hdist < trigger;
        if should_dodge && !self.avoid_offscreen {
            // cursor entered the pet's home area: remember home, start dodging
            if self.avoid_home.is_none() {
                self.avoid_home = Some((x, y));
            }
            self.avoid_offscreen = true;
            log_line(&format!("[avoid] dodge start (cursor {}px from pet)", hdist.round() as i32));
        } else if self.avoid_offscreen && hdist > trigger * avoid.hysteresis.max(1.0) {
            // cursor left the area (with hysteresis): begin the glide home
            self.avoid_offscreen = false;
            log_line("[avoid] cursor left home area -> returning");
        }

        let home = self.avoid_home.unwrap_or((x, y));
        if self.avoid_offscreen {
            // Dodge target: home + `shift` px along (home_visual_center ->
            // cursor). Anchoring on home keeps the orbiting point reachable,
            // so the pet settles exactly `shift` away and always returns home.
            let (hrx, hry, hw, hh) = anchor(home.0, home.1);
            let hcx = hrx as f32 + hw as f32 / 2.0;
            let hcy = hry as f32 + hh as f32 / 2.0;
            let (mut vx, mut vy) = (hcx - cx as f32, hcy - cy as f32);
            let len = (vx * vx + vy * vy).sqrt();
            if len < 1.0 {
                vx = -1.0;
                vy = -1.0; // cursor dead-center: pick up-left
            } else {
                vx /= len;
                vy /= len;
            }
            let goal_x = home.0 as f32 + vx * avoid.shift;
            let goal_y = home.1 as f32 + vy * avoid.shift;
            let (goal_x, goal_y) = self.clamp_inside_screen(goal_x as i32, goal_y as i32, h);
            let ((nx, ny), _) = move_toward((x, y), (goal_x, goal_y), avoid.dodge_speed, dt);
            if nx != x || ny != y {
                window::set_window_rect(self.hwnd, nx, ny, w, h);
            }
        } else if let Some(home) = self.avoid_home {
            // Returning: glide back to the recorded home at return speed.
            let (goal_x, goal_y) = self.clamp_inside_screen(home.0, home.1, h);
            let ((nx, ny), done) = move_toward((x, y), (goal_x, goal_y), avoid.return_speed, dt);
            if done {
                self.avoid_home = None;
                log_line("[avoid] back home");
            }
            if nx != x || ny != y {
                window::set_window_rect(self.hwnd, nx, ny, w, h);
            }
        }
    }

    /// 宠物所在显示器的工作区底边(底部任务栏时=任务栏顶边),自动收起的
    /// 身体裁剪用。任务栏在其它边缘时退化为整屏高度(不裁剪)。
    fn work_area_bottom(&self) -> i32 {
        unsafe {
            let mon = MonitorFromWindow(self.hwnd, MONITOR_DEFAULTTONEAREST);
            let mut mi = MONITORINFO {
                cbSize: std::mem::size_of::<MONITORINFO>() as u32,
                ..Default::default()
            };
            if GetMonitorInfoW(mon, &mut mi).as_bool() {
                mi.rcWork.bottom
            } else {
                screen_size().1
            }
        }
    }

    /// 自动收起(auto-hide)状态机(见 config.auto_hide):
    /// - idle/offline 持续 `after_sec` 秒 → 收起:从原位**向下滑动**到
    ///   y = 屏幕高度 − y_factor × 窗口高度(se.pxl `slide_speed` 速度,保持置顶,
    ///   头悬浮在所有窗口之上),到达后身体部分裁剪留透明(任务栏从透明区
    ///   透出)、透明度再乘 `opacity`、鼠标点击穿透(WS_EX_TRANSPARENT)。
    /// - 退出:状态离开 idle/offline(新消息)或鼠标悬停超过 `hover_sec` 秒
    ///   → **滑回原位**。
    fn update_collapse(&mut self, dt: u64) {
        let ah = self.cfg.auto_hide.clone();
        // 滑动动画优先推进(收起/恢复共用;逐帧 move_toward,位置平滑)
        if let Some((gx, gy)) = self.collapse_anim {
            let (x, y, w, h) = self.window_size();
            let speed = if self.collapsed { ah.slide_speed } else { ah.slide_speed.max(1.0) };
            let ((nx, ny), done) = move_toward((x, y), (gx, gy), speed.max(1.0), dt);
            if nx != x || ny != y {
                window::set_window_rect(self.hwnd, nx, ny, w, h);
            }
            if done {
                self.collapse_anim = None;
                if self.collapsed {
                    // 到达收起位:启用鼠标穿透
                    window::set_click_through(self.hwnd, true);
                    self.dirty_request = true;
                }
            }
        }
        if self.dragging || self.hidden {
            return; // 拖拽/全屏隐藏期间不参与
        }
        let resting = matches!(self.base_mode, Mode::Idle | Mode::Offline);
        if self.collapsed {
            if !resting || !ah.enabled {
                // 新消息/新活动(或关闭开关):滑回原位
                self.exit_collapse();
                return;
            }
            if self.collapse_anim.is_some() {
                return; // 滑动中:等到达后再做悬停/裁剪
            }
            // 计算裁剪:被任务栏盖住的身体部分留透明(任务栏从透明区透出),
            // 头部保持置顶悬浮在所有窗口之上
            let (_, y, _, h) = self.window_size();
            let wb = self.work_area_bottom();
            self.collapse_clip = (h - (wb - y)).clamp(0, h);
            // 悬停检测:点击穿透后收不到鼠标消息,用光标位置轮询。
            // 只认"可见部分":裁剪线以上 ∩ 本体可见框——被任务栏挡住
            // 的身体区域不应触发唤回(否则划过任务栏就把宠物拽出来)。
            let (cx, cy) = window::cursor_pos();
            let (wx, wy, ww, wh) = self.window_size();
            let vis_bottom = wy + (wh - self.collapse_clip).max(0); // 裁剪线的屏幕 y
            let off = self.pet_offset_x(); // 气泡让位:本体右移,bbox 起点随之右移
            let (hx0, hy0, hx1, hy1) = match self.avoid_box {
                // 本体可见框(window-local)∩ 裁剪可见区
                Some((bx, by, bw, bh)) => {
                    let top = wy + by;
                    let bottom = (wy + by + bh).min(vis_bottom);
                    (wx + bx + off, top, wx + bx + off + bw, bottom.max(top))
                }
                None => (wx, wy, wx + ww, vis_bottom),
            };
            let over = cx >= hx0 && cx < hx1 && cy >= hy0 && cy < hy1;            if over {
                self.collapse_hover_since.get_or_insert(now_ms());
            } else {
                self.collapse_hover_since = None;
            }
            if self
                .collapse_hover_since
                .map(|t| now_ms().saturating_sub(t) >= ah.hover_sec.saturating_mul(1000))
                .unwrap_or(false)
            {
                self.exit_collapse();
            }
            return;
        }
        if !ah.enabled || !resting {
            self.collapse_idle_since = None;
            return;
        }
        let since = *self.collapse_idle_since.get_or_insert(Instant::now());
        if since.elapsed().as_secs() >= ah.after_sec {
            self.enter_collapse();
        }
    }

    fn enter_collapse(&mut self) {
        let (x, y, _w, h) = self.window_size();
        self.collapse_home = Some((x, y));
        self.collapsed = true;
        self.collapse_hover_since = None;
        self.collapse_idle_since = None;
        // 目标:任务栏区域。保持置顶(头部悬浮在所有窗口之上),身体部分
        // 到达后通过 draw 裁剪(见 collapse_clip)留透明;先滑动后穿透。
        // 锚点用窗口所在显示器的工作区底(=任务栏顶),而非主屏高度——
        // 多屏不同分辨率/DPI 时不会错位;任务栏高度变化由 update_collapse
        // 里每帧重查的 work_area_bottom 兜底。
        let yf = self.cfg.auto_hide.y_factor.clamp(0.05, 1.0);
        let wb = self.work_area_bottom();
        let ty = (wb as f32 - yf * h as f32).round() as i32;
        self.collapse_anim = Some((x, ty));
        // 收起期间不参与回避模式(位置/穿透冲突)
        self.avoid_offscreen = false;
        self.avoid_home = None;
        self.avoid_arm_after_leave = false;
        self.dirty_request = true; // 透明度过渡需要重绘
        log_line(&format!("[auto-hide] sliding down to y={ty}"));
    }

    fn exit_collapse(&mut self) {
        if !self.collapsed {
            return;
        }
        self.collapsed = false;
        self.collapse_hover_since = None;
        self.collapse_clip = 0;
        // 收起时的点击穿透恢复为"配置要求的状态":鼠标穿透开关开启时
        // 保持穿透,否则解除(原来无条件解除会破坏「鼠标穿透」设置)
        window::set_click_through(self.hwnd, self.cfg.click_through.enabled);
        if let Some((hx, hy)) = self.collapse_home.take() {
            // 滑回原位(从当前位置开始)
            self.collapse_anim = Some((hx, hy));
            log_line("[auto-hide] sliding home");
        }
        // 重新计时,避免刚恢复又立刻收起;
        // 回避模式挂起,等光标首次离开宠物再武装(否则悬停触发恢复后立刻被躲开)
        self.collapse_idle_since = Some(Instant::now());
        self.avoid_arm_after_leave = self.cfg.avoid.enabled;
        self.dirty_request = true;
    }

    /// 托盘「自动收起」勾选切换,即时生效并写回 config.json。
    fn toggle_auto_hide(&mut self) {
        self.cfg.auto_hide.enabled = !self.cfg.auto_hide.enabled;
        let _ = self.cfg.save(&self.exe_dir().join("config.json"));
        log_line(&format!("[auto-hide] tray toggle -> {}", self.cfg.auto_hide.enabled));
        if !self.cfg.auto_hide.enabled && self.collapsed {
            self.exit_collapse();
        } else if self.cfg.auto_hide.enabled {
            // 从头计时,避免用旧的 idle 时长立即触发
            self.collapse_idle_since = Some(Instant::now());
        }
    }

    /// 托盘「开机自启」勾选切换:写回 config.json 并同步 HKCU Run 注册表项。
    fn toggle_autostart(&mut self) {
        self.cfg.autostart = !self.cfg.autostart;
        let _ = self.cfg.save(&self.exe_dir().join("config.json"));
        let exe_path =
            std::env::current_exe().unwrap_or_else(|_| self.exe_dir().join("hannis.exe"));
        apply_autostart(&self.cfg, &exe_path);
        log_line(&format!("[autostart] tray toggle -> {}", self.cfg.autostart));
    }

    /// 托盘「鼠标穿透」勾选切换:写回 config.json 并即时生效。开启后窗口
    /// 加 WS_EX_TRANSPARENT,宠物不再拦截鼠标(也无法拖拽),点击/滚轮全部
    /// 落到下层界面;光标悬浮在宠物上时不透明度压到 hover_opacity(默认
    /// 0.1 = 透明度 90%),本体与气泡一起透视下层(见 update_fade)。重启保持。
    fn toggle_click_through(&mut self) {
        self.cfg.click_through.enabled = !self.cfg.click_through.enabled;
        let _ = self.cfg.save(&self.exe_dir().join("config.json"));
        // 收起状态本身依赖点击穿透:此时关掉开关也保持穿透,等退出收起时
        // 由 exit_collapse 按配置恢复
        let effective = self.cfg.click_through.enabled || self.collapsed;
        window::set_click_through(self.hwnd, effective);
        log_line(&format!("[click-through] tray toggle -> {}", self.cfg.click_through.enabled));
    }

    /// 托盘「提示音」开关:写回 config.json,立即生效(下一拍按新状态播)。
    fn toggle_sound(&mut self) {
        self.cfg.sound.enabled = !self.cfg.sound.enabled;
        let _ = self.cfg.save(&self.exe_dir().join("config.json"));
        log_line(&format!("[sound] tray toggle -> {}", self.cfg.sound.enabled));
    }

    /// Re-scan the loaded animation's first frame for the bbox of non-
    /// transparent pixels and cache it in `avoid_box` (window-local coords; the
    /// sprite is drawn at (0, 0) in `compose`). One frame scan per
    /// animation load — ~sub-ms for an 800px canvas, and nothing per tick.
    /// Called whenever `self.anim` changes.
    fn refresh_avoid_box(&mut self) {
        self.avoid_box = self.anim.as_ref().and_then(|a| {
            a.frame(0)
                .alpha_bbox()
                .map(|(bx, by, bw, bh)| (bx, by, bw, bh))
        });
    }

    /// 气泡在左侧让出的空间(px):气泡右缘按 动图宽×DPI 的固定百分比
    /// (bubble::BUBBLE_RIGHT_FRACTION)定位;当气泡左缘越过本体左缘时,
    /// 本体向右偏移该距离、窗口相应向左加宽,本体屏幕位置保持不变。
    fn pet_offset_x(&self) -> i32 {
        bubble_overhang_px(self.pet_size().0, self.comp.dpi_scale())
    }

    /// Non-transparent pet rect in window-local coords (x_off, y_off, w, h).
    /// Falls back to the full window rect until an animation bbox is scanned.
    fn pet_visual_rect(&self) -> (i32, i32, i32, i32) {
        let off = self.pet_offset_x();
        if let Some((bx, by, bw, bh)) = self.avoid_box {
            (bx + off, by, bw, bh)
        } else {
            let (_, _, w, h) = self.window_size();
            (off, 0, w - off, h)
        }
    }

    /// Keep a window position reachable on the virtual screen so a dodge/
    /// return can never park the pet somewhere unreachable (same clamp as
    /// dragging). 本体大部分可拖出屏外,只要求至少保留 DRAG_SLIVER_PX 可见;
    /// 横向按本体的可见盒(pet_drag_x_bounds),窗口留白允许悬出屏外。
    fn clamp_inside_screen(&self, x: i32, y: i32, h: i32) -> (i32, i32) {
        let (lo, hi) = pet_drag_x_bounds(self.pet_size().0, self.pet_offset_x());
        let nx = x.clamp(lo, hi);
        let (lo, hi) = pet_drag_y_bounds(h);
        let ny = y.clamp(lo.min(hi), lo.max(hi));
        (nx, ny)
    }

    fn toggle_avoid(&mut self) {
        self.cfg.avoid.enabled = !self.cfg.avoid.enabled;
        let _ = self.cfg.save(&self.exe_dir().join("config.json"));
        log_line(&format!("[avoid] tray toggle -> {}", self.cfg.avoid.enabled));
        if !self.cfg.avoid.enabled {
            // stop dodging; a displaced pet eases back home on the next tick
            self.avoid_offscreen = false;
        }
    }

    /// 清空多源堆叠状态(模式离开 Working/Thinking、堆叠开关关闭时)。
    /// 下个 tick 由对应路径重建;卡池清空即恢复单源气泡显示。
    fn clear_stack(&mut self) {
        if self.stack_state.is_empty() && self.stack_pool.is_empty() && self.stack_cache.is_empty()
        {
            return;
        }
        self.stack_state.clear();
        self.stack_pool.clear();
        self.stack_cache.clear();
        self.dirty_request = true;
    }

    /// 多源堆叠气泡(Working/Thinking,bubble.stack 开启):
    /// - 轮流显示跨所有源(source=None 的 rotate_pick 选前排会话),前排卡
    ///   驻留逻辑与单源一致:内容静默 ≥ bubble.rotate_ms 才交给下一个候选;
    ///   所有源都在活跃输出时前排卡不动。
    /// - stack_cards 判定每源出卡(出现/消失滞后)与顺序(非前排按注册序、
    ///   前排固定末位、上限 4 张)。
    /// - 每卡内容 bubble_text_pinned(Some(src), …):前排卡带打字机 reveal,
    ///   非前排卡 lines 清空(只画头部);标题按源纠正(全局 mode 只反映
    ///   最高优先级)。
    /// - 卡片 layout 进池(stack_pool),文本与几何都没变时零开销;有变化
    ///   置 dirty。旧的单源 bubble widget 置空。
    fn update_stack_bubble(
        &mut self,
        snap: &Snapshot,
        effective: Mode,
        tick_now: u64,
        dt: u64,
        type_cps: u32,
        max_chars: usize,
    ) {
        // 前排卡驻留:跨源的 rotate_pick + 内容变更重置静默计时
        let stale = self
            .bubble_stale_since
            .map(|at| tick_now.saturating_sub(at) >= self.cfg.bubble.rotate_ms.max(1))
            .unwrap_or(false);
        let pick = bubble_text::rotate_pick(snap, None, effective, self.bubble_pick.as_deref(), stale);
        if pick != self.bubble_pick {
            self.bubble_pick = pick.clone();
            self.bubble_stale_since = Some(tick_now);
        }
        let prefer = self.bubble_pick.as_deref();
        // 前排会话所属源(前排卡固定显示它的流式内容)
        let front_src: Option<dshpet::state::Source> = prefer.and_then(|sid| {
            snap.working
                .iter()
                .chain(&snap.thinking)
                .find(|s| s.session_id == sid)
                .map(|s| s.source)
        });
        // 打字机 reveal 只推进前排卡(前排会话不存在时不追踪任何流)
        let pos = if type_cps > 0 {
            let stream = front_src.and_then(|_| bubble_text::live_stream_pinned(snap, front_src, effective, prefer));
            let same = match (&self.reveal, &stream) {
                (Some((sid, kind, _)), Some(s)) => *sid == s.session_id && *kind == s.kind,
                (None, None) => true,
                _ => false,
            };
            if !same {
                self.reveal = stream.as_ref().map(|s| (s.session_id.clone(), s.kind, 0.0));
            }
            match (&mut self.reveal, &stream) {
                (Some((_, _, pos)), Some(s)) => {
                    *pos += type_cps as f32 * dt as f32 / 1000.0;
                    let p = (*pos as usize).min(s.len);
                    if s.len.saturating_sub(p) > TYPE_LAG_CHARS {
                        *pos = (s.len - TYPE_LAG_CHARS) as f32;
                    }
                    Some(p)
                }
                _ => None,
            }
        } else {
            self.reveal = None;
            None
        };
        // 前排卡文字(带 prefer 固定到轮换会话);无前排 = 空
        let front_text = front_src.map(|src| {
            let mut t = bubble_text::bubble_text_pinned(snap, Some(src), prefer, pos, max_chars);
            bubble_stack::fix_card_title(&mut t, snap, src);
            t
        });
        let new_text = front_text.clone().unwrap_or_default();
        if new_text != self.bubble_text {
            // a changed message resets the staleness timer
            self.bubble_stale_since = Some(tick_now);
            self.bubble_text = new_text;
            self.dirty_request = true;
        }
        // 出卡判定(滞后/排序/截断)+ 卡池同步
        let cards = bubble_stack::stack_cards(snap, prefer, tick_now, &mut self.stack_state);
        let keep: BTreeSet<u16> = cards.iter().map(|c| c.id).collect();
        let pool_before: BTreeSet<u16> = self.stack_pool.keys().copied().collect();
        self.stack_pool.retain(|id, _| keep.contains(id));
        if pool_before != keep {
            // 卡片消失(滞留期到)/新增:compose 的级联集合变了,必须重绘
            self.dirty_request = true;
        }
        // 每卡按当前 pet 尺寸重排;级联占位(下层卡向右下偏移)会压缩可用
        // 高度,按卡数预留,保证前排卡下缘不越过本体(窗口不长大)
        let pet_w = self.pet_size().0;
        let pet_h = self.pet_size().1;
        let dpi = self.comp.dpi_scale();
        let reserve = (cards.len().saturating_sub(1)) as u32 * bubble::scaled(bubble::STACK_OFFSET_Y, dpi);
        let pet_h_stack = pet_h.saturating_sub(reserve);
        let theme = self.bubble.theme.clone();
        for card in &cards {
            let entry = self.stack_pool.entry(card.id).or_insert_with(|| {
                (bubble::Bubble { theme: theme.clone(), ..Default::default() }, tick_now)
            });
            let mut t = if card.front {
                front_text.clone().unwrap_or_default()
            } else {
                let mut t = bubble_text::bubble_text_pinned(
                    snap,
                    Some(dshpet::state::Source::Script(card.id)),
                    None,
                    None,
                    max_chars,
                );
                t.lines.clear(); // 非前排卡只画头部(From 药丸 + 状态标题)
                t
            };
            bubble_stack::fix_card_title(&mut t, snap, dshpet::state::Source::Script(card.id));
            if entry.0.layout(t, pet_w, pet_h_stack, &self.comp) {
                self.dirty_request = true;
            }
        }
        self.stack_cache = cards;
        // 每卡 200ms 淡入期间保持重绘(淡入走 appear 系数,若无内容变化置
        // dirty,淡入会停在第一帧)
        if self
            .stack_pool
            .values()
            .any(|(_, appeared)| tick_now.saturating_sub(*appeared) < 200)
        {
            self.dirty_request = true;
        }
        // 旧的单源 bubble widget 置空(堆叠期由卡池接管 compose 绘制)
        let cleared = self.bubble.layout(bubble_text::BubbleText::default(), 0, 0, &self.comp);
        if cleared {
            self.dirty_request = true;
        }
    }

    /// 托盘「多源堆叠显示」勾选切换:写回 config.json,清空堆叠状态,
    /// 下个 tick 按新开关走对应路径,立即生效,重启保持。
    fn toggle_stack(&mut self) {
        self.cfg.bubble.stack = !self.cfg.bubble.stack;
        let _ = self.cfg.save(&self.exe_dir().join("config.json"));
        self.clear_stack();
        self.dirty_request = true;
        log_line(&format!("[bubble] stack toggle -> {}", self.cfg.bubble.stack));
    }

    fn compose(&mut self) {
        let compose_t0 = std::time::Instant::now();
        let (pet_w, pet_h) = self.pet_size();
        // 自动收起:身体被任务栏盖住的部分留透明(任务栏从透明区透出)
        self.comp.clip_bottom = if self.collapsed { self.collapse_clip } else { 0 };
        // 亚克力截屏需要窗口屏幕原点(compose 每次更新)
        let (wx, wy, _, _) = self.window_size();
        self.comp.set_screen_pos(wx, wy);
        // The window is the pet plus gutters: WINDOW_EXTRA_W of room on the
        // right, and — when the pet is small enough that the bubble would
        // reach past its left edge — the same amount of room reserved on the
        // LEFT (the pet is offset right by that much). The bubble's right
        // edge is anchored at a fixed FRACTION of the pet width (see
        // BUBBLE_RIGHT_FRACTION), so the "pet body occludes the bubble's
        // right sliver" look holds at every display scale; without the left
        // gutter a small pet would sit fully on top of the bubble.
        let dpi = self.comp.dpi_scale();
        // 气泡左缘(相对本体):右缘 = 动图宽×DPI × 百分比,减掉气泡宽。
        let bx_rel = bubble_left_rel(pet_w, dpi);
        let overhang = (-bx_rel).max(0.0).round() as u32;
        let pet_x = overhang as i32;
        let win_w = pet_w + WINDOW_EXTRA_W + overhang;
        let win_h = pet_h;
        let (old_x, old_y, old_w, old_h) = self.window_size();
        if win_w as i32 != old_w || win_h as i32 != old_h {
            // anchor the pet BOTTOM-RIGHT: keep both the right edge and the
            // bottom edge fixed so the pet doesn't drift on screen when the
            // window size changes - the left/top may move to accommodate
            let new_x = old_x + old_w - win_w as i32;
            let new_y = (old_y + old_h - win_h as i32).max(0);
            self.comp.resize(win_w, win_h);
            window::set_window_rect(self.hwnd, new_x, new_y, win_w as i32, win_h as i32);
        } else {
            self.comp.resize(win_w, win_h);
        }

        self.comp.clear();
        // 本体锚定在窗口左缘 + 气泡让位偏移(小 scale 时气泡伸到左侧);
        // 窗口右侧保留 EXTRA 留白。
        let pet_y = 0i32;
        let alpha = self.fade_alpha * self.cfg.opacity_for(&self.mode);

        // the phone bubble is composited BEFORE the pet sprite, so the
        // enlarged bubble may be partially occluded by the body — the pet
        // naturally covers whatever overlaps it (可以被本体遮挡一部分)。
        // 多源堆叠:stack_cache(后→前,末位 = 前排卡)依次级联绘制,第 i 张
        // 卡偏移 (i×STACK_OFFSET_X, i×STACK_OFFSET_Y);只有一张卡时与单源
        // 现状完全一致(同位置、同内容、show_body=true)。
        let bx = pet_x + bx_rel.round() as i32 + ((1.0 - self.bubble_fade) * 8.0) as i32; // 从宠物方向滑入
        let by = bubble::scaled(bubble::BUBBLE_MARGIN_Y, dpi) as i32;
        // 鼠标穿透悬浮:气泡(单源/堆叠)随本体一起压到 hover_opacity
        let dim = self.bubble_hover_dim.clamp(0.05, 1.0);
        if self.cfg.bubble.stack && !self.stack_cache.is_empty() {
            let ox = bubble::scaled(bubble::STACK_OFFSET_X, dpi) as i32;
            let oy = bubble::scaled(bubble::STACK_OFFSET_Y, dpi) as i32;
            let now = now_ms();
            let n = self.stack_cache.len();
            for (i, card) in self.stack_cache.iter().enumerate() {
                let Some((b, appeared)) = self.stack_pool.get(&card.id) else { continue };
                // 每卡出现时 200ms 淡入,再乘悬浮变暗系数
                let appear = ((now.saturating_sub(*appeared) as f32 / 200.0).clamp(0.0, 1.0) * dim).clamp(0.0, 1.0);
                let m = if card.working { Mode::Working } else { Mode::Thinking };
                b.draw(&mut self.comp, bx + i as i32 * ox, by + i as i32 * oy, m, appear, i + 1 == n);
            }
        } else if self.bubble.visible() {
            self.bubble.draw(&mut self.comp, bx, by, self.mode, self.bubble_fade * dim, true);
        }

        // 本体方向光投影(演示方案 3):偏移 (3,3)px、模糊 12px、α 40%
        // (按 DPI 缩放);层序:气泡 → 投影 → 本体,随本体一起淡入淡出。
        if let Some(a) = &self.anim {
            let idx = self.player.as_ref().map(|p| p.idx).unwrap_or(0);
            let s = self.comp.dpi_scale();
            let sdx = (3.0 * s).round() as i32;
            let sdy = (3.0 * s).round() as i32;
            let blur = ((12.0 * s).round() as u32).max(3);
            self.comp.draw_frame_shadow(a.frame(idx), pet_x, pet_y, sdx, sdy, blur, 102, alpha);
        }

        if self.mode == Mode::Offline {
            // static grayscale idle frame once loaded; until then keep the
            // previous animation on screen (grayscale) so nothing vanishes
            if let Some(a) = &self.anim {
                if a.name == "idle" {
                    self.comp.draw_frame(a.frame(0), pet_x, pet_y, alpha, true);
                } else {
                    let idx = self.player.as_ref().map(|p| p.idx).unwrap_or(0);
                    self.comp.draw_frame(a.frame(idx), pet_x, pet_y, alpha, true);
                }
            }
        } else if let Some(a) = &self.anim {
            let idx = self.player.as_ref().map(|p| p.idx).unwrap_or(0);
            if a.name == self.mode.asset() {
                if let Some(from) = &self.cf_from {
                    let t = self.cf_t.min(1.0);
                    let (fw, fh) = (from.w as i32, from.h as i32);
                    let fx = pet_x + (pet_w as i32 - fw) / 2;
                    let fy = pet_y + (pet_h as i32 - fh) / 2;
                    self.comp.draw_frame(from, fx, fy, alpha * (1.0 - t), false);
                    self.comp.draw_frame(a.frame(idx), pet_x, pet_y, alpha * t, false);
                    if t >= 1.0 {
                        self.cf_from = None;
                    }
                } else {
                    self.comp.draw_frame(a.frame(idx), pet_x, pet_y, alpha, false);
                }
            } else {
                // transitional: new animation still loading, keep old frame
                self.comp.draw_frame(a.frame(idx), pet_x, pet_y, alpha, false);
            }
        }
        self.comp.present();
        // 耗时打点(限频 5s 一行):实测 1/2/4 卡流式帧耗时,验证堆叠的
        // 缓存放大器(LRU 文字块 + 阴影几何缓存)是否达标
        {
            static LAST_LOG: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let now = now_ms();
            let last = LAST_LOG.load(Ordering::Relaxed);
            if now.saturating_sub(last) >= 5000
                && LAST_LOG
                    .compare_exchange(last, now, Ordering::Relaxed, Ordering::Relaxed)
                    .is_ok()
            {
                let ms = compose_t0.elapsed().as_secs_f32() * 1000.0;
                log_line(&format!(
                    "[compose] {ms:.1} ms cards={} stack={}",
                    self.stack_cache.len(),
                    self.cfg.bubble.stack
                ));
            }
        }
    }

    /// Dragging is always allowed (reposition the pet anytime); the MOVE
    /// animation only plays while dragging a resting (idle) pet — a busy pet
    /// just slides with its current state animation untouched.
    fn effective_mode(&self) -> Mode {
        if self.dragging && self.base_mode == Mode::Idle {
            Mode::Move
        } else {
            self.base_mode
        }
    }

    pub fn on_lbutton_down(&mut self) {
        // 若在收起(或滑动中),用户想拖走宠物:取消收起并清掉滑动目标
        if self.collapsed {
            self.exit_collapse();
            self.collapse_anim = None;
        }
        self.dragging = true;
        if self.cfg.auto_hide.enabled {
            self.collapse_idle_since = Some(Instant::now());
        }
        // the user grabbed the pet: cancel any avoidance; the spot it ends up
        // at after the drag becomes its new implicit home
        self.avoid_offscreen = false;
        self.avoid_home = None;
        self.last_interaction = Instant::now();
        log_line("[gui] lbutton down (drag start)");
        let (x, y, _, _) = self.window_size();
        self.drag_win = (x, y);
        self.drag_cursor = window::cursor_pos();
        unsafe {
            let _ = SetCapture(self.hwnd);
        }
    }

    pub fn on_mouse_move(&mut self) {
        self.last_interaction = Instant::now();
        // 鼠标在宠物上活动 = 不是空闲,重新计自动收起的 idle 时长
        if self.cfg.auto_hide.enabled && !self.collapsed {
            self.collapse_idle_since = Some(Instant::now());
        }
        if self.dragging {
            let (cx, cy) = window::cursor_pos();
            let nx0 = self.drag_win.0 + (cx - self.drag_cursor.0);
            let ny0 = self.drag_win.1 + (cy - self.drag_cursor.1);
            let (_, _, w, h) = self.window_size();
            // 允许把宠物大部分拖出屏幕,但保留 DRAG_SLIVER_PX 可见的安全底线
            // (坐标跳变/多显示器切换也不会让窗口完全离屏——完全离屏会丢
            // 窗口且残留实例会挡住重启;留一条边就能随时再抓回来)。
            let (nx, ny) = self.clamp_inside_screen(nx0, ny0, h);
            window::set_window_rect(self.hwnd, nx, ny, w, h);
        }
    }

    pub fn on_lbutton_up(&mut self) {
        self.last_interaction = Instant::now();
        if self.dragging {
            self.dragging = false;
            log_line("[gui] lbutton up (drag end)");
            // remember the spot the pet was dropped at (config window_pos),
            // so the next launch restores it instead of the default corner
            let (x, y, _, _) = self.window_size();
            if self.cfg.window_pos.x != Some(x) || self.cfg.window_pos.y != Some(y) {
                self.cfg.window_pos.x = Some(x);
                self.cfg.window_pos.y = Some(y);
                let _ = self.cfg.save(&self.exe_dir().join("config.json"));
                log_line(&format!("[gui] saved window pos ({x}, {y})"));
            }
            unsafe {
                let _ = ReleaseCapture();
            }
        }
    }

    pub fn show_tray_menu(&mut self) {
        if let Some(t) = &self.tray {
            let scripts = self.script_menu_entries();
            if let Some(cmd) = t.show_menu(
                self.cfg.avoid.enabled,
                self.cfg.auto_hide.enabled,
                self.cfg.autostart,
                self.cfg.bubble.stack,
                self.cfg.click_through.enabled,
                self.cfg.sound.enabled,
                &scripts,
            ) {
                match cmd {
                    tray::MENU_QUIT => unsafe {
                        PostQuitMessage(0);
                    },
                    tray::MENU_AVOID_TOGGLE => self.toggle_avoid(),
                    tray::MENU_AUTOHIDE_TOGGLE => self.toggle_auto_hide(),
                    tray::MENU_AUTOSTART_TOGGLE => self.toggle_autostart(),
                    tray::MENU_STACK_TOGGLE => self.toggle_stack(),
                    tray::MENU_CLICKTHROUGH_TOGGLE => self.toggle_click_through(),
                    tray::MENU_SOUND_TOGGLE => self.toggle_sound(),
                    tray::MENU_ENDPOINTS => settings::open_settings(self),
                    c if c >= tray::MENU_SCRIPT_BASE => self.toggle_script(c - tray::MENU_SCRIPT_BASE),
                    _ => {}
                }
            }
        }
    }

    /// 托盘"接入口"子菜单项:(启用?, 显示名)。
    fn script_menu_entries(&self) -> Vec<(bool, String)> {
        self.cfg
            .scripts
            .iter()
            .enumerate()
            .map(|(i, s)| {
                let label = if s.name.is_empty() { format!("脚本 {}", i + 1) } else { s.name.clone() };
                (s.enabled, label)
            })
            .collect()
    }

    /// 切换接入口启停:写回 config.json,停旧线程,启用时立即重启。
    fn toggle_script(&mut self, i: usize) {
        let Some(sc) = self.cfg.scripts.get(i).cloned() else { return };
        let on = !sc.enabled;
        self.cfg.scripts[i].enabled = on;
        let _ = self.cfg.save(&self.exe_dir().join("config.json"));
        if on {
            spawn_script(i, &sc, &self.tx, &self.exe_dir());
            log_line(&format!("[lua] scripts[{i}] enabled -> respawned"));
        } else if let Some(t) = script_stops().lock().unwrap().get(&(i as u16)) {
            t.store(true, Ordering::Relaxed);
            log_line(&format!("[lua] scripts[{i}] disabled"));
        }
    }

    pub fn on_tray(&mut self, wparam: WPARAM, lparam: LPARAM) {
        let _ = wparam;
        let ev = (lparam.0 & 0xFFFF) as u32;
        if ev == WM_RBUTTONUP {
            self.show_tray_menu();
        }
    }

    fn exe_dir(&self) -> PathBuf {
        std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(|d| d.to_path_buf()))
            .unwrap_or_else(|| PathBuf::from("."))
    }
}

/// Best-effort termination of every OTHER Hannis process. Called at startup
/// when the single-instance mutex is held but no "hannis" window exists:
/// the previous instance is a windowless zombie that would otherwise block
/// all relaunches forever (the user reports "the pet vanished while
/// dragging and now the app won't open").
fn kill_stale_instances() {
    unsafe {
        let snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0).unwrap_or(HANDLE::default());
        if snap.is_invalid() {
            return;
        }
        let self_pid = std::process::id();
        let mut entry = PROCESSENTRY32W {
            dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
            ..Default::default()
        };
        let mut killed = false;
        let mut ok = Process32FirstW(snap, &mut entry).is_ok();
        while ok {
            let name: String = entry
                .szExeFile
                .iter()
                .take_while(|&&c| c != 0)
                .map(|&c| c as u8 as char)
                .collect();
            if entry.th32ProcessID != self_pid && name.eq_ignore_ascii_case("hannis.exe") {
                if let Ok(h) = OpenProcess(PROCESS_TERMINATE, false, entry.th32ProcessID) {
                    let _ = TerminateProcess(h, 1);
                    killed = true;
                }
            }
            ok = Process32NextW(snap, &mut entry).is_ok();
        }
        let _ = CloseHandle(snap);
        if killed {
            eprintln!("[gui] killed stale Hannis process");
            log_line("[gui] killed stale Hannis process");
        }
    }
}

/// autostart via HKCU\Software\Microsoft\Windows\CurrentVersion\Run
fn apply_autostart(cfg: &Config, exe_path: &PathBuf) {
    const RUN_KEY: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\Run";
    unsafe {
        let mut hkey = HKEY::default();
        let key_w: Vec<u16> = RUN_KEY.encode_utf16().chain(Some(0)).collect();
        let rc = RegOpenKeyExW(
            HKEY_CURRENT_USER,
            windows::core::PCWSTR(key_w.as_ptr()),
            0,
            KEY_SET_VALUE,
            &mut hkey,
        );
        if rc != ERROR_SUCCESS {
            eprintln!("[gui] autostart: cannot open run key ({rc:?})");
            return;
        }
        let set = |name: &str, value: &Vec<u16>| {
            let name_w: Vec<u16> = name.encode_utf16().chain(Some(0)).collect();
            let mut data: Vec<u8> = value.iter().flat_map(|c| c.to_le_bytes()).collect();
            data.push(0);
            data.push(0); // REG_SZ null terminator
            let _ = RegSetValueExW(
                hkey,
                windows::core::PCWSTR(name_w.as_ptr()),
                0,
                REG_SZ,
                Some(&data),
            );
        };
        if cfg.autostart {
            let mut value: Vec<u16> = "\"".encode_utf16().collect();
            value.extend(exe_path.to_string_lossy().encode_utf16());
            value.extend("\"".encode_utf16());
            set("Hannis", &value);
        } else {
            let name: Vec<u16> = "Hannis".encode_utf16().chain(Some(0)).collect();
            let _ = RegDeleteValueW(hkey, windows::core::PCWSTR(name.as_ptr()));
            // clean up the pre-rename registry value if present
            let old: Vec<u16> = "dshpet".encode_utf16().chain(Some(0)).collect();
            let _ = RegDeleteValueW(hkey, windows::core::PCWSTR(old.as_ptr()));
        }
        let _ = RegCloseKey(hkey);
    }
}
