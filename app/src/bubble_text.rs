//! Bubble text assembly (pure): plan §5.4 — backend indication + live model
//! output; design doc §4.3 for per-state wording.

use crate::state::{Mode, SessionInfo, Snapshot, Source};

pub const MAX_LINE: usize = 120;

/// 轮流显示: how long a bubble message may stay unchanged before the pet
/// hands the bubble to another session that has content.
pub const ROTATE_AFTER_MS: u64 = 5000;

/// Structured bubble content: a header row (state title left, "From
/// <client>" right-aligned), a 1px divider, and the message stream below
/// it. The bubble renderer (gui::bubble) owns the geometry; this is the
/// pure content model.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct BubbleText {
    /// Header title (left-aligned), e.g. "思考中…".
    pub title: String,
    /// Client name for the right-aligned "From <name>" suffix; None = none.
    pub from: Option<String>,
    /// Message lines below the divider (no "[<src>] " tag prefix).
    pub lines: Vec<String>,
}

fn truncate(s: &str, n: usize) -> String {
    let v: Vec<char> = s.chars().collect();
    if v.len() <= n {
        s.to_string()
    } else {
        let mut t: String = v[..n].iter().collect();
        t.push('…');
        t
    }
}

/// Keep the TAIL of a streaming text: live content grows from the front, so
/// the head is static once it exceeds the limit - showing the tail means the
/// bubble keeps scrolling with the newest content.
fn truncate_tail(s: &str, n: usize) -> String {
    let v: Vec<char> = s.chars().collect();
    if v.len() <= n {
        s.to_string()
    } else {
        let mut t = String::from("…");
        t.extend(v[v.len() - n..].iter());
        t
    }
}

/// Pick the session whose live text the bubble shows for the active modes
/// (same rule for the plain line and the typewriter reveal). `prefer` pins
/// the pick to one session (rotation); it is honoured only while that
/// session is still in the candidate list.
fn pick_session<'a>(snap: &'a Snapshot, source: Option<Source>, mode: Mode, prefer: Option<&str>) -> Option<&'a SessionInfo> {
    let sessions: Vec<&SessionInfo> = match mode {
        Mode::Working => snap.working.iter().collect(),
        Mode::Thinking => snap.thinking.iter().collect(),
        _ => return None,
    };
    let sessions: Vec<&SessionInfo> = sessions
        .into_iter()
        .filter(|s| source.map(|x| s.source == x).unwrap_or(true))
        .collect();
    if let Some(p) = prefer {
        // the rotation pick may live in EITHER list (working ∪ thinking)
        let mut all = snap.working.iter().chain(snap.thinking.iter());
        if let Some(s) = all.find(|s| s.session_id == p && source.map(|x| s.source == x).unwrap_or(true)) {
            return Some(s);
        }
    }
    sessions
        .iter()
        .find(|s| !s.live.text.is_empty() || !s.live.reasoning.is_empty())
        .copied()
        .or_else(|| {
            // 谁在干活显示谁: among equally-streamless sessions prefer the
            // one whose current work started EARLIEST (deepest into the task —
            // e.g. a long-running build beats a freshly-started short tool);
            // sessions without tools (thinking) keep list order.
            sessions
                .iter()
                .min_by_key(|s| snap.working_since.get(&s.session_id).copied().unwrap_or(u64::MAX))
                .copied()
        })
}

/// Sessions with visible content (live stream or a running tool) from both
/// the mode's primary list and the other list — the rotation candidate pool.
fn rotation_candidates<'a>(snap: &'a Snapshot, source: Option<Source>, mode: Mode) -> Vec<&'a SessionInfo> {
    let mut v: Vec<&SessionInfo> = match mode {
        Mode::Working => snap.working.iter().chain(snap.thinking.iter()).collect(),
        Mode::Thinking => snap.thinking.iter().chain(snap.working.iter()).collect(),
        _ => return Vec::new(),
    };
    v.retain(|s| !s.live.text.is_empty() || !s.live.reasoning.is_empty() || s.tool.is_some());
    v.retain(|s| source.map(|x| s.source == x).unwrap_or(true));
    v
}

