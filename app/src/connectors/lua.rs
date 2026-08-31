//! Lua script connector — the open interface for user-written sources.
//!
//! Each configured script (`config.json` → `scripts` array) runs in its own
//! thread with its own embedded Lua 5.4 state (vendored, statically linked —
//! the exe keeps its zero-runtime-dependency promise; users only need to
//! know Lua, not install it). The script monitors whatever it wants (log
//! files, processes, files…) and feeds the pet through a `pet.*` API that
//! mirrors the StateEvent contract (see scripts-guide.md).
//!
//! Isolation: a script error only marks ITS source unhealthy and stops that
//! script thread; the pet, the GUI and all other sources are unaffected.
//! Scripts must loop themselves and sleep with `pet.wait(ms)` (interruptible
//! slice sleep) so the app can shut down promptly.
//!
//! Sandbox (per-script `"sandbox": true`): os/io/package/require/dofile/
//! loadfile/load/debug are removed from the globals — the script can only
//! compute and call the pet API (no filesystem/process/network access).

use super::sleep_interruptible;
use crate::config::ScriptEntryConfig;
use crate::state::{register_script_label, SessionItem, Source, StateEvent, TodoItem, TurnEndReason};
use mlua::{Lua, Table, Value};
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::mpsc::Sender;
use std::sync::Arc;

/// 脚本事件发送包装:`debug: true` 时把每条 `pet.*` 调用(事件名+关键字段)
/// 写进日志,便于脚本作者确认"调用发出去了没/发成什么样"(排查宠物卡状态)。
/// debug 关闭时零开销(不格式化、不发日志)。
struct EvSender {
    tx: Sender<StateEvent>,
    debug: bool,
    log: Arc<dyn Fn(String) + Send + Sync>,
}

impl Clone for EvSender {
    fn clone(&self) -> Self {
        EvSender { tx: self.tx.clone(), debug: self.debug, log: self.log.clone() }
    }
}

fn send(ev_tx: &EvSender, ev: StateEvent) {
    if ev_tx.debug {
        (ev_tx.log)(format!("ev: {}", ev_summary(&ev)));
    }
    let _ = ev_tx.tx.send(ev);
}

/// 事件的一行摘要(debug 轨迹用)。
fn ev_summary(ev: &StateEvent) -> String {
    match ev {
        StateEvent::SourceHealth { healthy, .. } => format!("health {healthy}"),
        StateEvent::Poll { items, .. } => format!("poll {} items", items.len()),
        StateEvent::SessionStatus { session_id, running, .. } => {
            format!("session_status {} {}", session_id, running)
        }
        StateEvent::TurnStarted { session_id, turn, .. } => {
            format!("session_started {} turn={}", session_id, turn)
        }
        StateEvent::TurnEnded { session_id, turn, reason, .. } => {
            format!("session_ended {} turn={} {:?}", session_id, turn, reason)
        }
        StateEvent::ToolStarted { session_id, name, .. } => {
            format!("tool_started {} {}", session_id, name)
        }
        StateEvent::ToolEnded { session_id, name, error, .. } => {
            format!("tool_ended {} {} err={}", session_id, name, error)
        }
        StateEvent::TodoSnapshot { session_id, todos, .. } => {
            format!("todo {} n={}", session_id, todos.len())
        }
        StateEvent::ApprovalRequested { id, session_id, tool, .. } => {
            format!("approval_requested {} {} {}", id, session_id, tool)
        }
        StateEvent::ApprovalResolved { id, .. } => format!("approval_resolved {}", id),
        StateEvent::QuestionRequested { id, session_id, .. } => {
            format!("question {} {}", id, session_id)
        }
        StateEvent::QuestionResolved { id, .. } => format!("answer {}", id),
        StateEvent::PendingSync { .. } => "pending_sync".to_string(),
        StateEvent::LiveText { session_id, reasoning, text, .. } => format!(
            "live_text {} r+{} t+{}",
            session_id,
            reasoning.as_ref().map(|s| s.chars().count()).unwrap_or(0),
            text.as_ref().map(|s| s.chars().count()).unwrap_or(0),
        ),
        StateEvent::UserMessage { session_id, text, .. } => {
            format!("user_message {} {} chars", session_id, text.chars().count())
        }
        StateEvent::QueueChanged { pending, .. } => format!("queue {}", pending),
        StateEvent::Tick => "tick".to_string(),
    }
}

pub struct LuaScriptConnector {
    /// Script source id (= registration index in the `scripts` array).
    pub id: u16,
    pub entry: ScriptEntryConfig,
    /// Optional log file for `pet.log`/errors (GUI passes hannis.log; the
    /// headless driver leaves None and uses stderr only).
    pub log_path: Option<PathBuf>,
}

impl LuaScriptConnector {
    pub fn spawn(self, tx: Sender<StateEvent>, stop: Arc<AtomicBool>) {
        std::thread::Builder::new()
            .name(format!("lua-{}", self.entry.name))
            .spawn(move || self.run(tx, stop))
            .ok();
    }

    fn log(&self, msg: &str) {
        eprintln!("[lua:{}] {msg}", self.entry.name);
        if let Some(p) = &self.log_path {
            if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(p) {
                use std::io::Write;
                let _ = writeln!(f, "[lua:{}] {msg}", self.entry.name);
            }
        }
    }

    fn fail(&self, tx: &Sender<StateEvent>, msg: &str) {
        self.log(msg);
        super::send(tx, StateEvent::SourceHealth { source: Source::Script(self.id), healthy: false });
    }

    /// Load + run the script (synchronous; also used by tests). The script is
    /// expected to loop internally and return only to stop.
    pub fn run(&self, tx: Sender<StateEvent>, stop: Arc<AtomicBool>) {
        let src = match std::fs::read_to_string(&self.entry.file) {
            Ok(s) => s,
            Err(e) => return self.fail(&tx, &format!("script load failed: {e}")),
        };
        let lua = Lua::new();
        if let Err(e) = self.setup(&lua, tx.clone(), stop.clone()) {
            return self.fail(&tx, &format!("script setup failed: {e}"));
        }
        // compile first: syntax errors surface without touching the state
        let func = match lua
            .load(&src)
            .set_name(self.entry.file.clone())
            .into_function()
        {
            Ok(f) => f,
            Err(e) => return self.fail(&tx, &format!("script compile failed: {e}")),
        };
        super::send(&tx, StateEvent::SourceHealth { source: Source::Script(self.id), healthy: true });
        // 启动即回显 args(配置传错一眼可见);debug 模式再展开每条事件
        let args_s = self
            .entry
            .args
            .as_ref()
            .map(|a| a.to_string())
            .unwrap_or_default();
        let args_s: String = args_s.chars().take(300).collect();
        self.log(&format!("started ({} bytes) args={}", src.len(), args_s));
        match func.call::<()>(()) {
            Ok(()) => {
                self.fail(&tx, "script returned (not looping) — source stopped");
                // 脚本线程退出:它已发出的审批/提问再也没有人能 resolve(没有
                // replay——服务端重放只在 ws 场景、且脚本已死),补发 pending_sync
                // 清掉本源残留,否则宠物最长卡 Attention 30 分钟(等 TTL 兜底)
                super::send(&tx, StateEvent::PendingSync { source: Source::Script(self.id) });
            }
            Err(e) => {
                self.fail(&tx, &format!("script error: {e}"));
                super::send(&tx, StateEvent::PendingSync { source: Source::Script(self.id) });
            }
        }
    }

