//! Bubble text assembly (pure): plan §5.4 — backend indication + live model
//! output; design doc §4.3 for per-state wording.

use crate::state::{Mode, SessionInfo, Snapshot, Source};

pub const MAX_LINE: usize = 120;

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

/// Build bubble lines for the given snapshot, showing the selected source.
/// `source=None` (all offline) -> offline message.
pub fn bubble_lines(snap: &Snapshot, source: Option<Source>) -> Vec<String> {
    match snap.mode {
        Mode::Offline => {
            vec!["连不上 DSH 和 Hermes 😢".to_string(), "自动重试中…".to_string()]
        }
        Mode::Attention => {
            let mut out = vec![format!("需要你确认 · {} 项", snap.pending_approvals.len() + snap.pending_questions.len())];
            for (sid, text, _) in &snap.pending_questions {
                out.push(format!("❓ {}: {}", session_label(snap, sid), truncate(text, MAX_LINE)));
            }
            for (sid, tool, _) in &snap.pending_approvals {
                out.push(format!("🔧 {}: 请求使用 {tool}", session_label(snap, sid)));
            }
            out
        }
        Mode::Failed => {
            let mut out = vec!["出错了!".to_string()];
            for f in &snap.failed {
                out.push(format!("✗ [{}] {}", f.source.label(), task_label(f)));
            }
            out
        }
        // working / thinking: ONE line showing the actual work content —
        // the live text stream (reasoning while thinking, output while
        // working), falling back to the running tool, then a plain label.
        Mode::Working => single_live_line(snap, source, Mode::Working, None, MAX_LINE),
        Mode::Thinking => single_live_line(snap, source, Mode::Thinking, None, MAX_LINE),
        Mode::Done => {
            let mut out = vec!["任务完成啦 🎉".to_string()];
            for d in &snap.done {
                out.push(format!("✓ [{}] {}", d.source.label(), task_label(d)));
            }
            out
        }
        Mode::Idle => {
            let src = source.unwrap_or(Source::Dsh);
            if snap.queue_len > 0 {
                vec![
                    format!("[{}] 休息中 💤", src.label()),
                    format!("队列中还有 {} 个任务待处理", snap.queue_len),
                ]
            } else {
                vec![format!("[{}] 休息中 💤", src.label()), "没有运行中的任务".to_string()]
            }
        }
        Mode::Move => vec!["拖动中…".to_string()],
    }
}

/// Pick the session whose live text the bubble shows for the active modes
/// (same rule for the plain line and the typewriter reveal).
fn pick_session<'a>(snap: &'a Snapshot, source: Option<Source>, mode: Mode) -> Option<&'a SessionInfo> {
    let sessions: Vec<&SessionInfo> = match mode {
        Mode::Working => snap.working.iter().collect(),
        Mode::Thinking => snap.thinking.iter().collect(),
        _ => return None,
    };
    let sessions: Vec<&SessionInfo> = sessions
        .into_iter()
        .filter(|s| source.map(|x| s.source == x).unwrap_or(true))
        .collect();
    sessions
        .iter()
        .find(|s| !s.live.text.is_empty() || !s.live.reasoning.is_empty())
        .copied()
        .or_else(|| sessions.first().copied())
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