fn best_of<'a>(cands: &[&'a SessionInfo], snap: &Snapshot) -> Option<&'a SessionInfo> {
    cands
        .iter()
        .find(|s| !s.live.text.is_empty() || !s.live.reasoning.is_empty())
        .copied()
        .or_else(|| {
            cands
                .iter()
                .min_by_key(|s| snap.working_since.get(&s.session_id).copied().unwrap_or(u64::MAX))
                .copied()
        })
}

/// 轮流显示: keep `current` while its message keeps changing; once it has
/// been static for ROTATE_AFTER_MS (`stale`) and another session has content,
/// hand the bubble over to that session. Returns the session id to show.
pub fn rotate_pick(
    snap: &Snapshot,
    source: Option<Source>,
    mode: Mode,
    current: Option<&str>,
    stale: bool,
) -> Option<String> {
    let cands = rotation_candidates(snap, source, mode);
    if cands.is_empty() {
        return None;
    }
    let current_valid = current
        .map(|c| cands.iter().any(|s| s.session_id == c))
        .unwrap_or(false);
    if !current_valid {
        return best_of(&cands, snap).map(|s| s.session_id.clone());
    }
    if !stale {
        return current.map(|c| c.to_string());
    }
    let others: Vec<&SessionInfo> = cands
        .iter()
        .filter(|s| s.session_id != current.unwrap())
        .copied()
        .collect();
    if others.is_empty() {
        return current.map(|c| c.to_string());
    }
    best_of(&others, snap).map(|s| s.session_id.clone())
}

/// Identity + length of the live stream the bubble currently renders, for
/// the typewriter reveal cursor. `None` = nothing is being typed (no live
/// text, or a tool is shown instead).
pub struct LiveStream {
    pub session_id: String,
    /// 0 = reasoning stream (🧠), 1 = text stream (💬).
    pub kind: u8,
    pub len: usize,
}

/// Live stream identity of the picked session; `prefer` pins the pick (rotation).
pub fn live_stream_pinned(
    snap: &Snapshot,
    source: Option<Source>,
    mode: Mode,
    prefer: Option<&str>,
) -> Option<LiveStream> {
    let w = pick_session(snap, source, mode, prefer)?;
    let prefer_text = mode == Mode::Working;
    // NOTE: the live stream is followed even while a tool runs — the pet
    // shows the DSH output/reasoning that keeps streaming, not a frozen
    // tool label (the tool is only a fallback when nothing has streamed).
    let (kind, s): (u8, &str) = if prefer_text {
        if !w.live.text.is_empty() {
            (1, &w.live.text)
        } else if !w.live.reasoning.is_empty() {
            (0, &w.live.reasoning)
        } else {
            return None;
        }
    } else if !w.live.reasoning.is_empty() {
        (0, &w.live.reasoning)
    } else if !w.live.text.is_empty() {
        (1, &w.live.text)
    } else {
        return None;
    };
    Some(LiveStream { session_id: w.session_id.clone(), kind, len: s.chars().count() })
}

/// Structured bubble content for the current snapshot: header title,
/// optional right-aligned client name ("From DSH") and the stream below
/// the divider. Working/thinking show the state label + the live stream;
/// other states show their plain multi-line content (source tags stay in
/// the item lines).
pub fn bubble_text_pinned(
    snap: &Snapshot,
    source: Option<Source>,
    prefer: Option<&str>,
    reveal: Option<usize>,
    max_line: usize,
) -> BubbleText {
    match snap.mode {
        Mode::Working | Mode::Thinking => {
            let src = source.unwrap_or(Source::Dsh);
            let (title, content) = live_parts(snap, source, snap.mode, prefer, reveal, max_line);
            let lines = content.map(|c| vec![c]).unwrap_or_default();
            BubbleText { title, from: Some(src.label().to_string()), lines }
        }
        _ => plain_bubble(snap, source),
    }
}

