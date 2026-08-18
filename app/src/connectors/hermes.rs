//! Hermes connector: read-only SQLite polling (schema verified on the live
//! machine). Timestamps in `sessions`/`messages` are SECONDS.
//! Open strategy: mode=ro first; on failure (e.g. WAL/shm permission on some
//! mounts) fall back to copying db+wal+shm into a temp dir and reading the
//! copy (verified working).

use super::{send, sleep_interruptible};
use crate::state::{SessionItem, Source, StateEvent, TurnEndReason};
use std::collections::HashMap;
use std::sync::Arc;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;

const FRESHNESS_MS: u64 = 10 * 60 * 1000; // ended_at NULL but stale -> not running
const LOOKBACK_SECS: i64 = 2 * 3600; // only recent ended sessions enter the pet

#[derive(Debug, Clone, Default)]
struct PrevSession {
    running: bool,
    turn: u64,
    last_msg_id: i64,
    last_content: Option<String>,
    last_reasoning: Option<String>,
    pending_tool: Option<String>,
    /// Open interactive question: (tool_call_id, rendered question text).
    /// Hermes writes the assistant row with a `clarify` tool call and only
    /// writes the tool result row after the user answers — the pet surfaces
    /// this as the attention state.
    pending_question: Option<(String, String)>,
    title: Option<String>,
}

pub struct HermesConnector {
    pub db_path: PathBuf,
    pub poll_ms_active: u64,
    pub poll_ms_idle: u64,
}

impl HermesConnector {
    pub fn spawn(self, tx: Sender<StateEvent>, stop: Arc<AtomicBool>) {
        std::thread::Builder::new()
            .name("hermes-poll".into())
            .spawn(move || self.loop_run(tx, stop))
            .ok();
    }

    fn loop_run(&self, tx: Sender<StateEvent>, stop: Arc<AtomicBool>) {
        let mut prev: HashMap<String, PrevSession> = HashMap::new();
        let mut healthy = false;
        let mut conn: Option<rusqlite::Connection> = self.open().ok();
        let mut db_tmp: Option<PathBuf> = None;
        if conn.is_none() {
            eprintln!("[hermes] db open failed at {}", self.db_path.display());
        }
        loop {
            if stop.load(Ordering::Relaxed) {
                break;
            }
            if conn.is_none() {
                if let Ok((c, tmp)) = self.open_with_fallback() {
                    eprintln!("[hermes] db opened (fallback copy: {})", tmp.display());
                    conn = Some(c);
                    db_tmp = Some(tmp);
                }
            }
            let mut any_running = false;
            let result = match &mut conn {
                Some(c) => self.poll_once(c, &mut prev, &mut any_running),
                None => Err("no db connection".into()),
            };
            match result {
                Ok(events) => {
                    if !healthy {
                        healthy = true;
                        send(&tx, StateEvent::SourceHealth { source: Source::Hermes, healthy: true });
                        eprintln!("[hermes] health -> true");
                    }
                    for ev in events {
                        send(&tx, ev);
                    }
                }
                Err(e) => {
                    eprintln!("[hermes] poll error: {e}");
                    if healthy {
                        healthy = false;
                        send(&tx, StateEvent::SourceHealth { source: Source::Hermes, healthy: false });
                        eprintln!("[hermes] health -> false");
                    }
                    conn = None;
                    if let Some(t) = db_tmp.take() {
                        let _ = std::fs::remove_dir_all(&t);
                    }
                }
            }
            let wait = if any_running { self.poll_ms_active } else { self.poll_ms_idle };
            sleep_interruptible(wait, &stop);
        }
    }

    fn open(&self) -> Result<rusqlite::Connection, String> {
        let flags = rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX;
        rusqlite::Connection::open_with_flags(&self.db_path, flags)
            .map_err(|e| format!("open {}: {e}", self.db_path.display()))
    }

