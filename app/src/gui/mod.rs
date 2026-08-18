//! Windows GUI. Compiled only on Windows; empty module elsewhere.

#![cfg(target_os = "windows")]

pub mod bubble;
pub mod icon;
pub mod plaintext;
pub mod render;
pub mod tray;
pub mod window;

use dshpet::anim::{load_animation, load_loop_animation, Animation, Frame, Player};
use dshpet::bubble_text;
use dshpet::config::Config;
use dshpet::connectors::comfyui::ComfyUiConnector;
use dshpet::connectors::dsh::DshConnector;
use dshpet::connectors::hermes::HermesConnector;
use dshpet::connectors::stop_flag;
use dshpet::state::{Mode, PetState, Snapshot, StateEvent};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver};
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

pub(crate) fn log_line(msg: &str) {
    if let Ok(mut g) = LOG.lock() {
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
    pub stop: Arc<AtomicBool>,
    pub load_slot: Arc<Mutex<LoadSlot>>,
    pub loading: Option<String>,
    pub anim: Option<Arc<Animation>>,
    pub player: Option<Player>,
    /// Preloaded separate loop animation (`<state>_loop.webp`), if any.
    pub loop_anim: Option<Arc<Animation>>,
    pub loop_switched: bool,
    pub mode: Mode,
    pub base_mode: Mode,
    pub pending: Option<Mode>,
    pub comp: render::Compositor,
    pub bubble: bubble::Bubble,
    pub bubble_lines: Vec<String>,
    /// "Behind the pet" text stream overlay (config `text.mode == "behind"`).
    pub text_overlay: plaintext::TextOverlay,
    /// Typewriter reveal cursor: (session_id, stream kind, chars revealed).
    pub reveal: Option<(String, u8, f32)>,
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
    /// True when a fullscreen app on the same monitor is covering the pet;
    /// the window is hidden via ShowWindow and not interacted with until the
    /// fullscreen app exits.
    pub hidden: bool,
    pub last_fs_check: Instant,
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
    if cfg.comfyui.enabled {
        ComfyUiConnector {
            url: cfg.comfyui.url.clone(),
            poll_ms: cfg.comfyui.poll_ms,
            ws: cfg.comfyui.ws,
        }
        .spawn(tx.clone(), stop.clone());
    }

    let done_ms = cfg.windows.done_sec * 1000;
    let fail_ms = cfg.windows.fail_sec * 1000;
    let celebrate_ms = cfg.windows.celebrate_sec * 1000;
    let font_scale = cfg.bubble.font_scale;
    let scale = cfg.display.scale.clamp(0.25, 2.0);
    let w = ((800.0 + WINDOW_EXTRA_W as f32) * scale) as i32;
    let h = (800.0 * scale) as i32;
    let (sx, sy) = screen_size();
    let x = (sx - w - RIGHT_MARGIN).max(0);
    let y = (sy - h - 80).max(0);

    let mut app = Box::new(App {
        cfg,
        resource_dir,
        pet: PetState::new(done_ms, fail_ms),
        rx,
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
        bubble_lines: Vec::new(),
        text_overlay: plaintext::TextOverlay::default(),
        reveal: None,
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
    });
    app.pet.set_celebrate_ms(celebrate_ms);
    let ptr = &mut *app as *mut App;
    let hwnd = window::create_main_window(ptr, w, h, icon.unwrap_or(HICON::default()));
    app.hwnd = hwnd;
    app.comp = render::Compositor::new(hwnd, font_scale);
    window::set_window_rect(hwnd, x, y, w, h);
    window::show(hwnd);
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
        let use_split = self.cfg.display.use_split.clone();
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
                match load_animation(&dir, &state2, scale, &use_split) {
                    Ok(a) => {
                        let a = Arc::new(a);
                        // preload the separate loop animation while we are at it
                        let loop_anim = load_loop_animation(&dir, &state2, scale, &use_split)
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

        // 6) text: hidden while resting or fully disconnected. Both renderers
        //    use the wide per-line window (text.max_chars): the phone bubble
        //    tail-fits it into its enlarged box in bubble::layout, the
        //    behind-the-pet stream lays it out against the pet box.
        let sel = self.pet.select_bubble_source();
        let type_cps = self.cfg.bubble.type_cps;
        let max_chars = self.cfg.text.max_chars;
        let lines = if matches!(effective, Mode::Idle | Mode::Offline | Mode::Move) {
            Vec::new()
        } else if type_cps > 0 && matches!(effective, Mode::Thinking | Mode::Working) {
            // typewriter: reveal the live stream char by char
            let stream = bubble_text::live_stream(&snap, sel, effective);
            let same = match (&self.reveal, &stream) {
                (Some((sid, kind, _)), Some(s)) => *sid == s.session_id && *kind == s.kind,
                (None, None) => true,
                _ => false,
            };
            if !same {
                // new session / new stream (e.g. next turn): start over
                self.reveal = stream.as_ref().map(|s| (s.session_id.clone(), s.kind, 0.0));
            }
            let pos = match (&mut self.reveal, &stream) {
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
            bubble_text::stream_lines(&snap, sel, pos, max_chars)
        } else {
            self.reveal = None;
            bubble_text::stream_lines(&snap, sel, None, max_chars)
        };
        if lines != self.bubble_lines {
            self.bubble_lines = lines;
            let pet_w = self.pet_size().0;
            let pet_h = self.pet_size().1;
            self.bubble.layout(self.bubble_lines.clone(), pet_w, pet_h, &self.comp);
        }

        // 7) fade
        self.update_fade(dt, &snap);

        // 8) compose + present (skip while hidden to save CPU during long
        //    fullscreen sessions; the DIB keeps the last frame for the
        //    SW_SHOW transition)
        if !self.hidden {
            self.compose();
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
        let target = if disabled || idle_for < self.cfg.fade.fade_after_sec {
            1.0
        } else {
            self.cfg.fade.fade_target.clamp(0.0, 1.0)
        };
        let fade_ms = self.cfg.fade.fade_ms.max(50) as f32;
        let k = ((dt as f32) * 4.0 / fade_ms).min(1.0);
        self.fade_alpha += (target - self.fade_alpha) * k;
        if (self.fade_alpha - target).abs() < 0.005 {
            self.fade_alpha = target;
        }
    }

    fn compose(&mut self) {
        let (pet_w, pet_h) = self.pet_size();
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
        let pet_x = WINDOW_EXTRA_W as i32;
        let pet_y = 0i32;
        let alpha = self.fade_alpha * self.cfg.opacity_for(&self.mode);

        // "behind" mode draws the outlined text FIRST; the phone bubble (the
        // default) is also composited BEFORE the pet sprite, so the enlarged
        // bubble may be partially occluded by the body — the pet naturally
        // covers whatever overlaps it (可以被本体遮挡一部分)。
        let use_behind = self.cfg.text.mode == "behind";
        if use_behind {
            self.text_overlay.layout_if_needed(
                self.bubble_lines.clone(),
                pet_w,
                pet_h,
                pet_x,
                pet_y,
                self.cfg.text.max_lines,
                &self.comp,
            );
            self.text_overlay.draw(&mut self.comp, &self.cfg.text);
        } else {
            // phone bubble at the pet's top-left; drawn under the pet, the
            // right/bottom edge may slip behind the sprite
            if self.bubble.visible() {
                let dpi = self.comp.dpi_scale();
                let bx = bubble::scaled(bubble::BUBBLE_MARGIN_X, dpi) as i32;
                let by = bubble::scaled(bubble::BUBBLE_MARGIN_Y, dpi) as i32;
                self.bubble.draw(&mut self.comp, bx, by);
            }
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
        self.dragging = true;
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
            let vx = unsafe { GetSystemMetrics(SM_XVIRTUALSCREEN) };
            let vy = unsafe { GetSystemMetrics(SM_YVIRTUALSCREEN) };
            let vw = unsafe { GetSystemMetrics(SM_CXVIRTUALSCREEN) };
            let vh = unsafe { GetSystemMetrics(SM_CYVIRTUALSCREEN) };
            let (lo, hi) = (vx - w + 48, vx + vw - 48);
            let nx = nx0.clamp(lo.min(hi), lo.max(hi));
            let (lo, hi) = (vy - h + 48, vy + vh - 48);
            let ny = ny0.clamp(lo.min(hi), lo.max(hi));
            window::set_window_rect(self.hwnd, nx, ny, w, h);
        }
    }

    pub fn on_lbutton_up(&mut self) {
        self.last_interaction = Instant::now();
        if self.dragging {
            self.dragging = false;
            log_line("[gui] lbutton up (drag end)");
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
        // reload at the new scale; keep the old frames on screen meanwhile
        let asset = if self.mode == Mode::Offline { "idle" } else { self.mode.asset() };
        self.pending = Some(self.mode);
        self.spawn_load(asset);
    }

    pub fn show_tray_menu(&mut self) {
        if let Some(t) = &self.tray {
            if let Some(cmd) = t.show_menu() {
                if cmd == tray::MENU_QUIT {
                    unsafe {
                        PostQuitMessage(0);
                    }
                }
            }
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