/// Structured bubble content for the non-streaming states.
fn plain_bubble(snap: &Snapshot, source: Option<Source>) -> BubbleText {
    match snap.mode {
        Mode::Offline => BubbleText {
            title: "连不上 DSH 和 Hermes 😢".to_string(),
            from: None,
            lines: vec!["自动重试中…".to_string()],
        },
        Mode::Attention => BubbleText {
            title: format!(
                "需要你确认 · {} 项",
                snap.pending_approvals.len() + snap.pending_questions.len()
            ),
            from: None,
            lines: {
                let mut out = Vec::new();
                for (sid, text, _) in &snap.pending_questions {
                    out.push(format!("❓ {}: {}", session_label(snap, sid), truncate(text, MAX_LINE)));
                }
                for (sid, tool, _) in &snap.pending_approvals {
                    out.push(format!("🔧 {}: 请求使用 {tool}", session_label(snap, sid)));
                }
                out
            },
        },
        Mode::Failed => BubbleText {
            title: "出错了!".to_string(),
            from: None,
            lines: snap
                .failed
                .iter()
                .map(|f| format!("✗ [{}] {}", f.source.label(), task_label(f)))
                .collect(),
        },
        Mode::Done => BubbleText {
            title: "任务完成啦 🎉".to_string(),
            from: None,
            lines: snap
                .done
                .iter()
                .map(|d| format!("✓ [{}] {}", d.source.label(), task_label(d)))
                .collect(),
        },
        Mode::Idle => {
            let src = source.unwrap_or(Source::Dsh);
            let lines = if snap.queue_len > 0 {
                vec![format!("队列中还有 {} 个任务待处理", snap.queue_len)]
            } else {
                vec!["没有运行中的任务".to_string()]
            };
            BubbleText { title: "休息中 💤".to_string(), from: Some(src.label().to_string()), lines }
        }
        Mode::Move => BubbleText { title: "拖动中…".to_string(), from: None, lines: Vec::new() },
        _ => BubbleText::default(),
    }
}

/// (状态标题, 实际内容) for the active work states: the state label
/// ("思考中…"/"正在干活…") plus the live stream content WITHOUT the
/// "[<src>] " tag prefix — the bubble renderer puts the title and the
/// "From <client>" suffix into the header row itself. `None` content means
/// no session is picked (the caller falls back to the title alone).
fn live_parts(
    snap: &Snapshot,
    source: Option<Source>,
    mode: Mode,
    prefer: Option<&str>,
    reveal: Option<usize>,
    max_line: usize,
) -> (String, Option<String>) {
    let title = if mode == Mode::Working { "正在干活…" } else { "思考中…" };
    let Some(w) = pick_session(snap, source, mode, prefer) else {
        return (title.to_string(), None);
    };
    let content = if mode == Mode::Working {
        if w.live.text.is_empty() && w.live.reasoning.is_empty() {
            match &w.tool {
                Some(t) => match &w.tool_args {
                    Some(args) if !args.is_empty() => {
                        format!("⚙ {t}: {}", truncate_tail(args, max_line.saturating_sub(24)))
                    }
                    _ => format!("⚙ 正在执行: {t}"),
                },
                None => "正在干活…".to_string(),
            }
        } else {
            live_reveal(w, true, reveal, max_line)
        }
    } else {
        live_reveal(w, false, reveal, max_line)
    };
    (title.to_string(), Some(content))
}

