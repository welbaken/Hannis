//! ComfyUI connector: read-only HTTP polling + WebSocket push.
//!
//! Data sources (public, no auth, no plugin required):
//! - `GET /queue`        -> { queue_running: [...], queue_pending: [...] }
//! - `GET /history/{id}` -> terminal status: status.status_str = success|error
//! - `ws://host/ws`      -> execution_start / executing / progress /
//!                          execution_success / execution_error /
//!                          execution_interrupted / status
//!
//! Mapping to pet semantics (design doc §3/§4):
//! - prompt_id            -> session_id; one prompt execution = one turn;
//! - executing node       -> ToolStarted / ToolEnded (name = node_type);
//! - progress             -> refreshed tool arguments "value/max" (e.g. 12/20);
//! - success / error / interrupted -> TurnEnded (Completed / Error / Interrupted);
//! - queue_pending        -> StateEvent::QueueChanged (bubble queue count);
//! - ComfyUI has NO approval/question concept -> attention is never raised.
//!
//! Polling is the reliability baseline (`/queue` running + `/history` terminal
//! fallback); the WS stream only adds node granularity and instant terminal
//! events (design doc §1: push never replaces polling).

use super::{send, sleep_interruptible};
use crate::http::{request, Url, Ws, WsError, WS_OP_TEXT};
use crate::state::{SessionItem, Source, StateEvent, TurnEndReason};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Shared execution bookkeeping between the poll thread and the ws thread.
/// Guarded by a single mutex; both threads emit events for the same prompts,
/// so every transition is deduped here (a prompt is started once and
/// terminalled once).
#[derive(Default)]
struct RunState {
    /// prompt_id -> turn (always 1; presence = turn is open).
    running: HashMap<String, u32>,
    /// prompt_id -> finish timestamp ms (dedupe guard for terminal events).
    finished: HashMap<String, u64>,
    /// prompt_id -> current node_type (None = no node executing).
    node: HashMap<String, Option<String>>,
    /// prompt_id -> node-id -> class_type. The ws `executing` frame only
    /// carries the numeric node id; the class name comes from the prompt
    /// graph (`prompt` field of the queue item) captured by the poll thread.
    node_classes: HashMap<String, HashMap<String, String>>,
    /// progress percent buckets, for throttling arg refreshes.
    progress_pct: HashMap<String, u32>,
}

/// finished entries older than this are reaped (prompt ids never repeat).
const FINISHED_TTL_MS: u64 = 3 * 60 * 1000;

pub struct ComfyUiConnector {
    pub url: String,
    pub poll_ms: u64,
    pub ws: bool,
}