    fn setup(&self, lua: &Lua, tx: Sender<StateEvent>, stop: Arc<AtomicBool>) -> mlua::Result<()> {
        if self.entry.sandbox {
            let g = lua.globals();
            for k in [
                "os", "io", "package", "require", "dofile", "loadfile", "load", "debug",
            ] {
                g.set(k, Value::Nil)?;
            }
        }
        let pet = self.build_pet_api(lua, tx, stop)?;
        lua.globals().set("pet", pet)?;
        Ok(())
    }

    /// The `pet.*` API table. Every function translates to a StateEvent with
    /// `Source::Script(id)` and a `script-<id>-` session id prefix (sessions
    /// are shared across sources; the prefix keeps script sessions from
    /// colliding with DSH/Hermes/MAA/other scripts).
    fn build_pet_api(
        &self,
        lua: &Lua,
        tx: Sender<StateEvent>,
        stop: Arc<AtomicBool>,
    ) -> mlua::Result<Table> {
        let source = Source::Script(self.id);
        let prefix = format!("script-{}-", self.id);
        let name = self.entry.name.clone();
        let poll_ms = self.entry.poll_ms;
        let args = self.entry.args.clone();
        let log_path = self.log_path.clone();
        // 日志去重:30 秒内完全相同的消息只写一次(通道连不上时不再刷屏)
        let log_state = Arc::new(std::sync::Mutex::new(
            (String::new(), std::time::Instant::now() - std::time::Duration::from_secs(99)),
        ));
        let log = {
            let name = name.clone();
            let log_path = log_path.clone();
            let state = log_state.clone();
            move |msg: String| {
                let dup = {
                    let mut g = state.lock().unwrap();
                    if g.0 == msg && g.1.elapsed() < std::time::Duration::from_secs(30) {
                        true
                    } else {
                        g.0 = msg.clone();
                        g.1 = std::time::Instant::now();
                        false
                    }
                };
                if dup {
                    return;
                }
                eprintln!("[lua:{name}] {msg}");
                if let Some(p) = &log_path {
                    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(p) {
                        use std::io::Write;
                        let _ = writeln!(f, "[lua:{name}] {msg}");
                    }
                }
            }
        };
        let sid = {
            let prefix = prefix.clone();
            move |s: String| format!("{prefix}{s}")
        };
        // 事件调试日志:debug=true 时每条 pet.* 调用写一行(不经 30s 去重,
        // 需要完整轨迹);关闭时零成本。所有 send 走 EvSender 统一出口。
        let ev_log: Arc<dyn Fn(String) + Send + Sync> = {
            let name = name.clone();
            let log_path = log_path.clone();
            let on = self.entry.debug;
            Arc::new(move |msg: String| {
                if on {
                    eprintln!("[lua:{name}] {msg}");
                    if let Some(p) = &log_path {
                        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(p) {
                            use std::io::Write;
                            let _ = writeln!(f, "[lua:{name}] {msg}");
                        }
                    }
                }
            })
        };
        let tx = EvSender { tx, debug: self.entry.debug, log: ev_log };

        let pet = lua.create_table()?;

        let f = {
            let log = log.clone();
            lua.create_function(move |_, (level, msg): (String, String)| {
                log(format!("{level}: {msg}"));
                Ok(())
            })?
        };
        pet.set("log", f)?;

        let f = {
            lua.create_function(move |_, ms: u64| {
                sleep_interruptible(ms, &stop);
                Ok(())
            })?
        };
        pet.set("wait", f)?;

        let f = {
            let tx = tx.clone();
            lua.create_function(move |_, ok: bool| {
                send(&tx, StateEvent::SourceHealth { source, healthy: ok });
                Ok(())
            })?
        };
        pet.set("health", f)?;

        let f = {
            let tx = tx.clone();
            let sid = sid.clone();
            lua.create_function(move |_, (id, turn): (String, u64)| {
                send(&tx, StateEvent::TurnStarted {
                    source,
                    session_id: sid(id),
                    turn,
                });
                Ok(())
            })?
        };
        pet.set("session_started", f)?;

        let f = {
            let tx = tx.clone();
            let sid = sid.clone();
            lua.create_function(move |_, (id, turn, reason): (String, u64, String)| {
                let reason = match reason.as_str() {
                    "completed" => TurnEndReason::Completed,
                    "error" => TurnEndReason::Error,
                    "max_tokens" => TurnEndReason::MaxTokens,
                    "aborted" => TurnEndReason::Aborted,
                    "interrupted" => TurnEndReason::Interrupted,
                    "blocked" => TurnEndReason::Blocked,
                    other => {
                        return Err(mlua::Error::runtime(format!(
                            "unknown reason '{other}' (completed|error|max_tokens|aborted|interrupted|blocked)"
                        )))
                    }
                };
                send(&tx, StateEvent::TurnEnded {
                    source,
                    session_id: sid(id),
                    turn,
                    reason,
                });
                Ok(())
            })?
        };
        pet.set("session_ended", f)?;

        let f = {
            let tx = tx.clone();
            let sid = sid.clone();
            lua.create_function(move |_, (id, name, args): (String, String, Option<String>)| {
                send(&tx, StateEvent::ToolStarted {
                    source,
                    session_id: sid(id),
                    name,
                    arguments: args,
                });
                Ok(())
            })?
        };
        pet.set("tool_started", f)?;

        let f = {
            let tx = tx.clone();
            let sid = sid.clone();
            lua.create_function(move |_, (id, name, err): (String, String, Option<bool>)| {
                send(&tx, StateEvent::ToolEnded {
                    source,
                    session_id: sid(id),
                    name,
                    error: err.unwrap_or(false),
                });
                Ok(())
            })?
        };
        pet.set("tool_ended", f)?;

        let f = {
            let tx = tx.clone();
            let sid = sid.clone();
            lua.create_function(move |_, (id, t): (String, Table)| {
                let reasoning: Option<String> = t.get("reasoning")?;
                let text: Option<String> = t.get("text")?;
                let tool_name: Option<String> = t.get("tool_name")?;
                send(&tx, StateEvent::LiveText {
                    source,
                    session_id: sid(id),
                    reasoning,
                    text,
                    tool_name,
                });
                Ok(())
            })?
        };
        pet.set("live_text", f)?;

        let f = {
            let tx = tx.clone();
            let sid = sid.clone();
            let prefix = prefix.clone();
            lua.create_function(move |_, (qid, id, text): (String, String, String)| {
                send(&tx, StateEvent::QuestionRequested {
                    source,
                    id: format!("{prefix}{qid}"),
                    session_id: sid(id),
                    text,
                });
                Ok(())
            })?
        };
        pet.set("question", f)?;

        let f = {
            let tx = tx.clone();
            let prefix = prefix.clone();
            lua.create_function(move |_, qid: String| {
                send(&tx, StateEvent::QuestionResolved {
                    source,
                    id: format!("{prefix}{qid}"),
                });
                Ok(())
            })?
        };
        pet.set("answer", f)?;

        // pet.session_status(id, running):running 翻转(host/session-status、hermes 会话结束)
        let f = {
            let tx = tx.clone();
            let sid = sid.clone();
            lua.create_function(move |_, (id, running): (String, bool)| {
                send(&tx, StateEvent::SessionStatus {
                    source,
                    session_id: sid(id),
                    running,
                });
                Ok(())
            })?
        };
        pet.set("session_status", f)?;

        // pet.approval_requested(aid, id, tool)/pet.approval_resolved(aid):
        // DSH events.mux 的 approval/requested|resolved(id 自动加前缀)
        let f = {
            let tx = tx.clone();
            let sid = sid.clone();
            let prefix = prefix.clone();
            lua.create_function(move |_, (aid, id, tool): (String, String, String)| {
                send(&tx, StateEvent::ApprovalRequested {
                    source,
                    id: format!("{prefix}{aid}"),
                    session_id: sid(id),
                    tool,
                });
                Ok(())
            })?
        };
        pet.set("approval_requested", f)?;
        let f = {
            let tx = tx.clone();
            let prefix = prefix.clone();
            lua.create_function(move |_, aid: String| {
                send(&tx, StateEvent::ApprovalResolved {
                    source,
                    id: format!("{prefix}{aid}"),
                });
                Ok(())
            })?
        };
        pet.set("approval_resolved", f)?;

        // pet.pending_sync():WS 重连后清空该源的审批/提问(服务端会重放当前
        // 仍在等待的请求;本地残留的已失效请求不能把宠物卡在 attention 上)
        let f = {
            let tx = tx.clone();
            lua.create_function(move |_, _: ()| {
                send(&tx, StateEvent::PendingSync { source });
                Ok(())
            })?
        };
        pet.set("pending_sync", f)?;

        let f = {
            let tx = tx.clone();
            let sid = sid.clone();
            lua.create_function(move |_, (id, text): (String, String)| {
                send(&tx, StateEvent::UserMessage {
                    source,
                    session_id: sid(id),
                    text,
                });
                Ok(())
            })?
        };
        pet.set("user_message", f)?;

        let f = {
            let tx = tx.clone();
            lua.create_function(move |_, n: u32| {
                send(&tx, StateEvent::QueueChanged { source, pending: n });
                Ok(())
            })?
        };
        pet.set("queue", f)?;

        let f = {
            let tx = tx.clone();
            let sid = sid.clone();
            lua.create_function(move |_, (id, todos): (String, Table)| {
                let mut out = Vec::new();
                for v in todos.sequence_values::<Value>() {
                    let v = v?;
                    // 宽容解析:用户脚本的信息常有缺漏。content 缺失/非字符串
                    // 跳过该条,status 缺失按 pending——绝不让一个坏条目把整条
                    // 脚本炸掉(旧实现类型不符直接 Err,脚本线程即死)。
                    let Value::Table(t) = v else { continue };
                    let content = match t.get::<Value>("content")? {
                        Value::String(s) => s.to_str()?.to_string(),
                        _ => continue,
                    };
                    let status = match t.get::<Value>("status")? {
                        Value::String(s) => s.to_str()?.to_string(),
                        _ => "pending".to_string(),
                    };
                    out.push(TodoItem { content, status });
                }
                send(&tx, StateEvent::TodoSnapshot {
                    source,
                    session_id: sid(id),
                    todos: out,
                });
                Ok(())
            })?
        };
        pet.set("todo", f)?;

        let f = {
            let tx = tx.clone();
            let sid = sid.clone();
            lua.create_function(move |_, items: Table| {
                let mut out = Vec::new();
                for v in items.sequence_values::<Value>() {
                    let v = v?;
                    let Value::Table(t) = v else { continue };
                    // 宽容解析(见 pet.todo):session_id 缺失/类型不符 → 跳过
                    // 该条目(数字 id 转字符串);running 缺失 → false;title/
                    // todos 类型不符 → 忽略。基线快照容错降级,而不是脚本崩掉。
                    let session_id = match t.get::<Value>("session_id")? {
                        Value::String(s) => s.to_str()?.to_string(),
                        Value::Integer(i) => i.to_string(),
                        Value::Number(n) => n.to_string(),
                        _ => continue,
                    };
                    let running = matches!(t.get::<Value>("running")?, Value::Boolean(true));
                    let title = match t.get::<Value>("title")? {
                        Value::String(s) => Some(s.to_str()?.to_string()),
                        _ => None,
                    };
                    let todos = match t.get::<Value>("todos")? {
                        Value::Table(tt) => {
                            let mut v = Vec::new();
                            for x in tt.sequence_values::<Value>() {
                                let x = x?;
                                let Value::Table(xt) = x else { continue };
                                let content = match xt.get::<Value>("content")? {
                                    Value::String(s) => s.to_str()?.to_string(),
                                    _ => continue,
                                };
                                let status = match xt.get::<Value>("status")? {
                                    Value::String(s) => s.to_str()?.to_string(),
                                    _ => "pending".to_string(),
                                };
                                v.push(TodoItem { content, status });
                            }
                            Some(v)
                        }
                        _ => None,
                    };
                    out.push(SessionItem {
                        session_id: sid(session_id),
                        running,
                        title,
                        todos,
                    });
                }
                send(&tx, StateEvent::Poll { source, items: out, ok: true, error: None });
                Ok(())
            })?
        };
        pet.set("poll", f)?;

        let f = {
            let sandbox = self.entry.sandbox;
            lua.create_function(move |_, (url, timeout): (String, Option<u64>)| {
                // HTTP 走宿主零依赖客户端(与内置连接器同一套);沙箱不允许
                if sandbox {
                    return Err(mlua::Error::runtime("pet.http is disabled in sandbox mode"));
                }
                let u = crate::http::Url::parse(&url).map_err(mlua::Error::runtime)?;
                let ms = timeout.unwrap_or(5000).min(60_000).max(1);
                let body = crate::http::request(
                    &u,
                    "GET",
                    &[],
                    None,
                    std::time::Duration::from_millis(ms),
                )
                .map_err(|e| mlua::Error::runtime(e))?;
                Ok((body.status as i64, String::from_utf8_lossy(&body.body).to_string()))
            })?
        };
        pet.set("http", f)?;

        // pet.http_post(url, body, timeout_ms?) → status, body
        // (Content-Type: application/json;DSH 的 session.list/history 都是这种)
        let f = {
            let sandbox = self.entry.sandbox;
            lua.create_function(move |_, (url, body_text, timeout): (String, String, Option<u64>)| {
                if sandbox {
                    return Err(mlua::Error::runtime("pet.http_post is disabled in sandbox mode"));
                }
                let u = crate::http::Url::parse(&url).map_err(mlua::Error::runtime)?;
                let ms = timeout.unwrap_or(5000).min(60_000).max(1);
                let resp = crate::http::request(
                    &u,
                    "POST",
                    &[("Content-Type", "application/json".into())],
                    Some(body_text.as_bytes()),
                    std::time::Duration::from_millis(ms),
                )
                .map_err(|e| mlua::Error::runtime(e))?;
                Ok((resp.status as i64, String::from_utf8_lossy(&resp.body).to_string()))
            })?
        };
        pet.set("http_post", f)?;

        // ---- WebSocket 流式(如 DSH 的 /api/events.mux)----
        // pet.ws(url, path, timeout_ms?) → handle
        // pet.ws_read(handle) → 文本帧(string)|nil(关闭/超时;ping/pong 自动跳过)
        // pet.ws_close(handle)
        let ws_reg = Arc::new(std::sync::Mutex::new(std::collections::HashMap::<u64, crate::http::Ws>::new()));
        let ws_seq = Arc::new(std::sync::atomic::AtomicU64::new(1));
        let f = {
            let sandbox = self.entry.sandbox;
            let reg = ws_reg.clone();
            let seq = ws_seq.clone();
            lua.create_function(move |_, (url, path, timeout): (String, String, Option<u64>)| {
                if sandbox {
                    return Err(mlua::Error::runtime("pet.ws is disabled in sandbox mode"));
                }
                let u = crate::http::Url::parse(&url).map_err(mlua::Error::runtime)?;
                let ms = timeout.unwrap_or(8000).min(60_000).max(1);
                let ws = crate::http::Ws::connect(&u, &path, std::time::Duration::from_millis(ms))
                    .map_err(|e| mlua::Error::runtime(e))?;
                let id = seq.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                reg.lock().unwrap().insert(id, ws);
                Ok(id as i64)
            })?
        };
        pet.set("ws", f)?;
        // 返回值三态:
        //   string = 收到一帧文本/二进制
        //   nil    = 读超时(连接还活着,只是这个时间窗内没有新帧)
        //   false  = 连接已关闭/出错(脚本应 ws_close + 重连)
        let f = {
            let reg = ws_reg.clone();
            lua.create_function(move |lua, id: i64| {
                let mut reg = reg.lock().unwrap();
                let ws = reg
                    .get_mut(&(id as u64))
                    .ok_or_else(|| mlua::Error::runtime("ws handle closed or invalid"))?;
                loop {
                    match ws.read_frame() {
                        Ok(fr) => {
                            if fr.opcode == crate::http::WS_OP_TEXT || fr.opcode == crate::http::WS_OP_BINARY {
                                return Ok(Value::String(lua.create_string(&fr.payload)?));
                            }
                            if fr.opcode == crate::http::WS_OP_CLOSE {
                                return Ok(Value::Boolean(false)); // 对端关闭
                            }
                            // ping/pong 等控制帧:继续等
                        }
                        Err(crate::http::WsError::Timeout) => return Ok(Value::Nil), // 超时:无新帧
                        Err(_) => return Ok(Value::Boolean(false)),                   // 关闭/错误:连接死了
                    }
                }
            })?
        };
        pet.set("ws_read", f)?;
        let f = {
            let reg = ws_reg.clone();
            lua.create_function(move |_, id: i64| {
                reg.lock().unwrap().remove(&(id as u64));
                Ok(())
            })?
        };
        pet.set("ws_close", f)?;

        // pet.sqlite(path, sql, params?) → 行数组(每行 {列名=值};沙箱禁用)
        let f = {
            let sandbox = self.entry.sandbox;
            lua.create_function(move |lua, (path, sql, params): (String, String, Option<Table>)| {
                if sandbox {
                    return Err(mlua::Error::runtime("pet.sqlite is disabled in sandbox mode"));
                }
                use rusqlite::types::Value as SqlValue;
                let conn = rusqlite::Connection::open_with_flags(
                    &path,
                    rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
                )
                .map_err(|e| mlua::Error::runtime(format!("open {path}: {e}")))?;
                let mut stmt = conn
                    .prepare(&sql)
                    .map_err(|e| mlua::Error::runtime(format!("prepare: {e}")))?;
                let params: Vec<SqlValue> = match params {
                    Some(t) => {
                        let mut v = Vec::new();
                        for x in t.sequence_values::<Value>() {
                            let x = x?;
                            v.push(match x {
                                Value::String(s) => SqlValue::Text(s.to_str()?.to_string()),
                                Value::Integer(i) => SqlValue::Integer(i),
                                Value::Number(n) => {
                                    if n.fract() == 0.0 && n >= -9.2e18 && n <= 9.2e18 {
                                        SqlValue::Integer(n as i64)
                                    } else {
                                        SqlValue::Real(n)
                                    }
                                }
                                Value::Boolean(b) => SqlValue::Integer(b as i64),
                                Value::Nil => SqlValue::Null,
                                _ => return Err(mlua::Error::runtime("unsupported parameter type")),
                            });
                        }
                        v
                    }
                    None => Vec::new(),
                };
                use rusqlite::params_from_iter;
                let cols: Vec<String> = stmt.column_names().iter().map(|s| (*s).to_string()).collect();
                let mut rows = stmt
                    .query(params_from_iter(params))
                    .map_err(|e| mlua::Error::runtime(format!("query: {e}")))?;
                let out = lua.create_table()?;
                let mut ri = 1i64;
                while let Some(row) = rows
                    .next()
                    .map_err(|e| mlua::Error::runtime(format!("row: {e}")))?
                {
                    let t = lua.create_table()?;
                    for (ci, name) in cols.iter().enumerate() {
                        let v = row
                            .get_ref(ci)
                            .map_err(|e| mlua::Error::runtime(format!("col: {e}")))?;
                        match v {
                            rusqlite::types::ValueRef::Null => t.set(name.as_str(), Value::Nil)?,
                            rusqlite::types::ValueRef::Integer(i) => t.set(name.as_str(), i)?,
                            rusqlite::types::ValueRef::Real(f) => t.set(name.as_str(), f)?,
                            rusqlite::types::ValueRef::Text(b) => {
                                t.set(name.as_str(), String::from_utf8_lossy(b).to_string())?
                            }
                            rusqlite::types::ValueRef::Blob(b) => {
                                let hex: String = b.iter().map(|x| format!("{x:02x}")).collect();
                                t.set(name.as_str(), hex)?;
                            }
                        }
                    }
                    out.raw_set(ri, t)?;
                    ri += 1;
                }
                Ok(out)
            })?
        };
        pet.set("sqlite", f)?;

        let f = {
            let script_dir = std::path::Path::new(&self.entry.file)
                .parent()
                .map(|p| p.display().to_string())
                .unwrap_or_default();
            lua.create_function(move |lua, _: ()| {
                // config(): { name, poll_ms, args = <user args as Lua table>, _dir }
                let t = lua.create_table()?;
                t.set("name", name.clone())?;
                t.set("poll_ms", poll_ms)?;
                t.set("_dir", script_dir.clone())?;
                let v = match &args {
                    Some(a) => json_to_lua(lua, a)?,
                    None => Value::Nil,
                };
                t.set("args", v)?;
                Ok(t)
            })?
        };
        pet.set("config", f)?;

        Ok(pet)
    }
}