/// Render the live stream of a session (working prefers 💬 text, thinking
/// prefers 🧠 reasoning), revealing at most `reveal` chars when given. Keep
/// at most the last `max_line` chars visible so long streams tail-scroll
/// instead of freezing a stale head.
fn live_reveal(w: &SessionInfo, prefer_text: bool, reveal: Option<usize>, max_line: usize) -> String {
    let pick = |s: &str| match reveal {
        Some(r) if r < s.chars().count() => reveal_tail(s, r, max_line),
        _ => truncate_tail(s, max_line),
    };
    if prefer_text {
        if !w.live.text.is_empty() {
            format!("💬 {}", pick(&w.live.text))
        } else if !w.live.reasoning.is_empty() {
            format!("🧠 {}", pick(&w.live.reasoning))
        } else {
            "正在干活…".to_string()
        }
    } else if !w.live.reasoning.is_empty() {
        format!("🧠 {}", pick(&w.live.reasoning))
    } else if !w.live.text.is_empty() {
        format!("💬 {}", pick(&w.live.text))
    } else {
        "思考中…".to_string()
    }
}

/// Tail-truncated view of the first `reveal` chars of a stream: the line
/// grows one character at a time; once past the cap it scrolls like the
/// plain tail view.
fn reveal_tail(s: &str, reveal: usize, n: usize) -> String {
    let v: Vec<char> = s.chars().collect();
    let r = reveal.min(v.len());
    if r <= n {
        v[..r].iter().collect()
    } else {
        let mut t = String::from("…");
        t.extend(v[r - n..r].iter());
        t
    }
}

/// Task name shown in done/failed bubbles: title -> last user message ->
/// first todo item -> placeholder.
fn task_label(s: &SessionInfo) -> String {
    if let Some(t) = &s.task {
        if !t.trim().is_empty() {
            return truncate(t, 24);
        }
    }
    if !s.title.is_empty() {
        return truncate(&s.title, 24);
    }
    "(未命名会话)".to_string()
}

fn title_or(t: &str) -> String {
    if t.is_empty() {
        "(未命名会话)".to_string()
    } else {
        truncate(t, 24)
    }
}

/// Human-readable label for a pending question/approval session: its title
/// when known (from the active/done/failed session lists), else the raw id.
fn session_label(snap: &Snapshot, sid: &str) -> String {
    let title = snap
        .working
        .iter()
        .chain(&snap.thinking)
        .chain(&snap.done)
        .chain(&snap.failed)
        .find(|s| s.session_id == sid)
        .map(|s| s.title.as_str())
        .unwrap_or("");
    title_or(if title.is_empty() { sid } else { title })
}