pub fn live_stream(snap: &Snapshot, source: Option<Source>, mode: Mode) -> Option<LiveStream> {
    let w = pick_session(snap, source, mode)?;
    let prefer_text = mode == Mode::Working;
    // working mode shows the running tool instead of the live text: nothing
    // is being typed while a tool is on screen
    if prefer_text && w.tool.is_some() {
        return None;
    }
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

/// Like [`bubble_lines`], but the streaming single line is revealed
/// character-by-character up to `reveal` chars (typewriter effect).
/// `reveal=None` behaves exactly like [`bubble_lines`].
pub fn bubble_lines_reveal(snap: &Snapshot, source: Option<Source>, reveal: Option<usize>) -> Vec<String> {
    match snap.mode {
        Mode::Working => single_live_line(snap, source, Mode::Working, reveal, MAX_LINE),
        Mode::Thinking => single_live_line(snap, source, Mode::Thinking, reveal, MAX_LINE),
        _ => bubble_lines(snap, source),
    }
}

/// Behind-the-pet stream lines with a caller-tunable per-line char window:
/// the same live stream as the bubble, but `max_line` (text.max_chars in
/// config) can be far larger than [`MAX_LINE`], so a long DSH response keeps
/// much more of itself visible instead of shrinking to the last 120 chars.
/// Non-streaming states fall back to [`bubble_lines`] (those are short).
pub fn stream_lines(
    snap: &Snapshot,
    source: Option<Source>,
    reveal: Option<usize>,
    max_line: usize,
) -> Vec<String> {
    match snap.mode {
        Mode::Working => single_live_line(snap, source, Mode::Working, reveal, max_line),
        Mode::Thinking => single_live_line(snap, source, Mode::Thinking, reveal, max_line),
        _ => bubble_lines(snap, source),
    }
}

/// Single-line bubble for the active work states (plan: show the real work
/// content, e.g. the reasoning stream while thinking; one line at a time).
/// With `reveal`, the streamed text appears progressively (逐字出现).
fn single_live_line(snap: &Snapshot, source: Option<Source>, mode: Mode, reveal: Option<usize>, max_line: usize) -> Vec<String> {
    let src = source.unwrap_or(Source::Dsh);
    let tag = format!("[{}] ", src.label());
    let Some(w) = pick_session(snap, source, mode) else {
        let label = if mode == Mode::Working { "正在干活…" } else { "思考中…" };
        return vec![format!("{tag}{label}")];
    };
    // working: the actual work content = the running tool + its arguments;
    // thinking: the reasoning stream (tail-truncated so it keeps scrolling)
    let content = if mode == Mode::Working {
        if let Some(t) = &w.tool {
            match &w.tool_args {
                Some(args) if !args.is_empty() => {
                    format!("⚙ {t}: {}", truncate_tail(args, max_line.saturating_sub(24)))
                }
                _ => format!("⚙ 正在执行: {t}"),
            }
        } else {
            live_reveal(w, true, reveal, max_line)
        }
    } else {
        live_reveal(w, false, reveal, max_line)
    };
    vec![format!("{tag}{content}")]
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
mod tests {
    use super::*;
    use crate::state::{LiveText, Mode, SessionInfo, Snapshot, Source};

    fn snap(mode: Mode) -> Snapshot {
        Snapshot { mode, ..Default::default() }
    }

    #[test]
    fn attention_question_shows_session_title() {
        let mut s = snap(Mode::Attention);
        s.thinking.push(SessionInfo {
            session_id: "mswfow6bou90rb".into(),
            source: Source::Hermes,
            title: "继续中断的对话".into(),
            tool: None,
            tool_args: None,
            task: None,
            todos: vec![],
            live: LiveText::default(),
        });
        s.pending_questions.push(("mswfow6bou90rb".into(), "如何处理?（重启 / 放弃）".into(), String::new()));
        let l = bubble_lines(&s, Some(Source::Hermes));
        assert!(l[0].contains("需要你确认 · 1 项"));
        assert!(l.iter().any(|x| x.contains("继续中断的对话")));
        assert!(!l.iter().any(|x| x.contains("mswfow6bou90rb")));
        // unknown session id falls back to the raw id
        s.pending_questions.push(("unknown-id".into(), "继续吗?".into(), String::new()));
        let l = bubble_lines(&s, Some(Source::Hermes));
        assert!(l.iter().any(|x| x.contains("unknown-id")));
    }

    #[test]
    fn offline_text() {
        let l = bubble_lines(&snap(Mode::Offline), None);
        assert!(l[0].contains("连不上"));
    }

    #[test]
    fn working_shows_backend_and_tool() {
        let mut s = snap(Mode::Working);
        s.working.push(SessionInfo {
            session_id: "s1".into(),
            source: Source::Dsh,
            title: "修 bug".into(),
            tool: Some("bash".into()),
            tool_args: None,
            task: None,
            todos: vec![],
            live: LiveText::default(),
        });
        let l = bubble_lines(&s, Some(Source::Dsh));
        assert_eq!(l.len(), 1); // one line at a time
        assert!(l[0].contains("[DSH]"));
        assert!(l[0].contains("bash"));
    }

    #[test]
    fn working_prefers_tool_content_over_stale_output() {
        // while a tool runs, the actual work content (tool) wins over the
        // previous step's output text
        let mut s = snap(Mode::Working);
        s.working.push(SessionInfo {
            session_id: "s1".into(),
            source: Source::Dsh,
            title: "t".into(),
            tool: Some("bash".into()),
            tool_args: None,
            task: None,
            todos: vec![],
            live: LiveText { reasoning: String::new(), text: "上一段输出".into(), tool_name: None },
        });
        let l = bubble_lines(&s, Some(Source::Dsh));
        assert_eq!(l.len(), 1);
        assert!(l[0].contains("bash"));
        assert!(!l[0].contains("上一段输出"));
    }

    #[test]
    fn working_shows_tool_arguments() {
        let mut s = snap(Mode::Working);
        s.working.push(SessionInfo {
            session_id: "s1".into(),
            source: Source::Dsh,
            title: "t".into(),
            tool: Some("bash".into()),
            tool_args: Some("ls -la /tmp".into()),
            task: None,
            todos: vec![],
            live: LiveText::default(),
        });
        let l = bubble_lines(&s, Some(Source::Dsh));
        assert_eq!(l.len(), 1);
        assert!(l[0].contains("bash"));
        assert!(l[0].contains("ls -la /tmp"));
    }

    #[test]
    fn stream_shows_tail_not_frozen_head() {
        let mut s = snap(Mode::Thinking);
        let long = "开头".repeat(200); // > MAX_LINE chars
        s.thinking.push(SessionInfo {
            session_id: "s1".into(),
            source: Source::Dsh,
            title: "t".into(),
            tool: None,
            tool_args: None,
            task: None,
            todos: vec![],
            live: LiveText { reasoning: long.clone(), text: String::new(), tool_name: None },
        });
        let l1 = bubble_lines(&s, Some(Source::Dsh));
        let s2 = snap(Mode::Thinking);
        let mut s2 = s2;
        s2.thinking.push(SessionInfo {
            session_id: "s1".into(),
            source: Source::Dsh,
            title: "t".into(),
            tool: None,
            tool_args: None,
            task: None,
            todos: vec![],
            live: LiveText { reasoning: format!("{long}新增内容"), text: String::new(), tool_name: None },
        });
        let l2 = bubble_lines(&s2, Some(Source::Dsh));
        // the visible line changes as the stream grows (tail scrolling)
        assert_ne!(l1[0], l2[0]);
        assert!(l2[0].contains("新增内容"));
        assert!(l2[0].starts_with("[DSH] 🧠 …"));
    }

    #[test]
    fn done_bubble_shows_task_name() {
        let mut s = snap(Mode::Done);
        s.done.push(SessionInfo {
            session_id: "s1".into(),
            source: Source::Dsh,
            title: String::new(),
            tool: None,
            tool_args: None,
            task: Some("修复webp闪烁像素".into()),
            todos: vec![],
            live: LiveText::default(),
        });
        let l = bubble_lines(&s, Some(Source::Dsh));
        assert!(l[0].contains("任务完成"));
        assert!(l.iter().any(|x| x.contains("修复webp闪烁像素")));
    }

    #[test]
    fn idle_shows_pending_queue() {
        let mut s = snap(Mode::Idle);
        s.queue_len = 3;
        let l = bubble_lines(&s, Some(Source::ComfyUi));
        assert!(l[0].contains("ComfyUI"));
        assert!(l[1].contains("3"));
        // empty queue falls back to the plain idle line
        let s2 = snap(Mode::Idle);
        let l2 = bubble_lines(&s2, Some(Source::Dsh));
        assert!(l2[1].contains("没有运行中的任务"));
    }

    #[test]
    fn thinking_prefers_reasoning_stream() {
        let mut s = snap(Mode::Thinking);
        s.thinking.push(SessionInfo {
            session_id: "s1".into(),
            source: Source::Hermes,
            title: "t".into(),
            tool: None,
            tool_args: None,
            task: None,
            todos: vec![],
            live: LiveText { reasoning: "思考中文字流…".into(), text: String::new(), tool_name: None },
        });
        let l = bubble_lines(&s, Some(Source::Hermes));
        assert_eq!(l.len(), 1);
        assert!(l[0].contains("思考中文字流"));
    }

    #[test]
    fn thinking_live_reasoning() {
        let mut s = snap(Mode::Thinking);
        s.thinking.push(SessionInfo {
            session_id: "s1".into(),
            source: Source::Hermes,
            title: "分析中".into(),
            tool: None,
            tool_args: None,
            task: None,
            todos: vec![],
            live: LiveText { reasoning: "正在推导…".into(), text: String::new(), tool_name: None },
        });
        let l = bubble_lines(&s, Some(Source::Hermes));
        assert!(l.iter().any(|x| x.contains("正在推导")));
    }

    #[test]
    fn working_filters_other_source_when_selected() {
        let mut s = snap(Mode::Working);
        s.working.push(SessionInfo {
            session_id: "d1".into(),
            source: Source::Dsh,
            title: "d".into(),
            tool: Some("t1".into()),
            tool_args: None,
            task: None,
            todos: vec![],
            live: LiveText::default(),
        });
        s.working.push(SessionInfo {
            session_id: "h1".into(),
            source: Source::Hermes,
            title: "h".into(),
            tool: Some("t2".into()),
            tool_args: None,
            task: None,
            todos: vec![],
            live: LiveText::default(),
        });
        let l = bubble_lines(&s, Some(Source::Hermes));
        assert!(l.iter().any(|x| x.contains("t2")));
        assert!(!l.iter().any(|x| x.contains("t1")));
    }

    #[test]
    fn long_text_truncated() {
        let mut s = snap(Mode::Thinking);
        s.thinking.push(SessionInfo {
            session_id: "s1".into(),
            source: Source::Dsh,
            title: "t".into(),
            tool: None,
            tool_args: None,
            task: None,
            todos: vec![],
            live: LiveText { reasoning: "x".repeat(500), text: String::new(), tool_name: None },
        });
        let l = bubble_lines(&s, Some(Source::Dsh));
        assert!(l.iter().any(|x| x.contains('…')));
    }

    #[test]
    fn reveal_types_out_char_by_char() {
        let mut s = snap(Mode::Thinking);
        s.thinking.push(SessionInfo {
            session_id: "s1".into(),
            source: Source::Hermes,
            title: "t".into(),
            tool: None,
            tool_args: None,
            task: None,
            todos: vec![],
            live: LiveText { reasoning: "正在推导方案".into(), text: String::new(), tool_name: None },
        });
        let l0 = bubble_lines_reveal(&s, Some(Source::Hermes), Some(0));
        assert_eq!(l0[0], "[Hermes] 🧠 ");
        let l2 = bubble_lines_reveal(&s, Some(Source::Hermes), Some(2));
        assert!(l2[0].ends_with("正在"));
        assert!(!l2[0].contains("推导"));
        // reveal >= len -> identical to the plain line
        let full = bubble_lines(&s, Some(Source::Hermes));
        let lfull = bubble_lines_reveal(&s, Some(Source::Hermes), Some(100));
        assert_eq!(lfull, full);
        assert!(full[0].ends_with("正在推导方案"));
    }

    #[test]
    fn reveal_scrolls_after_cap() {
        let mut s = snap(Mode::Thinking);
        let long: String = "字".repeat(200); // > MAX_LINE
        s.thinking.push(SessionInfo {
            session_id: "s1".into(),
            source: Source::Dsh,
            title: "t".into(),
            tool: None,
            tool_args: None,
            task: None,
            todos: vec![],
            live: LiveText { reasoning: long.clone(), text: String::new(), tool_name: None },
        });
        // revealed 150 of 200 chars: shows the tail of the revealed prefix,
        // i.e. chars [150-120, 150) with a leading ellipsis
        let l = bubble_lines_reveal(&s, Some(Source::Dsh), Some(150));
        assert!(l[0].starts_with("[DSH] 🧠 …"));
        assert!(l[0].contains('字'));
        // does NOT contain the chars after the reveal point (all identical
        // here, so verify the ellipsis is present and length is capped)
        assert!(l[0].chars().count() < 200);
    }

    #[test]
    fn stream_lines_wide_window_keeps_more_than_bubble() {
        // a 300-char stream: the behind-the-pet window (max_chars) keeps all
        // of it, while the bubble window (MAX_LINE=120) tail-truncates
        let mut s = snap(Mode::Thinking);
        let long: String = "字".repeat(300);
        s.thinking.push(SessionInfo {
            session_id: "s1".into(),
            source: Source::Dsh,
            title: "t".into(),
            tool: None,
            tool_args: None,
            task: None,
            todos: vec![],
            live: LiveText { reasoning: long.clone(), text: String::new(), tool_name: None },
        });
        let b = bubble_lines(&s, Some(Source::Dsh));
        let w = stream_lines(&s, Some(Source::Dsh), None, 300);
        // the behind-the-pet window keeps the full 300 chars (no ellipsis),
        // the bubble window tail-truncates to 120 and adds "…", so it is
        // visibly shorter
        let tag_len = "[DSH] 🧠 ".chars().count();
        assert_eq!(w[0].chars().count() - tag_len, 300);
        assert!(w[0].chars().count() > b[0].chars().count());
        assert!(!w[0].contains('…')); // nothing dropped yet
    }

    #[test]
    fn live_stream_kind_matches_displayed_stream() {
        let mut s = snap(Mode::Thinking);
        s.thinking.push(SessionInfo {
            session_id: "s1".into(),
            source: Source::Dsh,
            title: "t".into(),
            tool: None,
            tool_args: None,
            task: None,
            todos: vec![],
            live: LiveText { reasoning: "推导".into(), text: "正文".into(), tool_name: None },
        });
        // thinking shows reasoning first
        let st = live_stream(&s, Some(Source::Dsh), Mode::Thinking).unwrap();
        assert_eq!(st.kind, 0);
        assert_eq!(st.len, 2);
        assert_eq!(st.session_id, "s1");
        // working shows text first
        let mut w = snap(Mode::Working);
        w.working.push(SessionInfo {
            session_id: "s1".into(),
            source: Source::Dsh,
            title: "t".into(),
            tool: None,
            tool_args: None,
            task: None,
            todos: vec![],
            live: LiveText { reasoning: "推导".into(), text: "正文".into(), tool_name: None },
        });
        let st = live_stream(&w, Some(Source::Dsh), Mode::Working).unwrap();
        assert_eq!(st.kind, 1);
    }

    #[test]
    fn live_stream_none_when_tool_or_no_live() {
        // working with a running tool: the bubble shows the tool, nothing types
        let mut s = snap(Mode::Working);
        s.working.push(SessionInfo {
            session_id: "s1".into(),
            source: Source::Dsh,
            title: "t".into(),
            tool: Some("bash".into()),
            tool_args: None,
            task: None,
            todos: vec![],
            live: LiveText { reasoning: "推导".into(), text: String::new(), tool_name: None },
        });
        assert!(live_stream(&s, Some(Source::Dsh), Mode::Working).is_none());
        // thinking session without any live text yet
        let mut t = snap(Mode::Thinking);
        t.thinking.push(SessionInfo {
            session_id: "s1".into(),
            source: Source::Dsh,
            title: "t".into(),
            tool: None,
            tool_args: None,
            task: None,
            todos: vec![],
            live: LiveText::default(),
        });
        assert!(live_stream(&t, Some(Source::Dsh), Mode::Thinking).is_none());
    }
}
