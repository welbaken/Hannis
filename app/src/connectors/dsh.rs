//! DSH connector.
//!
//! Data sources (wire formats verified against the live instance):
//! - `session.list`  polling: baseline session state (running flag, titles).
//! - `session.history` polling: THE session-event source. Empirically the
//!   events.mux stream does NOT forward `session/event` frames to external
//!   clients on the current runtime, but `session.history` returns the full
//!   event log including per-token `assistant/chunk` (reasoning-delta /
//!   text-delta). We poll the tail window (`maxMessages=2`, seq-deduped)
//!   every `history_ms` for running sessions, which yields near-real-time
//!   thinking/output text plus turn/tool/todo events.
//! - `events.mux` WebSocket: approvals, questions, jobs, queue (these DO
//!   flow to external clients).
//! - `events.host` WebSocket: host/session-status flips.

use super::{send, sleep_interruptible};
use crate::http::{request, Url, Ws, WsError, WS_OP_TEXT};
use crate::state::{SessionItem, Source, StateEvent, TodoItem, TurnEndReason};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

struct Health {
    poll_ok: bool,
    last_frame: Instant,
    healthy: bool,
}

pub struct DshConnector {
    pub url: String,
    pub poll_ms: u64,
    /// session.history poll interval (ms) for live text/event streaming.
    pub history_ms: u64,
}

impl DshConnector {
    pub fn spawn(self, tx: Sender<StateEvent>, stop: Arc<AtomicBool>) {
        let health = Arc::new(Mutex::new(Health {
            poll_ok: false,
            last_frame: Instant::now() - Duration::from_secs(60),
            healthy: false,
        }));
        let url = self.url.clone();
        let poll_ms = self.poll_ms;
        let history_ms = self.history_ms;
        let (h, t, s) = (health.clone(), tx.clone(), stop.clone());
        let url1 = url.clone();
        std::thread::Builder::new()
            .name("dsh-poll".into())
            .spawn(move || {
                let c = DshConnector { url: url1, poll_ms, history_ms: 0 };
                c.poll_loop(h, t, s)
            })
            .ok();
        let (h, t, s) = (health.clone(), tx.clone(), stop.clone());
        let url2 = url.clone();
        std::thread::Builder::new()
            .name("dsh-mux".into())
            .spawn(move || stream_loop("mux", &url2, h, t, s, handle_mux_frame))
            .ok();
        let (h, t, s) = (health.clone(), tx.clone(), stop.clone());
        let url3 = url.clone();
        std::thread::Builder::new()
            .name("dsh-host".into())
            .spawn(move || stream_loop("host", &url3, h, t, s, handle_host_frame))
            .ok();
        // history poller: sole source of session events (turns/tools/chunks)
        let (t, s) = (tx.clone(), stop.clone());
        let url4 = url;
        std::thread::Builder::new()
            .name("dsh-history".into())
            .spawn(move || history_loop(&url4, history_ms, t, s))
            .ok();
    }

    fn poll_loop(&self, health: Arc<Mutex<Health>>, tx: Sender<StateEvent>, stop: Arc<AtomicBool>) {
        let mut last_err: Option<String> = None;
        let mut seq = 0u64;
        let mut url = Url::parse(&self.url).unwrap_or_else(|_| Url {
            host: "127.0.0.1".into(),
            port: 3080,
            path: "/".into(),
        });
        while !stop.load(Ordering::Relaxed) {
            let t0 = Instant::now();
            seq += 1;
            match fetch_sessions(&mut url, seq) {
                Ok(items) => {
                    if let Ok(mut h) = health.lock() {
                        h.poll_ok = true;
                    }
                    send(&tx, StateEvent::Poll { source: Source::Dsh, items, ok: true, error: None });
                    last_err = None;
                }
                Err(e) => {
                    if last_err.as_deref() != Some(e.as_str()) {
                        eprintln!("[dsh] poll error: {e}");
                        last_err = Some(e.clone());
                    }
                    if let Ok(mut h) = health.lock() {
                        h.poll_ok = false;
                    }
                    send(&tx, StateEvent::Poll { source: Source::Dsh, items: vec![], ok: false, error: Some(e) });
                }
            }
            let healthy = {
                let h = health.lock().unwrap();
                h.poll_ok || h.last_frame.elapsed() < Duration::from_secs(10)
            };
            let mut flip = false;
            {
                let mut h = health.lock().unwrap();
                if h.healthy != healthy {
                    h.healthy = healthy;
                    flip = true;
                }
            }
            if flip {
                send(&tx, StateEvent::SourceHealth { source: Source::Dsh, healthy });
                eprintln!("[dsh] health -> {healthy}");
            }
            let elapsed = t0.elapsed().as_millis() as u64;
            let wait = self.poll_ms.saturating_sub(elapsed);
            sleep_interruptible(wait.max(100), &stop);
        }
    }
}