impl ComfyUiConnector {
    pub fn spawn(self, tx: Sender<StateEvent>, stop: Arc<AtomicBool>) {
        let state = Arc::new(Mutex::new(RunState::default()));
        let url = self.url.clone();
        let poll_ms = self.poll_ms.max(500);
        let (t, s, st) = (tx.clone(), stop.clone(), state.clone());
        std::thread::Builder::new()
            .name("comfyui-poll".into())
            .spawn(move || poll_loop(&url, poll_ms, t, s, st))
            .ok();
        if self.ws {
            let (t, s, st) = (tx, stop, state);
            std::thread::Builder::new()
                .name("comfyui-ws".into())
                .spawn(move || ws_loop(&self.url.clone(), t, s, st))
                .ok();
        }
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Poll loop: /queue baseline + /history terminal fallback
// ---------------------------------------------------------------------------

fn poll_loop(
    url_str: &str,
    poll_ms: u64,
    tx: Sender<StateEvent>,
    stop: Arc<AtomicBool>,
    state: Arc<Mutex<RunState>>,
) {
    let mut url = Url::parse(url_str).unwrap_or_else(|_| Url {
        host: "127.0.0.1".into(),
        port: 8188,
        path: "/".into(),
    });
    let mut healthy = false;
    while !stop.load(Ordering::Relaxed) {
        let t0 = Instant::now();
        let result = {
            let mut st = state.lock().unwrap();
            poll_once(&mut url, &tx, &mut st)
        };
        match result {
            Ok(()) => {
                if !healthy {
                    healthy = true;
                    send(&tx, StateEvent::SourceHealth { source: Source::ComfyUi, healthy: true });
                    eprintln!("[comfyui] health -> true");
                }
            }
            Err(e) => {
                eprintln!("[comfyui] poll error: {e}");
                if healthy {
                    healthy = false;
                    send(&tx, StateEvent::SourceHealth { source: Source::ComfyUi, healthy: false });
                    eprintln!("[comfyui] health -> false");
                }
            }
        }
        let elapsed = t0.elapsed().as_millis() as u64;
        sleep_interruptible(poll_ms.saturating_sub(elapsed), &stop);
    }
}

fn poll_once(url: &mut Url, tx: &Sender<StateEvent>, st: &mut RunState) -> Result<(), String> {
    let now = now_ms();
    st.finished.retain(|_, at| now.saturating_sub(*at) < FINISHED_TTL_MS);
    let (running_items, pending) = fetch_queue(url)?;
    send(
        tx,
        StateEvent::QueueChanged { source: Source::ComfyUi, pending: pending as u32 },
    );

    // start turns for prompts that appeared in queue_running
    let mut items = Vec::with_capacity(running_items.len());
    let mut seen: HashSet<String> = HashSet::new();
    for it in &running_items {
        let Some((pid, number, classes)) = queue_item(it) else { continue };
        seen.insert(pid.clone());
        st.node_classes.insert(pid.clone(), classes);
        start_turn(st, tx, &pid);
        // poll baseline carries Working: start a representative node tool and
        // upgrade it as soon as the graph / ws stream provides better names.
        ensure_placeholder(st, tx, &pid);
        resolve_node_names(st, tx, &pid);
        items.push(SessionItem {
            session_id: pid,
            running: true,
            title: Some(format!("出图任务 #{number}")),
            todos: None,
        });
    }

    // prompts we tracked but that left the queue -> terminal state via history
    let gone: Vec<String> = st
        .running
        .keys()
        .filter(|k| !seen.contains(*k))
        .cloned()
        .collect();
    for pid in gone {
        let reason = history_terminal(url, &pid).unwrap_or(TurnEndReason::Completed);
        finish_prompt(st, tx, &pid, reason, now);
    }

    send(tx, StateEvent::Poll { source: Source::ComfyUi, items, ok: true, error: None });
    Ok(())
}

/// One queue item -> (prompt_id, queue number, node-id -> class_type map).
///
/// Modern ComfyUI `/queue` items are ARRAYS (verified against server.py):
/// `[number, prompt_id, prompt, extra_data, outputs_to_execute]`.
/// Very old builds use objects `{number, prompt_id, prompt}` - kept as
/// fallback.
fn queue_item(it: &Value) -> Option<(String, u64, HashMap<String, String>)> {
    if let Some(a) = it.as_array() {
        let pid = a.get(1)?.as_str()?.to_string();
        let number = a.get(0).and_then(Value::as_u64).unwrap_or(0);
        let classes = prompt_classes(a.get(2));
        Some((pid, number, classes))
    } else {
        let pid = it["prompt_id"].as_str()?.to_string();
        let number = it["number"].as_u64().unwrap_or(0);
        let classes = prompt_classes(Some(&it["prompt"]));
        Some((pid, number, classes))
    }
}

/// Node id -> class_type from the workflow's prompt graph:
/// `prompt = {"3": {"class_type": "KSampler", "inputs": {...}}, ...}`.
fn prompt_classes(prompt: Option<&Value>) -> HashMap<String, String> {
    let mut map = HashMap::new();
    if let Some(obj) = prompt.and_then(|p| p.as_object()) {
        for (id, node) in obj {
            if let Some(ct) = node["class_type"].as_str() {
                map.insert(id.clone(), ct.to_string());
            }
        }
    }
    map
}

/// Placeholder tool name used while the prompt starts executing, before any
/// node-level info is known. Most ComfyUI builds route node frames ONLY to
/// the submitting browser client (verified: server.py add_message with
/// broadcast=False + sid=client_id), so the poll baseline carries Working.
const PLACEHOLDER: &str = "执行中";

/// Pick a representative node label from the prompt graph for the bubble:
/// prefer a sampler, then the VAE decode, else any node. This is what the
/// poll baseline shows as the "tool" while the ws stream is unavailable.
fn default_node_label(classes: &HashMap<String, String>) -> Option<String> {
    let lower = |s: &str| s.to_ascii_lowercase();
    let sampler = classes
        .values()
        .find(|v| lower(v).contains("sampler"))
        .map(|s| s.to_string());
    sampler.or_else(|| {
        classes
            .values()
            .find(|v| lower(v).contains("vaedecode"))
            .cloned()
    })
    .or_else(|| classes.values().next().cloned())
}

/// `GET /queue` -> (queue_running items, queue_pending count).
fn fetch_queue(url: &mut Url) -> Result<(Vec<Value>, usize), String> {
    url.path = "/queue".into();
    let resp = request(url, "GET", &[], None, Duration::from_secs(5))?;
    if resp.status != 200 {
        return Err(format!("GET /queue status {}", resp.status));
    }
    let v: Value =
        serde_json::from_slice(&resp.body).map_err(|e| format!("GET /queue json: {e}"))?;
    let running = v["queue_running"].as_array().cloned().unwrap_or_default();
    let pending = v["queue_pending"].as_array().map(|a| a.len()).unwrap_or(0);
    Ok((running, pending))
}

/// `GET /history/{prompt_id}` -> terminal reason from the status record.
/// status_str: "error" -> Error; otherwise scan the messages list for an
/// `execution_interrupted` record (interrupts keep status_str "success");
/// anything else counts as completed.
fn history_terminal(url: &mut Url, pid: &str) -> Option<TurnEndReason> {
    url.path = format!("/history/{pid}");
    let resp = request(url, "GET", &[], None, Duration::from_secs(5)).ok()?;
    if resp.status != 200 {
        return None;
    }
    let v: Value = serde_json::from_slice(&resp.body).ok()?;
    let status = &v[pid]["status"];
    if status["status_str"].as_str() == Some("error") {
        return Some(TurnEndReason::Error);
    }
    if let Some(msgs) = status["messages"].as_array() {
        if msgs.iter().any(|m| m["type"].as_str() == Some("execution_interrupted")) {
            return Some(TurnEndReason::Interrupted);
        }
    }
    Some(TurnEndReason::Completed)
}

// ---------------------------------------------------------------------------
// WS loop: node-level execution + instant terminal events
// ---------------------------------------------------------------------------

fn ws_loop(
    url_str: &str,
    tx: Sender<StateEvent>,
    stop: Arc<AtomicBool>,
    state: Arc<Mutex<RunState>>,
) {
    let mut url = Url::parse(url_str).unwrap();
    loop {
        if stop.load(Ordering::Relaxed) {
            break;
        }
        url.path = "/ws".into();
        match Ws::connect(&url, "/ws", Duration::from_secs(8)) {
            Ok(mut ws) => {
                eprintln!("[comfyui] ws connected");
                loop {
                    if stop.load(Ordering::Relaxed) {
                        break;
                    }
                    match ws.read_frame() {
                        Ok(f) => {
                            if f.opcode != WS_OP_TEXT {
                                continue;
                            }
                            let text = String::from_utf8_lossy(&f.payload);
                            if let Ok(v) = serde_json::from_str::<Value>(&text) {
                                let mut st = state.lock().unwrap();
                                handle_ws_message(&mut st, &tx, &v, now_ms());
                            }
                        }
                        Err(WsError::Timeout) => {}
                        Err(e) => {
                            eprintln!("[comfyui] ws error: {e}, reconnecting");
                            break;
                        }
                    }
                }
            }
            Err(e) => eprintln!("[comfyui] ws connect failed: {e}"),
        }
        sleep_interruptible(3000, &stop);
    }
}

/// Dispatch one ComfyUI ws message. Frame types handled:
/// execution_start / executing / progress / execution_success /
/// execution_error / execution_interrupted / status. Everything else
/// (b_preview, executed, execution_cached, ...) is ignored.
fn handle_ws_message(st: &mut RunState, tx: &Sender<StateEvent>, msg: &Value, now: u64) {
    let mtype = msg.get("type").and_then(Value::as_str).unwrap_or("");
    let data = msg.get("data");
    let pid = data.and_then(|d| d["prompt_id"].as_str()).unwrap_or("");
    match mtype {
        "execution_start" => {
            if !pid.is_empty() {
                start_turn(st, tx, pid);
                // immediate Working in ws-broadcasting builds; the poll will
                // upgrade the placeholder once the prompt graph is captured.
                ensure_placeholder(st, tx, pid);
            }
        }
        "executing" => {
            if pid.is_empty() {
                return;
            }
            start_turn(st, tx, pid);
            // `node` is the numeric node id; null (=absent) ends the walk.
            let node_present = data.map(|d| !d["node"].is_null()).unwrap_or(false);
            if !node_present {
                // node=null marks the end of the node walk for this prompt
                end_node(st, tx, pid);
                return;
            }
            // The executing frame has no node_type in current ComfyUI;
            // resolve it from the prompt graph captured by the poll thread,
            // falling back to "#<id>" when the graph is not known yet.
            let node_id = data.map(|d| num_u64(&d["node"]).or_else(|| {
                d["node"].as_str().and_then(|s| s.parse::<u64>().ok())
            }));
            let node_type = node_id
                .flatten()
                .and_then(|id| {
                    st.node_classes
                        .get(pid)
                        .and_then(|m| m.get(&id.to_string()).cloned())
                        .or_else(|| Some(format!("#{id}")))
                })
                .unwrap_or_default();
            if node_type.is_empty() {
                return;
            }
            let cur = st.node.get(pid).cloned().unwrap_or(None);
            if cur.as_deref() != Some(node_type.as_str()) {
                if let Some(old) = cur {
                    send(
                        tx,
                        StateEvent::ToolEnded {
                            source: Source::ComfyUi,
                            session_id: pid.to_string(),
                            name: old,
                            error: false,
                        },
                    );
                }
                st.node.insert(pid.to_string(), Some(node_type.clone()));
                st.progress_pct.remove(pid);
                send(
                    tx,
                    StateEvent::ToolStarted {
                        source: Source::ComfyUi,
                        session_id: pid.to_string(),
                        name: node_type,
                        arguments: None,
                    },
                );
            }
        }
        "progress" => {
            let Some(d) = data else { return };
            if st.node.get(pid).cloned().flatten().is_none() {
                return; // progress only meaningful for the current node
            }
            let (Some(value), Some(max)) = (num_u64(&d["value"]), num_u64(&d["max"])) else {
                return;
            };
            if max == 0 {
                return;
            }
            let pct = ((value * 100) / max) as u32;
            let prev = st.progress_pct.get(pid).copied().unwrap_or(0);
            // refresh the bubble arg only on visible change: >=10% steps or the
            // final step (value == max), so we don't spam per-step events.
            if pct.abs_diff(prev) >= 10 || value == max {
                st.progress_pct.insert(pid.to_string(), pct);
                let args = format!("{value}/{max}");
                let cur = st.node.get(pid).cloned().flatten();
                if let Some(name) = cur {
                    send(
                        tx,
                        StateEvent::ToolStarted {
                            source: Source::ComfyUi,
                            session_id: pid.to_string(),
                            name,
                            arguments: Some(args),
                        },
                    );
                }
            }
        }
        "execution_success" => {
            if !pid.is_empty() {
                finish_prompt(st, tx, pid, TurnEndReason::Completed, now);
            }
        }
        "execution_error" => {
            if !pid.is_empty() {
                finish_prompt(st, tx, pid, TurnEndReason::Error, now);
            }
        }
        "execution_interrupted" => {
            if !pid.is_empty() {
                finish_prompt(st, tx, pid, TurnEndReason::Interrupted, now);
            }
        }
        "status" => {
            if let Some(rem) = data.and_then(|d| d["status"]["exec_info"]["queue_remaining"].as_u64()) {
                send(tx, StateEvent::QueueChanged { source: Source::ComfyUi, pending: rem as u32 });
            }
        }
        _ => {}
    }
}

/// Numeric coercion (ComfyUI may send int or float progress values).
fn num_u64(v: &Value) -> Option<u64> {
    v.as_u64().or_else(|| v.as_f64().map(|f| f as u64))
}

/// Open the turn for a prompt exactly once (deduped via `st.running`).
/// Already-terminalled prompts are NOT re-opened, even if they still appear
/// in `queue_running` while ComfyUI tears down (prompt ids never repeat).
fn start_turn(st: &mut RunState, tx: &Sender<StateEvent>, pid: &str) {
    if st.running.contains_key(pid) || st.finished.contains_key(pid) {
        return;
    }
    st.running.insert(pid.to_string(), 1);
    send(
        tx,
        StateEvent::TurnStarted { source: Source::ComfyUi, session_id: pid.to_string(), turn: 1 },
    );
}

/// Poll-side working baseline: if no node is tracked yet, start a
/// representative tool so the pet shows Working (not Thinking) for the whole
/// run — ComfyUI is deterministic graph execution, it has no thinking phase
/// (design doc §10.2). Name comes from the graph; "执行中" as last resort.
fn ensure_placeholder(st: &mut RunState, tx: &Sender<StateEvent>, pid: &str) {
    // never re-arm a tool for an already-terminalled prompt (it may still be
    // listed in queue_running while ComfyUI tears down)
    if st.finished.contains_key(pid) {
        return;
    }
    if st.node.get(pid).cloned().flatten().is_some() {
        return;
    }
    let name = st
        .node_classes
        .get(pid)
        .and_then(default_node_label)
        .unwrap_or_else(|| PLACEHOLDER.to_string());
    st.node.insert(pid.to_string(), Some(name.clone()));
    st.progress_pct.remove(pid);
    send(
        tx,
        StateEvent::ToolStarted {
            source: Source::ComfyUi,
            session_id: pid.to_string(),
            name,
            arguments: None,
        },
    );
}

/// Upgrade a low-fidelity tool name to the best known one once the prompt
/// graph is available:
/// - `执行中` placeholder -> representative node from the graph;
/// - `#<id>` fallback (ws beat the poll) -> the node's class_type.
fn resolve_node_names(st: &mut RunState, tx: &Sender<StateEvent>, pid: &str) {
    let Some(Some(cur)) = st.node.get(pid).cloned() else { return };
    let Some(classes) = st.node_classes.get(pid) else { return };
    let target = if cur == PLACEHOLDER {
        default_node_label(classes).unwrap_or_else(|| PLACEHOLDER.to_string())
    } else if let Some(id) = cur.strip_prefix('#').and_then(|s| s.parse::<u64>().ok()) {
        match classes.get(&id.to_string()) {
            Some(name) => name.clone(),
            None => return,
        }
    } else {
        return;
    };
    if target == cur {
        return;
    }
    st.node.insert(pid.to_string(), Some(target.clone()));
    st.progress_pct.remove(pid);
    send(
        tx,
        StateEvent::ToolEnded {
            source: Source::ComfyUi,
            session_id: pid.to_string(),
            name: cur,
            error: false,
        },
    );
    send(
        tx,
        StateEvent::ToolStarted {
            source: Source::ComfyUi,
            session_id: pid.to_string(),
            name: target,
            arguments: None,
        },
    );
}

/// Close the currently executing node (if any) with a non-error ToolEnded.
fn end_node(st: &mut RunState, tx: &Sender<StateEvent>, pid: &str) {
    if let Some(Some(name)) = st.node.remove(pid) {
        st.progress_pct.remove(pid);
        send(
            tx,
            StateEvent::ToolEnded {
                source: Source::ComfyUi,
                session_id: pid.to_string(),
                name,
                error: false,
            },
        );
    }
}

/// Terminal transition for a prompt: close open tool, clear tracking, and
/// emit TurnEnded exactly once (deduped via `st.finished`).
fn finish_prompt(st: &mut RunState, tx: &Sender<StateEvent>, pid: &str, reason: TurnEndReason, now: u64) {
    if st.finished.contains_key(pid) {
        return;
    }
    end_node(st, tx, pid);
    st.running.remove(pid);
    st.node_classes.remove(pid);
    st.finished.insert(pid.to_string(), now);
    send(
        tx,
        StateEvent::TurnEnded {
            source: Source::ComfyUi,
            session_id: pid.to_string(),
            turn: 1,
            reason,
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::mpsc::channel;

    fn setup() -> (RunState, Sender<StateEvent>, std::sync::mpsc::Receiver<StateEvent>) {
        let (tx, rx) = channel();
        (RunState::default(), tx, rx)
    }

    fn all(rx: &std::sync::mpsc::Receiver<StateEvent>) -> Vec<StateEvent> {
        rx.try_iter().collect()
    }

    /// Seed the prompt graph (id -> class_type) the way the poll thread does.
    fn seed_classes(st: &mut RunState, pid: &str, graph: Value) {
        st.node_classes.insert(pid.into(), prompt_classes(Some(&graph)));
    }

    #[test]
    fn queue_item_parses_array_format() {
        // Modern ComfyUI: [number, prompt_id, prompt, extra_data, outputs]
        let v: Value = serde_json::from_str(
            r#"[0, "p1", {"3": {"class_type": "KSampler", "inputs": {}}}, {}, []]"#,
        )
        .unwrap();
        let (pid, number, classes) = queue_item(&v).unwrap();
        assert_eq!(pid, "p1");
        assert_eq!(number, 0);
        assert_eq!(classes.get("3").map(String::as_str), Some("KSampler"));
        // very old builds used objects - still parsed
        let v2: Value = serde_json::from_str(
            r#"{"prompt_id":"p2","number":1,"prompt":{"5":{"class_type":"VAEDecode"}}}"#,
        )
        .unwrap();
        let (pid, number, classes) = queue_item(&v2).unwrap();
        assert_eq!(pid, "p2");
        assert_eq!(number, 1);
        assert_eq!(classes.get("5").map(String::as_str), Some("VAEDecode"));
    }

    #[test]
    fn queue_json_shape_counts_pending() {
        let v: Value = serde_json::from_str(
            r#"{"queue_running":[[0,"p1",{"3":{"class_type":"KSampler"}},{},[]]],
                "queue_pending":[[1,"p2",{} ,{},[]],[2,"p3",{},{},[]]]}"#,
        )
        .unwrap();
        let running = v["queue_running"].as_array().unwrap();
        let pending = v["queue_pending"].as_array().map(|a| a.len()).unwrap_or(0);
        assert_eq!(running.len(), 1);
        assert_eq!(pending, 2);
    }

    #[test]
    fn history_terminal_status_parses() {
        // error via status_str
        let v: Value = serde_json::from_str(
            r#"{"p1":{"status":{"status_str":"error","completed":true,
                 "messages":[{"type":"execution_error","data":{"exception_type":"CUDA OOM"}}]}}}"#,
        )
        .unwrap();
        assert_eq!(v["p1"]["status"]["status_str"], "error");
        // success
        let v2: Value = serde_json::from_str(r#"{"p2":{"status":{"status_str":"success"}}}"#).unwrap();
        assert_eq!(v2["p2"]["status"]["status_str"], "success");
        // interrupt keeps status_str "success" but records the message
        let v3: Value = serde_json::from_str(
            r#"{"p3":{"status":{"status_str":"success","completed":true,
                 "messages":[{"type":"execution_interrupted"}]}}}"#,
        )
        .unwrap();
        let msgs = v3["p3"]["status"]["messages"].as_array().unwrap();
        assert!(msgs.iter().any(|m| m["type"].as_str() == Some("execution_interrupted")));
    }

    #[test]
    fn execution_start_opens_turn_once() {
        let (mut st, tx, rx) = setup();
        handle_ws_message(&mut st, &tx, &json!({"type":"execution_start","data":{"prompt_id":"p1"}}), 0);
        handle_ws_message(&mut st, &tx, &json!({"type":"execution_start","data":{"prompt_id":"p1"}}), 0);
        let evs = all(&rx);
        // one turn + the placeholder working tool; the duplicate start frame
        // must not emit anything new
        assert_eq!(evs.len(), 2);
        assert!(matches!(&evs[0], StateEvent::TurnStarted { session_id, turn, .. }
            if session_id == "p1" && *turn == 1));
        assert!(matches!(&evs[1], StateEvent::ToolStarted { name, .. } if name == "执行中"));
    }

    #[test]
    fn executing_switches_nodes_and_progress_refreshes_args() {
        let (mut st, tx, rx) = setup();
        seed_classes(&mut st, "p1", json!({"10": {"class_type": "KSampler"}, "11": {"class_type": "VAEDecode"}}));
        // real frames carry only the numeric node id - class comes from the graph
        handle_ws_message(&mut st, &tx, &json!({"type":"executing","data":{"prompt_id":"p1","node":10}}), 0);
        let evs = all(&rx);
        assert!(matches!(&evs[0], StateEvent::TurnStarted { session_id, .. } if session_id == "p1"));
        assert!(matches!(&evs[1], StateEvent::ToolStarted { name, arguments, .. }
            if name == "KSampler" && arguments.is_none()));

        // progress 6/20 -> refresh args
        handle_ws_message(&mut st, &tx, &json!({"type":"progress","data":{"prompt_id":"p1","value":6,"max":20}}), 0);
        let evs = all(&rx);
        assert!(matches!(&evs[0], StateEvent::ToolStarted { name, arguments, .. }
            if name == "KSampler" && arguments.as_deref() == Some("6/20")));

        // tiny progress change (bucket step) -> no event
        handle_ws_message(&mut st, &tx, &json!({"type":"progress","data":{"prompt_id":"p1","value":7,"max":20}}), 0);
        assert!(all(&rx).is_empty());

        // node switch: KSampler ends, VAEDecode starts
        handle_ws_message(&mut st, &tx, &json!({"type":"executing","data":{"prompt_id":"p1","node":11}}), 0);
        let evs = all(&rx);
        assert!(matches!(&evs[0], StateEvent::ToolEnded { name, .. } if name == "KSampler"));
        assert!(matches!(&evs[1], StateEvent::ToolStarted { name, .. } if name == "VAEDecode"));

        // node null at end of the walk
        handle_ws_message(&mut st, &tx, &json!({"type":"executing","data":{"prompt_id":"p1","node":null}}), 0);
        let evs = all(&rx);
        assert!(matches!(&evs[0], StateEvent::ToolEnded { name, error, .. } if name == "VAEDecode" && !error));
    }

    #[test]
    fn executing_falls_back_to_node_id_without_graph() {
        let (mut st, tx, rx) = setup();
        // ws fired before the poll captured the prompt graph: use "#<id>"
        handle_ws_message(&mut st, &tx, &json!({"type":"executing","data":{"prompt_id":"p1","node":3}}), 0);
        let evs = all(&rx);
        assert!(matches!(&evs[1], StateEvent::ToolStarted { name, .. } if name == "#3"));
    }

    #[test]
    fn success_finishes_with_tool_cleanup_once() {
        let (mut st, tx, rx) = setup();
        seed_classes(&mut st, "p1", json!({"1": {"class_type": "KSampler"}}));
        handle_ws_message(&mut st, &tx, &json!({"type":"executing","data":{"prompt_id":"p1","node":1}}), 0);
        all(&rx); // drain start+tool
        handle_ws_message(&mut st, &tx, &json!({"type":"execution_success","data":{"prompt_id":"p1"}}), 100);
        handle_ws_message(&mut st, &tx, &json!({"type":"execution_success","data":{"prompt_id":"p1"}}), 100);
        let evs = all(&rx);
        // ToolEnded for the open node + one TurnEnded
        assert_eq!(evs.len(), 2);
        assert!(matches!(&evs[0], StateEvent::ToolEnded { name, .. } if name == "KSampler"));
        assert!(matches!(&evs[1], StateEvent::TurnEnded { reason: TurnEndReason::Completed, .. }));
        assert!(st.node_classes.is_empty());
    }

    #[test]
    fn error_and_interrupted_map_to_reasons() {
        let (mut st, tx, rx) = setup();
        handle_ws_message(&mut st, &tx, &json!({"type":"execution_error","data":{"prompt_id":"p1"}}), 0);
        let evs = all(&rx);
        assert!(matches!(&evs[0], StateEvent::TurnEnded { reason: TurnEndReason::Error, .. }));

        let (mut st2, tx2, rx2) = setup();
        handle_ws_message(&mut st2, &tx2, &json!({"type":"execution_interrupted","data":{"prompt_id":"p2"}}), 0);
        let evs = all(&rx2);
        assert!(matches!(&evs[0], StateEvent::TurnEnded { reason: TurnEndReason::Interrupted, .. }));
    }

    #[test]
    fn status_frame_updates_queue() {
        let (mut st, tx, rx) = setup();
        handle_ws_message(
            &mut st,
            &tx,
            &json!({"type":"status","data":{"status":{"exec_info":{"queue_remaining":3}}}}),
            0,
        );
        let evs = all(&rx);
        assert!(matches!(&evs[0], StateEvent::QueueChanged { pending: 3, .. }));
    }

    #[test]
    fn ignores_high_frequency_frames() {
        let (mut st, tx, rx) = setup();
        handle_ws_message(&mut st, &tx, &json!({"type":"b_preview","data":{"preview":"x"}}), 0);
        handle_ws_message(&mut st, &tx, &json!({"type":"execution_cached","data":{"nodes":[1]}}), 0);
        handle_ws_message(&mut st, &tx, &json!({"type":"executed","data":{"node":1}}), 0);
        assert!(all(&rx).is_empty());
    }

    #[test]
    fn no_turn_restart_after_finish_even_if_still_listed() {
        let (mut st, tx, rx) = setup();
        start_turn(&mut st, &tx, "p1");
        finish_prompt(&mut st, &tx, "p1", TurnEndReason::Completed, 0);
        all(&rx);
        // a later poll still lists p1 in queue_running -> must NOT re-open
        // the turn NOR re-arm a placeholder tool
        start_turn(&mut st, &tx, "p1");
        ensure_placeholder(&mut st, &tx, "p1");
        assert!(all(&rx).is_empty());
        assert!(!st.running.contains_key("p1"));
        assert_eq!(st.node.get("p1"), None);
    }

    #[test]
    fn fallback_name_upgraded_when_graph_arrives() {
        let (mut st, tx, rx) = setup();
        handle_ws_message(&mut st, &tx, &json!({"type":"executing","data":{"prompt_id":"p1","node":3}}), 0);
        all(&rx); // TurnStarted + ToolStarted #3
        seed_classes(&mut st, "p1", json!({"3": {"class_type": "KSampler"}}));
        resolve_node_names(&mut st, &tx, "p1");
        let evs = all(&rx);
        assert!(matches!(&evs[0], StateEvent::ToolEnded { name, .. } if name == "#3"));
        assert!(matches!(&evs[1], StateEvent::ToolStarted { name, .. } if name == "KSampler"));
    }

    #[test]
    fn default_node_label_prefers_sampler() {
        let mut classes = HashMap::new();
        classes.insert("2".into(), "LoadImage".into());
        classes.insert("3".into(), "KSampler".into());
        classes.insert("5".into(), "VAEDecode".into());
        assert_eq!(default_node_label(&classes).as_deref(), Some("KSampler"));
        // without a sampler: VAE decode wins
        let mut classes2 = HashMap::new();
        classes2.insert("2".into(), "LoadImage".into());
        classes2.insert("5".into(), "VAEDecode".into());
        assert_eq!(default_node_label(&classes2).as_deref(), Some("VAEDecode"));
        // empty graph -> None (caller uses the placeholder)
        assert_eq!(default_node_label(&HashMap::new()), None);
    }

    #[test]
    fn poll_placeholder_gives_working_baseline() {
        let (mut st, tx, rx) = setup();
        // poll path: graph captured -> start turn + placeholder tool
        st.node_classes.insert(
            "p1".into(),
            prompt_classes(Some(&json!({"3": {"class_type": "KSampler"}}))),
        );
        start_turn(&mut st, &tx, "p1");
        ensure_placeholder(&mut st, &tx, "p1");
        resolve_node_names(&mut st, &tx, "p1");
        let evs = all(&rx);
        assert!(matches!(&evs[0], StateEvent::TurnStarted { .. }));
        assert!(matches!(&evs[1], StateEvent::ToolStarted { name, .. } if name == "KSampler"));
        // terminal: placeholder tool is closed with the turn
        finish_prompt(&mut st, &tx, "p1", TurnEndReason::Completed, 0);
        let evs = all(&rx);
        assert!(matches!(&evs[0], StateEvent::ToolEnded { name, .. } if name == "KSampler"));
        assert!(matches!(&evs[1], StateEvent::TurnEnded { reason: TurnEndReason::Completed, .. }));
    }

    #[test]
    fn placeholder_upgraded_when_graph_arrives() {
        let (mut st, tx, rx) = setup();
        start_turn(&mut st, &tx, "p1");
        ensure_placeholder(&mut st, &tx, "p1"); // no graph yet -> 执行中
        let evs = all(&rx);
        assert!(matches!(&evs[1], StateEvent::ToolStarted { name, .. } if name == "执行中"));
        // poll captures the graph -> upgrade 执行中 -> KSampler
        st.node_classes
            .insert("p1".into(), prompt_classes(Some(&json!({"3": {"class_type": "KSampler"}}))));
        resolve_node_names(&mut st, &tx, "p1");
        let evs = all(&rx);
        assert!(matches!(&evs[0], StateEvent::ToolEnded { name, .. } if name == "执行中"));
        assert!(matches!(&evs[1], StateEvent::ToolStarted { name, .. } if name == "KSampler"));
    }

    #[test]
    fn execution_start_adds_placeholder_before_graph() {
        let (mut st, tx, rx) = setup();
        handle_ws_message(&mut st, &tx, &json!({"type":"execution_start","data":{"prompt_id":"p1"}}), 0);
        let evs = all(&rx);
        assert!(matches!(&evs[0], StateEvent::TurnStarted { .. }));
        assert!(matches!(&evs[1], StateEvent::ToolStarted { name, .. } if name == "执行中"));
        // a poll then captures the graph and upgrades the name
        st.node_classes
            .insert("p1".into(), prompt_classes(Some(&json!({"3": {"class_type": "KSampler"}}))));
        resolve_node_names(&mut st, &tx, "p1");
        let evs = all(&rx);
        assert!(matches!(&evs[1], StateEvent::ToolStarted { name, .. } if name == "KSampler"));
    }

    #[test]
    fn float_progress_values_parse() {
        let (mut st, tx, rx) = setup();
        seed_classes(&mut st, "p1", json!({"1": {"class_type": "KSampler"}}));
        handle_ws_message(&mut st, &tx, &json!({"type":"executing","data":{"prompt_id":"p1","node":1}}), 0);
        all(&rx);
        handle_ws_message(&mut st, &tx, &json!({"type":"progress","data":{"prompt_id":"p1","value":5.0,"max":20.0}}), 0);
        let evs = all(&rx);
        assert!(matches!(&evs[0], StateEvent::ToolStarted { arguments: Some(a), .. } if a == "5/20"));
    }
}