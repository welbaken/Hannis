# 为 Hannis 添加新的消息接口（连接器）指南

> **读者**：AI 或开发者。**目标**：给这个桌面宠物程序接入一个新的消息来源
> （另一个 agent 后端、聊天服务、消息平台等），复用现有状态机、气泡、动画体系。
>
> 阅读前建议先浏览 `app/src/state.rs`（事件契约）与本文件 §2，再动手。
> 现有四种来源可作范本：`connectors/dsh.rs`（HTTP 轮询 + SSE/WS 流式）、
> `connectors/hermes.rs`（SQLite 轮询 + 增量 diff）、`connectors/comfyui.rs`（仅队列）、
> `connectors/maa.rs`（本地追加日志 tail，纯 std 无新依赖）。

---

## 0. 架构总览（30 秒版）

```
                 ┌─────────────────────────────────────────────┐
 上游消息源  ───► │ connectors/xxx.rs（新连接器，独立线程）      │
 (HTTP/WS/SQLite)│  轮询/订阅 → 翻译成 StateEvent              │
                 └───────────────┬─────────────────────────────┘
                                 │ mpsc channel (Sender<StateEvent>)
                                 ▼
                 ┌─────────────────────────────────────────────┐
                 │ PetState（纯状态机，无 I/O，state.rs）        │
                 │  apply(ev) → sessions/approvals/questions…   │
                 │  snapshot() → Snapshot + Mode                │
                 └───────────────┬─────────────────────────────┘
                                 ▼
                 ┌─────────────────────────────────────────────┐
                 │ GUI（gui/mod.rs，Windows）                    │
                 │  模式→动画资产 / 气泡内容(bubble_text)        │
                 └─────────────────────────────────────────────┘
```

- **连接器 = 生产者**：每个来源 `spawn` 自己的线程，只做一件事——把上游数据翻译成
  `StateEvent` 发进共享 channel。**绝不碰 UI、绝不做状态决策**。
- **状态机 = 消费者**：`PetState::apply(ev)` 累积会话状态；`snapshot()` 产出
  `Snapshot`（含 `Mode`）供 GUI 每帧取用。
- 全部逻辑按 `Source` 枚举区分来源；新增来源 = 新增一个枚举变体 + 一个连接器模块。

**文件地图**：

| 文件 | 职责 |
|---|---|
| `app/src/state.rs` | `Source` / `Mode` / `StateEvent` / `PetState` / `Snapshot`（契约核心） |
| `app/src/connectors/mod.rs` | `send()` / `sleep_interruptible()` / `stop_flag()` 公共助手 |
| `app/src/connectors/<name>.rs` | 各来源连接器（照抄其模式） |
| `app/src/http.rs` | 零依赖 HTTP/1.1 + WebSocket + SSE 客户端（新增连接器优先复用） |
| `app/src/config.rs` | 配置段结构 + 默认值 |
| `app/src/bubble_text.rs` | 气泡文案（标题行 / "From X" / 内容） |
| `app/src/gui/mod.rs` | 连接器注册点（`run()`）与渲染 |
| `app/src/headless.rs` | 无 GUI 调试入口（**必须同步注册**，保持行为一致） |

---

## 1. 核心概念

- **Source**（`state.rs`）：来源枚举，`label()` 返回气泡里 "From X" 的显示名。
  注意 `sessions` 表**以 `session_id` 为唯一键**（不含来源），所以新来源的会话 id
  **必须加命名空间前缀**（如 `xxx-<id>`），否则会与 DSH/Hermes 的会话串台。
- **StateEvent**：连接器唯一的输出语言（§2 有完整契约）。**所有字段都是翻译后的**
  ——连接器负责把上游的 JSON/DB 差异消掉。
- **Mode**：宠物当前显示的状态（idle/thinking/working/…），由状态机从事件推导，
  连接器**不直接设置 Mode**。
- **会话生命周期**：`TurnStarted → (LiveText/ToolStarted/ToolEnded)* → TurnEnded`。
  一个"回合"（turn）≈ 一次模型回答；工具（tool）是回合内的工作阶段。

---

## 2. StateEvent 契约（最重要，写代码前先读透）

