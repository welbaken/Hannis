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

use super::{send, sleep_interruptible};
use crate::config::ScriptEntryConfig;
use crate::state::{register_script_label, SessionItem, Source, StateEvent, TodoItem, TurnEndReason};
use mlua::{Lua, Table, Value};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::Arc;

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
        send(tx, StateEvent::SourceHealth { source: Source::Script(self.id), healthy: false });
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
        send(&tx, StateEvent::SourceHealth { source: Source::Script(self.id), healthy: true });
        self.log(&format!("started ({} bytes)", src.len()));
        match func.call::<()>(()) {
            Ok(()) => self.fail(&tx, "script returned (not looping) — source stopped"),
            Err(e) => self.fail(&tx, &format!("script error: {e}")),
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

        let pet = lua.create_table()?;

        let f = {
            let tx = tx.clone();
            let log = log.clone();
            lua.create_function(move |_, (level, msg): (String, String)| {
                log(format!("{level}: {msg}"));
                Ok(())
            })?
        };
        pet.set("log", f)?;

        let f = {
            let tx = tx.clone();
            lua.create_function(move |_, ms: u64| {
                sleep_interruptible(ms, &stop);
                Ok(())
            })?
        };
        pet.set("wait", f)?;

        let f = {
            let tx = tx.clone();
            let sid = sid.clone();
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
                    if let Value::Table(t) = v {
                        let content: String = t.get("content")?;
                        let status: String = t.get("status")?;
                        out.push(TodoItem { content, status });
                    }
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
                    if let Value::Table(t) = v {
                        let session_id: String = t.get("session_id")?;
                        let running: bool = t.get("running")?;
                        let title: Option<String> = t.get("title")?;
                        let todos = match t.get::<Option<Table>>("todos")? {
                            Some(tt) => {
                                let mut v = Vec::new();
                                for x in tt.sequence_values::<Value>() {
                                    let x = x?;
                                    if let Value::Table(xt) = x {
                                        let content: String = xt.get("content")?;
                                        let status: String = xt.get("status")?;
                                        v.push(TodoItem { content, status });
                                    }
                                }
                                Some(v)
                            }
                            None => None,
                        };
                        out.push(SessionItem {
                            session_id: sid(session_id),
                            running,
                            title,
                            todos,
                        });
                    }
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
        let f = {
            let reg = ws_reg.clone();
            lua.create_function(move |_, id: i64| {
                let mut reg = reg.lock().unwrap();
                let ws = reg
                    .get_mut(&(id as u64))
                    .ok_or_else(|| mlua::Error::runtime("ws handle closed or invalid"))?;
                loop {
                    match ws.read_frame() {
                        Ok(fr) => {
                            if fr.opcode == crate::http::WS_OP_TEXT || fr.opcode == crate::http::WS_OP_BINARY {
                                return Ok(Some(String::from_utf8_lossy(&fr.payload).to_string()));
                            }
                            if fr.opcode == crate::http::WS_OP_CLOSE {
                                return Ok(None);
                            }
                            // ping/pong 等控制帧:继续等
                        }
                        Err(_) => return Ok(None), // 超时/关闭/错误
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
                let mut conn = rusqlite::Connection::open_with_flags(
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
    fn runtime_error_marks_unhealthy_and_does_not_panic() {
        let (_d, p) = tmp_script("pet.health(true)\nerror('boom')\n");
        let evs = run_script(entry(p, "ErrApp"), 3);
        // healthy=true then healthy=false (error)
        assert!(evs.iter().any(|e| matches!(e, StateEvent::SourceHealth { healthy: true, .. })));
        assert!(evs.iter().any(|e| matches!(e, StateEvent::SourceHealth { healthy: false, .. })));
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
        assert_eq!(Source::Dsh.label(), "DSH");
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
             assert(f2 == nil)\n\
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
}

/// Tiny temp-dir helper (no extra deps).
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