    /// mode=ro first; on failure copy db+wal+shm to temp dir and open the copy.
    fn open_with_fallback(&self) -> Result<(rusqlite::Connection, PathBuf), String> {
        if let Ok(c) = self.open() {
            return Ok((c, PathBuf::new()));
        }
        let dir = std::env::temp_dir().join(format!("dshpet-hermes-{}", std::process::id()));
        std::fs::create_dir_all(&dir).map_err(|e| format!("temp dir: {e}"))?;
        let mut copied = false;
        for suffix in ["", "-wal", "-shm"] {
            let src = PathBuf::from(format!("{}{}", self.db_path.display(), suffix));
            if src.exists() {
                let dst = dir.join(format!("hermes-web-ui.db{suffix}"));
                std::fs::copy(&src, &dst).map_err(|e| format!("copy {suffix}: {e}"))?;
                copied = true;
            }
        }
        if !copied {
            return Err("source db missing".into());
        }
        let flags = rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX;
        let c = rusqlite::Connection::open_with_flags(dir.join("hermes-web-ui.db"), flags)
            .map_err(|e| format!("open copy: {e}"))?;
        Ok((c, dir))
    }

    fn poll_once(
        &self,
        conn: &rusqlite::Connection,
        prev: &mut HashMap<String, PrevSession>,
        any_running: &mut bool,
    ) -> Result<Vec<StateEvent>, String> {
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        let now_s = (now_ms / 1000) as i64;
        let mut evs = Vec::new();

        // 1) sessions (recent window)
        let mut stmt = conn
            .prepare(
                "SELECT id, title, ended_at, end_reason, last_active \
                 FROM sessions \
                 WHERE ended_at IS NULL OR last_active > ?1 \
                 ORDER BY last_active DESC",
            )
            .map_err(|e| format!("sessions query: {e}"))?;
        let rows = stmt
            .query_map([now_s - LOOKBACK_SECS], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, Option<String>>(1)?,
                    r.get::<_, Option<i64>>(2)?,
                    r.get::<_, Option<String>>(3)?,
                    r.get::<_, Option<i64>>(4)?,
                ))
            })
            .map_err(|e| format!("sessions scan: {e}"))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| format!("sessions row: {e}"))?;
        drop(stmt);

        let mut items = Vec::with_capacity(rows.len());
        let mut seen: Vec<String> = Vec::with_capacity(rows.len());
        for (id, title, ended_at, end_reason, last_active) in rows {
            let fresh = last_active
                .map(|la| now_ms.saturating_sub((la as u64).saturating_mul(1000)) < FRESHNESS_MS)
                .unwrap_or(false);
            let running = ended_at.is_none() && fresh;
            seen.push(id.clone());
            let p = prev.entry(id.clone()).or_default();
            p.title = title.clone();
    let _ = end_reason; // edge handled via p.running flip

            if running && !p.running {
                p.running = true;
                p.turn += 1;
                p.pending_tool = None;
                evs.push(StateEvent::TurnStarted { source: Source::Hermes, session_id: id.clone(), turn: p.turn });
            } else if !running && p.running {
                p.running = false;
                let reason = match end_reason.as_deref() {
                    Some("complete") | Some("completed") => TurnEndReason::Completed,
                    Some("error") | Some("failed") => TurnEndReason::Error,
                    _ => TurnEndReason::Aborted,
                };
                if let Some(t) = p.pending_tool.take() {
                    evs.push(StateEvent::ToolEnded { source: Source::Hermes, session_id: id.clone(), name: t, error: false });
                }
                if let Some((pid, _)) = p.pending_question.take() {
                    evs.push(StateEvent::QuestionResolved { source: Source::Hermes, id: pid });
                }
                evs.push(StateEvent::TurnEnded { source: Source::Hermes, session_id: id.clone(), turn: p.turn, reason });
                evs.push(StateEvent::SessionStatus { source: Source::Hermes, session_id: id.clone(), running: false });
            }
            if running {
                *any_running = true;
                self.poll_messages(conn, &id, p, &mut evs)?;
            }
            items.push(SessionItem {
                session_id: id.clone(),
                running,
                title: title.clone(),
                todos: None,
            });
        }

        // 2) sessions that vanished (ended beyond lookback or deleted)
        let gone: Vec<String> = prev
            .keys()
            .filter(|k| !seen.contains(k))
            .cloned()
            .collect();
        for id in gone {
            let mut p = prev.remove(&id).unwrap();
            if p.running {
                if let Some(t) = p.pending_tool.take() {
                    evs.push(StateEvent::ToolEnded { source: Source::Hermes, session_id: id.clone(), name: t, error: false });
                }
                if let Some((pid, _)) = p.pending_question.take() {
                    evs.push(StateEvent::QuestionResolved { source: Source::Hermes, id: pid });
                }
                evs.push(StateEvent::TurnEnded {
                    source: Source::Hermes,
                    session_id: id.clone(),
                    turn: p.turn,
                    reason: TurnEndReason::Aborted,
                });
                evs.push(StateEvent::SessionStatus { source: Source::Hermes, session_id: id.clone(), running: false });
            }
        }

        evs.push(StateEvent::Poll { source: Source::Hermes, items, ok: true, error: None });
        Ok(evs)
    }

    fn poll_messages(
        &self,
        conn: &rusqlite::Connection,
        session_id: &str,
        p: &mut PrevSession,
        evs: &mut Vec<StateEvent>,
    ) -> Result<(), String> {
        let mut stmt = conn
            .prepare(
                "SELECT id, role, tool_name, content, display_content, reasoning_content, reasoning, \
                        tool_calls, finish_reason, tool_call_id \
                 FROM messages WHERE session_id = ?1 ORDER BY id DESC LIMIT 1",
            )
            .map_err(|e| format!("messages query: {e}"))?;
        let row = stmt.query_row([session_id], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, Option<String>>(2)?,
                r.get::<_, Option<String>>(3)?,
                r.get::<_, Option<String>>(4)?,
                r.get::<_, Option<String>>(5)?,
                r.get::<_, Option<String>>(6)?,
                r.get::<_, Option<String>>(7)?,
                r.get::<_, Option<String>>(8)?,
                r.get::<_, Option<String>>(9)?,
            ))
        });
        drop(stmt);
        let (id, role, tool_name, content, display_content, reasoning_content, reasoning, tool_calls, finish_reason, tool_call_id) =
            match row {
                Ok(r) => r,
                Err(rusqlite::Error::QueryReturnedNoRows) => return Ok(()),
                Err(e) => return Err(format!("messages row: {e}")),
            };
        let text = display_content.or(content).unwrap_or_default();
        let reasoning_text = reasoning_content.or(reasoning).unwrap_or_default();
        let changed = id != p.last_msg_id
            || p.last_content.as_deref() != Some(text.as_str())
            || p.last_reasoning.as_deref() != Some(reasoning_text.as_str());
        if !changed {
            return Ok(());
        }
        let new_msg = id != p.last_msg_id;
        // Capture the previous snapshot BEFORE overwriting: the streaming
        // delta below must diff against the OLD text/reasoning, not the one
        // we are about to store (previous code overwrote first, so same-id
        // growing rows always produced an empty delta and reasoning/text
        // streaming froze at the first snapshot).
        let prev_t = p.last_content.clone().unwrap_or_default();
        let prev_r = p.last_reasoning.clone().unwrap_or_default();
        p.last_msg_id = id;
        p.last_content = Some(text.clone());
        p.last_reasoning = Some(reasoning_text.clone());

        // ---- interactive question detection (waiting for user input) ----
        // Hermes records an unanswered `clarify` call as an assistant row with
        // finish_reason=tool_calls; the result row is written only after the
        // user answers. Surface that gap as QuestionRequested -> attention.
        let clarify = pending_clarify(tool_calls.as_deref(), finish_reason.as_deref());
        if let Some((pid, _)) = &p.pending_question {
            let answered = role == "tool" && tool_call_id.as_deref() == Some(pid.as_str());
            let superseded = !clarify.as_ref().map(|(cid, _)| cid == pid).unwrap_or(false);
            if answered || superseded {
                evs.push(StateEvent::QuestionResolved { source: Source::Hermes, id: pid.clone() });
                p.pending_question = None;
            }
        }
        if p.pending_question.is_none() {
            if let Some((cid, qtext)) = clarify {
                p.pending_question = Some((cid.clone(), qtext.clone()));
                evs.push(StateEvent::QuestionRequested {
                    source: Source::Hermes,
                    id: cid,
                    session_id: session_id.to_string(),
                    text: qtext,
                });
            }
        }

        if role == "tool" {
            if let Some(name) = tool_name.filter(|n| !n.is_empty()) {
                // clarify answer rows are the user's reply record, not work:
                // question resolution above already handled them
                if name == "clarify" {
                    return Ok(());
                }
                if p.pending_tool.as_deref() != Some(name.as_str()) {
                    if let Some(old) = p.pending_tool.take() {
                        evs.push(StateEvent::ToolEnded { source: Source::Hermes, session_id: session_id.into(), name: old, error: false });
                    }
                    p.pending_tool = Some(name.clone());
                    // Hermes tool rows are written at completion with the
                    // result JSON in `content` - surface its "output" (or the
                    // raw content) as the tool's actual work content.
                    let args = tool_content_preview(&text);
                    evs.push(StateEvent::ToolStarted { source: Source::Hermes, session_id: session_id.into(), name, arguments: args });
                }
            }
            return Ok(());
        }
        // non-tool message: close any pending tool, emit live text deltas
        if let Some(t) = p.pending_tool.take() {
            evs.push(StateEvent::ToolEnded { source: Source::Hermes, session_id: session_id.into(), name: t, error: false });
        }
        if role == "user" && !text.is_empty() {
            evs.push(StateEvent::UserMessage { source: Source::Hermes, session_id: session_id.into(), text: text.clone() });
            return Ok(());
        }
        if !new_msg {
            // same id, content grew (streaming write): emit only the delta
            let dtext = text.strip_prefix(&prev_t).unwrap_or(&text).to_string();
            let dreason = reasoning_text.strip_prefix(&prev_r).unwrap_or(&reasoning_text).to_string();
            if !dtext.is_empty() || !dreason.is_empty() {
                evs.push(StateEvent::LiveText {
                    source: Source::Hermes,
                    session_id: session_id.into(),
                    reasoning: if dreason.is_empty() { None } else { Some(dreason) },
                    text: if dtext.is_empty() { None } else { Some(dtext) },
                    tool_name: None,
                });
            }
            return Ok(());
        }
        evs.push(StateEvent::LiveText {
            source: Source::Hermes,
            session_id: session_id.into(),
            reasoning: if reasoning_text.is_empty() { None } else { Some(reasoning_text) },
            text: if text.is_empty() { None } else { Some(text) },
            tool_name: None,
        });
        Ok(())
    }
}