/// POST session.list and parse items.
fn fetch_sessions(url: &mut Url, seq: u64) -> Result<Vec<SessionItem>, String> {
    url.path = "/api/session.list".into();
    let envelope = serde_json::json!({
        "type": "client-request",
        "rpcId": format!("pet-{seq}"),
        "method": "session.list",
        "payload": {}
    });
    let resp = request(
        url,
        "POST",
        &[("Content-Type", "application/json".into())],
        Some(envelope.to_string().as_bytes()),
        Duration::from_secs(5),
    )?;
    if resp.status != 200 {
        return Err(format!("session.list status {}", resp.status));
    }
    let v: Value = serde_json::from_slice(&resp.body)
        .map_err(|e| format!("session.list json: {e}"))?;
    let result = &v["result"];
    let ok = result.get("ok").and_then(Value::as_bool).unwrap_or(false);
    if !ok {
        return Err(format!("session.list ok=false: {}", result));
    }
    let items = result
        .get("value")
        .and_then(|x| x.get("items"))
        .or_else(|| result.get("items"))
        .and_then(Value::as_array)
        .cloned()
        .ok_or_else(|| "session.list: no items".to_string())?;
    let mut out = Vec::with_capacity(items.len());
    for it in items {
        let session_id = it["sessionId"].as_str().unwrap_or("").to_string();
        if session_id.is_empty() {
            continue;
        }
        let running = it["running"].as_bool().unwrap_or(false);
        let values = &it["projections"]["values"];
        let title = values["title"].as_str().map(|s| s.to_string());
        let todos = values["todos"].as_array().map(|arr| {
            arr.iter()
                .filter_map(|t| {
                    let content = t["content"].as_str()?.to_string();
                    let status = t["status"].as_str().unwrap_or("pending").to_string();
                    Some(TodoItem { content, status })
                })
                .collect()
        });
        out.push(SessionItem { session_id, running, title, todos });
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// session.history poller
// ---------------------------------------------------------------------------

const HISTORY_MAX_MESSAGES: u64 = 2;
/// Baseline window for the FIRST poll of a session: it must reach far
/// enough back to include the open turn's `turn/start` and any open tool
/// calls. A too-small window (2 messages) only sees the tail of a long
/// running session, so the pet would never learn that the session is
/// thinking/working — it only appears when it finally ends.
const HISTORY_MAX_MESSAGES_BASELINE: u64 = 200;
/// extra cycles a just-stopped session stays polled (to catch the final
/// turn/end event that may land after `running` flips to false)
const GRACE_CYCLES: u32 = 3;

struct HistoryState {
    /// session -> last applied seq (absent before the seeding poll)
    last_seq: HashMap<String, u64>,
    /// session -> open tool calls (callId -> name), for tool/result pairing
    open_calls: HashMap<String, Vec<(String, String)>>,
    /// session -> cycles since it was last seen running (grace polling)
    recent: HashMap<String, u32>,
}

/// Apply one history batch for a session and return the StateEvents to emit.
///
/// First batch (baseline, big window): reconstruct the session's CURRENT
/// state — the open turn (from its `turn/start`, or inferred from any
/// event's `data.turn` as long as that turn did not end inside the window),
/// open tool calls, todos and the last user message — WITHOUT replaying live
/// text or turn endings (a replayed ending would stamp `last_end` with the
/// pet's wall clock and fake a done/fail burst).
///
/// Later batches (small window): emit the delta (seq > last_seq), including
/// live text and endings.
fn apply_history_events(session_id: &str, entries: &[Value], st: &mut HistoryState) -> Vec<StateEvent> {
    let seeded = st.last_seq.contains_key(session_id);
    let mut max_seq = st.last_seq.get(session_id).copied().unwrap_or(0);
    let mut evs: Vec<StateEvent> = Vec::new();
    let mut live_reasoning = String::new();
    let mut live_text = String::new();
    if !seeded {
        // ---- baseline: reconstruct the CURRENT state ----
        let mut open_turn: Option<u64> = None;
        for entry in entries {
            let ev = &entry["event"];
            let ev_seq = ev["seq"].as_u64().unwrap_or(0);
            if ev_seq > max_seq {
                max_seq = ev_seq;
            }
            if ev_seq <= st.last_seq.get(session_id).copied().unwrap_or(0) {
                continue;
            }
            let data = &ev["data"];
            let t = ev["type"].as_str().unwrap_or("");
            if t == "turn/start" {
                open_turn = data["turn"].as_u64();
                continue;
            }
            if t == "turn/end" {
                if data["turn"].as_u64() == open_turn {
                    open_turn = None;
                }
                continue;
            }
            // any event carrying a turn number hints the open turn when its
            // start event has left the window
            if open_turn.is_none() {
                open_turn = data["turn"].as_u64();
            }
            match t {
                "tool/call" => {
                    // only tools of the open turn are live; tools of a turn
                    // that ended inside the window are closed (a missing
                    // turn field is treated as the current turn)
                    let tool_turn = data["turn"].as_u64();
                    if open_turn.is_none() || tool_turn.is_none() || tool_turn == open_turn {
                        let call_id = data["callId"].as_str().unwrap_or("").to_string();
                        let name = data["name"].as_str().unwrap_or("tool").to_string();
                        let arguments = data["arguments"].as_str().map(|a| a.to_string());
                        st.open_calls.entry(session_id.to_string()).or_default().push((call_id, name.clone()));
                        evs.push(StateEvent::ToolStarted {
                            source: Source::Dsh,
                            session_id: session_id.to_string(),
                            name,
                            arguments,
                        });
                    }
                }
                "tool/result" => {
                    // close only calls whose start was emitted in this batch
                    let call_id = data["message"]["source"]["callId"].as_str().unwrap_or("");
                    if let Some(name) = st.open_calls.get_mut(session_id).and_then(|calls| {
                        let idx = calls.iter().position(|(id, _)| id == call_id);
                        idx.map(|i| calls.remove(i).1)
                    }) {
                        evs.push(StateEvent::ToolEnded {
                            source: Source::Dsh,
                            session_id: session_id.to_string(),
                            name,
                            error: data.get("error").is_some(),
                        });
                    }
                }
                "todo/write" => {
                    let todos = data["todos"]
                        .as_array()
                        .map(|arr| {
                            arr.iter()
                                .filter_map(|t| {
                                    Some(TodoItem {
                                        content: t["content"].as_str()?.to_string(),
                                        status: t["status"].as_str().unwrap_or("pending").to_string(),
                                    })
                                })
                                .collect()
                        })
                        .unwrap_or_default();
                    evs.push(StateEvent::TodoSnapshot {
                        source: Source::Dsh,
                        session_id: session_id.to_string(),
                        todos,
                    });
                }
                "user/message" => {
                    let text = message_text(data);
                    if !text.is_empty() {
                        evs.push(StateEvent::UserMessage {
                            source: Source::Dsh,
                            session_id: session_id.to_string(),
                            text,
                        });
                    }
                }
                _ => {}
            }
        }
        if let Some(turn) = open_turn {
            evs.push(StateEvent::TurnStarted {
                source: Source::Dsh,
                session_id: session_id.to_string(),
                turn,
            });
        }
    } else {
        // ---- delta: everything new since last_seq ----
        for entry in entries {
            let ev = &entry["event"];
            let ev_seq = ev["seq"].as_u64().unwrap_or(0);
            if ev_seq > max_seq {
                max_seq = ev_seq;
            }
            if ev_seq <= st.last_seq.get(session_id).copied().unwrap_or(0) {
                continue;
            }
            let data = &ev["data"];
            match ev["type"].as_str().unwrap_or("") {
                "turn/start" => {
                    evs.push(StateEvent::TurnStarted {
                        source: Source::Dsh,
                        session_id: session_id.to_string(),
                        turn: data["turn"].as_u64().unwrap_or(0),
                    });
                }
                "turn/end" => {
                    let kind = data["reason"]["kind"].as_str().unwrap_or("");
                    let reason = TurnEndReason::from_dsh_kind(kind).unwrap_or(TurnEndReason::Aborted);
                    evs.push(StateEvent::TurnEnded {
                        source: Source::Dsh,
                        session_id: session_id.to_string(),
                        turn: data["turn"].as_u64().unwrap_or(0),
                        reason,
                    });
                    flush_live(&mut evs, session_id, &mut live_reasoning, &mut live_text);
                }
                "tool/call" => {
                    let call_id = data["callId"].as_str().unwrap_or("").to_string();
                    let name = data["name"].as_str().unwrap_or("tool").to_string();
                    let arguments = data["arguments"].as_str().map(|a| a.to_string());
                    st.open_calls.entry(session_id.to_string()).or_default().push((call_id, name.clone()));
                    evs.push(StateEvent::ToolStarted {
                        source: Source::Dsh,
                        session_id: session_id.to_string(),
                        name,
                        arguments,
                    });
                    flush_live(&mut evs, session_id, &mut live_reasoning, &mut live_text);
                }
                "tool/result" => {
                    let call_id = data["message"]["source"]["callId"].as_str().unwrap_or("");
                    let name = st
                        .open_calls
                        .get_mut(session_id)
                        .and_then(|calls| {
                            let idx = calls.iter().position(|(id, _)| id == call_id);
                            idx.map(|i| calls.remove(i).1)
                        })
                        .unwrap_or_else(|| "tool".to_string());
                    let error = data.get("error").is_some();
                    evs.push(StateEvent::ToolEnded {
                        source: Source::Dsh,
                        session_id: session_id.to_string(),
                        name,
                        error,
                    });
                }
                "todo/write" => {
                    let todos = data["todos"]
                        .as_array()
                        .map(|arr| {
                            arr.iter()
                                .filter_map(|t| {
                                    Some(TodoItem {
                                        content: t["content"].as_str()?.to_string(),
                                        status: t["status"].as_str().unwrap_or("pending").to_string(),
                                    })
                                })
                                .collect()
                        })
                        .unwrap_or_default();
                    evs.push(StateEvent::TodoSnapshot {
                        source: Source::Dsh,
                        session_id: session_id.to_string(),
                        todos,
                    });
                }
                "assistant/chunk" => {
                    let chunk = &data["chunk"];
                    let ctype = chunk.get("type").and_then(Value::as_str).unwrap_or("");
                    let text = chunk.get("text").and_then(Value::as_str).unwrap_or("");
                    match ctype {
                        "reasoning-delta" => live_reasoning.push_str(text),
                        "text-delta" => live_text.push_str(text),
                        _ => {}
                    }
                }
                "user/message" => {
                    let text = message_text(data);
                    if !text.is_empty() {
                        evs.push(StateEvent::UserMessage {
                            source: Source::Dsh,
                            session_id: session_id.to_string(),
                            text,
                        });
                    }
                    flush_live(&mut evs, session_id, &mut live_reasoning, &mut live_text);
                }
                "assistant/message" | "step/end" => {
                    flush_live(&mut evs, session_id, &mut live_reasoning, &mut live_text);
                }
                _ => {}
            }
        }
        flush_live(&mut evs, session_id, &mut live_reasoning, &mut live_text);
    }
    st.last_seq.insert(session_id.to_string(), max_seq);
    evs
}

fn history_loop(url_str: &str, history_ms: u64, tx: Sender<StateEvent>, stop: Arc<AtomicBool>) {
    let mut hist_err: Option<String> = None;
    let mut url = Url::parse(url_str).unwrap();
    let seq = AtomicU64::new(1);
    let mut st = HistoryState {
        last_seq: HashMap::new(),
        open_calls: HashMap::new(),
        recent: HashMap::new(),
    };
    let interval = history_ms.max(300);
    while !stop.load(Ordering::Relaxed) {
        let t0 = Instant::now();
        let n = seq.fetch_add(1, Ordering::Relaxed);
        match fetch_sessions(&mut url, n) {
            Ok(items) => {
                let running: HashSet<String> =
                    items.iter().filter(|i| i.running).map(|i| i.session_id.clone()).collect();
                // sessions to poll: currently running + grace window
                let mut targets: Vec<String> = running.clone().into_iter().collect();
                let mut next_recent: HashMap<String, u32> = HashMap::new();
                for (id, cycles) in &st.recent {
                    let c = if running.contains(id) { 0 } else { *cycles + 1 };
                    if c <= GRACE_CYCLES {
                        targets.push(id.clone());
                        next_recent.insert(id.clone(), c);
                    }
                }
                for id in &running {
                    next_recent.entry(id.clone()).or_insert(0);
                }
                st.recent = next_recent;
                for id in &targets {
                    if stop.load(Ordering::Relaxed) {
                        break;
                    }
                    let n2 = seq.fetch_add(1, Ordering::Relaxed);
                    if let Err(e) = poll_history(&mut url, &id, n2, &mut st, &tx) {
                        if hist_err.as_deref() != Some(e.as_str()) {
                            eprintln!("[dsh] history {id}: {e}");
                            hist_err = Some(e);
                        }
                    }
                }
            }
            Err(e) => {
                if hist_err.as_deref() != Some(e.as_str()) {
                    eprintln!("[dsh] history session list: {e}");
                    hist_err = Some(e.clone());
                }
            }
        }
        let elapsed = t0.elapsed().as_millis() as u64;
        sleep_interruptible(interval.saturating_sub(elapsed), &stop);
    }
}

/// One session.history poll: fetch the tail window, apply events with
/// seq > last_seq. The first poll for a session only seeds last_seq.
fn poll_history(
    url: &mut Url,
    session_id: &str,
    seq: u64,
    st: &mut HistoryState,
    tx: &Sender<StateEvent>,
) -> Result<(), String> {
    url.path = "/api/session.history".into();
    // the FIRST poll of a session uses the big baseline window so the
    // current state (open turn / tools) can be reconstructed even for a
    // session that has been running for a while
    let max_messages = if st.last_seq.contains_key(session_id) {
        HISTORY_MAX_MESSAGES
    } else {
        HISTORY_MAX_MESSAGES_BASELINE
    };
    let envelope = serde_json::json!({
        "type": "client-request",
        "rpcId": format!("pet-{seq}"),
        "method": "session.history",
        "payload": { "sessionId": session_id, "maxMessages": max_messages }
    });
    let resp = request(
        url,
        "POST",
        &[("Content-Type", "application/json".into())],
        Some(envelope.to_string().as_bytes()),
        Duration::from_secs(5),
    )?;
    if resp.status != 200 {
        return Err(format!("status {}", resp.status));
    }
    let v: Value = serde_json::from_slice(&resp.body)
        .map_err(|e| format!("json: {e}"))?;
    let result = &v["result"];
    if result.get("ok").and_then(Value::as_bool).unwrap_or(false) != true {
        return Err(format!("ok=false: {result}"));
    }
    let entries = result["value"]["events"].as_array().cloned().unwrap_or_default();

    let evs = apply_history_events(session_id, &entries, st);
    for ev in evs {
        send(tx, ev);
    }
    Ok(())
}

/// Concatenate the text blocks of a message payload (user/assistant messages).
fn message_text(data: &Value) -> String {
    let mut out = String::new();
    if let Some(blocks) = data["message"]["content"].as_array() {
        for b in blocks {
            if b["type"].as_str() == Some("text") {
                if let Some(t) = b["text"].as_str() {
                    out.push_str(t);
                }
            }
        }
    }
    out
}

fn flush_live(
    evs: &mut Vec<StateEvent>,
    session_id: &str,
    reasoning: &mut String,
    text: &mut String,
) {
    if reasoning.is_empty() && text.is_empty() {
        return;
    }
    let r = std::mem::take(reasoning);
    let t = std::mem::take(text);
    evs.push(StateEvent::LiveText {
        source: Source::Dsh,
        session_id: session_id.to_string(),
        reasoning: if r.is_empty() { None } else { Some(r) },
        text: if t.is_empty() { None } else { Some(t) },
        tool_name: None,
    });
}

// ---------------------------------------------------------------------------
// mux / host WebSocket streams (approvals, questions, jobs, session status)
// ---------------------------------------------------------------------------

/// Generic WS stream loop with reconnect/backoff; dispatches envelopes via `handler`.
fn stream_loop(
    name: &str,
    base_url: &str,
    health: Arc<Mutex<Health>>,
    tx: Sender<StateEvent>,
    stop: Arc<AtomicBool>,
    handler: fn(&mut StreamCtx, &str) -> Option<Vec<StateEvent>>,
) {
    let mut url = Url::parse(base_url).unwrap();
    let mut stream_err: Option<String> = None;
    loop {
        if stop.load(Ordering::Relaxed) {
            break;
        }
        let path = format!("/api/events.{name}");
        url.path = path.clone();
        match Ws::connect(&url, &path, Duration::from_secs(8)) {
            Ok(mut ws) => {
                {
                    let mut h = health.lock().unwrap();
                    h.last_frame = Instant::now();
                }
                eprintln!("[dsh] {name} connected");
                stream_err = None;
                if name == "mux" {
                    // Reconcile pending requests with the server: right after
                    // connect the server replays its CURRENT pending
                    // questions/approvals, so drop our local copies first —
                    // anything the server no longer knows about (crash /
                    // restart, a resolved frame missed during a WS blip)
                    // must not keep the pet in attention forever, and the
                    // replay re-adds what is still genuinely pending.
                    send(&tx, StateEvent::PendingSync { source: Source::Dsh });
                }
                let mut ctx = StreamCtx::default();
                loop {
                    if stop.load(Ordering::Relaxed) {
                        break;
                    }
                    match ws.read_frame() {
                        Ok(f) => {
                            {
                                let mut h = health.lock().unwrap();
                                h.last_frame = Instant::now();
                            }
                            if f.opcode != WS_OP_TEXT {
                                continue;
                            }
                            for env in split_json(&f.payload) {
                                if let Some(evs) = handler(&mut ctx, &env) {
                                    for ev in evs {
                                        send(&tx, ev);
                                    }
                                }
                            }
                        }
                        Err(WsError::Timeout) => {}
                        Err(e) => {
                            let msg = e.to_string();
                            if stream_err.as_deref() != Some(msg.as_str()) {
                                eprintln!("[dsh] {name} stream error: {msg}, reconnecting");
                                stream_err = Some(msg);
                            }
                            break;
                        }
                    }
                }
            }
            Err(e) => {
                if stream_err.as_deref() != Some(e.as_str()) {
                    eprintln!("[dsh] {name} connect failed: {e}");
                    stream_err = Some(e.clone());
                }
            }
        }
        sleep_interruptible(3000, &stop);
    }
}

fn split_json(payload: &[u8]) -> Vec<String> {
    let text = String::from_utf8_lossy(payload);
    let it = serde_json::Deserializer::from_str(&text).into_iter::<Value>();
    let mut out = Vec::new();
    for v in it {
        match v {
            Ok(v) => out.push(v.to_string()),
            Err(_) => break,
        }
    }
    out
}

struct StreamCtx {
    jobs: HashMap<String, (String, String)>, // session -> (job_id, status)
}

impl Default for StreamCtx {
    fn default() -> Self {
        StreamCtx { jobs: HashMap::new() }
    }
}

/// Pending-question key for one item of a `question/requested` frame.
/// The server's `question/resolved` frame carries only the envelope rpcId
/// (`questionRpcId`), so the key is `rpcId\u{0}itemId` — state.rs clears a
/// whole request by prefix. Falls back to the bare item id for malformed
/// frames (envelope without rpcId).
fn question_pending_id(rpc_id: &str, item_id: &str) -> String {
    if rpc_id.is_empty() {
        item_id.to_string()
    } else {
        format!("{rpc_id}\u{0}{item_id}")
    }
}

/// Parse a `server-request` envelope payload (mux frame).
/// session/event frames are intentionally NOT handled here: session events
/// come from the session.history poller (mux does not deliver them to
/// external clients on the current runtime, and history is the single source).
fn handle_mux_frame(ctx: &mut StreamCtx, envelope: &str) -> Option<Vec<StateEvent>> {
    let v: Value = serde_json::from_str(envelope).ok()?;
    let payload = v.get("payload")?;
    let ftype = payload.get("type")?.as_str()?;
    let sid = payload.get("sessionId").and_then(Value::as_str).unwrap_or("");
    let mut evs = Vec::new();
    match ftype {
        "session/jobs" => {
            if let Some(jobs) = payload.get("jobs").and_then(Value::as_array) {
                let mut seen = Vec::new();
                for j in jobs {
                    let id = j["id"].as_str().unwrap_or("").to_string();
                    let status = j["status"].as_str().unwrap_or("").to_string();
                    if id.is_empty() {
                        continue;
                    }
                    seen.push(id.clone());
                    let prev = ctx.jobs.get(sid).cloned();
                    let cur = (id.clone(), status.clone());
                    if prev.as_ref() != Some(&cur) {
                        ctx.jobs.insert(sid.to_string(), cur);
                        let running = status == "running" || status == "queued";
                        let was_running =
                            prev.map(|(_, s)| s == "running" || s == "queued").unwrap_or(false);
                        if running && !was_running {
                            evs.push(StateEvent::ToolStarted {
                                source: Source::Dsh,
                                session_id: sid.to_string(),
                                name: format!("job:{id}"),
                                arguments: None,
                            });
                        } else if !running && was_running {
                            evs.push(StateEvent::ToolEnded {
                                source: Source::Dsh,
                                session_id: sid.to_string(),
                                name: format!("job:{id}"),
                                error: status == "failed",
                            });
                        }
                    }
                }
                if let Some(prev) = ctx.jobs.get(sid) {
                    if !seen.contains(&prev.0) {
                        ctx.jobs.remove(sid);
                    }
                }
            }
        }
        "approval/requested" => {
            evs.push(StateEvent::ApprovalRequested {
                source: Source::Dsh,
                id: payload["approvalId"].as_str().unwrap_or("").to_string(),
                session_id: sid.to_string(),
                tool: payload["toolName"].as_str().unwrap_or("").to_string(),
            });
        }
        "approval/resolved" => {
            evs.push(StateEvent::ApprovalResolved {
                source: Source::Dsh,
                id: payload["approvalId"].as_str().unwrap_or("").to_string(),
            });
        }
        "question/requested" => {
            // The server resolves the whole request through the ENVELOPE rpcId
            // (that same value arrives later as payload.questionRpcId in
            // `question/resolved`); the questions[] items only carry
            // model-supplied ids. Key each pending item by
            // `<rpcId>\u{0}<itemId>` so state.rs can clear the whole request
            // by prefix; a bare item id would never match the resolved frame
            // and the pet would stay in attention forever.
            let rpc_id = v.get("rpcId").and_then(Value::as_str).unwrap_or("").to_string();
            if let Some(qs) = payload["questions"].as_array() {
                for q in qs {
                    evs.push(StateEvent::QuestionRequested {
                        source: Source::Dsh,
                        id: question_pending_id(&rpc_id, q["id"].as_str().unwrap_or("")),
                        session_id: sid.to_string(),
                        text: q["question"].as_str().unwrap_or("").to_string(),
                    });
                }
            }
        }
        "question/resolved" => {
            evs.push(StateEvent::QuestionResolved {
                source: Source::Dsh,
                id: payload["questionRpcId"].as_str().unwrap_or("").to_string(),
            });
        }
        _ => {}
    }
    if evs.is_empty() {
        None
    } else {
        Some(evs)
    }
}

fn handle_host_frame(_ctx: &mut StreamCtx, envelope: &str) -> Option<Vec<StateEvent>> {
    let v: Value = serde_json::from_str(envelope).ok()?;
    let payload = v.get("payload")?;
    let ftype = payload.get("type")?.as_str()?;
    match ftype {
        "host/session-status" => {
            let sid = payload["sessionId"].as_str().unwrap_or("");
            let running = payload["running"].as_bool().unwrap_or(false);
            if !sid.is_empty() {
                Some(vec![StateEvent::SessionStatus {
                    source: Source::Dsh,
                    session_id: sid.to_string(),
                    running,
                }])
            } else {
                None
            }
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn split_multiple_envelopes() {
        let s = br#"{"type":"a","payload":{"type":"x"}}{"type":"b","payload":{"type":"y"}}"#;
        let parts = split_json(s);
        assert_eq!(parts.len(), 2);
        assert!(parts[0].contains("\"type\":\"a\""));
        assert!(parts[1].contains("\"type\":\"b\""));
    }

    #[test]
    fn mux_approval_question_jobs() {
        let mut ctx = StreamCtx::default();
        let env = json!({
            "type":"server-request","rpcId":"r","method":"approval/requested",
            "payload":{"type":"approval/requested","sessionId":"s1","approvalId":"a1","toolName":"bash"}
        })
        .to_string();
        let evs = handle_mux_frame(&mut ctx, &env).unwrap();
        assert!(matches!(&evs[0], StateEvent::ApprovalRequested { id, tool, .. } if id=="a1" && tool=="bash"));

        let env = json!({
            "type":"server-request","rpcId":"r-q1","method":"question/requested",
            "payload":{"type":"question/requested","sessionId":"s1","questions":[{"id":"q1","question":"继续吗?"}]}
        })
        .to_string();
        let evs = handle_mux_frame(&mut ctx, &env).unwrap();
        // pending key is the ENVELOPE rpcId (+ item id), because
        // question/resolved only carries the rpcId
        assert!(matches!(&evs[0], StateEvent::QuestionRequested { id, text, .. } if id=="r-q1\u{0}q1" && text=="继续吗?"));

        let env = json!({
            "type":"server-request","rpcId":"x","method":"question/resolved",
            "payload":{"type":"question/resolved","sessionId":"s1","questionRpcId":"r-q1","outcome":"answered"}
        })
        .to_string();
        let evs = handle_mux_frame(&mut ctx, &env).unwrap();
        assert!(matches!(&evs[0], StateEvent::QuestionResolved { id, .. } if id=="r-q1"));

        let job = |status: &str| {
            json!({
                "type":"server-request","rpcId":"r","method":"session/jobs",
                "payload":{"type":"session/jobs","sessionId":"s1","jobs":[{"id":"bash-1","kind":"bash","status":status}]}
            })
            .to_string()
        };
        let evs = handle_mux_frame(&mut ctx, &job("running")).unwrap();
        assert!(matches!(&evs[0], StateEvent::ToolStarted { name, .. } if name=="job:bash-1"));
        let evs = handle_mux_frame(&mut ctx, &job("completed")).unwrap();
        assert!(matches!(&evs[0], StateEvent::ToolEnded { name, error, .. } if name=="job:bash-1" && !error));
    }

    #[test]
    fn host_status_frame() {
        let mut ctx = StreamCtx::default();
        let env = json!({
            "type":"server-request","rpcId":"r","method":"host/session-status",
            "payload":{"type":"host/session-status","sessionId":"s1","running":true}
        })
        .to_string();
        let evs = handle_host_frame(&mut ctx, &env).unwrap();
        assert!(matches!(&evs[0], StateEvent::SessionStatus { running: true, .. }));
    }

    // ---- history parsing (without HTTP): feed the same event shapes ----

    fn entry(seq: u64, ev: Value) -> Value {
        json!({ "event": { "seq": seq, "type": ev["type"], "data": ev["data"] } })
    }

    fn chunk(seq: u64, ctype: &str, text: &str) -> Value {
        entry(
            seq,
            json!({ "type": "assistant/chunk", "data": { "turn": 1, "step": 1, "chunk": { "type": ctype, "text": text } } }),
        )
    }

    /// Run entries through the real parsing loop used by poll_history,
    /// without network: returns the emitted events and the final last_seq.
    fn parse_events(
        session_id: &str,
        entries: Vec<Value>,
        st: &mut HistoryState,
    ) -> (Vec<StateEvent>, u64) {
        let evs = apply_history_events(session_id, &entries, st);
        let last = st.last_seq.get(session_id).copied().unwrap_or(0);
        (evs, last)
    }

    #[test]
    fn history_seeds_then_streams_chunks() {
        let mut st = HistoryState {
            last_seq: HashMap::new(),
            open_calls: HashMap::new(),
            recent: HashMap::new(),
        };
        // poll 1: baseline - live text is NOT replayed, but the open turn is
        // reconstructed from the chunk turn numbers (its start left the
        // small window; this is the mid-run discovery fix)
        let (evs, _) = parse_events("s1", vec![chunk(100, "reasoning-delta", "旧内容"), chunk(101, "text-delta", "old")], &mut st);
        assert_eq!(evs.len(), 1);
        assert!(matches!(&evs[0], StateEvent::TurnStarted { turn: 1, .. }));
        // seeding also establishes state from the window: an open tool is
        // applied, but a turn that STARTED and ENDED inside the window is
        // not replayed (it would leave a phantom open turn)
        let mut st2 = HistoryState { last_seq: HashMap::new(), open_calls: HashMap::new(), recent: HashMap::new() };
        let (evs, _) = parse_events(
            "s1",
            vec![
                entry(1, json!({"type":"turn/start","data":{"turn":3}})),
                entry(2, json!({"type":"tool/call","data":{"turn":3,"callId":"c9","name":"bash"}})),
                entry(3, json!({"type":"turn/end","data":{"turn":3,"reason":{"kind":"completed"}}})),
            ],
            &mut st2,
        );
        assert_eq!(evs.len(), 1);
        assert!(matches!(&evs[0], StateEvent::ToolStarted { name, .. } if name == "bash"));
        // poll 2: new deltas stream in
        let (evs, _) = parse_events(
            "s1",
            vec![
                chunk(100, "reasoning-delta", "旧内容"),
                chunk(102, "reasoning-delta", "思考"),
                chunk(103, "text-delta", "hi"),
            ],
            &mut st,
        );
        assert!(matches!(&evs[0], StateEvent::LiveText { reasoning, text, .. }
            if reasoning.as_deref() == Some("思考") && text.as_deref() == Some("hi")));
        assert_eq!(evs.len(), 1);
        // poll 3: only newer seqs
        let (evs, _) = parse_events(
            "s1",
            vec![chunk(102, "reasoning-delta", "思考"), chunk(104, "text-delta", " more")],
            &mut st,
        );
        assert!(matches!(&evs[0], StateEvent::LiveText { text, .. } if text.as_deref() == Some(" more")));
    }

    #[test]
    fn baseline_recovers_midrun_session() {
        // a session discovered mid-run: turn/start is far outside the small
        // window, the tail only has chunks of turn 5 + an open tool call —
        // the baseline must reconstruct BOTH the open turn and the tool
        let mut st = HistoryState {
            last_seq: HashMap::new(),
            open_calls: HashMap::new(),
            recent: HashMap::new(),
        };
        let (evs, last) = parse_events(
            "s1",
            vec![
                entry(5000, json!({"type":"assistant/chunk","data":{"turn":5,"step":9,"chunk":{"type":"text-delta","text":"正在生成"}}})),
                entry(5001, json!({"type":"tool/call","data":{"turn":5,"step":9,"callId":"c9","name":"bash","arguments":"{}"}})),
            ],
            &mut st,
        );
        assert_eq!(last, 5001);
        assert!(matches!(&evs[0], StateEvent::ToolStarted { name, .. } if name == "bash"));
        assert!(matches!(&evs[1], StateEvent::TurnStarted { turn: 5, .. }));
        // no live text is replayed from the baseline window
        assert!(!evs.iter().any(|e| matches!(e, StateEvent::LiveText { .. })));
        // the delta pass then only carries NEW seqs
        let (evs, _) = parse_events("s1", vec![chunk(5002, "text-delta", "新内容")], &mut st);
        assert!(matches!(&evs[0], StateEvent::LiveText { text, .. } if text.as_deref() == Some("新内容")));
    }

    #[test]
    fn baseline_skips_turn_that_ended_in_window() {
        // turn 5 started outside the window; its chunks and its end are
        // inside -> the turn is NOT reconstructed (no phantom open turn)
        let mut st = HistoryState {
            last_seq: HashMap::new(),
            open_calls: HashMap::new(),
            recent: HashMap::new(),
        };
        let (evs, _) = parse_events(
            "s1",
            vec![
                entry(10, json!({"type":"assistant/chunk","data":{"turn":5,"step":1,"chunk":{"type":"text-delta","text":"x"}}})),
                entry(11, json!({"type":"turn/end","data":{"turn":5,"reason":{"kind":"completed"}}})),
            ],
            &mut st,
        );
        assert!(evs.is_empty());
    }

    #[test]
    fn baseline_net_closes_tools_completed_in_window() {
        // tool/call + tool/result both inside the baseline window: the tool
        // starts and ends in the same batch (net closed), and the turn that
        // carried it is reconstructed
        let mut st = HistoryState {
            last_seq: HashMap::new(),
            open_calls: HashMap::new(),
            recent: HashMap::new(),
        };
        let (evs, _) = parse_events(
            "s1",
            vec![
                entry(1, json!({"type":"tool/call","data":{"turn":7,"step":1,"callId":"c1","name":"bash"}})),
                entry(2, json!({"type":"tool/result","data":{"turn":7,"step":1,"message":{"source":{"callId":"c1"}}}})),
            ],
            &mut st,
        );
        assert!(matches!(&evs[0], StateEvent::ToolStarted { name, .. } if name == "bash"));
        assert!(matches!(&evs[1], StateEvent::ToolEnded { name, .. } if name == "bash"));
        assert!(matches!(&evs[2], StateEvent::TurnStarted { turn: 7, .. }));
        assert_eq!(evs.len(), 3);
    }

    #[test]
    fn history_turn_and_tool_events() {
        let mut st = HistoryState {
            last_seq: HashMap::new(),
            open_calls: HashMap::new(),
            recent: HashMap::new(),
        };
        // seed
        let _ = parse_events("s1", vec![entry(1, json!({"type":"turn/start","data":{"turn":1}}))], &mut st);
        let (evs, _) = parse_events(
            "s1",
            vec![
                entry(2, json!({"type":"turn/start","data":{"turn":2}})),
                entry(3, json!({"type":"tool/call","data":{"turn":2,"step":1,"callId":"c1","name":"bash","arguments":"{}"}})),
                entry(4, json!({"type":"tool/result","data":{"turn":2,"step":1,"message":{"source":{"callId":"c1"}}}})),
                entry(5, json!({"type":"turn/end","data":{"turn":2,"reason":{"kind":"completed"}}})),
            ],
            &mut st,
        );
        assert!(matches!(&evs[0], StateEvent::TurnStarted { turn: 2, .. }));
        assert!(matches!(&evs[1], StateEvent::ToolStarted { name, .. } if name == "bash"));
        assert!(matches!(&evs[2], StateEvent::ToolEnded { name, error, .. } if name == "bash" && !error));
        assert!(matches!(&evs[3], StateEvent::TurnEnded { reason: TurnEndReason::Completed, .. }));
    }

    #[test]
    fn history_tool_result_error_flag() {
        let mut st = HistoryState {
            last_seq: HashMap::new(),
            open_calls: HashMap::new(),
            recent: HashMap::new(),
        };
        let _ = parse_events("s1", vec![entry(1, json!({"type":"tool/call","data":{"callId":"c1","name":"bash"}}))], &mut st);
        let (evs, _) = parse_events(
            "s1",
            vec![entry(2, json!({"type":"tool/result","data":{"message":{"source":{"callId":"c1"}},"error":{"code":"x"}}}))],
            &mut st,
        );
        assert!(matches!(&evs[0], StateEvent::ToolEnded { error: true, .. }));
    }
}
