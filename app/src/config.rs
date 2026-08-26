//! Config: `config.json` next to the exe, defaults per plan §8.
//! Path resolution rules are source-verified (see plan §8):
//! - scripts[].file relative paths are resolved against the exe dir
//! - DSH url / Hermes db path now live in the DSH/Hermes script args (the
//!   scripts still honour the DSH_PET_URL / HERMES_WEB_UI_HOME env vars)

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub auto_hide: AutoHideConfig,
    /// User Lua scripts (open interface, see connectors/lua.rs). Each entry
    /// runs in its own thread with its own embedded Lua state.
    pub scripts: Vec<ScriptEntryConfig>,
    pub display: DisplayConfig,
    pub fade: FadeConfig,
    pub opacity: OpacityConfig,
    pub bubble: BubbleConfig,
    pub text: TextConfig,
    pub windows: WindowConfig,
    pub avoid: AvoidConfig,
    /// Saved window position (pet's last dragged spot). None = default anchor.
    pub window_pos: WindowPosConfig,
    pub autostart: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct DisplayConfig {
    /// 0.25 ..= 2.0
    pub scale: f32,
    /// Tail loop length in ms (non-idle states).
    pub tail_ms: u64,
    /// Exact tail frame count override (dev tuning). None = derive from tail_ms.
    pub tail_frames: Option<u32>,
    /// Uniform per-frame duration in ms (1 ..= 2000). Sheet manifests no
    /// longer carry per-frame timings; every frame plays for this long.
    pub frame_ms: u32,
}

impl Default for DisplayConfig {
    fn default() -> Self {
        DisplayConfig {
            scale: 1.0,
            tail_ms: 1000,
            tail_frames: None,
            frame_ms: 42,
        }
    }
}

/// Saved window position (the pet's last dragged spot). None = default
/// bottom-right anchor. Restored on startup, clamped to the virtual screen.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct WindowPosConfig {
    pub x: Option<i32>,
    pub y: Option<i32>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct FadeConfig {
    /// Seconds without interaction before fading out.
    pub fade_after_sec: u64,
    /// Target opacity when faded (0.0 = fully invisible).
    pub fade_target: f32,
    /// Fade transition duration in ms.
    pub fade_ms: u64,
    /// States that never fade out.
    pub fade_disabled_states: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct OpacityConfig {
    pub idle: f32,
    pub working: f32,
    pub thinking: f32,
    pub attention: f32,
    pub done: f32,
    pub fail: f32,
    #[serde(rename = "move")]
    pub r#move: f32,
    pub offline: f32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct BubbleConfig {
    /// Minimum interval between realtime text bubble updates (ms).
    pub throttle_ms: u64,
    /// Max characters kept for the live text area.
    pub max_text_len: usize,
    /// Bubble is excluded from the pet fade (stays readable).
    pub exempt_from_fade: bool,
    /// Bubble font size multiplier (0.5 - 2.5, on top of system DPI scaling).
    pub font_scale: f32,
    /// Typewriter speed: live text reveals N characters per second
    /// (0 = instant, current behavior).
    pub type_cps: u32,
    /// 气泡主题(浅色/深色预设 + 覆盖项 + 状态色)。
    pub theme: BubbleThemeConfig,
}

impl Default for BubbleConfig {
    fn default() -> Self {
        BubbleConfig {
            throttle_ms: 150,
            max_text_len: 600,
            exempt_from_fade: true,
            font_scale: 1.0,
            type_cps: 90,
            theme: BubbleThemeConfig::default(),
        }
    }
}

/// Bubble stream text window. (The former "behind the pet" renderer and its
/// styling fields were removed; only the per-stream char window remains.)
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct TextConfig {
    /// Per-stream char window: how many characters of the live text stay
    /// visible (the bubble keeps only 120). Larger = a long DSH response
    /// fills more of the bubble before the front scrolls off.
    pub max_chars: usize,
}

impl Default for TextConfig {
    fn default() -> Self {
        TextConfig { max_chars: 1200 }
    }
}

/// 气泡主题配置:dark 切换深浅色预设;各 `Option` 覆盖对应颜色/透明度;
/// 全部留空 = 预设观感。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct BubbleThemeConfig {
    /// true = 深色预设(游戏/深色桌面场景),false = 浅色(默认观感)。
    pub dark: bool,
    /// DWM 系统毛玻璃(Win10+ 实验性,失败自动回退为透明填充)。
    pub acrylic: bool,
    pub fill: Option<String>,
    pub fill_alpha: Option<u8>,
    pub border: Option<String>,
    pub border_alpha: Option<u8>,
    pub divider: Option<String>,
    pub divider_alpha: Option<u8>,
    pub title: Option<String>,
    pub from: Option<String>,
    pub shadow_alpha: Option<u8>,
    pub radius: Option<u32>,
    pub state_colors: Option<StateColorsConfig>,
}

impl Default for BubbleThemeConfig {
    fn default() -> Self {
        BubbleThemeConfig {
            dark: false,
            acrylic: false,
            fill: None,
            fill_alpha: None,
            border: None,
            border_alpha: None,
            divider: None,
            divider_alpha: None,
            title: None,
            from: None,
            shadow_alpha: None,
            radius: None,
            state_colors: None,
        }
    }
}

/// 各状态的点缀色(左侧色条/标题高亮/来源标签),如 "#4A8FE7"。
/// 顺序字段:thinking/working/done/fail/attention/neutral。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct StateColorsConfig {
    pub thinking: String,
    pub working: String,
    pub done: String,
    pub fail: String,
    pub attention: String,
    pub neutral: String,
}

