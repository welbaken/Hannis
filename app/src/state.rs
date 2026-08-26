//! PetState — pure state machine (design doc §4), no I/O.
//! Table-driven mode priority, TTL windows, live-text accumulation.
//! All time comes from the injected clock (`now_ms`), so tests are deterministic.

use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize, serde::Deserialize)]
pub enum Source {
    /// User-provided Lua scripts (connectors/lua.rs): id = registration index
    /// in the `scripts` array. DSH/Hermes/MAA/ComfyUI 也是脚本(注册顺序即
    /// 气泡平局时的优先顺序,DSH 注册在首位,延续"平局 DSH 赢")。
    Script(u16),
}

impl Default for Source {
    fn default() -> Self {
        Source::Script(0)
    }
}

/// Script source labels: id -> display name (registered at spawn). A global
/// registry because `Source` is Copy and must stay enum-only.
static SCRIPT_LABELS: std::sync::OnceLock<std::sync::Mutex<Vec<String>>> = std::sync::OnceLock::new();

pub fn register_script_label(id: u16, label: String) {
    let m = SCRIPT_LABELS.get_or_init(|| std::sync::Mutex::new(Vec::new()));
    if let Ok(mut g) = m.lock() {
        let i = id as usize;
        if g.len() <= i {
            g.resize(i + 1, String::new());
        }
        g[i] = label;
    }
}

pub fn script_label(id: u16) -> String {
    let m = SCRIPT_LABELS.get_or_init(|| std::sync::Mutex::new(Vec::new()));
    m.lock()
        .ok()
        .and_then(|g| g.get(id as usize).cloned())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| format!("Script {id}"))
}