| 事件 | 字段 | 何时发 | 状态机的处理 |
|---|---|---|---|
| `Poll` | `source, items: Vec<SessionItem>, ok, error` | 每次轮询的**基线快照**；`ok=false` 时 `items` 应为空 | `ok=false` 直接忽略（健康另由 SourceHealth 管）。`ok=true`：合并 title/todos、按 `running` 翻转、**本轮未出现的会话会被 reap**（不活跃且超过 done 窗口才删）；`running=false` 且曾有 turns/tools 且非等待用户且 10s 内没结束过 → 补发一个"已完成" |
| `SessionStatus` | `source, session_id, running` | running 翻转（推送式来源用；轮询式可不发，Poll 已含） | 同 Poll 的 running 分支 |
| `TurnStarted` | `source, session_id, turn: u64` | 会话开始新回合 | `turns += 1; running = true; waiting_user = false` |
| `TurnEnded` | `source, session_id, turn, reason: TurnEndReason` | 回合结束。`reason ∈ {Completed, Error, MaxTokens, Aborted, Interrupted, Blocked}` | `turns -= 1`；记录 `last_end`；`Blocked` → 进入等待用户（attention 语义）；`turns==0` 时清空 live 文本；`Completed`/`is_failure()` 置声音标记 |
| `ToolStarted` | `source, session_id, name, arguments: Option<String>` | 工具调用开始 | 记入 tools（→ **Working 模式**）、记录开始时间（气泡"谁在干活显示谁"的判据）、**清空该会话 live 文本**（新工作阶段，避免气泡显示调用前的旧思考） |
| `ToolEnded` | `source, session_id, name, error: bool` | 工具调用结束 | 从 tools 移除 |
| `TodoSnapshot` | `source, session_id, todos` | todo 列表变化 | 覆盖式保存（done/fail 气泡的任务名兜底） |
| `LiveText` | `source, session_id, reasoning: Option<String>, text: Option<String>, tool_name: Option<String>` | 模型实时输出（思考/正文），**增量追加** | 对 `Some` 的字段追加到对应缓冲（总长 cap 8000 字符）。**不要整段重发**——会重复累积 |
| `UserMessage` | `source, session_id, text` | 用户消息（任务提示） | 存 `last_user_text`（截断 120），用于 done/failed 气泡的任务名兜底 |
| `ApprovalRequested` / `ApprovalResolved` | `source, id, session_id, tool` / `source, id` | 工具审批请求/结果 | 未决审批 → **Attention 模式**；TTL 30 分钟；`id` 全局唯一 |
| `QuestionRequested` / `QuestionResolved` | `source, id, session_id, text` / `source, id` | 向用户提问/回答（如 Hermes `clarify`） | 同上；`id` 需全局唯一（Hermes 用 `<rpcId>\u{0}<itemId>` 前缀模式） |
| `SourceHealth` | `source, healthy` | **只在健康状态翻转时发一次**（不要每轮都发） | 记录 `sources` 表；全部离线 → **Offline 模式** |
| `QueueChanged` | `source, pending` | 队列深度变化 | 快照的 `queue_len` = 各源求和（气泡 idle 文案用） |
| `Tick` | — | 定时心跳 | 清理过期审批/提问 |

`SessionItem`（Poll 的 items 元素）：

```rust
pub struct SessionItem {
    pub session_id: String,
    pub running: bool,
    pub title: Option<String>,
    pub todos: Option<Vec<TodoItem>>, // TodoItem { content, status }
}
```

**Mode 推导优先级**（连接器不需要关心，但要知道自己的事件会触发什么）：

```
Offline(全部离线) > Attention(未决审批/提问) > Failed(失败窗口)
> Working(有工具在跑) > Thinking(turns>0) > Done(完成窗口) > Idle
```

---

## 3. 分步实现

### Step 0 — 设计决策（先想清楚，再写代码）

1. **数据怎么拿**：HTTP 轮询（dsh 模式）/ WebSocket 或 SSE 推送（http.rs 已支持）/
   SQLite 轮询（hermes 模式）/ 纯队列（comfyui 模式——**不是所有来源都要有会话**，
   只发 `QueueChanged` + `SourceHealth` 也合法）。
2. **事件映射表**：把上游的每个状态变化写成一列 `StateEvent`（对照 §2 表）。
   特别要明确：轮询式怎么**只发增量**（自己记忆 prev，参考 hermes 的 `PrevSession`）。
3. **会话 id 命名空间**：`<来源>-<上游id>`，避免与现有来源冲突。

### Step 1 — 配置（`app/src/config.rs`）

仿照现有段：

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct XxxConfig {
    pub url: String,
    pub poll_ms: u64,
    pub enabled: bool,
}

impl Default for XxxConfig {
    fn default() -> Self {
        XxxConfig { url: "http://127.0.0.1:9999".into(), poll_ms: 2000, enabled: true }
    }
}
```

然后在 `Config` 结构体加 `pub xxx: XxxConfig` 字段，并在 `Config::default()` 里赋值。
`config.json` 缺段/缺字段都能靠 `#[serde(default)]` 兜底（旧配置不会坏）。