#[cfg(test)]
#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{LiveText, Mode, SessionInfo, Snapshot, Source};

    fn snap(mode: Mode) -> Snapshot {
        Snapshot { mode, ..Default::default() }
    }

    fn sess(id: &str, src: Source) -> SessionInfo {
        SessionInfo {
            session_id: id.into(),
            source: src,
            title: "t".into(),
            tool: None,
            tool_args: None,
            task: None,
            todos: vec![],
            live: LiveText::default(),
        }
    }

    fn bt(s: &Snapshot, source: Option<Source>) -> BubbleText {
        bubble_text_pinned(s, source, None, None, MAX_LINE)
    }

    #[test]
    fn attention_question_shows_session_title() {
        let mut s = snap(Mode::Attention);
        s.thinking.push(SessionInfo {
            title: "继续中断的对话".into(),
            ..sess("mswfow6bou90rb", Source::Hermes)
        });
        s.pending_questions.push(("mswfow6bou90rb".into(), "如何处理?（重启 / 放弃）".into(), String::new()));
        let b = bt(&s, Some(Source::Hermes));
        assert!(b.title.contains("需要你确认 · 1 项"));
        assert!(b.lines.iter().any(|x| x.contains("继续中断的对话")));
        assert!(!b.lines.iter().any(|x| x.contains("mswfow6bou90rb")));
        // unknown session id falls back to the raw id
        s.pending_questions.push(("unknown-id".into(), "继续吗?".into(), String::new()));
        let b = bt(&s, Some(Source::Hermes));
        assert!(b.lines.iter().any(|x| x.contains("unknown-id")));
    }

    #[test]
    fn offline_text() {
        let b = bt(&snap(Mode::Offline), None);
        assert!(b.title.contains("连不上"));
        assert_eq!(b.from, None);
    }

    #[test]
    fn working_shows_backend_and_tool() {
        let mut s = snap(Mode::Working);
        s.working.push(SessionInfo { tool: Some("bash".into()), ..sess("s1", Source::Dsh) });
        let b = bt(&s, Some(Source::Dsh));
        assert_eq!(b.title, "正在干活…");
        assert_eq!(b.from.as_deref(), Some("DSH"));
        assert_eq!(b.lines.len(), 1); // one line at a time
        assert!(b.lines[0].contains("bash"));
    }

    #[test]
    fn working_pick_prefers_earliest_started_session() {
        // two sessions both running tools with no live text: the one whose
        // work started earliest (deepest into the task) wins the bubble,
        // not whichever session id sorts first
        let mut s = snap(Mode::Working);
        s.working.push(SessionInfo { tool: Some("read".into()), ..sess("zz-new", Source::Dsh) });
        s.working.push(SessionInfo { tool: Some("bash".into()), ..sess("aa-old", Source::Dsh) });
        s.working_since.insert("zz-new".into(), 2000);
        s.working_since.insert("aa-old".into(), 1000);
        let b = bt(&s, Some(Source::Dsh));
        assert!(b.lines[0].contains("bash"), "expected earliest-started tool, got: {}", b.lines[0]);
        // live text still outranks the earliest-start rule
        s.working[0].live.text = "新流出的内容".into();
        let b = bt(&s, Some(Source::Dsh));
        assert!(b.lines[0].contains("新流出的内容"));
    }

    #[test]
    fn rotate_pick_keeps_fresh_and_moves_stale() {
        let mut s = snap(Mode::Working);
        s.working.push(SessionInfo { tool: Some("bash".into()), ..sess("a", Source::Dsh) });
        s.working.push(SessionInfo { tool: Some("read".into()), ..sess("b", Source::Dsh) });
        s.working_since.insert("a".into(), 1000);
        s.working_since.insert("b".into(), 2000);
        // fresh message: the current session stays
        assert_eq!(rotate_pick(&s, Some(Source::Dsh), Mode::Working, Some("a"), false), Some("a".into()));
        // stale + another candidate: moves to it
        assert_eq!(rotate_pick(&s, Some(Source::Dsh), Mode::Working, Some("a"), true), Some("b".into()));
        // stale but the current session is the only candidate: stays
        let mut s1 = snap(Mode::Working);
        s1.working.push(SessionInfo { tool: Some("bash".into()), ..sess("a", Source::Dsh) });
        assert_eq!(rotate_pick(&s1, Some(Source::Dsh), Mode::Working, Some("a"), true), Some("a".into()));
        // no valid current: normal preference (live text first, else earliest)
        assert_eq!(rotate_pick(&s, Some(Source::Dsh), Mode::Working, None, false), Some("a".into()));
        // a thinking session with live text is a rotation candidate too
        s.thinking.push(SessionInfo {
            live: LiveText { reasoning: "在想事情…".into(), ..LiveText::default() },
            ..sess("c", Source::Dsh)
        });
        assert_eq!(rotate_pick(&s, Some(Source::Dsh), Mode::Working, Some("a"), true), Some("c".into()));
        // pinned rendering follows the rotation pick
        let b = bubble_text_pinned(&s, Some(Source::Dsh), Some("c"), None, MAX_LINE);
        assert!(b.lines[0].contains("在想事情"));
    }

    #[test]
    fn working_prefers_live_stream_over_running_tool() {
        // a running tool no longer hides the DSH stream: while text keeps
        // streaming in, the pet shows it (the tool is only a fallback when
        // nothing has streamed yet)
        let mut s = snap(Mode::Working);
        s.working.push(SessionInfo {
            tool: Some("bash".into()),
            live: LiveText { reasoning: String::new(), text: "正在输出的正文".into(), tool_name: None },
            ..sess("s1", Source::Dsh)
        });
        let b = bt(&s, Some(Source::Dsh));
        assert_eq!(b.lines.len(), 1);
        assert!(b.lines[0].contains("正在输出的正文"));
        assert!(!b.lines[0].contains("bash"));
    }

    #[test]
    fn working_shows_tool_arguments() {
        let mut s = snap(Mode::Working);
        s.working.push(SessionInfo {
            tool: Some("bash".into()),
            tool_args: Some("ls -la /tmp".into()),
            ..sess("s1", Source::Dsh)
        });
        let b = bt(&s, Some(Source::Dsh));
        assert!(b.lines[0].contains("bash"));
        assert!(b.lines[0].contains("ls -la /tmp"));
    }

    #[test]
    fn stream_shows_tail_not_frozen_head() {
        let mut s = snap(Mode::Thinking);
        let long = "开头".repeat(200); // > MAX_LINE chars
        s.thinking.push(SessionInfo {
            live: LiveText { reasoning: long.clone(), text: String::new(), tool_name: None },
            ..sess("s1", Source::Dsh)
        });
        let l1 = bt(&s, Some(Source::Dsh)).lines;
        let mut s2 = snap(Mode::Thinking);
        s2.thinking.push(SessionInfo {
            live: LiveText { reasoning: format!("{long}新增内容"), text: String::new(), tool_name: None },
            ..sess("s1", Source::Dsh)
        });
        let l2 = bt(&s2, Some(Source::Dsh)).lines;
        // the visible line changes as the stream grows (tail scrolling)
        assert_ne!(l1[0], l2[0]);
        assert!(l2[0].contains("新增内容"));
        assert!(l2[0].starts_with("🧠 …"));
    }

    #[test]
    fn done_bubble_shows_task_name() {
        let mut s = snap(Mode::Done);
        s.done.push(SessionInfo {
            task: Some("修复webp闪烁像素".into()),
            ..sess("s1", Source::Dsh)
        });
        let b = bt(&s, Some(Source::Dsh));
        assert!(b.title.contains("任务完成"));
        assert!(b.lines.iter().any(|x| x.contains("修复webp闪烁像素")));
    }

    #[test]
    fn idle_shows_pending_queue() {
        let mut s = snap(Mode::Idle);
        crate::state::register_script_label(0, "ComfyUI".to_string());
        s.queue_len = 3;
        let b = bt(&s, Some(Source::Script(0)));
        assert_eq!(b.title, "休息中 💤");
        assert_eq!(b.from.as_deref(), Some("ComfyUI"));
        assert!(b.lines[0].contains("3"));
        // empty queue falls back to the plain idle line
        let s2 = snap(Mode::Idle);
        let b2 = bt(&s2, Some(Source::Dsh));
        assert!(b2.lines[0].contains("没有运行中的任务"));
    }

    #[test]
    fn thinking_prefers_reasoning_stream() {
        let mut s = snap(Mode::Thinking);
        s.thinking.push(SessionInfo {
            live: LiveText { reasoning: "思考中文字流…".into(), text: String::new(), tool_name: None },
            ..sess("s1", Source::Hermes)
        });
        let b = bt(&s, Some(Source::Hermes));
        assert_eq!(b.lines.len(), 1);
        assert!(b.lines[0].contains("思考中文字流"));
    }

    #[test]
    fn working_filters_other_source_when_selected() {
        let mut s = snap(Mode::Working);
        s.working.push(SessionInfo { tool: Some("t1".into()), ..sess("d1", Source::Dsh) });
        s.working.push(SessionInfo { tool: Some("t2".into()), ..sess("h1", Source::Hermes) });
        let b = bt(&s, Some(Source::Hermes));
        assert!(b.lines[0].contains("t2"));
        assert!(!b.lines[0].contains("t1"));
    }

    #[test]
    fn long_text_truncated() {
        let mut s = snap(Mode::Thinking);
        s.thinking.push(SessionInfo {
            live: LiveText { reasoning: "x".repeat(500), text: String::new(), tool_name: None },
            ..sess("s1", Source::Dsh)
        });
        let b = bt(&s, Some(Source::Dsh));
        assert!(b.lines[0].contains('…'));
    }

    #[test]
    fn reveal_types_out_char_by_char() {
        let mut s = snap(Mode::Thinking);
        s.thinking.push(SessionInfo {
            live: LiveText { reasoning: "正在推导方案".into(), text: String::new(), tool_name: None },
            ..sess("s1", Source::Hermes)
        });
        let b0 = bubble_text_pinned(&s, Some(Source::Hermes), None, Some(0), MAX_LINE);
        assert_eq!(b0.lines[0], "🧠 ");
        let b2 = bubble_text_pinned(&s, Some(Source::Hermes), None, Some(2), MAX_LINE);
        assert!(b2.lines[0].ends_with("正在"));
        assert!(!b2.lines[0].contains("推导"));
        // reveal >= len -> identical to the plain line
        let full = bt(&s, Some(Source::Hermes));
        let lfull = bubble_text_pinned(&s, Some(Source::Hermes), None, Some(100), MAX_LINE);
        assert_eq!(lfull, full);
        assert!(full.lines[0].ends_with("正在推导方案"));
    }

    #[test]
    fn reveal_scrolls_after_cap() {
        let mut s = snap(Mode::Thinking);
        let long: String = "字".repeat(200); // > MAX_LINE
        s.thinking.push(SessionInfo {
            live: LiveText { reasoning: long.clone(), text: String::new(), tool_name: None },
            ..sess("s1", Source::Dsh)
        });
        // revealed 150 of 200 chars: shows the tail of the revealed prefix,
        // i.e. chars [150-120, 150) with a leading ellipsis
        let b = bubble_text_pinned(&s, Some(Source::Dsh), None, Some(150), MAX_LINE);
        assert!(b.lines[0].starts_with("🧠 …"));
        assert!(b.lines[0].contains('字'));
        assert!(b.lines[0].chars().count() < 200);
    }

    #[test]
    fn max_chars_window_keeps_more_than_bubble() {
        // a 300-char stream: the wide window (text.max_chars) keeps all of
        // it, while the 120-char bubble window tail-truncates
        let mut s = snap(Mode::Thinking);
        let long: String = "字".repeat(300);
        s.thinking.push(SessionInfo {
            live: LiveText { reasoning: long.clone(), text: String::new(), tool_name: None },
            ..sess("s1", Source::Dsh)
        });
        let b = bubble_text_pinned(&s, Some(Source::Dsh), None, None, 120);
        let w = bubble_text_pinned(&s, Some(Source::Dsh), None, None, 300);
        assert_eq!(w.lines[0].chars().count() - "🧠 ".chars().count(), 300);
        assert!(w.lines[0].chars().count() > b.lines[0].chars().count());
        assert!(!w.lines[0].contains('…')); // nothing dropped yet
    }

    #[test]
    fn bubble_text_header_title_from_and_stream() {
        let mut s = snap(Mode::Thinking);
        s.thinking.push(SessionInfo {
            live: LiveText { reasoning: "正在推导…".into(), text: String::new(), tool_name: None },
            ..sess("s1", Source::Hermes)
        });
        let b = bubble_text_pinned(&s, Some(Source::Hermes), None, None, MAX_LINE);
        assert_eq!(b.title, "思考中…");
        assert_eq!(b.from.as_deref(), Some("Hermes"));
        assert_eq!(b.lines, vec!["🧠 正在推导…"]);
        // no session picked: title only, no stream lines
        let s2 = snap(Mode::Thinking);
        let b = bubble_text_pinned(&s2, Some(Source::Dsh), None, None, MAX_LINE);
        assert_eq!(b.title, "思考中…");
        assert_eq!(b.from.as_deref(), Some("DSH"));
        assert!(b.lines.is_empty());
    }

    #[test]
    fn bubble_text_working_tool_below_header() {
        let mut s = snap(Mode::Working);
        s.working.push(SessionInfo {
            tool: Some("bash".into()),
            tool_args: Some("ls -la /tmp".into()),
            ..sess("s1", Source::Dsh)
        });
        let b = bubble_text_pinned(&s, Some(Source::Dsh), None, None, MAX_LINE);
        assert_eq!(b.title, "正在干活…");
        assert_eq!(b.from.as_deref(), Some("DSH"));
        assert!(b.lines[0].contains("bash"));
        assert!(b.lines[0].contains("ls -la /tmp"));
    }

    #[test]
    fn bubble_text_done_shows_title_and_items() {
        let mut s = snap(Mode::Done);
        s.done.push(SessionInfo {
            task: Some("修复webp闪烁像素".into()),
            ..sess("s1", Source::Dsh)
        });
        let b = bubble_text_pinned(&s, Some(Source::Dsh), None, None, MAX_LINE);
        assert_eq!(b.title, "任务完成啦 🎉");
        assert_eq!(b.from, None);
        assert!(b.lines[0].contains("修复webp闪烁像素"));
        assert!(b.lines[0].contains("[DSH]"));
    }

    #[test]
    fn bubble_text_idle_has_from() {
        let mut s = snap(Mode::Idle);
        crate::state::register_script_label(0, "ComfyUI".to_string());
        s.queue_len = 3;
        let b = bubble_text_pinned(&s, Some(Source::Script(0)), None, None, MAX_LINE);
        assert_eq!(b.title, "休息中 💤");
        assert_eq!(b.from.as_deref(), Some("ComfyUI"));
        assert!(b.lines[0].contains("3"));
    }

    #[test]
    fn live_stream_kind_matches_displayed_stream() {
        let mut s = snap(Mode::Thinking);
        s.thinking.push(SessionInfo {
            live: LiveText { reasoning: "推导".into(), text: "正文".into(), tool_name: None },
            ..sess("s1", Source::Dsh)
        });
        // thinking shows reasoning first
        let st = live_stream_pinned(&s, Some(Source::Dsh), Mode::Thinking, None).unwrap();
        assert_eq!(st.kind, 0);
        assert_eq!(st.len, 2);
        assert_eq!(st.session_id, "s1");
        // working shows text first
        let mut w = snap(Mode::Working);
        w.working.push(SessionInfo {
            live: LiveText { reasoning: "推导".into(), text: "正文".into(), tool_name: None },
            ..sess("s1", Source::Dsh)
        });
        let st = live_stream_pinned(&w, Some(Source::Dsh), Mode::Working, None).unwrap();
        assert_eq!(st.kind, 1);
    }

    #[test]
    fn live_stream_follows_live_content_even_with_tool() {
        // working with a running tool: the live reasoning is still streamed
        // (working prefers text, then reasoning)
        let mut s = snap(Mode::Working);
        s.working.push(SessionInfo {
            tool: Some("bash".into()),
            live: LiveText { reasoning: "推导".into(), text: String::new(), tool_name: None },
            ..sess("s1", Source::Dsh)
        });
        let st = live_stream_pinned(&s, Some(Source::Dsh), Mode::Working, None).unwrap();
        assert_eq!(st.kind, 0);
        assert_eq!(st.len, 2);
        // no live content at all -> still None (tool is only a fallback)
        s.working[0].live = LiveText::default();
        assert!(live_stream_pinned(&s, Some(Source::Dsh), Mode::Working, None).is_none());
        // thinking session without any live text yet
        let mut t = snap(Mode::Thinking);
        t.thinking.push(sess("s1", Source::Dsh));
        assert!(live_stream_pinned(&t, Some(Source::Dsh), Mode::Thinking, None).is_none());
    }
}