impl Source {
    /// Display name used in the bubble ("From <name>") and debug output.
    pub fn label(&self) -> String {
        match self {
            Source::Script(id) => script_label(*id),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub enum Mode {
    Offline,
    Attention,
    Failed,
    Working,
    Thinking,
    Done,
    Idle,
    /// Transient user-interaction overlay (dragging), set by UI only.
    #[default]
    Move,
}

impl Mode {
    /// Priority, high -> low. Table-driven (design doc §4.1).
    #[allow(dead_code)]
    fn priority(self) -> u8 {
        match self {
            Mode::Offline => 8,
            Mode::Attention => 7,
            Mode::Failed => 6,
            Mode::Working => 5,
            Mode::Thinking => 4,
            Mode::Done => 3,
            Mode::Idle => 2,
            Mode::Move => 1,
        }
    }

    /// Animation asset name (resource/<name>.sheet). Offline is composited.
    pub fn asset(&self) -> &'static str {
        match self {
            Mode::Offline => "idle",
            Mode::Attention => "attention",
            Mode::Failed => "fail",
            Mode::Working => "working",
            Mode::Thinking => "think",
            Mode::Done => "done",
            Mode::Idle => "idle",
            Mode::Move => "move",
        }
    }

    /// idle is the only state that loops the full animation (plan §6.2).
    pub fn loops_full(&self) -> bool {
        matches!(self, Mode::Idle)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum TurnEndReason {
    Completed,
    Error,
    MaxTokens,
    Aborted,
    Interrupted,
    Blocked,
}

impl TurnEndReason {
    pub fn from_dsh_kind(kind: &str) -> Option<TurnEndReason> {
        Some(match kind {
            "completed" => TurnEndReason::Completed,
            "error" => TurnEndReason::Error,
            "max-tokens" => TurnEndReason::MaxTokens,
            "aborted" => TurnEndReason::Aborted,
            "interrupted" => TurnEndReason::Interrupted,
            "blocked" => TurnEndReason::Blocked,
            _ => return None,
        })
    }

    pub fn is_failure(&self) -> bool {
        matches!(self, TurnEndReason::Error | TurnEndReason::MaxTokens)
    }

    pub fn is_neutral(&self) -> bool {
        matches!(self, TurnEndReason::Aborted | TurnEndReason::Interrupted)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct TodoItem {
    pub content: String,
    pub status: String, // pending | in_progress | completed
}

#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct LiveText {
    pub reasoning: String,
    pub text: String,
    pub tool_name: Option<String>,
}

#[derive(Debug, Clone)]
pub struct SessionItem {
    pub session_id: String,
    pub running: bool,
    pub title: Option<String>,
    pub todos: Option<Vec<TodoItem>>,
}

#[derive(Debug, Clone)]
pub enum StateEvent {
    /// Baseline poll result from a source.
    Poll { source: Source, items: Vec<SessionItem>, ok: bool, error: Option<String> },
    /// host/session-status running flip (DSH) or hermes ended_at edge.
    SessionStatus { source: Source, session_id: String, running: bool },
    TurnStarted { source: Source, session_id: String, turn: u64 },
    TurnEnded { source: Source, session_id: String, turn: u64, reason: TurnEndReason },
    ToolStarted { source: Source, session_id: String, name: String, arguments: Option<String> },
    ToolEnded { source: Source, session_id: String, name: String, error: bool },
    TodoSnapshot { source: Source, session_id: String, todos: Vec<TodoItem> },
    ApprovalRequested { source: Source, id: String, session_id: String, tool: String },
    ApprovalResolved { source: Source, id: String },
    QuestionRequested { source: Source, id: String, session_id: String, text: String },
    QuestionResolved { source: Source, id: String },
    /// The mux stream (re)connected: the server replays its CURRENT pending
    /// requests right after connect. Clear local copies for that source so
    /// requests the server no longer knows about (server crash/restart, or a
    /// missed resolved frame during a WS blip) cannot keep the pet in
    /// attention forever — the replay re-adds what is still genuinely pending.
    PendingSync { source: Source },
    /// Throttled realtime model output (DSH chunk aggregation / Hermes poll delta).
    LiveText { source: Source, session_id: String, reasoning: Option<String>, text: Option<String>, tool_name: Option<String> },
    /// A user-role message (task prompt fallback for done/failed bubbles).
    UserMessage { source: Source, session_id: String, text: String },
    /// Connector health flip.
    SourceHealth { source: Source, healthy: bool },
    /// Backend queue depth update (ComfyUI queue_pending, DSH session/queue).
    /// The snapshot's queue_len is the sum across sources.
    QueueChanged { source: Source, pending: u32 },
    Tick,
}

#[derive(Debug, Clone, Default)]
struct SessionState {
    source: Source,
    title: Option<String>,
    running: bool,
    turns: u32,
    tools: BTreeSet<String>,
    tool_args: BTreeMap<String, String>,
    /// tool name -> when it started (now_ms), for "who is actually working".
    tool_since: BTreeMap<String, u64>,
    last_user_text: Option<String>,
    waiting_user: bool,
    last_end: Option<(TurnEndReason, u64)>,
    todos: Vec<TodoItem>,
    live: LiveText,
}

impl SessionState {
    fn active(&self) -> bool {
        self.running || self.turns > 0 || !self.tools.is_empty()
    }
}

#[derive(Debug, Clone)]
struct PendingApproval {
    session_id: String,
    tool: String,
    at_ms: u64,
    source: Source,
}

#[derive(Debug, Clone)]
struct PendingQuestion {
    session_id: String,
    text: String,
    at_ms: u64,
    source: Source,
}

/// Per-session live text cap (keeps memory bounded on long generations).
pub const LIVE_TEXT_CAP: usize = 8000;

fn cap_text(s: &mut String) {
    if s.chars().count() > LIVE_TEXT_CAP {
        let tail: String = s.chars().skip(s.chars().count() - LIVE_TEXT_CAP).collect();
        *s = tail;
    }
}

fn truncate_chars(s: &str, n: usize) -> String {
    let v: Vec<char> = s.chars().collect();
    if v.len() <= n {
        s.to_string()
    } else {
        let mut t: String = v[..n].iter().collect();
        t.push('…');
        t
    }
}

pub const TTL_APPROVAL_MS: u64 = 30 * 60 * 1000;
pub const DEFAULT_DONE_MS: u64 = 120 * 1000;
pub const DEFAULT_FAIL_MS: u64 = 10 * 1000;
/// Guaranteed top-priority window right after a done/fail event, so the
/// animation is always visible even when the agent immediately starts the
/// next turn (which would otherwise mask the low-priority done/fail states).
pub const DEFAULT_CELEBRATE_MS: u64 = 4 * 1000;

#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct SessionInfo {
    pub session_id: String,
    pub source: Source,
    pub title: String,
    pub tool: Option<String>,
    pub tool_args: Option<String>,
    /// Task label: title -> last user message -> first non-pending todo.
    pub task: Option<String>,
    pub todos: Vec<TodoItem>,
    pub live: LiveText,
}

#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct Snapshot {
    pub mode: Mode,
    pub sources: BTreeMap<Source, bool>, // source -> healthy
    pub pending_approvals: Vec<(String, String, String)>, // (session, tool, source)
    pub pending_questions: Vec<(String, String, String)>, // (session, text, source)
    pub working: Vec<SessionInfo>,
    pub thinking: Vec<SessionInfo>,
    pub failed: Vec<SessionInfo>,
    pub done: Vec<SessionInfo>,
    pub queue_len: u32,
    pub done_sound_pending: bool,
    pub fail_sound_pending: bool,
    /// session_id -> earliest start time of its currently open tools.
    /// Bubble tie-break: "谁在干活显示谁" — the session deepest into a task
    /// wins over one that merely started a short tool.
    pub working_since: BTreeMap<String, u64>,
}

pub struct PetState {
    pub now_ms: u64,
    sessions: BTreeMap<String, SessionState>,
    approvals: BTreeMap<String, PendingApproval>,
    questions: BTreeMap<String, PendingQuestion>,
    source_health: BTreeMap<Source, bool>,
    queue_pending: BTreeMap<Source, u32>,
    done_ms: u64,
    fail_ms: u64,
    celebrate_ms: u64,
    done_sound_pending: bool,
    fail_sound_pending: bool,
}

impl PetState {
    pub fn new(done_ms: u64, fail_ms: u64) -> Self {
        PetState {
            now_ms: 0,
            sessions: BTreeMap::new(),
            approvals: BTreeMap::new(),
            questions: BTreeMap::new(),
            source_health: BTreeMap::new(),
            queue_pending: BTreeMap::new(),
            done_ms,
            fail_ms,
            celebrate_ms: DEFAULT_CELEBRATE_MS,
            done_sound_pending: false,
            fail_sound_pending: false,
        }
    }

    pub fn set_celebrate_ms(&mut self, ms: u64) {
        self.celebrate_ms = ms;
    }

    pub fn apply(&mut self, ev: StateEvent) {
        match ev {
            StateEvent::SourceHealth { source, healthy } => {
                self.source_health.insert(source, healthy);
            }
            StateEvent::Poll { source, items, ok, .. } => {
                if !ok {
                    return;
                }
                let now = self.now_ms;
                let mut any_done = false;
                let mut seen = BTreeSet::new();
                for it in items {
                    seen.insert(it.session_id.clone());
                    let s = self.session_mut(source, &it.session_id);
                    if it.title.is_some() {
                        s.title = it.title;
                    }
                    if let Some(todos) = it.todos {
                        s.todos = todos;
                    }
                    if !it.running {
                        // running true->false without an explicit turn/end (push gap):
                        // done fallback, unless the session is waiting on the user.
                        let recently_ended = s
                            .last_end
                            .map(|(_, at)| now.saturating_sub(at) < 10_000)
                            .unwrap_or(false);
                        if (s.turns > 0 || !s.tools.is_empty()) && !s.waiting_user && !recently_ended {
                            s.last_end = Some((TurnEndReason::Completed, now));
                            any_done = true;
                        }
                        s.turns = 0;
                        s.tools.clear();
                        s.tool_since.clear();
                        s.running = false;
                    } else {
                        s.running = true;
                    }
                }
                if any_done {
                    self.done_sound_pending = true;
                }
                // reap inactive sessions that vanished from their source's poll
                let keep: Vec<String> = self
                    .sessions
                    .iter()
                    .filter(|(id, s)| {
                        if s.source != source {
                            return true;
                        }
                        if seen.contains(*id) {
                            return true;
                        }
                        // drop only if fully inactive and past the done window
                        let ended_long_ago = s
                            .last_end
                            .map(|(_, at)| now.saturating_sub(at) > self.done_ms)
                            .unwrap_or(true);
                        s.active() || !ended_long_ago
                    })
                    .map(|(id, _)| id.clone())
                    .collect();
                self.sessions.retain(|id, _| keep.contains(id));
            }
            StateEvent::SessionStatus { source, session_id, running } => {
                let now = self.now_ms;
                let mut any_done = false;
                {
                    let s = self.session_mut(source, &session_id);
                    s.running = running;
                    if !running {
                        let recently_ended = s
                            .last_end
                            .map(|(_, at)| now.saturating_sub(at) < 10_000)
                            .unwrap_or(false);
                        if (s.turns > 0 || !s.tools.is_empty()) && !s.waiting_user && !recently_ended {
                            s.last_end = Some((TurnEndReason::Completed, now));
                            any_done = true;
                        }
                        s.turns = 0;
                        s.tools.clear();
                        s.tool_since.clear();
                    }
                }
                if any_done {
                    self.done_sound_pending = true;
                }
            }
            StateEvent::TurnStarted { source, session_id, .. } => {
                let s = self.session_mut(source, &session_id);
                s.turns += 1;
                s.waiting_user = false;
                s.running = true;
            }
            StateEvent::TurnEnded { source, session_id, reason, .. } => {
                let now = self.now_ms;
                let (done, fail, blocked) = {
                    let s = self.session_mut(source, &session_id);
                    s.turns = s.turns.saturating_sub(1);
                    s.last_end = Some((reason, now));
                    match reason {
                        TurnEndReason::Blocked => s.waiting_user = true,
                        _ => {}
                    }
                    // a finished turn usually closes its live text
                    if s.turns == 0 {
                        s.live = LiveText::default();
                    }
                    (
                        matches!(reason, TurnEndReason::Completed),
                        reason.is_failure(),
                        matches!(reason, TurnEndReason::Blocked),
                    )
                };
                if done {
                    self.done_sound_pending = true;
                }
                if fail {
                    self.fail_sound_pending = true;
                }
                let _ = blocked;
            }
            StateEvent::ToolStarted { source, session_id, name, arguments, .. } => {
                let now = self.now_ms;
                let s = self.session_mut(source, &session_id);
                s.tools.insert(name.clone());
                if let Some(args) = arguments {
                    s.tool_args.insert(name.clone(), args);
                }
                s.tool_since.insert(name.clone(), now);
                s.running = true;
                // A tool run is a new work phase: drop the pre-tool stream so
                // the Working bubble shows the actual work (⚙ tool label, or
                // fresh text streamed after the tool started) instead of the
                // stale thinking/output text from before the call.
                s.live = LiveText::default();
            }
            StateEvent::ToolEnded { source, session_id, name, .. } => {
                let s = self.session_mut(source, &session_id);
                s.tools.remove(&name);
                s.tool_args.remove(&name);
                s.tool_since.remove(&name);
            }
            StateEvent::TodoSnapshot { source, session_id, todos, .. } => {
                self.session_mut(source, &session_id).todos = todos;
            }
            StateEvent::ApprovalRequested { id, session_id, tool, source } => {
                self.approvals.insert(
                    id.clone(),
                    PendingApproval { session_id, tool, at_ms: self.now_ms, source },
                );
            }
            StateEvent::ApprovalResolved { id, .. } => {
                self.approvals.remove(&id);
            }
            StateEvent::QuestionRequested { id, session_id, text, source } => {
                self.questions.insert(
                    id,
                    PendingQuestion { session_id, text, at_ms: self.now_ms, source },
                );
            }
            StateEvent::QuestionResolved { id, .. } => {
                // Clears a whole request: the exact key (Hermes clarify id,
                // legacy frames) OR every item the DSH connector keyed under
                // `<id>\u{0}<itemId>` — `question/resolved` carries only the
                // request's rpcId, which must not leave sibling items pending
                // (that would keep the pet in attention forever).
                let prefix = format!("{id}\u{0}");
                self.questions.retain(|k, _| k != &id && !k.starts_with(&prefix));
            }
            StateEvent::PendingSync { source } => {
                self.approvals.retain(|_, a| a.source != source);
                self.questions.retain(|_, q| q.source != source);
            }
            StateEvent::UserMessage { source, session_id, text } => {
                let s = self.session_mut(source, &session_id);
                let text = text.trim().to_string();
                if !text.is_empty() {
                    s.last_user_text = Some(truncate_chars(&text, 120));
                }
            }
            StateEvent::LiveText { source, session_id, reasoning, text, tool_name } => {
                let s = self.session_mut(source, &session_id);
                if let Some(r) = reasoning {
                    if !r.is_empty() {
                        s.live.reasoning.push_str(&r);
                        cap_text(&mut s.live.reasoning);
                    }
                }
                if let Some(t) = text {
                    if !t.is_empty() {
                        s.live.text.push_str(&t);
                        cap_text(&mut s.live.text);
                    }
                }
                if tool_name.is_some() {
                    s.live.tool_name = tool_name;
                }
            }
            StateEvent::QueueChanged { source, pending } => {
                if pending == 0 {
                    self.queue_pending.remove(&source);
                } else {
                    self.queue_pending.insert(source, pending);
                }
            }
            StateEvent::Tick => {
                let now = self.now_ms;
                self.approvals.retain(|_, a| now.saturating_sub(a.at_ms) < TTL_APPROVAL_MS);
                self.questions.retain(|_, q| now.saturating_sub(q.at_ms) < TTL_APPROVAL_MS);
            }
        }
    }

    fn session_mut(&mut self, source: Source, id: &str) -> &mut SessionState {
        self.sessions
            .entry(id.to_string())
            .or_insert_with(|| SessionState { source, ..Default::default() })
    }

    pub fn queue_len(&self) -> u32 {
        self.queue_pending.values().sum()
    }

    pub fn mode(&self) -> Mode {
        if self.all_sources_down() {
            return Mode::Offline;
        }
        if !self.approvals.is_empty() || !self.questions.is_empty() {
            return Mode::Attention;
        }
        let now = self.now_ms;
        // celebration window: a fresh done/fail event shows for a guaranteed
        // few seconds even if the next turn already started
        let celebrate_fail = self.sessions.values().any(|s| {
            s.last_end
                .map(|(r, at)| r.is_failure() && now.saturating_sub(at) < self.celebrate_ms)
                .unwrap_or(false)
        });
        if celebrate_fail {
            return Mode::Failed;
        }
        let celebrate_done = self.sessions.values().any(|s| {
            s.last_end
                .map(|(r, at)| r == TurnEndReason::Completed && now.saturating_sub(at) < self.celebrate_ms)
                .unwrap_or(false)
        });
        if celebrate_done {
            return Mode::Done;
        }
        let failed = self.sessions.values().any(|s| {
            s.last_end
                .map(|(r, at)| r.is_failure() && now.saturating_sub(at) < self.fail_ms)
                .unwrap_or(false)
        });
        if failed {
            return Mode::Failed;
        }
        if self.sessions.values().any(|s| !s.tools.is_empty()) {
            return Mode::Working;
        }
        if self.sessions.values().any(|s| s.turns > 0) {
            return Mode::Thinking;
        }
        let done = self.sessions.values().any(|s| {
            s.last_end
                .map(|(r, at)| r == TurnEndReason::Completed && now.saturating_sub(at) < self.done_ms)
                .unwrap_or(false)
        });
        if done {
            return Mode::Done;
        }
        Mode::Idle
    }

    fn all_sources_down(&self) -> bool {
        !self.source_health.is_empty() && self.source_health.values().all(|h| !h)
    }

    pub fn snapshot(&self) -> Snapshot {
        let mode = self.mode();
        let mut snap = Snapshot {
            mode,
            sources: self.source_health.clone(),
            pending_approvals: self
                .approvals
                .values()
                .map(|a| (a.session_id.clone(), a.tool.clone(), String::new()))
                .collect(),
            pending_questions: self
                .questions
                .values()
                .map(|q| (q.session_id.clone(), q.text.clone(), String::new()))
                .collect(),
            queue_len: self.queue_pending.values().sum(),
            ..Default::default()
        };
        let now = self.now_ms;
        for (id, s) in &self.sessions {
            let title = s.title.clone().unwrap_or_default();
            let task = if !title.is_empty() {
                Some(title.clone())
            } else {
                s.last_user_text.clone().or_else(|| {
                    s.todos
                        .iter()
                        .find(|t| t.status != "pending")
                        .map(|t| truncate_chars(&t.content, 120))
                })
            };
            if let Some(&earliest) = s.tool_since.values().min() {
                snap.working_since.insert(id.clone(), earliest);
            }
            let info = SessionInfo {
                session_id: id.clone(),
                source: s.source,
                title,
                tool: s.tools.iter().next().cloned(),
                tool_args: s.tools.iter().next().and_then(|t| s.tool_args.get(t).cloned()),
                task,
                todos: s.todos.clone(),
                live: s.live.clone(),
            };
            let failed = s
                .last_end
                .map(|(r, at)| r.is_failure() && now.saturating_sub(at) < self.fail_ms)
                .unwrap_or(false);
            let done = s
                .last_end
                .map(|(r, at)| r == TurnEndReason::Completed && now.saturating_sub(at) < self.done_ms)
                .unwrap_or(false);
            if failed {
                snap.failed.push(info);
            } else if done {
                snap.done.push(info);
            } else if !s.tools.is_empty() {
                snap.working.push(info);
            } else if s.turns > 0 {
                snap.thinking.push(info);
            }
        }
        snap
    }

    pub fn consume_sounds(&mut self) -> (bool, bool) {
        let r = (self.done_sound_pending, self.fail_sound_pending);
        self.done_sound_pending = false;
        self.fail_sound_pending = false;
        r
    }

    /// Bubble source selection (plan §5.4): among online sources pick the one
    /// with the highest activity level; tie -> the earliest-registered script
    /// (BTreeMap iterates in Source order, so Script(0) — i.e. DSH, registered
    /// first — wins ties, preserving the old "tie -> DSH" rule).
    pub fn select_bubble_source(&self) -> Option<Source> {
        let mut best: Option<(u8, Source)> = None;
        for (&source, &healthy) in &self.source_health {
            if !healthy {
                continue;
            }
            let active = self.sessions.values().any(|s| {
                s.source == source && (s.turns > 0 || !s.tools.is_empty() || s.waiting_user)
            });
            let level: u8 = if active { 2 } else { 1 };
            let better = match best {
                None => true,
                Some((bl, _)) => level > bl,
            };
            if better {
                best = Some((level, source));
            }
        }
        best.map(|(_, s)| s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> PetState {
        let mut p = PetState::new(DEFAULT_DONE_MS, DEFAULT_FAIL_MS);
        p.apply(StateEvent::SourceHealth { source: Source::Script(0), healthy: true });
        p.apply(StateEvent::SourceHealth { source: Source::Script(1), healthy: true });
        p
    }

    fn poll_item(id: &str, running: bool) -> SessionItem {
        SessionItem {
            session_id: id.into(),
            running,
            title: None,
            todos: None,
        }
    }

    #[test]
    fn offline_when_all_sources_down() {
        let mut p = base();
        p.apply(StateEvent::SourceHealth { source: Source::Script(0), healthy: false });
        p.apply(StateEvent::SourceHealth { source: Source::Script(1), healthy: false });
        assert_eq!(p.mode(), Mode::Offline);
    }

    #[test]
    fn one_source_up_keeps_pet_alive() {
        let mut p = base();
        p.apply(StateEvent::SourceHealth { source: Source::Script(0), healthy: false });
        assert_eq!(p.mode(), Mode::Idle);
    }

    #[test]
    fn priority_attention_over_working() {
        let mut p = base();
        p.apply(StateEvent::Poll {
            source: Source::Script(0),
            items: vec![poll_item("s1", true)],
            ok: true,
            error: None,
        });
        p.apply(StateEvent::TurnStarted { source: Source::Script(0), session_id: "s1".into(), turn: 1 });
        p.apply(StateEvent::ToolStarted { source: Source::Script(0), session_id: "s1".into(), name: "bash".into(), arguments: None });
        assert_eq!(p.mode(), Mode::Working);
        p.apply(StateEvent::ApprovalRequested {
            source: Source::Script(0),
            id: "ap1".into(),
            session_id: "s1".into(),
            tool: "bash".into(),
        });
        assert_eq!(p.mode(), Mode::Attention);
        p.apply(StateEvent::ApprovalResolved { source: Source::Script(0), id: "ap1".into() });
        assert_eq!(p.mode(), Mode::Working);
    }

    #[test]
    fn question_resolved_clears_whole_request() {
        let mut p = base();
        p.apply(StateEvent::TurnStarted { source: Source::Script(0), session_id: "s1".into(), turn: 1 });
        // one ask() request with two question items, keyed as the DSH
        // connector does: `<rpcId>\u{0}<itemId>`
        p.apply(StateEvent::QuestionRequested {
            source: Source::Script(0),
            id: "r1\u{0}q1".into(),
            session_id: "s1".into(),
            text: "继续吗?".into(),
        });
        p.apply(StateEvent::QuestionRequested {
            source: Source::Script(0),
            id: "r1\u{0}q2".into(),
            session_id: "s1".into(),
            text: "选哪个?".into(),
        });
        assert_eq!(p.mode(), Mode::Attention);
        // user answers -> question/resolved carries only the request rpcId
        p.apply(StateEvent::QuestionResolved { source: Source::Script(0), id: "r1".into() });
        assert_eq!(p.snapshot().pending_questions.len(), 0);
        assert_eq!(p.mode(), Mode::Thinking); // the running turn is back on top
    }

    #[test]
    fn working_then_thinking_on_tool_end() {
        let mut p = base();
        p.apply(StateEvent::TurnStarted { source: Source::Script(0), session_id: "s1".into(), turn: 1 });
        p.apply(StateEvent::ToolStarted { source: Source::Script(0), session_id: "s1".into(), name: "web".into(), arguments: None });
        assert_eq!(p.mode(), Mode::Working);
        p.apply(StateEvent::ToolEnded { source: Source::Script(0), session_id: "s1".into(), name: "web".into(), error: false });
        assert_eq!(p.mode(), Mode::Thinking);
    }

    #[test]
    fn tool_start_clears_stale_live_text() {
        let mut p = base();
        p.apply(StateEvent::TurnStarted { source: Source::Script(0), session_id: "s1".into(), turn: 1 });
        // thinking stream accumulated before the tool call
        p.apply(StateEvent::LiveText {
            source: Source::Script(0),
            session_id: "s1".into(),
            reasoning: Some("思考中…".into()),
            text: Some("让我先看看…".into()),
            tool_name: None,
        });
        let snap = p.snapshot();
        assert_eq!(snap.thinking[0].live.text, "让我先看看…");
        // tool starts -> Working; the stale pre-tool stream must be gone so
        // the bubble falls back to the ⚙ tool label instead of old think text
        p.apply(StateEvent::ToolStarted { source: Source::Script(0), session_id: "s1".into(), name: "bash".into(), arguments: None });
        assert_eq!(p.mode(), Mode::Working);
        let snap = p.snapshot();
        assert_eq!(snap.working.len(), 1);
        assert!(snap.working[0].live.text.is_empty());
        assert!(snap.working[0].live.reasoning.is_empty());
        // fresh content streamed after the tool started shows again
        p.apply(StateEvent::LiveText {
            source: Source::Script(0),
            session_id: "s1".into(),
            reasoning: None,
            text: Some("新内容".into()),
            tool_name: None,
        });
        let snap = p.snapshot();
        assert_eq!(snap.working[0].live.text, "新内容");
    }

    #[test]
    fn failed_window_expires() {
        let mut p = base();
        p.apply(StateEvent::TurnStarted { source: Source::Script(0), session_id: "s1".into(), turn: 1 });
        p.apply(StateEvent::TurnEnded {
            source: Source::Script(0),
            session_id: "s1".into(),
            turn: 1,
            reason: TurnEndReason::Error,
        });
        assert_eq!(p.mode(), Mode::Failed);
        p.now_ms = DEFAULT_FAIL_MS + 1;
        assert_eq!(p.mode(), Mode::Idle);
    }

    #[test]
    fn done_window_expires() {
        let mut p = base();
        p.apply(StateEvent::TurnStarted { source: Source::Script(0), session_id: "s1".into(), turn: 1 });
        p.apply(StateEvent::TurnEnded {
            source: Source::Script(0),
            session_id: "s1".into(),
            turn: 1,
            reason: TurnEndReason::Completed,
        });
        assert_eq!(p.mode(), Mode::Done);
        p.now_ms = DEFAULT_DONE_MS + 1;
        assert_eq!(p.mode(), Mode::Idle);
    }

    #[test]
    fn celebrate_done_shows_even_when_next_turn_starts() {
        let mut p = base();
        p.apply(StateEvent::TurnStarted { source: Source::Script(0), session_id: "s1".into(), turn: 1 });
        p.apply(StateEvent::TurnEnded {
            source: Source::Script(0),
            session_id: "s1".into(),
            turn: 1,
            reason: TurnEndReason::Completed,
        });
        // next turn starts immediately - normally thinking would mask done
        p.apply(StateEvent::TurnStarted { source: Source::Script(0), session_id: "s1".into(), turn: 2 });
        assert_eq!(p.mode(), Mode::Done); // celebration window wins
        p.now_ms = DEFAULT_CELEBRATE_MS + 1;
        assert_eq!(p.mode(), Mode::Thinking); // after celebration, priority resumes
    }

    #[test]
    fn celebrate_fail_beats_working() {
        let mut p = base();
        p.apply(StateEvent::TurnStarted { source: Source::Script(0), session_id: "s1".into(), turn: 1 });
        p.apply(StateEvent::TurnEnded {
            source: Source::Script(0),
            session_id: "s1".into(),
            turn: 1,
            reason: TurnEndReason::Error,
        });
        // another session keeps working
        p.apply(StateEvent::TurnStarted { source: Source::Script(0), session_id: "s2".into(), turn: 1 });
        p.apply(StateEvent::ToolStarted { source: Source::Script(0), session_id: "s2".into(), name: "bash".into(), arguments: None });
        assert_eq!(p.mode(), Mode::Failed);
        // celebration expired but the regular fail window (fail_ms) still
        // outranks working
        p.now_ms = DEFAULT_CELEBRATE_MS + 1;
        assert_eq!(p.mode(), Mode::Failed);
        // once the fail window itself expires, working takes over
        p.now_ms = DEFAULT_FAIL_MS + 1;
        assert_eq!(p.mode(), Mode::Working);
    }

    #[test]
    fn aborted_is_neutral() {
        let mut p = base();
        p.apply(StateEvent::TurnStarted { source: Source::Script(0), session_id: "s1".into(), turn: 1 });
        p.apply(StateEvent::TurnEnded {
            source: Source::Script(0),
            session_id: "s1".into(),
            turn: 1,
            reason: TurnEndReason::Aborted,
        });
        assert_eq!(p.mode(), Mode::Idle);
    }

    #[test]
    fn blocked_counts_as_attention_candidate() {
        let mut p = base();
        p.apply(StateEvent::TurnStarted { source: Source::Script(0), session_id: "s1".into(), turn: 1 });
        p.apply(StateEvent::TurnEnded {
            source: Source::Script(0),
            session_id: "s1".into(),
            turn: 1,
            reason: TurnEndReason::Blocked,
        });
        // blocked marks waiting_user; derive still needs the pending list to be
        // attention — synthetic pending question covers it in practice (mux).
        assert_eq!(p.mode(), Mode::Idle);
        let s = p.sessions.get("s1").unwrap();
        assert!(s.waiting_user);
        // and a poll saying running=false must NOT turn this into "done"
        p.apply(StateEvent::Poll {
            source: Source::Script(0),
            items: vec![poll_item("s1", false)],
            ok: true,
            error: None,
        });
        let s = p.sessions.get("s1").unwrap();
        assert!(!s.last_end.is_some_and(|(r, _)| r == TurnEndReason::Completed));
    }

    #[test]
    fn poll_running_false_is_done_fallback() {
        let mut p = base();
        p.apply(StateEvent::TurnStarted { source: Source::Script(0), session_id: "s1".into(), turn: 1 });
        p.apply(StateEvent::ToolStarted { source: Source::Script(0), session_id: "s1".into(), name: "bash".into(), arguments: None });
        assert_eq!(p.mode(), Mode::Working);
        // push gap: no turn/end event, poll flips to stopped
        p.apply(StateEvent::Poll {
            source: Source::Script(0),
            items: vec![poll_item("s1", false)],
            ok: true,
            error: None,
        });
        assert_eq!(p.mode(), Mode::Done);
    }

    #[test]
    fn ttl_cleans_old_approvals() {
        let mut p = base();
        p.apply(StateEvent::ApprovalRequested {
            source: Source::Script(0),
            id: "ap1".into(),
            session_id: "s1".into(),
            tool: "bash".into(),
        });
        assert_eq!(p.mode(), Mode::Attention);
        p.now_ms = TTL_APPROVAL_MS + 1;
        p.apply(StateEvent::Tick);
        assert_eq!(p.mode(), Mode::Idle);
    }

    #[test]
    fn pending_sync_clears_only_that_source() {
        let mut p = base();
        p.apply(StateEvent::QuestionRequested {
            source: Source::Script(0),
            id: "r1\u{0}q1".into(),
            session_id: "s1".into(),
            text: "继续?".into(),
        });
        p.apply(StateEvent::QuestionRequested {
            source: Source::Script(1),
            id: "call_h1".into(),
            session_id: "h9".into(),
            text: "确认?".into(),
        });
        p.apply(StateEvent::ApprovalRequested {
            source: Source::Script(0),
            id: "ap1".into(),
            session_id: "s1".into(),
            tool: "bash".into(),
        });
        assert_eq!(p.mode(), Mode::Attention);
        // server restarted: mux reconnect clears DSH pendings; the replay
        // re-adds whatever is still pending — here nothing DSH-side remains
        p.apply(StateEvent::PendingSync { source: Source::Script(0) });
        let snap = p.snapshot();
        assert_eq!(snap.pending_questions.len(), 1); // hermes clarify survives
        assert_eq!(snap.pending_approvals.len(), 0);
        assert_eq!(p.mode(), Mode::Attention);
        p.apply(StateEvent::PendingSync { source: Source::Script(1) });
        assert_eq!(p.mode(), Mode::Idle);
    }

    #[test]
    fn live_text_accumulates_and_clears_on_turn_end() {
        let mut p = base();
        p.apply(StateEvent::TurnStarted { source: Source::Script(0), session_id: "s1".into(), turn: 1 });
        p.apply(StateEvent::LiveText {
            source: Source::Script(0),
            session_id: "s1".into(),
            reasoning: Some("思考中…".into()),
            text: Some("hello".into()),
            tool_name: None,
        });
        let snap = p.snapshot();
        assert_eq!(snap.thinking[0].live.reasoning, "思考中…");
        p.apply(StateEvent::TurnStarted { source: Source::Script(0), session_id: "s1".into(), turn: 1 });
        p.apply(StateEvent::TurnEnded {
            source: Source::Script(0),
            session_id: "s1".into(),
            turn: 1,
            reason: TurnEndReason::Completed,
        });
        let snap = p.snapshot();
        assert!(snap.thinking.is_empty());
    }

    #[test]
    fn bubble_source_prefers_active_then_earliest_registered() {
        let mut p = base();
        // both idle -> earliest-registered script (Script(0), i.e. DSH)
        assert_eq!(p.select_bubble_source(), Some(Source::Script(0)));
        // script(1) active, script(0) idle -> active wins
        p.apply(StateEvent::TurnStarted { source: Source::Script(1), session_id: "h1".into(), turn: 1 });
        assert_eq!(p.select_bubble_source(), Some(Source::Script(1)));
        // both active -> tie, earliest-registered wins
        p.apply(StateEvent::TurnStarted { source: Source::Script(0), session_id: "d1".into(), turn: 1 });
        assert_eq!(p.select_bubble_source(), Some(Source::Script(0)));
        // script(0) offline -> script(1)
        p.apply(StateEvent::SourceHealth { source: Source::Script(0), healthy: false });
        assert_eq!(p.select_bubble_source(), Some(Source::Script(1)));
    }

    #[test]
    fn sessions_reaped_when_inactive_and_gone() {
        let mut p = base();
        p.apply(StateEvent::Poll {
            source: Source::Script(0),
            items: vec![poll_item("gone", false)],
            ok: true,
            error: None,
        });
        p.now_ms = DEFAULT_DONE_MS + 1;
        p.apply(StateEvent::Poll { source: Source::Script(0), items: vec![], ok: true, error: None });
        assert!(p.sessions.is_empty());
    }

    #[test]
    fn task_label_falls_back_to_user_message_and_todo() {
        let mut p = base();
        // no title: task = last user message
        p.apply(StateEvent::UserMessage { source: Source::Script(0), session_id: "s1".into(), text: "修复webp闪烁".into() });
        let snap = p.snapshot();
        assert_eq!(snap.done.len(), 0); // not done yet, just check the info via thinking? use a done session instead
        // make it a completed session and inspect the task through snapshot
        p.apply(StateEvent::TurnStarted { source: Source::Script(0), session_id: "s1".into(), turn: 1 });
        p.apply(StateEvent::TurnEnded { source: Source::Script(0), session_id: "s1".into(), turn: 1, reason: TurnEndReason::Completed });
        let snap = p.snapshot();
        assert_eq!(snap.done[0].task.as_deref(), Some("修复webp闪烁"));
        // todo fallback when no user message
        let mut p2 = base();
        p2.apply(StateEvent::TodoSnapshot {
            source: Source::Script(0),
            session_id: "s2".into(),
            todos: vec![TodoItem { content: "部署服务".into(), status: "in_progress".into() }],
        });
        p2.apply(StateEvent::TurnStarted { source: Source::Script(0), session_id: "s2".into(), turn: 1 });
        p2.apply(StateEvent::TurnEnded { source: Source::Script(0), session_id: "s2".into(), turn: 1, reason: TurnEndReason::Completed });
        let snap = p2.snapshot();
        assert_eq!(snap.done[0].task.as_deref(), Some("部署服务"));
    }

    #[test]
    fn mode_priority_table_is_stable() {
        // The priority order must match design doc §4.1 exactly.
        let order = [
            Mode::Offline,
            Mode::Attention,
            Mode::Failed,
            Mode::Working,
            Mode::Thinking,
            Mode::Done,
            Mode::Idle,
        ];
        for w in order.windows(2) {
            assert!(w[0].priority() > w[1].priority(), "{:?} > {:?}", w[0], w[1]);
        }
    }

    #[test]
    fn comfyui_run_maps_to_modes() {
        let mut p = base();
        p.apply(StateEvent::SourceHealth { source: Source::Script(0), healthy: true });
        // poll sets the running prompt + its bubble title (as the connector does)
        p.apply(StateEvent::Poll {
            source: Source::Script(0),
            items: vec![SessionItem {
                session_id: "p1".into(),
                running: true,
                title: Some("出图任务 #1".into()),
                todos: None,
            }],
            ok: true,
            error: None,
        });
        p.apply(StateEvent::TurnStarted { source: Source::Script(0), session_id: "p1".into(), turn: 1 });
        assert_eq!(p.mode(), Mode::Thinking);
        p.apply(StateEvent::ToolStarted {
            source: Source::Script(0),
            session_id: "p1".into(),
            name: "KSampler".into(),
            arguments: None,
        });
        assert_eq!(p.mode(), Mode::Working);
        p.apply(StateEvent::ToolEnded {
            source: Source::Script(0),
            session_id: "p1".into(),
            name: "KSampler".into(),
            error: false,
        });
        p.apply(StateEvent::TurnEnded {
            source: Source::Script(0),
            session_id: "p1".into(),
            turn: 1,
            reason: TurnEndReason::Completed,
        });
        assert_eq!(p.mode(), Mode::Done);
        // snapshot bucket lands the finished prompt in done
        let snap = p.snapshot();
        assert_eq!(snap.done.len(), 1);
        assert_eq!(snap.done[0].source, Source::Script(0));
        assert_eq!(snap.done[0].title, "出图任务 #1");
    }

    #[test]
    fn comfyui_failure_and_interrupt() {
        let mut p = base();
        p.apply(StateEvent::SourceHealth { source: Source::Script(0), healthy: true });
        p.apply(StateEvent::TurnStarted { source: Source::Script(0), session_id: "p1".into(), turn: 1 });
        p.apply(StateEvent::TurnEnded {
            source: Source::Script(0),
            session_id: "p1".into(),
            turn: 1,
            reason: TurnEndReason::Error,
        });
        assert_eq!(p.mode(), Mode::Failed);
        // interrupted is neutral (like aborted): no celebration
        p.now_ms = DEFAULT_FAIL_MS + 1;
        p.apply(StateEvent::TurnStarted { source: Source::Script(0), session_id: "p2".into(), turn: 1 });
        p.apply(StateEvent::TurnEnded {
            source: Source::Script(0),
            session_id: "p2".into(),
            turn: 1,
            reason: TurnEndReason::Interrupted,
        });
        assert_eq!(p.mode(), Mode::Idle);
    }

    #[test]
    fn queue_depth_aggregates_across_sources() {
        let mut p = base();
        p.apply(StateEvent::QueueChanged { source: Source::Script(0), pending: 3 });
        p.apply(StateEvent::QueueChanged { source: Source::Script(2), pending: 2 });
        assert_eq!(p.snapshot().queue_len, 5);
        p.apply(StateEvent::QueueChanged { source: Source::Script(0), pending: 0 });
        assert_eq!(p.snapshot().queue_len, 2);
    }

    #[test]
    fn comfyui_offline_only_when_everything_down() {
        let mut p = base();
        p.apply(StateEvent::SourceHealth { source: Source::Script(0), healthy: true });
        assert_eq!(p.mode(), Mode::Idle);
        p.apply(StateEvent::SourceHealth { source: Source::Script(0), healthy: false });
        p.apply(StateEvent::SourceHealth { source: Source::Script(0), healthy: false });
        p.apply(StateEvent::SourceHealth { source: Source::Script(1), healthy: false });
        assert_eq!(p.mode(), Mode::Offline);
    }
}