### Step 2 — 连接器模块（`app/src/connectors/xxx.rs`）

骨架（最小 HTTP 轮询版，直接照抄这个模板）：

```rust
//! Xxx connector: <一句话说明数据来源与协议>。
use super::{send, sleep_interruptible};
use crate::http::Url;
use crate::state::{SessionItem, Source, StateEvent};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;

pub struct XxxConnector {
    pub url: String,
    pub poll_ms: u64,
}

impl XxxConnector {
    /// 入口契约：消费 self，起命名线程，只通过 tx 发事件。
    pub fn spawn(self, tx: Sender<StateEvent>, stop: Arc<AtomicBool>) {
        let url = self.url.clone();
        let poll_ms = self.poll_ms;
        std::thread::Builder::new()
            .name("xxx-poll".into())
            .spawn(move || poll_loop(&url, poll_ms, tx, stop))
            .ok();
    }
}

fn poll_loop(url: &str, poll_ms: u64, tx: Sender<StateEvent>, stop: Arc<AtomicBool>) {
    let mut healthy = false;
    let mut prev: Option<PrevState> = None; // 增量 diff 的记忆（参考 hermes）
    while !stop.load(Ordering::Relaxed) {
        match fetch(url) {
            Ok(raw) => {
                // 解析必须是纯函数，便于单测（见 Step 6）
                let events = parse_events(&raw, &mut prev);
                for ev in events {
                    send(&tx, ev);
                }
                send(&tx, StateEvent::Poll {
                    source: Source::Xxx,
                    items: parse_items(&raw),
                    ok: true,
                    error: None,
                });
                if !healthy {
                    healthy = true;
                    send(&tx, StateEvent::SourceHealth { source: Source::Xxx, healthy: true });
                }
            }
            Err(e) => {
                eprintln!("[xxx] poll error: {e}");
                send(&tx, StateEvent::Poll { source: Source::Xxx, items: vec![], ok: false, error: Some(e.to_string()) });
                if healthy {
                    healthy = false;
                    send(&tx, StateEvent::SourceHealth { source: Source::Xxx, healthy: false });
                }
            }
        }
        sleep_interruptible(poll_ms, &stop);
    }
}

/// 纯解析：上游 JSON → 事件列表。不要在这里做 I/O。
fn parse_events(raw: &str, prev: &mut Option<PrevState>) -> Vec<StateEvent> { /* … */ }
fn parse_items(raw: &str) -> Vec<SessionItem> { /* … */ }
```

要点：

- **健康翻转**：只在 `healthy` 变化时发 `SourceHealth`（否则刷屏）。
- **增量**：轮询式必须自己记忆上一轮（`PrevState`），只把**新增/变化**翻译成事件；
  基线情况全部交给 `Poll`（title/todos/running 都会合并）。
- **LiveText 增量**：只发新追加的尾巴，不是整段文本。
- **复用 `http.rs`**：`Url::parse`、GET/POST、SSE、WebSocket 都有现成实现，零新依赖。
- 需要 SQLite 时用 `rusqlite`（bundled，已存在）；**尽量不加新 crate**（exe 有体积预算）。

### Step 3 — Source 枚举（`app/src/state.rs`）

```rust
pub enum Source {
    Dsh,
    Hermes,
    ComfyUi,
    Xxx,               // ← 新增
}
// label() 是唯一的穷尽 match，编译错误会指出所有要补的地方
```

`label()` 返回的名字会出现在气泡头部右侧："思考中… From Xxx"。

### Step 4 — 注册（两处，必须都改）

`app/src/gui/mod.rs` 的 `run()`（约 204 行附近，ComfyUi 之后）：

```rust
if cfg.xxx.enabled {
    XxxConnector { url: cfg.xxx.url.clone(), poll_ms: cfg.xxx.poll_ms }
        .spawn(tx.clone(), stop.clone());
}
```

`app/src/headless.rs` 的 `drive()` 与 `debug_run()` 同样注册（调试入口行为必须与 GUI 一致）。

### Step 5 — 气泡文案（如需要定制）

`app/src/bubble_text.rs`：

- `Source::label()` 已自动用于 "From X"，一般不用改。
- 非流式状态的标题/条目在 `plain_bubble()`；Working/Thinking 的标题在 `live_parts()`。
- 队列相关文案（"队列中还有 N 个任务"）读 `snapshot.queue_len`，新来源发
  `QueueChanged` 即自动生效。

### Step 6 — 测试

1. **解析函数纯化**：`parse_events` / `parse_items` 不碰 I/O → 直接喂样例 JSON 断言
   事件序列（参考 dsh.rs 测试：`handle_mux_frame` / `history_turn_and_tool_events` 等）。
