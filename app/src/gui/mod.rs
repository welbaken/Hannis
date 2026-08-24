//! Windows GUI. Compiled only on Windows; empty module elsewhere.

#![cfg(target_os = "windows")]

pub mod bubble;
pub mod icon;
pub mod render;
pub mod tray;
pub mod window;

use dshpet::anim::{load_animation, load_loop_animation, Animation, Frame, Player};
use dshpet::bubble_text;
use dshpet::config::Config;
use dshpet::connectors::dsh::DshConnector;
use dshpet::connectors::hermes::HermesConnector;
use dshpet::connectors::stop_flag;
use dshpet::state::{Mode, PetState, Snapshot, StateEvent};
pub mod settings;
use dshpet::config::ScriptEntryConfig;
use std::collections::HashMap;
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
    /// unchanged for ROTATE_AFTER_MS is handed to another session with content.
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
    /// 自动收起恢复后:回避模式先挂起,直到光标首次离开宠物(避免"叫回来
    /// 立刻又被躲开/被回避拉扯回不了原位")。
    pub avoid_arm_after_leave: bool,
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
            let existing = unsafe { FindWindowW(w!("hannis"), PCWSTR::null()) }
                .unwrap_or(HWND::default());
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
                unsafe { GetWindowRect(existing, &mut r) };
                let vx = unsafe { GetSystemMetrics(SM_XVIRTUALSCREEN) };
                let vy = unsafe { GetSystemMetrics(SM_YVIRTUALSCREEN) };
                let vw = unsafe { GetSystemMetrics(SM_CXVIRTUALSCREEN) };
                let vh = unsafe { GetSystemMetrics(SM_CYVIRTUALSCREEN) };
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
    DshConnector { url: cfg.dsh_url(), poll_ms: cfg.dsh.poll_ms, history_ms: cfg.dsh.history_ms }.spawn(tx.clone(), stop.clone());
    if let Some(db) = cfg.hermes_db_path() {
        HermesConnector {
            db_path: db,
            poll_ms_active: cfg.hermes.poll_ms_active,
            poll_ms_idle: cfg.hermes.poll_ms_idle,
        }
        .spawn(tx.clone(), stop.clone());
    } else {
        eprintln!("hermes db path unresolvable -> hermes disabled");
    }
    // 用户 Lua 脚本(开放接口):每脚本一线程 + 独立 Lua state
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
        // screen so a monitor layout change can never park it unreachable
        unsafe {
            let vx = GetSystemMetrics(SM_XVIRTUALSCREEN);
            let vy = GetSystemMetrics(SM_YVIRTUALSCREEN);
            let vw = GetSystemMetrics(SM_CXVIRTUALSCREEN);
            let vh = GetSystemMetrics(SM_CYVIRTUALSCREEN);
            let (lo, hi) = (vx - w + 48, vx + vw - 48);
            x = px.clamp(lo.min(hi), lo.max(hi));
            let (lo, hi) = (vy - h + 48, vy + vh - 48);
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
        avoid_arm_after_leave: false,
    });
    app.pet.set_celebrate_ms(celebrate_ms);
    app.bubble.theme = bubble_theme;
    let ptr = &mut *app as *mut App;
    let hwnd = window::create_main_window(ptr, w, h, icon.unwrap_or(HICON::default()));
    app.hwnd = hwnd;
    app.comp = render::Compositor::new(hwnd, font_scale);
    // 上一帧快照只在亚克力需要反混合时保留(省 ~3MB 常驻)
    app.comp.keep_last_frame = acrylic_on;
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
        let fade_target = if self.bubble.visible() { 1.0 } else { 0.0 };
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
            // 轮流显示: keep the current session while its message keeps
            // updating; once it has been static for ROTATE_AFTER_MS and
            // another session has content, hand the bubble over to it.
            let stale = self
                .bubble_stale_since
                .map(|at| tick_now.saturating_sub(at) >= bubble_text::ROTATE_AFTER_MS)
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
                let pet_w = self.pet_size().0;
                let pet_h = self.pet_size().1;
                self.bubble.layout(self.bubble_text.clone(), pet_w, pet_h, &self.comp);
            }
        } else {
            self.reveal = None;
            self.bubble_pick = None;
            self.bubble_stale_since = None;
            let text = bubble_text::bubble_text_pinned(&snap, sel, None, None, max_chars);
            if text != self.bubble_text {
                self.bubble_text = text;
                self.dirty_request = true;
                let pet_w = self.pet_size().0;
                let pet_h = self.pet_size().1;
                self.bubble.layout(self.bubble_text.clone(), pet_w, pet_h, &self.comp);
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
        let mut target = if disabled || idle_for < self.cfg.fade.fade_after_sec {
            1.0
        } else {
            self.cfg.fade.fade_target.clamp(0.0, 1.0)
        };
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
                let (goal_x, goal_y) = self.clamp_inside_screen(home.0, home.1, w, h);
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
            let (goal_x, goal_y) = self.clamp_inside_screen(goal_x as i32, goal_y as i32, w, h);
            let ((nx, ny), _) = move_toward((x, y), (goal_x, goal_y), avoid.dodge_speed, dt);
            if nx != x || ny != y {
                window::set_window_rect(self.hwnd, nx, ny, w, h);
            }
        } else if let Some(home) = self.avoid_home {
            // Returning: glide back to the recorded home at return speed.
            let (goal_x, goal_y) = self.clamp_inside_screen(home.0, home.1, w, h);
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
            let (hx0, hy0, hx1, hy1) = match self.avoid_box {
                // 本体可见框(window-local)∩ 裁剪可见区
                Some((bx, by, bw, bh)) => {
                    let top = wy + by;
                    let bottom = (wy + by + bh).min(vis_bottom);
                    (wx + bx, top, wx + bx + bw, bottom.max(top))
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
        let (x, y, w, h) = self.window_size();
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
        window::set_click_through(self.hwnd, false);
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

    /// Non-transparent pet rect in window-local coords (x_off, y_off, w, h).
    /// Falls back to the full window rect until an animation bbox is scanned.
    fn pet_visual_rect(&self) -> (i32, i32, i32, i32) {
        if let Some(b) = self.avoid_box {
            b
        } else {
            let (_, _, w, h) = self.window_size();
            (0, 0, w, h)
        }
    }

    /// Keep a window position on the virtual screen so a dodge/return can
    /// never park the pet somewhere unreachable (same clamp as dragging).
    fn clamp_inside_screen(&self, x: i32, y: i32, w: i32, h: i32) -> (i32, i32) {
        unsafe {
            let vx = GetSystemMetrics(SM_XVIRTUALSCREEN);
            let vy = GetSystemMetrics(SM_YVIRTUALSCREEN);
            let vw = GetSystemMetrics(SM_CXVIRTUALSCREEN);
            let vh = GetSystemMetrics(SM_CYVIRTUALSCREEN);
            let (lo, hi) = (vx - w + 48, vx + vw - 48);
            let nx = x.clamp(lo.min(hi), lo.max(hi));
            let (lo, hi) = (vy - h + 48, vy + vh - 48);
            let ny = y.clamp(lo.min(hi), lo.max(hi));
            (nx, ny)
        }
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

    fn compose(&mut self) {
        let (pet_w, pet_h) = self.pet_size();
        // 自动收起:身体被任务栏盖住的部分留透明(任务栏从透明区透出)
        self.comp.clip_bottom = if self.collapsed { self.collapse_clip } else { 0 };
        // 亚克力截屏需要窗口屏幕原点(compose 每次更新)
        let (wx, wy, _, _) = self.window_size();
        self.comp.set_screen_pos(wx, wy);
        // The window is the pet plus a horizontal gutter on the left/right
        // so the phone-style bubble at the top-left has room without
        // crowding the character's face. The pet stays centered in the
        // window; the bubble hangs at the top-left.
        let win_w = pet_w + WINDOW_EXTRA_W;
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
        // 本体锚定在窗口左缘(用户确认的位置);窗口右侧保留 EXTRA 留白。
        let pet_x = 0i32;
        let pet_y = 0i32;
        let alpha = self.fade_alpha * self.cfg.opacity_for(&self.mode);

        // the phone bubble is composited BEFORE the pet sprite, so the
        // enlarged bubble may be partially occluded by the body — the pet
        // naturally covers whatever overlaps it (可以被本体遮挡一部分)。
        if self.bubble.visible() {
            let dpi = self.comp.dpi_scale();
            let bx = bubble::scaled(bubble::BUBBLE_MARGIN_X, dpi) as i32
                + ((1.0 - self.bubble_fade) * 8.0) as i32; // 从宠物方向滑入
            let by = bubble::scaled(bubble::BUBBLE_MARGIN_Y, dpi) as i32;
            self.bubble.draw(&mut self.comp, bx, by, self.mode, self.bubble_fade);
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
            // Never let a drag lose the window off the virtual screen (a
            // coordinate leap / edge drag / multi-monitor quirk can park the
            // pet somewhere unreachable; a windowless stale instance would
            // then block all relaunches). Keep at least a sliver visible so
            // the pet can always be grabbed again.
            let (nx, ny) = self.clamp_inside_screen(nx0, ny0, w, h);
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

    pub fn on_zoom(&mut self, wheel_delta: i32) {
        let steps = (wheel_delta / 120) as f32;
        let mut s = self.cfg.display.scale + steps * 0.05;
        s = s.clamp(0.25, 2.0);
        if (s - self.cfg.display.scale).abs() < 1e-4 {
            return;
        }
        self.cfg.display.scale = s;
        let _ = self.cfg.save(&self.exe_dir().join("config.json"));
        self.dirty_request = true;
        // resizing re-anchors the window bottom-right, invalidating any
        // remembered avoid home; the next dodge re-records it
        self.avoid_offscreen = false;
        self.avoid_home = None;
        // reload at the new scale; keep the old frames on screen meanwhile
        let asset = if self.mode == Mode::Offline { "idle" } else { self.mode.asset() };
        self.pending = Some(self.mode);
        self.spawn_load(asset);
    }

    pub fn show_tray_menu(&mut self) {
        if let Some(t) = &self.tray {
            let scripts = self.script_menu_entries();
            if let Some(cmd) = t.show_menu(self.cfg.avoid.enabled, self.cfg.auto_hide.enabled, &scripts) {
                match cmd {
                    tray::MENU_QUIT => unsafe {
                        PostQuitMessage(0);
                    },
                    tray::MENU_AVOID_TOGGLE => self.toggle_avoid(),
                    tray::MENU_AUTOHIDE_TOGGLE => self.toggle_auto_hide(),
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