/// Extract a short preview of a Hermes tool row's `content` JSON:
/// the `output` field when present, otherwise the raw content.
fn tool_content_preview(content: &str) -> Option<String> {
    const MAX: usize = 160;
    let out = if let Ok(v) = serde_json::from_str::<serde_json::Value>(content) {
        v.get("output")
            .and_then(|o| o.as_str())
            .map(|s| s.to_string())
            .unwrap_or_else(|| content.to_string())
    } else {
        content.to_string()
    };
    let trimmed: String = out.chars().take(MAX).collect();
    if trimmed.is_empty() { None } else { Some(trimmed) }
}

/// Detect an unanswered `clarify` call in an assistant row: Hermes's
/// interactive "ask the user" tool. Returns (tool_call_id, rendered
/// question) when the row is a tool_calls finish with a clarify call and no
/// result row has been written yet (detected by the caller via the latest
/// row still being this assistant row).
fn pending_clarify(tool_calls: Option<&str>, finish_reason: Option<&str>) -> Option<(String, String)> {
    if !finish_reason.unwrap_or("").contains("tool_calls") {
        return None;
    }
    let v: serde_json::Value = serde_json::from_str(tool_calls?).ok()?;
    let arr = v.as_array()?;
    for call in arr {
        if call["function"]["name"].as_str() != Some("clarify") {
            continue;
        }
        let call_id = call["id"].as_str().unwrap_or("").to_string();
        if call_id.is_empty() {
            continue;
        }
        let mut text = String::new();
        if let Ok(args) = serde_json::from_str::<serde_json::Value>(
            call["function"]["arguments"].as_str().unwrap_or(""),
        ) {
            if let Some(q) = args["question"].as_str() {
                text.push_str(q);
            }
            if let Some(choices) = args["choices"].as_array() {
                let ch: Vec<&str> = choices.iter().filter_map(|c| c.as_str()).collect();
                if !ch.is_empty() {
                    let choices_s = ch.join(" / ");
                    if text.is_empty() {
                        text = choices_s;
                    } else {
                        text.push_str("（");
                        text.push_str(&choices_s);
                        text.push('）');
                    }
                }
            }
        }
        if text.trim().is_empty() {
            text = "等待你确认…".to_string();
        }
        return Some((call_id, text));
    }
    None
}