2. **状态机无需改**：`PetState` 的测试用注入时钟（`now_ms`）验证事件组合；
   新来源的事件与现有来源共用同一套语义。
3. 运行：

```bash
source .tools/env.sh && cd app
cargo test --lib                                    # 单元测试
cargo check --target x86_64-pc-windows-gnu          # Windows 目标编译检查
cargo run -- --self-test                            # 素材自检（与连接器无关，但顺手验证）
```

### Step 7 — 冒烟验证

1. `DSH_PET_DEBUG=1 ./target/debug/hannis`（headless）：看新来源的事件流是否正确
   （Poll/LiveText/TurnStarted/TurnEnded/健康翻转）。
2. Windows 上 `./build-wsl.sh` 后运行，确认：气泡头部出现 "From Xxx"、会话活动时
   宠物切 thinking/working、离线时整体 Offline、审批/提问出现 attention。

---

## 4. 约定与坑（务必遵守）

1. **线程模型**：连接器线程绝不调用 GUI/渲染代码；GUI 线程只消费事件。
2. **退出**：循环必须检查 `stop`（用 `sleep_interruptible` 切片睡眠），否则程序退不掉。
3. **幂等与乱序**：事件要能容忍重复/乱序——`Poll` 是基线（幂等合并），增量事件
   最好也能重复应用（`turns` 用 `saturating_sub`、集合用 insert/remove 这类操作）。
4. **LiveText cap**：状态机有 8000 字符上限，但连接器**不要**每 chunk 发一次事件
   ——自行聚合（dsh 聚合 chunk、hermes 轮询 delta），否则 channel 会被打爆。
5. **session_id 唯一性**：跨来源共享一张 sessions 表，id 必须带来源前缀。
6. **健康语义**：`healthy=false` 的 Poll 会被状态机忽略，但 `SourceHealth` 翻转必须发
   （否则 GUI 不知道它掉线，气泡源选择/Offline 模式都会错）。
7. **时间**：状态机测试用注入时钟；连接器内部随意用 `std::time`。
8. **日志**：连接器自己的 `eprintln!("[xxx] …")`，前缀与模块名一致。
9. **体积**：优先 `http.rs`（HTTP/1.1 + WebSocket + SSE 自带）；新依赖慎重。
10. **素材/动画**：新来源不需要新动画——`Mode::asset()` 只按模式映射，
    不用改；除非你要引入全新的模式（那是更大的改动，需要动 Mode 优先级表）。

---

## 5. 完成检查清单

- [ ] `Source` 新增变体，`label()` 已更新，编译无未穷尽 match
- [ ] `config.rs`：`XxxConfig` + `Config.xxx` + `Default` + 默认值测试
- [ ] `connectors/xxx.rs`：`spawn` 契约、命名线程、stop 检查、健康翻转、增量事件
- [ ] `gui/mod.rs` 与 `headless.rs` 都注册了
- [ ] 会话 id 带命名空间前缀
- [ ] 解析函数纯化且有单测（样例 JSON → 期望事件序列）
- [ ] `cargo test --lib` 全绿；Windows target `cargo check` 零错误
- [ ] 真机冒烟：气泡 "From Xxx"、状态切换、离线、attention（如适用）

---

## 6. 参考索引

| 想抄什么 | 看哪里 |
|---|---|
| 连接器最小骨架 / 健康翻转 | `connectors/dsh.rs` 的 `poll_loop`（约 79 行） |
| 增量 diff 记忆 | `connectors/hermes.rs` 的 `PrevSession`（约 19 行） |
| WS/SSE 流式 | `connectors/dsh.rs` 的 `stream_loop`（约 486 行）/ `connectors/comfyui.rs` 的 `ws_loop`（约 277 行） |
| 纯解析 + 事件翻译 | `connectors/dsh.rs` 的 `handle_mux_frame`（约 586 行）/ `handle_host_frame`（约 683 行）/ `history_loop`（约 201 行） |
| 仅队列来源（无会话） | `connectors/comfyui.rs` |
| 本地文件 tail（纯 std，无新依赖） | `connectors/maa.rs`——追加式日志轮询 + 偏移量记忆 + 截断/清空检测 + 启动时尾部恢复 |
| 事件消费语义 | `state.rs` 的 `PetState::apply`（288 行起） |
| 气泡源选择 / 平局规则 | `state.rs` 的 `select_bubble_source`（644 行） |
| 气泡文案 | `bubble_text.rs` 的 `plain_bubble`（212 行）/ `live_parts`（273 行） |
| HTTP/WS 客户端 | `http.rs` |