impl Default for StateColorsConfig {
    fn default() -> Self {
        StateColorsConfig {
            thinking: "#7FB4EF".into(), // 淡蓝(思考)
            working: "#4A8FE7".into(),  // 蓝(干活,原思考色)
            done: "#E8A33D".into(),
            fail: "#E05B4C".into(),
            attention: "#F28C3B".into(),
            neutral: "#9E9E9E".into(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rgb {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

impl Rgb {
    pub fn hex(s: &str) -> Option<Rgb> {
        parse_hex_color(s).map(|(r, g, b)| Rgb { r, g, b })
    }
}

/// 解析后的气泡主题(渲染层直接使用,无查找开销)。
#[derive(Debug, Clone)]
pub struct BubbleTheme {
    pub dark: bool,
    /// 软件亚克力(截屏模糊;win10/11 分层窗口均有效)。
    pub acrylic: bool,
    pub fill: Rgb,
    pub fill_alpha: u8,
    pub border: Rgb,
    pub border_alpha: u8,
    pub divider: Rgb,
    pub divider_alpha: u8,
    pub title: Rgb,
    pub from: Rgb,
    pub shadow_alpha: u8,
    pub radius: u32,
    /// [thinking, working, done, fail, attention, neutral]
    pub state: [Rgb; 6],
}

impl Default for BubbleTheme {
    fn default() -> Self {
        BubbleTheme::resolve(&BubbleThemeConfig::default())
    }
}

impl BubbleTheme {
    /// 从配置解析主题:先取预设(light/dark),再套用户覆盖项;非法十六进制
    /// 自动回退为预设色。
    pub fn resolve(cfg: &BubbleThemeConfig) -> BubbleTheme {
        let (fill, fill_a, border, border_a, divider, divider_a, title, from, shadow_a, radius, states) =
            if cfg.dark {
                (
                    (28, 30, 34),
                    170u8,
                    (74, 80, 92),
                    190u8,
                    (74, 80, 92),
                    140u8,
                    (0xE6, 0xE8, 0xEC),
                    (0x9A, 0xA0, 0xA8),
                    60u8,
                    12u32,
                    [
                        "#9DC3F2", "#6FA8F0", "#F0B45A", "#F06A5A", "#F59B52", "#8A8A8A",
                    ],
                )
            } else {
                (
                    (255, 255, 255),
                    80u8,
                    (205, 205, 205),
                    190u8,
                    (196, 196, 196),
                    170u8,
                    (0x26, 0x26, 0x26),
                    (0x8F, 0x8F, 0x8F),
                    38u8,
                    12u32,
                    [
                        "#7FB4EF", "#4A8FE7", "#E8A33D", "#E05B4C", "#F28C3B", "#9E9E9E",
                    ],
                )
            };
        let st = |preset: [&str; 6]| -> [Rgb; 6] {
            let user: [Option<&str>; 6] = match &cfg.state_colors {
                Some(c) => [
                    Some(c.thinking.as_str()),
                    Some(c.working.as_str()),
                    Some(c.done.as_str()),
                    Some(c.fail.as_str()),
                    Some(c.attention.as_str()),
                    Some(c.neutral.as_str()),
                ],
                None => [None; 6],
            };
            let mut out = [Rgb { r: 0, g: 0, b: 0 }; 6];
            for i in 0..6 {
                out[i] = user[i]
                    .and_then(Rgb::hex)
                    .or_else(|| Rgb::hex(preset[i]))
                    .unwrap();
            }
            out
        };
        let pick = |ov: &Option<String>, preset: (u8, u8, u8)| -> Rgb {
            ov.as_ref()
                .and_then(|s| Rgb::hex(s))
                .unwrap_or(Rgb { r: preset.0, g: preset.1, b: preset.2 })
        };
        let alpha = |v: &Option<u8>, preset: u8| match v { Some(x) => *x, None => preset };
        BubbleTheme {
            dark: cfg.dark,
            acrylic: cfg.acrylic,
            fill: pick(&cfg.fill, fill),
            fill_alpha: alpha(&cfg.fill_alpha, fill_a),
            border: pick(&cfg.border, border),
            border_alpha: alpha(&cfg.border_alpha, border_a),
            divider: pick(&cfg.divider, divider),
            divider_alpha: alpha(&cfg.divider_alpha, divider_a),
            title: pick(&cfg.title, title),
            from: pick(&cfg.from, from),
            shadow_alpha: alpha(&cfg.shadow_alpha, shadow_a),
            radius: cfg.radius.unwrap_or(radius),
            state: st(states),
        }
    }
}

/// Parse "#RRGGBB" into (r, g, b). Returns None for anything else so the
/// GUI can fall back to the default palette instead of breaking.
pub fn parse_hex_color(s: &str) -> Option<(u8, u8, u8)> {
    let s = s.trim().trim_start_matches('#');
    if s.len() != 6 || !s.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    Some((
        u8::from_str_radix(&s[0..2], 16).ok()?,
        u8::from_str_radix(&s[2..4], 16).ok()?,
        u8::from_str_radix(&s[4..6], 16).ok()?,
    ))
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct WindowConfig {
    /// done state hold window in seconds.
    pub done_sec: u64,
    /// failed state hold window in seconds.
    pub fail_sec: u64,
    /// Guaranteed top-priority display right after a done/fail event.
    pub celebrate_sec: u64,
}

/// 回避模式 (avoid mode): when enabled the pet scurries away from the mouse
/// cursor and glides back to its original spot once the cursor leaves the pet's
/// home area. Runtime toggle lives in the tray menu; the current state is
/// persisted to config.json so it survives restarts.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AvoidConfig {
    /// Master switch. Defaults OFF; the tray menu item 回避模式 flips it live.
    pub enabled: bool,
    /// Trigger radius (px): cursor closer than this to the pet's *home* rect
    /// starts a dodge. `distance * hysteresis` is where the pet decides the
    /// cursor is gone and returns home, so the boundary has no jitter.
    pub distance: f32,
    /// How far (px) the pet jumps away from the cursor while dodging.
    pub shift: f32,
    /// Return threshold multiplier over `distance` (>= 1.0).
    pub hysteresis: f32,
    /// Dodge travel speed (px/s). High = the pet snatches away promptly.
    pub dodge_speed: f32,
    /// Return-to-home travel speed (px/s). Low = a slow, readable glide back.
    pub return_speed: f32,
}

impl Default for AvoidConfig {
    fn default() -> Self {
        AvoidConfig {
            enabled: false,
            distance: 190.0,
            shift: 380.0,
            hysteresis: 1.6,
            dodge_speed: 2600.0,
            return_speed: 700.0,
        }
    }
}

/// 自动收起(auto-hide):勾选后,idle/offline 持续超过 `after_sec` 秒,宠物
/// 自动下移到任务栏区域(y = 屏幕高度 − y_factor × 窗口高度,任务栏自然盖住
/// 下半身)、透明度进一步降低、鼠标点击穿透;有新消息(状态离开
/// idle/offline)或鼠标悬停超过 `hover_sec` 秒后恢复原位与不透明度。
/// 仅用窗口样式与位置实现,无额外权限/依赖。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AutoHideConfig {
    /// 总开关(托盘右键「自动收起」可即时切换并写回)。
    pub enabled: bool,
    /// idle/offline 持续多少秒后收起。
    pub after_sec: u64,
    /// 收起位置系数:y = 屏幕高度 − y_factor × 窗口高度。
    pub y_factor: f32,
    /// 收起时额外透明度(0.05–1.0,叠加在渐隐系数上)。
    pub opacity: f32,
    /// 鼠标悬停超过该秒数退出收起状态。
    pub hover_sec: u64,
    /// 收起/恢复的滑动速度(px/s)。
    pub slide_speed: f32,
}

impl Default for AutoHideConfig {
    fn default() -> Self {
        AutoHideConfig {
            enabled: false,
            after_sec: 30,
            y_factor: 0.4,
            opacity: 0.3,
            hover_sec: 3,
            slide_speed: 600.0,
        }
    }
}

/// One entry of the `scripts` array (user-written Lua sources).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ScriptEntryConfig {
    /// Display name (bubble "From <name>"); empty = "Script N".
    pub name: String,
    /// Lua file path (relative to the exe dir or absolute).
    pub file: String,
    /// Hint for scripts (exposed via `pet.config()`); most scripts use their
    /// own polling loop with `pet.wait`.
    pub poll_ms: u64,
    /// Script-specific configuration, exposed as `pet.config().args`.
    pub args: Option<serde_json::Value>,
    /// true = remove os/io/package/require/dofile/loadfile/load/debug from the
    /// script's globals (filesystem/process access is disabled).
    pub sandbox: bool,
    /// false = 不启动该接入口(托盘"接入口"子菜单可切换,写回 config.json)。
    pub enabled: bool,
    /// true = 把该脚本每条 `pet.*` 调用(事件名+关键字段)和启动时的 args
    /// 写进 hannis.log,便于排查"调用发出去了没/发成什么样"。默认 false。
    pub debug: bool,
}

impl Default for ScriptEntryConfig {
    fn default() -> Self {
        ScriptEntryConfig {
            name: String::new(),
            file: String::new(),
            poll_ms: 1000,
            args: None,
            sandbox: false,
            enabled: true,
            debug: false,
        }
    }
}

/// 出厂默认接入口:DSH + Hermes(与迁移前的内置默认行为一致——没有 config.json
/// 时自动生成的配置也会监控 DSH 与 Hermes)。MAA/ComfyUI 由随包发布的
/// config.json 注册,不进默认(裸配置不该去连不存在的程序)。
fn default_scripts() -> Vec<ScriptEntryConfig> {
    vec![
        ScriptEntryConfig {
            name: "DSH".into(),
            file: "scripts/dsh.lua".into(),
            poll_ms: 1000,
            args: Some(serde_json::json!({
                "url": "http://127.0.0.1:3080",
                "poll_ms": 2000,
                "history_ms": 1000
            })),
            sandbox: false,
            enabled: true,
            debug: false,
        },
        ScriptEntryConfig {
            name: "Hermes".into(),
            file: "scripts/hermes.lua".into(),
            poll_ms: 1000,
            args: Some(serde_json::json!({
                "db_path": null, // null = 自动解析(env HERMES_WEB_UI_HOME → 用户主目录)
                "poll_ms_active": 1000,
                "poll_ms_idle": 2000
            })),
            sandbox: false,
            enabled: true,
            debug: false,
        },
    ]
}

impl Default for Config {
    fn default() -> Self {
        Config {
            auto_hide: AutoHideConfig::default(),
            scripts: default_scripts(),
            display: DisplayConfig::default(),
            fade: FadeConfig {
                fade_after_sec: 5,
                fade_target: 0.7,
                fade_ms: 1200,
                fade_disabled_states: vec!["attention".into()],
            },
            opacity: OpacityConfig {
                idle: 1.0,
                working: 1.0,
                thinking: 1.0,
                attention: 1.0,
                done: 1.0,
                fail: 1.0,
                r#move: 1.0,
                offline: 1.0,
            },
            bubble: BubbleConfig {
                throttle_ms: 150,
                max_text_len: 600,
                exempt_from_fade: true,
                font_scale: 1.0,
                type_cps: 90,
                theme: BubbleThemeConfig::default(),
            },
            text: TextConfig::default(),
            windows: WindowConfig { done_sec: 10, fail_sec: 10, celebrate_sec: 4 },
            avoid: AvoidConfig::default(),
            window_pos: WindowPosConfig::default(),
            autostart: false,
        }
    }
}

impl Config {
    pub fn opacity_for(&self, mode: &crate::state::Mode) -> f32 {
        match mode {
            crate::state::Mode::Offline => self.opacity.offline,
            crate::state::Mode::Attention => self.opacity.attention,
            crate::state::Mode::Failed => self.opacity.fail,
            crate::state::Mode::Working => self.opacity.working,
            crate::state::Mode::Thinking => self.opacity.thinking,
            crate::state::Mode::Done => self.opacity.done,
            crate::state::Mode::Idle => self.opacity.idle,
            crate::state::Mode::Move => self.opacity.r#move,
        }
    }

    /// Resolve "~" prefix against user home (config authoring convenience).
    pub fn expand_home(p: &str) -> PathBuf {
        if let Some(rest) = p.strip_prefix("~/").or_else(|| p.strip_prefix("~\\")) {
            user_home().map(|h| h.join(rest)).unwrap_or_else(|| PathBuf::from(p))
        } else {
            PathBuf::from(p)
        }
    }

    /// Resolve a scripts[].file entry: absolute paths pass through; relative
    /// paths are taken relative to `base` (the exe dir for the GUI). The
    /// process CWD must not matter — shortcuts often start the exe from
    /// System32, and a CWD-relative script path would then silently fail to
    /// load (the source would just report unhealthy).
    pub fn resolve_script_path(base: &Path, file: &str) -> PathBuf {
        let p = PathBuf::from(file.trim());
        if p.is_absolute() { p } else { base.join(p) }
    }
}

pub fn user_home() -> Option<PathBuf> {
    if cfg!(target_os = "windows") {
        std::env::var("USERPROFILE").ok().map(PathBuf::from)
    } else {
        std::env::var("HOME").ok().map(PathBuf::from)
    }
}

impl Config {
    pub fn load(path: &Path) -> Config {
        match std::fs::read_to_string(path) {
            Ok(s) => serde_json::from_str(&s).unwrap_or_else(|e| {
                eprintln!("config parse error ({e}), using defaults");
                Config::default()
            }),
            Err(_) => Config::default(),
        }
    }

    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        let s = serde_json::to_string_pretty(self)?;
        std::fs::write(path, s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_roundtrip() {
        let c = Config::default();
        let s = serde_json::to_string(&c).unwrap();
        let c2: Config = serde_json::from_str(&s).unwrap();
        assert_eq!(c2.display.scale, 1.0);
        assert_eq!(c2.display.frame_ms, 42);
        assert_eq!(c2.fade.fade_after_sec, 5);
        assert!((c2.fade.fade_target - 0.7).abs() < 1e-6);
        assert_eq!(c2.fade.fade_disabled_states, vec!["attention".to_string()]);
        assert_eq!(c2.bubble.type_cps, 90);
        assert!((c2.bubble.font_scale - 1.0).abs() < 1e-6);
        assert_eq!(c2.windows.done_sec, 10);
        // auto_hide (自动收起) defaults OFF;slide_speed sane
        assert!(!c2.auto_hide.enabled);
        assert_eq!(c2.auto_hide.after_sec, 30);
        assert!((c2.auto_hide.slide_speed - 600.0).abs() < 1e-6);
        // avoid (回避模式) defaults to OFF with sane tuning
        assert!(!c2.avoid.enabled);
        assert!((c2.avoid.distance - 190.0).abs() < 1e-6);
        assert!((c2.avoid.hysteresis - 1.6).abs() < 1e-6);
        // old configs without the avoid section parse to the same default
        let old: Config = serde_json::from_str(r#"{"bubble":{"throttle_ms":150}}"#).unwrap();
        assert!(old.avoid.enabled == false && old.avoid.shift > 0.0);
        assert!((old.avoid.return_speed - 700.0).abs() < 1e-6);
        // text section: only the stream char window remains (behind-the-pet
        // renderer and its styling were removed)
        assert_eq!(c2.text.max_chars, 1200);
        // missing type_cps in an old config falls back to the default
        let old: Config = serde_json::from_str(r#"{"bubble":{"throttle_ms":150}}"#).unwrap();
        assert_eq!(old.bubble.type_cps, 90);
        // old configs without a text section must not break
        let old: Config = serde_json::from_str(r#"{"bubble":{"throttle_ms":150}}"#).unwrap();
        assert_eq!(old.text.max_chars, 1200);
        // a config with the removed behind-mode fields still parses (ignored)
        let b: Config = serde_json::from_str(r#"{"text":{"mode":"behind","outline_width":2}}"#).unwrap();
        assert_eq!(b.text.max_chars, 1200);
        // window position defaults to None (default anchor) and round-trips
        assert!(c2.window_pos.x.is_none() && c2.window_pos.y.is_none());
        let mut c3 = Config::default();
        c3.window_pos = WindowPosConfig { x: Some(123), y: Some(456) };
        let s3 = serde_json::to_string(&c3).unwrap();
        let c4: Config = serde_json::from_str(&s3).unwrap();
        assert_eq!(c4.window_pos.x, Some(123));
        assert_eq!(c4.window_pos.y, Some(456));
    }

    #[test]
    fn hex_color_parse() {
        assert_eq!(parse_hex_color("#FFFFFF"), Some((255, 255, 255)));
        assert_eq!(parse_hex_color("161616"), Some((0x16, 0x16, 0x16)));
        assert_eq!(parse_hex_color("#123456"), Some((0x12, 0x34, 0x56)));
        assert_eq!(parse_hex_color(""), None);
        assert_eq!(parse_hex_color("red"), None);
        assert_eq!(parse_hex_color("#FFFFF"), None);
        assert_eq!(parse_hex_color("#GGGGGG"), None);
    }

    #[test]
    fn bubble_theme_presets_and_overrides() {
        // 浅色预设(默认观感)
        let t = BubbleTheme::resolve(&BubbleThemeConfig::default());
        assert_eq!(t.fill, Rgb { r: 255, g: 255, b: 255 });
        assert_eq!(t.fill_alpha, 80);
        assert_eq!(t.border, Rgb { r: 205, g: 205, b: 205 });
        assert_eq!(t.title, Rgb { r: 0x26, g: 0x26, b: 0x26 });
        assert_eq!(t.from, Rgb { r: 0x8f, g: 0x8f, b: 0x8f });
        assert_eq!(t.shadow_alpha, 38);
        assert_eq!(t.radius, 12);
        assert_eq!(t.state[1], Rgb { r: 0x4A, g: 0x8F, b: 0xE7 }); // working = 蓝(原思考色)
        // 深色预设
        let d = BubbleTheme::resolve(&BubbleThemeConfig { dark: true, ..Default::default() });
        assert_eq!(d.fill, Rgb { r: 28, g: 30, b: 34 });
        assert_eq!(d.fill_alpha, 170);
        assert_eq!(d.title, Rgb { r: 0xE6, g: 0xE8, b: 0xEC });
        assert_ne!(t.state[3], d.state[3]); // fail 色深浅不同
        // 覆盖项 + 非法十六进制回退预设
        let o = BubbleTheme::resolve(&BubbleThemeConfig {
            fill_alpha: Some(200),
            title: Some("#1A2B3C".into()),
            border: Some("#GGGGGG".into()), // 非法 → 预设
            radius: Some(20),
            ..Default::default()
        });
        assert_eq!(o.fill_alpha, 200);
        assert_eq!(o.title, Rgb { r: 0x1A, g: 0x2B, b: 0x3C });
        assert_eq!(o.border, Rgb { r: 205, g: 205, b: 205 });
        assert_eq!(o.radius, 20);
        // 用户状态色覆盖
        let o2 = BubbleTheme::resolve(&BubbleThemeConfig {
            state_colors: Some(StateColorsConfig {
                working: "#FF00FF".into(),
                ..Default::default()
            }),
            ..Default::default()
        });
        assert_eq!(o2.state[1], Rgb { r: 255, g: 0, b: 255 });
        assert_eq!(o2.state[0], Rgb { r: 0x7F, g: 0xB4, b: 0xEF }); // 未覆盖的用默认(淡蓝思考)
    }

    #[test]
    fn scripts_defaults_and_roundtrip() {
        // 出厂默认:DSH + Hermes(自动生成的配置开箱即监控它们)
        let c = Config::default();
        let names: Vec<&str> = c.scripts.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["DSH", "Hermes"]);
        assert_eq!(c.scripts[0].file, "scripts/dsh.lua");
        assert_eq!(c.scripts[1].file, "scripts/hermes.lua");
        assert!(c.scripts[0].enabled && c.scripts[1].enabled);
        // 旧配置/部分配置没写 scripts 段 → 同样补上默认接入口(开箱即监控
        // DSH/Hermes,与迁移前的行为一致);显式 "scripts":[] 则尊重(用户可关)
        let old: Config = serde_json::from_str(r#"{"dsh":{"url":"http://x"}}"#).unwrap();
        assert_eq!(old.scripts.len(), 2, "missing scripts field fills from Config::default()");
        let explicit: Config = serde_json::from_str(r#"{"scripts":[]}"#).unwrap();
        assert!(explicit.scripts.is_empty());
        // 注册项解析(name/file/poll_ms/args/sandbox)与默认值
        let c2: Config = serde_json::from_str(r#"{"scripts":[{"name":"A","file":"a.lua"}]}"#).unwrap();
        assert_eq!(c2.scripts.len(), 1);
        assert_eq!(c2.scripts[0].name, "A");
        assert_eq!(c2.scripts[0].poll_ms, 1000);
        assert!(!c2.scripts[0].sandbox);
        assert!(c2.scripts[0].args.is_none());
        let c3: Config = serde_json::from_str(
            r#"{"scripts":[{"name":"B","file":"b.lua","poll_ms":500,"sandbox":true,"args":{"log":"x"}}]}"#,
        ).unwrap();
        assert_eq!(c3.scripts[0].poll_ms, 500);
        assert!(c3.scripts[0].sandbox);
        assert!(!c3.scripts[0].debug, "debug 默认关");
        assert_eq!(c3.scripts[0].args.as_ref().unwrap()["log"], "x");
        // debug 字段可解析
        let c4: Config = serde_json::from_str(r#"{"scripts":[{"name":"D","file":"d.lua","debug":true}]}"#).unwrap();
        assert!(c4.scripts[0].debug);
    }
}