/// Helper for tests / headless: open with explicit path (wraps the connector).
pub fn open_readonly(path: &Path) -> Result<rusqlite::Connection, String> {
    let c = HermesConnector {
        db_path: path.to_path_buf(),
        poll_ms_active: 1000,
        poll_ms_idle: 2000,
    };
    c.open_with_fallback().map(|(c, _)| c)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_db() -> (tempfile_like::TempDir, rusqlite::Connection) {
        // minimal temp dir without extra deps: use std temp + pid + counter
        let dir = std::env::temp_dir().join(format!(
            "dshpet-hermes-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("hermes-web-ui.db");
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE TABLE sessions (id TEXT PRIMARY KEY, title TEXT, source TEXT, agent TEXT,
                started_at INTEGER, ended_at INTEGER, end_reason TEXT, last_active INTEGER);
             CREATE TABLE messages (id INTEGER PRIMARY KEY AUTOINCREMENT, session_id TEXT, role TEXT,
                content TEXT, display_content TEXT, tool_name TEXT, timestamp INTEGER,
                reasoning TEXT, reasoning_content TEXT,
                tool_calls TEXT, finish_reason TEXT, tool_call_id TEXT);",
        )
        .unwrap();
        (tempfile_like::TempDir(dir), conn)
    }

    mod tempfile_like {
        pub struct TempDir(pub std::path::PathBuf);
        impl Drop for TempDir {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
    }

    #[test]
    fn reasoning_streaming_delta_on_same_message() {
        // regression: a growing message row (same id) must emit the delta of
        // both text and reasoning, otherwise Hermes thinking freezes at the
        // first snapshot (often empty) and is never shown
        let (_td, conn) = tmp_db();
        let now_s = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        conn.execute(
            "INSERT INTO sessions (id, title, source, started_at, last_active) VALUES ('h1','t','cli',?1,?1)",
            [now_s],
        )
        .unwrap();
        let c = HermesConnector { db_path: PathBuf::new(), poll_ms_active: 1000, poll_ms_idle: 2000 };
        let mut prev = HashMap::new();
        let mut any = false;
        let _ = c.poll_once(&conn, &mut prev, &mut any).unwrap();

        // first snapshot: reasoning empty, only a little text
        conn.execute(
            "INSERT INTO messages (session_id, role, content, reasoning_content, timestamp) VALUES ('h1','assistant','开头','',?1)",
            [now_s],
        )
        .unwrap();
        let mut any = false;
        let evs1 = c.poll_once(&conn, &mut prev, &mut any).unwrap();
        assert!(evs1.iter().any(|e| matches!(e, StateEvent::LiveText { reasoning: None, text: Some(t), .. } if t == "开头")));

        // same row grows: reasoning appears + text extends -> delta emitted
        conn.execute(
            "UPDATE messages SET reasoning_content='思考中', content='开头后续' WHERE session_id='h1'",
            [],
        )
        .unwrap();
        let mut any = false;
        let evs2 = c.poll_once(&conn, &mut prev, &mut any).unwrap();
        assert!(evs2.iter().any(|e| matches!(e, StateEvent::LiveText { reasoning: Some(r), text: Some(t), .. }
            if r == "思考中" && t == "后续")));

        // unchanged -> no event
        let mut any = false;
        let evs3 = c.poll_once(&conn, &mut prev, &mut any).unwrap();
        assert!(!evs3.iter().any(|e| matches!(e, StateEvent::LiveText { .. })));
    }

    #[test]
    fn session_edges_and_messages() {
        let (_td, conn) = tmp_db();
        let now_s = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        conn.execute(
            "INSERT INTO sessions (id, title, source, started_at, last_active) VALUES ('h1','测试会话','cli',?1,?1)",
            [now_s],
        )
        .unwrap();
        let c = HermesConnector {
            db_path: PathBuf::new(),
            poll_ms_active: 1000,
            poll_ms_idle: 2000,
        };
        let mut prev = HashMap::new();
        let mut any = false;
        let evs = c.poll_once(&conn, &mut prev, &mut any).unwrap();
        assert!(any);
        assert!(evs.iter().any(|e| matches!(e, StateEvent::TurnStarted { session_id, .. } if session_id == "h1")));
        assert!(evs.iter().any(|e| matches!(e, StateEvent::Poll { items, .. } if items.len() == 1 && items[0].running)));

        // new assistant message with reasoning
        conn.execute(
            "INSERT INTO messages (session_id, role, content, reasoning_content, timestamp) VALUES ('h1','assistant','你好','思考中',?1)",
            [now_s],
        )
        .unwrap();
        let evs = c.poll_once(&conn, &mut prev, &mut any).unwrap();
        assert!(evs.iter().any(|e| matches!(e, StateEvent::LiveText { reasoning, text, .. }
            if reasoning.as_deref() == Some("思考中") && text.as_deref() == Some("你好"))));

        // end session
        conn.execute(
            "UPDATE sessions SET ended_at = ?1, end_reason = 'complete' WHERE id='h1'",
            [now_s + 1],
        )
        .unwrap();
        let mut any = false;
        let evs = c.poll_once(&conn, &mut prev, &mut any).unwrap();
        assert!(evs.iter().any(|e| matches!(e, StateEvent::TurnEnded { reason: TurnEndReason::Completed, .. })));
        assert!(evs.iter().any(|e| matches!(e, StateEvent::SessionStatus { running: false, .. })));
        assert!(!any);
    }

    #[test]
    fn stale_session_not_running() {
        let (_td, conn) = tmp_db();
        let now_s = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        conn.execute(
            "INSERT INTO sessions (id, title, source, started_at, last_active) VALUES ('h2','旧会话','cli',?1,?1)",
            [now_s - 3600],
        )
        .unwrap();
        let c = HermesConnector { db_path: PathBuf::new(), poll_ms_active: 1000, poll_ms_idle: 2000 };
        let mut prev = HashMap::new();
        let mut any = false;
        let evs = c.poll_once(&conn, &mut prev, &mut any).unwrap();
        // stale (no ended_at but last_active 1h ago) -> not running, no TurnStarted
        assert!(!any);
        let poll = evs.iter().find_map(|e| match e {
            StateEvent::Poll { items, .. } => Some(items.clone()),
            _ => None,
        });
        assert_eq!(poll.unwrap()[0].running, false);
    }

    #[test]
    fn tool_content_preview_extracts_output() {
        assert_eq!(
            tool_content_preview(r#"{"output": "ls done\nexit 0", "exit_code": 0}"#).as_deref(),
            Some("ls done\nexit 0")
        );
        // non-JSON content falls back to raw text (truncated)
        let long = "x".repeat(500);
        let p = tool_content_preview(&long).unwrap();
        assert!(p.chars().count() <= 160);
        assert!(tool_content_preview("").is_none());
    }

    #[test]
    fn pending_clarify_parses_question_and_choices() {
        let tc = r#"[{"id":"call_00_abc","type":"function","function":{"name":"clarify","arguments":"{\"question\":\"继续吗?\",\"choices\":[\"继续\",\"停下\"],\"multi_select\":false}"}}]"#;
        let (cid, text) = pending_clarify(Some(tc), Some("tool_calls")).unwrap();
        assert_eq!(cid, "call_00_abc");
        assert!(text.contains("继续吗?"));
        assert!(text.contains("继续 / 停下"));
        // non-clarify tool calls -> no question
        let tc2 = r#"[{"id":"c2","type":"function","function":{"name":"bash","arguments":"{}"}}]"#;
        assert!(pending_clarify(Some(tc2), Some("tool_calls")).is_none());
        // plain assistant output -> no question
        assert!(pending_clarify(None, Some("stop")).is_none());
        assert!(pending_clarify(None, None).is_none());
    }

    #[test]
    fn clarify_emits_question_requested_then_resolved() {
        let (_td, conn) = tmp_db();
        let now_s = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        conn.execute(
            "INSERT INTO sessions (id, title, source, started_at, last_active) VALUES ('h1','测试会话','cli',?1,?1)",
            [now_s],
        )
        .unwrap();
        let c = HermesConnector {
            db_path: PathBuf::new(),
            poll_ms_active: 1000,
            poll_ms_idle: 2000,
        };
        let mut prev = HashMap::new();
        let mut any = false;
        let _ = c.poll_once(&conn, &mut prev, &mut any).unwrap();

        // assistant row requesting clarify -> QuestionRequested
        let tc = r#"[{"id":"call_00_q1","type":"function","function":{"name":"clarify","arguments":"{\"question\":\"如何处理?\",\"choices\":[\"重启\",\"放弃\"]}"}}]"#;
        conn.execute(
            "INSERT INTO messages (session_id, role, content, tool_calls, finish_reason, timestamp) VALUES ('h1','assistant','',?1,'tool_calls',?2)",
            rusqlite::params![tc, now_s],
        )
        .unwrap();
        let mut any = false;
        let evs = c.poll_once(&conn, &mut prev, &mut any).unwrap();
        assert!(evs.iter().any(|e| matches!(e, StateEvent::QuestionRequested { id, session_id, text, .. }
            if id == "call_00_q1" && session_id == "h1" && text.contains("如何处理?") && text.contains("重启 / 放弃"))));

        // user answers -> Hermes writes the tool result row with the call id
        conn.execute(
            "INSERT INTO messages (session_id, role, tool_name, content, tool_call_id, timestamp) VALUES ('h1','tool','clarify','{\"output\":\"重启\"}','call_00_q1',?1)",
            [now_s],
        )
        .unwrap();
        let mut any = false;
        let evs = c.poll_once(&conn, &mut prev, &mut any).unwrap();
        assert!(evs.iter().any(|e| matches!(e, StateEvent::QuestionResolved { id, .. } if id == "call_00_q1")));
        // the clarify answer row must NOT surface as a working tool
        assert!(!evs.iter().any(|e| matches!(e, StateEvent::ToolStarted { name, .. } if name == "clarify")));
        // no duplicate request after resolution
        assert!(!evs.iter().any(|e| matches!(e, StateEvent::QuestionRequested { .. })));
    }

    #[test]
    fn clarify_resolved_when_session_ends() {
        let (_td, conn) = tmp_db();
        let now_s = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        conn.execute(
            "INSERT INTO sessions (id, title, source, started_at, last_active) VALUES ('h2','等待中','cli',?1,?1)",
            [now_s],
        )
        .unwrap();
        let c = HermesConnector {
            db_path: PathBuf::new(),
            poll_ms_active: 1000,
            poll_ms_idle: 2000,
        };
        let mut prev = HashMap::new();
        let mut any = false;
        let _ = c.poll_once(&conn, &mut prev, &mut any).unwrap();

        let tc = r#"[{"id":"call_00_q2","type":"function","function":{"name":"clarify","arguments":"{\"question\":\"继续吗?\"}"}}]"#;
        conn.execute(
            "INSERT INTO messages (session_id, role, content, tool_calls, finish_reason, timestamp) VALUES ('h2','assistant','',?1,'tool_calls',?2)",
            rusqlite::params![tc, now_s],
        )
        .unwrap();
        let mut any = false;
        let _ = c.poll_once(&conn, &mut prev, &mut any).unwrap();

        // user abandons the session -> ended_at set -> question resolved
        conn.execute(
            "UPDATE sessions SET ended_at = ?1, end_reason = 'abort' WHERE id='h2'",
            [now_s + 1],
        )
        .unwrap();
        let mut any = false;
        let evs = c.poll_once(&conn, &mut prev, &mut any).unwrap();
        assert!(evs.iter().any(|e| matches!(e, StateEvent::QuestionResolved { id, .. } if id == "call_00_q2")));
    }

    #[test]
    fn ordinary_tool_calls_do_not_raise_questions() {
        let (_td, conn) = tmp_db();
        let now_s = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        conn.execute(
            "INSERT INTO sessions (id, title, source, started_at, last_active) VALUES ('h3','干活','cli',?1,?1)",
            [now_s],
        )
        .unwrap();
        let c = HermesConnector {
            db_path: PathBuf::new(),
            poll_ms_active: 1000,
            poll_ms_idle: 2000,
        };
        let mut prev = HashMap::new();
        let mut any = false;
        let _ = c.poll_once(&conn, &mut prev, &mut any).unwrap();

        let tc = r#"[{"id":"call_00_t1","type":"function","function":{"name":"write_file","arguments":"{\"path\":\"/tmp/x\"}"}}]"#;
        conn.execute(
            "INSERT INTO messages (session_id, role, content, tool_calls, finish_reason, timestamp) VALUES ('h3','assistant','',?1,'tool_calls',?2)",
            rusqlite::params![tc, now_s],
        )
        .unwrap();
        let mut any = false;
        let evs = c.poll_once(&conn, &mut prev, &mut any).unwrap();
        assert!(!evs.iter().any(|e| matches!(e, StateEvent::QuestionRequested { .. })));
    }

    #[test]
    fn fallback_copy_open() {
        let (_td, conn) = tmp_db();
        conn.execute_batch("INSERT INTO sessions (id, started_at) VALUES ('x', 1);").unwrap();
        let path = PathBuf::from(conn.path().expect("db path"));
        drop(conn);
        let c = open_readonly(&path).expect("fallback open works");
        let n: i64 = c.query_row("SELECT count(*) FROM sessions", [], |r| r.get(0)).unwrap();
        assert_eq!(n, 1);
    }
}
