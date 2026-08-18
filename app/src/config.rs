//! Config: `config.json` next to the exe, defaults per plan §8.
//! Path resolution rules are source-verified (see plan §8):
//! - DSH url: env DSH_PET_URL > config > default http://127.0.0.1:3080
//! - Hermes db: config > env HERMES_WEB_UI_HOME > %USERPROFILE%\.hermes-web-ui\hermes-web-ui.db

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub const DEFAULT_DSH_URL: &str = "http://127.0.0.1:3080";
pub const DEFAULT_COMFYUI_URL: &str = "http://127.0.0.1:8188";
pub const HERMES_RELATIVE_DB: &str = ".hermes-web-ui/hermes-web-ui.db";
pub const HERMES_HOME_ENV: &str = "HERMES_WEB_UI_HOME";
pub const DSH_URL_ENV: &str = "DSH_PET_URL";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub dsh: DshConfig,
    pub hermes: HermesConfig,
    pub comfyui: ComfyUiConfig,
    pub display: DisplayConfig,
    pub fade: FadeConfig,
    pub opacity: OpacityConfig,
    pub bubble: BubbleConfig,
    pub windows: WindowConfig,
    pub autostart: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct DshConfig {
    /// Base url, e.g. http://127.0.0.1:3080
    pub url: String,
    pub poll_ms: u64,
    /// session.history poll interval (ms) - live thinking/output text
    /// streaming granularity for DSH sessions.
    pub history_ms: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct HermesConfig {
    /// Explicit db path. null = auto-resolve (env HERMES_WEB_UI_HOME -> user home).
    pub db_path: Option<String>,
    pub poll_ms_active: u64,
    pub poll_ms_idle: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ComfyUiConfig {
    /// Monitor the local ComfyUI server (default exposes it on 8188).
    pub enabled: bool,
    /// Base url, e.g. http://127.0.0.1:8188
    pub url: String,
    /// /queue poll interval (ms) - baseline running/pending/terminal states.
    pub poll_ms: u64,
    /// Subscribe to the /ws stream for node-level progress and instant
    /// terminal events. Never replaces polling (push is an enhancement).
    pub ws: bool,
}

impl Default for ComfyUiConfig {
    /// A missing `comfyui` section in an existing config.json must NOT
    /// silently disable the connector: new capability defaults to ON at the
    /// stock URL (explicit `"enabled": false` still opts out).
    fn default() -> Self {
        ComfyUiConfig {
            enabled: true,
            url: DEFAULT_COMFYUI_URL.into(),
            poll_ms: 2000,
            ws: true,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct DisplayConfig {
    /// 0.25 ..= 2.0
    pub scale: f32,
    /// Tail loop length in ms (non-idle states).
    pub tail_ms: u64,
    /// Exact tail frame count override (dev tuning). None = derive from tail_ms.
    pub tail_frames: Option<u32>,
    /// Load split frames when resource/<state>/manifest.json exists.
    pub use_split: String, // "auto" | "true" | "false"
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
}

impl Default for BubbleConfig {
    fn default() -> Self {
        BubbleConfig {
            throttle_ms: 150,
            max_text_len: 600,
            exempt_from_fade: true,
            font_scale: 1.0,
            type_cps: 90,
        }
    }
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

impl Default for Config {
    fn default() -> Self {
        Config {
            dsh: DshConfig { url: DEFAULT_DSH_URL.into(), poll_ms: 2000, history_ms: 1000 },
            hermes: HermesConfig {
                db_path: None,
                poll_ms_active: 1000,
                poll_ms_idle: 2000,
            },
            comfyui: ComfyUiConfig::default(),
            display: DisplayConfig {
                scale: 1.0,
                tail_ms: 1000,
                tail_frames: None,
                use_split: "auto".into(),
            },
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
            },
            windows: WindowConfig { done_sec: 10, fail_sec: 10, celebrate_sec: 4 },
            autostart: false,
        }
    }
}

impl Config {
    /// Effective DSH url: env DSH_PET_URL > config.url.
    pub fn dsh_url(&self) -> String {
        std::env::var(DSH_URL_ENV)
            .ok()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| self.dsh.url.clone())
    }

    /// Effective Hermes db path, or None if unresolvable.
    pub fn hermes_db_path(&self) -> Option<PathBuf> {
        if let Some(p) = &self.hermes.db_path {
            let p = p.trim();
            if !p.is_empty() {
                return Some(PathBuf::from(p));
            }
        }
        resolve_hermes_default_db()
    }

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
}

pub fn user_home() -> Option<PathBuf> {
    if cfg!(target_os = "windows") {
        std::env::var("USERPROFILE").ok().map(PathBuf::from)
    } else {
        std::env::var("HOME").ok().map(PathBuf::from)
    }
}

/// env HERMES_WEB_UI_HOME > %USERPROFILE% (or $HOME) + ".hermes-web-ui/hermes-web-ui.db"
pub fn resolve_hermes_default_db() -> Option<PathBuf> {
    if let Ok(h) = std::env::var(HERMES_HOME_ENV) {
        let h = h.trim();
        if !h.is_empty() {
            return Some(PathBuf::from(h).join("hermes-web-ui.db"));
        }
    }
    let home = user_home()?;
    // Windows uses forward-slash separators fine on PathBuf.
    Some(home.join(HERMES_RELATIVE_DB))
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
        assert_eq!(c2.dsh.url, DEFAULT_DSH_URL);
        assert_eq!(c2.display.scale, 1.0);
        assert_eq!(c2.fade.fade_after_sec, 5);
        assert!((c2.fade.fade_target - 0.7).abs() < 1e-6);
        assert_eq!(c2.fade.fade_disabled_states, vec!["attention".to_string()]);
        assert_eq!(c2.bubble.type_cps, 90);
        assert!((c2.bubble.font_scale - 1.0).abs() < 1e-6);
        assert_eq!(c2.windows.done_sec, 10);
        // missing type_cps in an old config falls back to the default
        let old: Config = serde_json::from_str(r#"{"bubble":{"throttle_ms":150}}"#).unwrap();
        assert_eq!(old.bubble.type_cps, 90);
    }

    #[test]
    fn dsh_url_env_wins() {
        let c = Config::default();
        std::env::set_var(DSH_URL_ENV, "http://127.0.0.1:9999");
        assert_eq!(c.dsh_url(), "http://127.0.0.1:9999");
        std::env::remove_var(DSH_URL_ENV);
        assert_eq!(c.dsh_url(), DEFAULT_DSH_URL);
    }

    #[test]
    fn hermes_path_resolution() {
        // single sequential test: env var wins, then user-profile fallback
        // (kept serial because parallel tests racing on the same env var is flaky)
        let c = Config::default();
        std::env::set_var(HERMES_HOME_ENV, "C:\\custom\\hermes");
        let p = c.hermes_db_path().unwrap();
        assert_eq!(p.file_name().unwrap(), "hermes-web-ui.db");
        assert_eq!(p.parent().unwrap(), Path::new("C:\\custom\\hermes"));
        std::env::remove_var(HERMES_HOME_ENV);
        let home = user_home().expect("home present in test env");
        let p = c.hermes_db_path().unwrap();
        assert_eq!(p.file_name().unwrap(), "hermes-web-ui.db");
        assert_eq!(p.parent().unwrap(), home.join(".hermes-web-ui").as_path());
    }

    #[test]
    fn explicit_db_path_wins() {
        let mut c = Config::default();
        c.hermes.db_path = Some("D:\\data\\hermes.db".into());
        assert_eq!(c.hermes_db_path().unwrap(), PathBuf::from("D:\\data\\hermes.db"));
    }

    #[test]
    fn missing_comfyui_section_defaults_enabled() {
        // old configs without the comfyui section must NOT disable the monitor
        let old: Config = serde_json::from_str(r#"{"dsh":{"url":"http://x"}}"#).unwrap();
        assert!(old.comfyui.enabled);
        assert_eq!(old.comfyui.url, DEFAULT_COMFYUI_URL);
        assert_eq!(old.comfyui.poll_ms, 2000);
        assert!(old.comfyui.ws);
        // explicit opt-out still works
        let off: Config = serde_json::from_str(r#"{"comfyui":{"enabled":false}}"#).unwrap();
        assert!(!off.comfyui.enabled);
    }
}