/// serde_json::Value → Lua value (small enough to hand-roll; keeps the mlua
/// feature surface minimal).
fn json_to_lua(lua: &Lua, v: &serde_json::Value) -> mlua::Result<Value> {
    match v {
        serde_json::Value::Null => Ok(Value::Nil),
        serde_json::Value::Bool(b) => Ok(Value::Boolean(*b)),
        serde_json::Value::Number(n) => Ok(Value::Number(n.as_f64().unwrap_or(0.0))),
        serde_json::Value::String(s) => Ok(Value::String(lua.create_string(s)?)),
        serde_json::Value::Array(a) => {
            let t = lua.create_table()?;
            for (i, x) in a.iter().enumerate() {
                t.raw_set((i + 1) as i64, json_to_lua(lua, x)?)?;
            }
            Ok(Value::Table(t))
        }
        serde_json::Value::Object(o) => {
            let t = lua.create_table()?;
            for (k, x) in o {
                t.raw_set(k.clone(), json_to_lua(lua, x)?)?;
            }
            Ok(Value::Table(t))
        }
    }
}

/// Convenience for tests / headless: registers the label and returns a
/// connector ready to spawn.
pub fn make(id: u16, entry: ScriptEntryConfig, log_path: Option<PathBuf>) -> LuaScriptConnector {
    let label = if entry.name.trim().is_empty() {
        format!("Script {}", id + 1)
    } else {
        entry.name.clone()
    };
    register_script_label(id, label);
    LuaScriptConnector { id, entry, log_path }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::TurnEndReason;
    use std::io::{Read, Write};
    use std::sync::atomic::Ordering;
    use std::time::{Duration, Instant};

    /// 随包脚本必须能被 Lua 编译(防语法错误回归;只编译不执行,
    /// 脚本主循环不会真的跑起来)。
    #[test]
    fn shipped_scripts_compile() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .join("scripts");
        let lua = mlua::Lua::new();
        let mut checked = 0usize;
        for entry in std::fs::read_dir(&dir).unwrap() {
            let p = entry.unwrap().path();
            if p.extension().and_then(|e| e.to_str()) != Some("lua") {
                continue;
            }
            let src = std::fs::read_to_string(&p).unwrap();
            let r = lua.load(&src).set_name(p.display().to_string()).into_function();
            assert!(r.is_ok(), "lua compile failed for {}: {:?}", p.display(), r.err());
            checked += 1;
        }
        assert!(checked >= 5, "expected the shipped scripts, found {checked}");
    }

    fn tmp_script(src: &str) -> (tempfile::Dir, PathBuf) {
        let dir = tempfile::Dir::new();
        let p = dir.path().join("test.lua");
        std::fs::write(&p, src).unwrap();
        (dir, p)
    }

    fn entry(file: PathBuf, name: &str) -> ScriptEntryConfig {
        ScriptEntryConfig { name: name.into(), file: file.display().to_string(), ..Default::default() }
    }

    fn run_script(cfg: ScriptEntryConfig, id: u16) -> Vec<StateEvent> {
        let c = make(id, cfg, None);
        let (tx, rx) = std::sync::mpsc::channel();
        let stop = crate::connectors::stop_flag();
        c.run(tx, stop);
        rx.try_iter().collect()
    }

    #[test]
    fn pet_api_emits_prefixed_events() {
        let (_d, p) = tmp_script(
            r#"
            pet.health(true)
            pet.session_started("s1", 1)
            pet.tool_started("s1", "bash", "ls -la")
            pet.live_text("s1", { text = "hello", reasoning = "想" })
            pet.session_ended("s1", 1, "completed")
            "#,
        );
        let evs = run_script(entry(p, "TestApp"), 0);
        let get = |f: &dyn Fn(&StateEvent) -> bool| evs.iter().any(|e| f(e));
        assert!(get(&|e| matches!(e, StateEvent::SourceHealth { source: Source::Script(0), healthy: true })));
        assert!(get(&|e| matches!(e, StateEvent::TurnStarted { session_id, turn: 1, .. } if session_id == "script-0-s1")));
        assert!(get(&|e| matches!(e, StateEvent::ToolStarted { name, arguments, session_id, .. } if name == "bash" && arguments.as_deref() == Some("ls -la") && session_id == "script-0-s1")));
        assert!(get(&|e| matches!(e, StateEvent::LiveText { text, reasoning, .. } if text.as_deref() == Some("hello") && reasoning.as_deref() == Some("想"))));
        assert!(get(&|e| matches!(e, StateEvent::TurnEnded { reason: TurnEndReason::Completed, .. })));
    }

    #[test]
    fn poll_and_todo_convert_tables() {
        let (_d, p) = tmp_script(
            r#"
            pet.poll({
              { session_id = "a", running = true, title = "任务A", todos = { { content = "干活", status = "in_progress" } } },
              { session_id = "b", running = false },
            })
            pet.todo("a", { { content = "做完了", status = "completed" } })
            "#,
        );
        let evs = run_script(entry(p, "PollApp"), 1);
        let poll = evs.iter().find_map(|e| match e {
            StateEvent::Poll { items, ok, .. } if *ok => Some(items.clone()),
            _ => None,
        });
        let items = poll.expect("poll event");
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].session_id, "script-1-a");
        assert_eq!(items[0].title.as_deref(), Some("任务A"));
        assert_eq!(items[0].todos.as_ref().unwrap()[0].content, "干活");
        assert_eq!(items[1].running, false);
        assert!(evs.iter().any(|e| matches!(e, StateEvent::TodoSnapshot { session_id, todos, .. }
            if session_id == "script-1-a" && todos[0].status == "completed")));
    }

    #[test]
    fn question_answer_and_user_message() {
        let (_d, p) = tmp_script(
            r#"
            pet.question("q1", "s1", "继续吗?")
            pet.answer("q1")
            pet.user_message("s1", "帮我查一下")
            pet.queue(3)
            "#,
        );
        let evs = run_script(entry(p, "QApp"), 2);
        assert!(evs.iter().any(|e| matches!(e, StateEvent::QuestionRequested { id, text, session_id, .. }
            if id == "script-2-q1" && text == "继续吗?" && session_id == "script-2-s1")));
        assert!(evs.iter().any(|e| matches!(e, StateEvent::QuestionResolved { id, .. } if id == "script-2-q1")));
        assert!(evs.iter().any(|e| matches!(e, StateEvent::UserMessage { text, .. } if text == "帮我查一下")));
        assert!(evs.iter().any(|e| matches!(e, StateEvent::QueueChanged { pending: 3, .. })));
    }

    #[test]
    fn session_status_approval_and_pending_sync() {
        let (_d, p) = tmp_script(
            r#"
            pet.session_status("s1", true)
            pet.approval_requested("a1", "s1", "bash")
            pet.approval_resolved("a1")
            pet.pending_sync()
            "#,
        );
        let evs = run_script(entry(p, "StateApp"), 20);
        assert!(evs.iter().any(|e| matches!(e, StateEvent::SessionStatus { session_id, running, .. }
            if session_id == "script-20-s1" && *running)));
        assert!(evs.iter().any(|e| matches!(e, StateEvent::ApprovalRequested { id, session_id, tool, .. }
            if id == "script-20-a1" && session_id == "script-20-s1" && tool == "bash")));
        assert!(evs.iter().any(|e| matches!(e, StateEvent::ApprovalResolved { id, .. } if id == "script-20-a1")));
        assert!(evs.iter().any(|e| matches!(e, StateEvent::PendingSync { source: Source::Script(20) })));
    }

    #[test]
    fn runtime_error_marks_unhealthy_and_does_not_panic() {
        let (_d, p) = tmp_script("pet.health(true)\nerror('boom')\n");
        let evs = run_script(entry(p, "ErrApp"), 3);
        // healthy=true then healthy=false (error)
        assert!(evs.iter().any(|e| matches!(e, StateEvent::SourceHealth { healthy: true, .. })));
        assert!(evs.iter().any(|e| matches!(e, StateEvent::SourceHealth { healthy: false, .. })));
        // 脚本死亡:补发 pending_sync 清残留审批/提问(否则最长卡 Attention 30min)
        assert!(evs.iter().any(|e| matches!(e, StateEvent::PendingSync { source: Source::Script(3) })));
    }

    #[test]
    fn poll_and_todo_tolerate_missing_fields() {
        // 用户脚本的信息常有缺漏:缺字段/类型不符必须降级处理(跳过条目、
        // 默认值),而不是类型错误把整条脚本炸掉
        let (_d, p) = tmp_script(
            r#"
            pet.poll({
              { session_id = "a", running = true, todos = { { content = "c1" }, { content = "c2", status = "done" }, "junk" } },
              { running = true },   -- 缺 session_id:整条跳过
              { session_id = 42 },  -- 数字 id:转字符串,running 默认 false
            })
            pet.todo("a", { { content = "t1" }, { content = "t2", status = "completed" }, 7 })
            "#,
        );
        let evs = run_script(entry(p, "LenientApp"), 21);
        assert!(evs.iter().any(|e| matches!(e, StateEvent::SourceHealth { healthy: true, .. })),
            "缺字段不得把脚本炸掉: {evs:?}");
        let poll = evs.iter().find_map(|e| match e {
            StateEvent::Poll { items, ok, .. } if *ok => Some(items.clone()),
            _ => None,
        });
        let items = poll.expect("poll event");
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].session_id, "script-21-a");
        assert!(items[0].running);
        let todos = items[0].todos.as_ref().unwrap();
        assert_eq!(todos.len(), 2);
        assert_eq!(todos[0].status, "pending"); // 缺 status → 默认 pending
        assert_eq!(todos[1].status, "done");
        assert_eq!(items[1].session_id, "script-21-42");
        assert!(!items[1].running); // 缺 running → false
        let todo = evs.iter().find_map(|e| match e {
            StateEvent::TodoSnapshot { todos, .. } => Some(todos.clone()),
            _ => None,
        });
        let todos = todo.expect("todo event");
        assert_eq!(todos.len(), 2);
        assert_eq!(todos[0].status, "pending");
    }

    #[test]
    fn compile_error_marks_unhealthy() {
        let (_d, p) = tmp_script("this is not lua at all !!!\n");
        let evs = run_script(entry(p, "BadApp"), 4);
        assert!(!evs.iter().any(|e| matches!(e, StateEvent::SourceHealth { healthy: true, .. })));
        assert!(evs.iter().any(|e| matches!(e, StateEvent::SourceHealth { healthy: false, .. })));
    }

    #[test]
    fn missing_file_marks_unhealthy() {
        let c = make(5, entry(PathBuf::from("/nonexistent/nope.lua"), "NoApp"), None);
        let (tx, rx) = std::sync::mpsc::channel();
        c.run(tx, crate::connectors::stop_flag());
        let evs: Vec<_> = rx.try_iter().collect();
        assert!(evs.iter().any(|e| matches!(e, StateEvent::SourceHealth { healthy: false, .. })));
    }

    #[test]
    fn sandbox_removes_dangerous_libs() {
        let (_d, p) = tmp_script("assert(os == nil and io == nil and require == nil and loadfile == nil)\n");
        let mut e = entry(p, "SafeApp");
        e.sandbox = true;
        let evs = run_script(e, 6);
        // script ran fine (health true), sandbox assertions passed
        assert!(evs.iter().any(|e| matches!(e, StateEvent::SourceHealth { healthy: true, .. })));
        // non-sandbox scripts keep the stdlib
        let (_d2, p2) = tmp_script("assert(os ~= nil and io ~= nil)\n");
        let evs2 = run_script(entry(p2, "FullApp"), 7);
        assert!(evs2.iter().any(|e| matches!(e, StateEvent::SourceHealth { healthy: true, .. })));
    }

    #[test]
    fn config_merges_name_poll_ms_and_args() {
        let (_d, p) = tmp_script(
            r#"
            local c = pet.config()
            assert(c.name == "CfgApp")
            assert(c.poll_ms == 500)
            assert(c.args.log == "x.log")
            assert(c.args.n == 5)
            "#,
        );
        let mut e = entry(p, "CfgApp");
        e.poll_ms = 500;
        e.args = Some(serde_json::json!({"log": "x.log", "n": 5}));
        let evs = run_script(e, 8);
        assert!(evs.iter().any(|e| matches!(e, StateEvent::SourceHealth { healthy: true, .. })));
    }

    #[test]
    fn unknown_end_reason_is_a_lua_error() {
        let (_d, p) = tmp_script("pet.session_ended('s1', 1, 'weird')\n");
        let evs = run_script(entry(p, "RApp"), 9);
        assert!(evs.iter().any(|e| matches!(e, StateEvent::SourceHealth { healthy: false, .. })));
    }

    #[test]
    fn source_label_registry() {
        let _ = make(10, entry(PathBuf::new(), "我的程序"), None);
        assert_eq!(Source::Script(10).label(), "我的程序");
        // unregistered ids fall back to a placeholder
        assert_eq!(Source::Script(65535).label(), "Script 65535");
        // 内置源已全部迁移为脚本:未注册的 id 显示占位名
        assert_eq!(Source::Script(60000).label(), "Script 60000");
    }

    #[test]
    fn sqlite_queries_rows() {
        let dir = tempfile::Dir::new();
        let db = dir.path().join("t.db");
        {
            let c = rusqlite::Connection::open(&db).unwrap();
            c.execute_batch(
                "CREATE TABLE m (id INTEGER PRIMARY KEY, session TEXT, role TEXT, content TEXT, n REAL);
                 INSERT INTO m (session, role, content, n) VALUES ('s1','assistant','你好',1.5);
                 INSERT INTO m (session, role, content, n) VALUES ('s2','user','x',2.0);",
            )
            .unwrap();
        }
        let script = format!(
            "local rows = pet.sqlite('{}', \"SELECT session, role, content, n FROM m WHERE session='s1'\")\n\
             assert(#rows == 1)\n\
             assert(rows[1].session == 's1' and rows[1].role == 'assistant')\n\
             assert(rows[1].content == '你好')\n\
             assert(rows[1].n == 1.5)\n",
            db.display().to_string().replace('\\', "\\\\")
        );
        let (_d, p) = tmp_script(&script);
        let evs = run_script(entry(p, "SqlApp"), 11);
        assert!(evs.iter().any(|e| matches!(e, StateEvent::SourceHealth { healthy: true, .. })));
        // params + 空结果
        let script2 = format!(
            "local rows = pet.sqlite('{}', 'SELECT content FROM m WHERE session = ?', {{'nope'}})\n\
             assert(#rows == 0)\n",
            db.display().to_string().replace('\\', "\\\\")
        );
        let (_d2, p2) = tmp_script(&script2);
        let evs2 = run_script(entry(p2, "SqlApp2"), 12);
        assert!(evs2.iter().any(|e| matches!(e, StateEvent::SourceHealth { healthy: true, .. })));
    }

    #[test]
    fn http_variants_error_on_bad_target() {
        // 死端口:http/http_post 都通过 pcall 抛错,不影响脚本其他部分
        let (_d, p) = tmp_script(
            "local ok1 = pcall(pet.http, 'http://127.0.0.1:1/x', 300)\n\
             local ok2 = pcall(pet.http_post, 'http://127.0.0.1:1/x', '{}', 300)\n\
             assert(not ok1 and not ok2)\n",
        );
        let evs = run_script(entry(p, "NetApp"), 13);
        assert!(evs.iter().any(|e| matches!(e, StateEvent::SourceHealth { healthy: true, .. })));
    }

    #[test]
    fn ws_streams_text_frames() {
        // 最小 ws 回环:先发 101 升级,再发一个文本帧 + close
        let (url, done_flag) = {
            let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let url = format!("http://{}", l.local_addr().unwrap());
            let done = Arc::new(std::sync::atomic::AtomicU32::new(0));
            let d2 = done.clone();
            std::thread::spawn(move || {
                let (mut s, _) = l.accept().unwrap();
                let mut buf = [0u8; 8192];
                let _ = s.read(&mut buf);
                let _ = s.write_all(
                    b"HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: abc\r\n\r\n",
                );
                let _ = s.write_all(&[0x81, 0x02, b'h', b'i']);
                let _ = s.write_all(&[0x88, 0x00]);
                let _ = s.flush();
                std::thread::sleep(std::time::Duration::from_millis(150));
                d2.store(1, std::sync::atomic::Ordering::SeqCst);
            });
            (url, done)
        };
        let script = format!(
            "local h = pet.ws('{url}', '/api/events.mux', 2000)\n\
             local f = pet.ws_read(h)\n\
             assert(f == 'hi', tostring(f))\n\
             local f2 = pet.ws_read(h)\n\
             assert(f2 == false, tostring(f2)) -- close frame -> false (not nil)\n\
             pet.ws_close(h)\n",
        );
        let (_d, p) = tmp_script(&script);
        let evs = run_script(entry(p, "WsApp"), 14);
        assert!(
            evs.iter().any(|e| matches!(e, StateEvent::SourceHealth { healthy: true, .. })),
            "ws script must succeed: {evs:?}"
        );
        for _ in 0..50 {
            if done_flag.load(std::sync::atomic::Ordering::SeqCst) == 1 {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert_eq!(done_flag.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[test]
    fn sandbox_blocks_network_and_sqlite() {
        let (_d, p) = tmp_script(
            "local a = pcall(pet.http, 'http://127.0.0.1:1/x', 300)\n\
             local b = pcall(pet.http_post, 'http://127.0.0.1:1/x', '{}', 300)\n\
             local c = pcall(pet.ws, 'http://127.0.0.1:1', '/x', 300)\n\
             local d = pcall(pet.sqlite, '/tmp/x.db', 'SELECT 1')\n\
             assert(not a and not b and not c and not d)\n",
        );
        let mut e = entry(p, "SafeApp2");
        e.sandbox = true;
        let evs = run_script(e, 15);
        assert!(evs.iter().any(|e| matches!(e, StateEvent::SourceHealth { healthy: true, .. })));
    }

    /// scripts/ 下随包脚本路径(行为级测试直接跑真脚本)。
    fn script_path(name: &str) -> String {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .join("scripts")
            .join(name)
            .display()
            .to_string()
    }

    /// 回归:dsh.lua 的 session/jobs 必须按 (sessionId, jobId) 成对发
    /// tool_started/tool_ended。旧实现每会话只留一槽:同帧两个 job 会漏发
    /// 第二个的 started、第一个的 ended,宿主被钉在 Working;job 从快照
    /// 消失的帧也必须补发 tool_ended(旧实现静默清除)。
    #[test]
    fn dsh_mux_jobs_track_per_job() {
        use std::sync::atomic::AtomicU32;
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", l.local_addr().unwrap());
        let mux_served = Arc::new(AtomicU32::new(0));
        let served2 = mux_served.clone();
        std::thread::spawn(move || {
            for stream in l.incoming() {
                let Ok(s) = stream else { break };
                let served3 = served2.clone();
                std::thread::spawn(move || {
                    let mut s = s;
                    let mut buf = [0u8; 8192];
                    let n = s.read(&mut buf).unwrap_or(0);
                    let req = String::from_utf8_lossy(&buf[..n]).to_string();
                    let is_ws = req.contains("events.mux") || req.contains("events.host");
                    if !is_ws {
                        // session.list 等 HTTP 请求:500 快速失败
                        let _ = s.write_all(
                            b"HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\n\r\n",
                        );
                        return;
                    }
                    let _ = s.write_all(
                        b"HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: abc\r\n\r\n",
                    );
                    let _ = s.flush();
                    if req.contains("events.mux")
                        && served3.swap(1, Ordering::SeqCst) == 0
                    {
                        let mut send_text = |text: &str| {
                            let payload = text.as_bytes();
                            let mut frame = vec![0x81u8];
                            if payload.len() < 126 {
                                frame.push(payload.len() as u8);
                            } else {
                                frame.push(126);
                                frame.extend_from_slice(&(payload.len() as u16).to_be_bytes());
                            }
                            frame.extend_from_slice(payload);
                            let _ = s.write_all(&frame);
                            let _ = s.flush();
                        };
                        send_text(
                            r#"{"payload":{"type":"session/jobs","sessionId":"s1","jobs":[{"id":"A","status":"running"},{"id":"B","status":"running"}]}}"#,
                        );
                        std::thread::sleep(std::time::Duration::from_millis(300));
                        // 空快照:两个 job 消失 → 都要补 tool_ended
                        send_text(r#"{"payload":{"type":"session/jobs","sessionId":"s1","jobs":[]}}"#);
                    }
                    // 保持连接打开,避免脚本立刻重连干扰断言
                    std::thread::sleep(std::time::Duration::from_millis(10_000));
                });
            }
        });

        let cfg = ScriptEntryConfig {
            name: "DSH".into(),
            file: script_path("dsh.lua"),
            poll_ms: 1000,
            args: Some(serde_json::json!({ "url": url })),
            ..Default::default()
        };
        let stop = crate::connectors::stop_flag();
        let (tx, rx) = std::sync::mpsc::channel();
        make(22, cfg, None).spawn(tx, stop.clone());

        let started = Instant::now();
        let mut evs = Vec::new();
        while started.elapsed() < Duration::from_secs(8) {
            match rx.recv_timeout(Duration::from_millis(100)) {
                Ok(ev) => evs.push(ev),
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                Err(_) => break,
            }
            let has = |pat: &dyn Fn(&StateEvent) -> bool| evs.iter().any(|e| pat(e));
            let started_ok = has(&|e| matches!(e, StateEvent::ToolStarted { name, session_id, .. } if session_id == "script-22-s1" && name == "job:A"))
                && has(&|e| matches!(e, StateEvent::ToolStarted { name, .. } if name == "job:B"));
            let ended_ok = has(&|e| matches!(e, StateEvent::ToolEnded { name, .. } if name == "job:A"))
                && has(&|e| matches!(e, StateEvent::ToolEnded { name, .. } if name == "job:B"));
            if started_ok && ended_ok {
                break;
            }
        }
        stop.store(true, Ordering::SeqCst);

        let has = |pat: &dyn Fn(&StateEvent) -> bool| evs.iter().any(|e| pat(e));
        assert!(
            has(&|e| matches!(e, StateEvent::ToolStarted { name, session_id, .. } if session_id == "script-22-s1" && name == "job:A"))
                && has(&|e| matches!(e, StateEvent::ToolStarted { name, .. } if name == "job:B")),
            "两个 job 都必须发 tool_started: {evs:?}"
        );
        assert!(
            has(&|e| matches!(e, StateEvent::ToolEnded { name, .. } if name == "job:A"))
                && has(&|e| matches!(e, StateEvent::ToolEnded { name, .. } if name == "job:B")),
            "job 从快照消失必须补发 tool_ended: {evs:?}"
        );
    }

    /// 回归:maa.lua 的"资深干员-only 链"(链内从未有连接/任务)在 attention
    /// 自动解除后必须中性收尾,否则会话永远 running,宠物被永久钉在 Thinking。
    #[test]
    fn maa_senior_only_chain_ends_neutrally() {
        let dir = tempfile::Dir::new();
        let log = dir.path().join("gui.log");
        std::fs::write(&log, "").unwrap();
        let cfg = ScriptEntryConfig {
            name: "MAA".into(),
            file: script_path("maa.lua"),
            poll_ms: 50,
            args: Some(serde_json::json!({ "log": log.display().to_string(), "attention_ms": 0 })),
            ..Default::default()
        };
        let stop = crate::connectors::stop_flag();
        let (tx, rx) = std::sync::mpsc::channel();
        make(23, cfg, None).spawn(tx, stop.clone());
        // 等启动扫描读完(空)日志、进入 tail 循环后再追加"资深干员"行:
        // 启动扫描只认 connect/start/done/stop/clear,已有行不会驱动状态机
        std::thread::sleep(Duration::from_millis(300));
        {
            use std::io::Write as _;
            let mut f = std::fs::OpenOptions::new().append(true).open(&log).unwrap();
            writeln!(f, "[01-01 10:00:00.000][TRACE][MeoAssistant] <1> 识别到资深干员信息").unwrap();
        }

        let started = Instant::now();
        let mut evs = Vec::new();
        while started.elapsed() < Duration::from_secs(5) {
            match rx.recv_timeout(Duration::from_millis(100)) {
                Ok(ev) => evs.push(ev),
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                Err(_) => break,
            }
            if evs.iter().any(|e| matches!(e, StateEvent::TurnEnded { .. })) {
                break;
            }
        }
        stop.store(true, Ordering::SeqCst);

        assert!(
            evs.iter().any(|e| matches!(e, StateEvent::TurnStarted { session_id, .. } if session_id.starts_with("script-23-maa-"))),
            "资深干员应开链: {evs:?}"
        );
        assert!(
            evs.iter().any(|e| matches!(e, StateEvent::QuestionRequested { text, .. } if text.contains("资深干员"))),
            "应发出提问: {evs:?}"
        );
        assert!(
            evs.iter().any(|e| matches!(e, StateEvent::QuestionResolved { .. })),
            "attention 应自动解除: {evs:?}"
        );
        assert!(
            evs.iter().any(|e| matches!(e, StateEvent::TurnEnded { reason: crate::state::TurnEndReason::Aborted, .. })),
            "空链必须中性收尾(否则永久 Thinking): {evs:?}"
        );
    }
}

/// Tiny temp-dir helper (no extra deps; tests only).
#[cfg(test)]
mod tempfile {
    pub struct Dir(pub std::path::PathBuf);
    impl Dir {
        pub fn new() -> Dir {
            let p = std::env::temp_dir().join(format!(
                "dshpet-lua-test-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            std::fs::create_dir_all(&p).unwrap();
            Dir(p)
        }
        pub fn path(&self) -> &std::path::Path {
            &self.0
        }
    }
    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
}
