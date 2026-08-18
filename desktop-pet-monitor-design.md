# 独立桌面宠物监控方案设计(DSH v1 / Hermes v2)

> 目标:开发一个**独立桌面宠物**——透明置顶悬浮窗(非浏览器内),实时监控 AI Agent 工作状态并表现。
> 版本策略:**v1 只接 DSH**,**v2 接入 Hermes(多后端)**。
> 本文汇总前期调研结论(三个参考仓库源码通读 + DSH 运行时源码核查 + 本机 Hermes 实例实测),作为开发指导。
> 基线代码:[dsh-plugin-pet-rs](https://github.com/HuanLinOTO/dsh-plugin-pet-rs)(MIT,三 crate 分层,可直接作为骨架);本方案将其 5 态粗粒度状态机升级为全粒度,并落地 port & adapter 多后端架构。

---

## 1. 设计原则(来自三个参考项目的教训)

| 教训 | 来源 | 落地 |
|---|---|---|
| **轮询是唯一可靠基线** | kun-pet:实测部分部署里 Agent 状态事件不流经插件总线(831 次事件观测,status 类为 0),事件监听永远等不到完成 | 任何后端都保留轮询基线,推送只做延迟增强,两者永不互相替代 |
| **推送能降延迟就降** | whale-girl:SSE 事件广播后客户端立即拉取,延迟从 pollMs(3s)降到单次往返 | 收到事件 → 立即重算快照并重绘 |
| **数据落盘永远可读** | 本机 Hermes 实测:HTTP API 认证打不开(401),但 SQLite 数据库裸奔可读 | 连接器优先选"不需要对方配合"的通道;API 认证是可选增强 |
| **状态机文法单源** | whale-girl `STATE_TABLE`:行序即优先级,加状态只改表 | 状态优先级用表驱动,禁止散落 if 链 |
| **语义分层** | whale-girl:Node half 出"事实窗口"、client half 做"本地交互" | 状态机(纯逻辑)与 UI/交互严格分离,两层都可单测 |
| **信息边界写进契约** | 外部客户端拿不到宿主总线事件(见 §3.7) | 每个后端的"可见信号集"在连接器层固化,状态机不依赖未承诺的信号 |

## 2. 总体架构(port & adapter)

```
┌────────────────────────────────────────────────────┐
│ UI 层:透明置顶窗 + 渲染器 + 托盘 + 音频 + 设置面板    │ ← 只消费 Snapshot
├────────────────────────────────────────────────────┤
│ 状态机层:PetState(纯逻辑,无 I/O,可单测)             │ ← 只消费 StateEvent(语义事件)
├────────────────────────────────────────────────────┤
│ 连接器层:AgentSource trait                          │ ← 后端协议 → 语义事件
│   ├─ DSH Connector(v1):session.list + events.mux/host │
│   └─ Hermes Connector(v2):SQLite 直读 + HTTP API 增强 │
└────────────────────────────────────────────────────┘
```

关键抽象:

```rust
/// 语义事件——状态机唯一输入,禁止携带任何后端专有字段
pub enum StateEvent {
    Poll { sessions: Vec<SessionItem>, ok: bool, error: Option<String> },
    ApprovalRequested { id: String, session_id: String, tool: String },
    ApprovalResolved { id: String },
    QuestionRequested { id: String, session_id: String, text: String },
    QuestionResolved { id: String },
    TurnStarted { session_id: String, turn: u64 },
    TurnEnded { session_id: String, turn: u64, reason: TurnEndReason },
    ToolStarted { session_id: String, name: String },          // tool/call
    ToolEnded { session_id: String, name: String, error: bool }, // tool/result
    TodoSnapshot { session_id: String, todos: Vec<TodoItem> },
    SessionStatus { session_id: String, running: bool },       // host/session-status
    Tick,
}

/// 连接器抽象:每个后端一个实现
pub trait AgentSource {
    fn poll(&self) -> Result<Vec<SessionItem>, String>;          // 轮询基线
    fn events(&self) -> Option<EventStream>;                    // 推送通道(None = 仅轮询)
    fn health(&self) -> bool;
}
```

> 注:pet-rs 现状是 `StateEvent::{Poll, MuxFrame, HostFrame, Tick}`,DSH 帧解析混在状态机里(`apply_mux_frame`)。本方案把这一步上移:连接器负责解析 `server-request` 信封与帧类型,状态机只收上面的语义事件——这是支持 Hermes 的前提。

## 3. DSH 数据面(v1 的全部原料,已从运行时源码核实)

### 3.1 通道总览

| 通道 | 用法 | 作用 |
|---|---|---|
| `session.list` RPC | `POST /api/session.list`,信封 `{type:"client-request", rpcId, method, payload:{}}`,2s 轮询 | **基线**:会话 running 位 + title + todos 快照;推送缺口兜底 |
| `events.mux` WebSocket | `/api/events.mux` | 审批 / 提问 / 队列 / **会话事件全量**(见 §3.4) |
| `events.host` WebSocket | `/api/events.host` | `host/session-status` running 翻转即时推送 |
| 本地 tick | 30s | TTL 过期清理 |

传输细节:
- 两条 WS 都是**服务端单向下行**,信封为 `{type:'server-request', rpcId, method, payload}`,`payload` 才是业务帧;客户端发任何数据都会被服务端断开。
- 当前 DSH 运行时的 `dsh-host-apiproxy` 对 `GET /api/events.mux|host` 也返回 **SSE**(`sseResponse`);老版本只收 WS 升级、GET 返回 426。连接器策略:**先试 WS,失败降级 SSE GET**(两者帧格式相同)。
- 断线自动重连,3s 退避;`events.host` 重连后立即补一次 `session.list`(防连接期间的翻转漏掉)。

### 3.2 session.list 返回结构

```
items: [{
  sessionId: string,
  running: bool,
  projections: { values: { title?: string, todos?: TodoItem[] | null } }
}]
TodoItem = { content: string, status: 'pending' | 'in_progress' | 'completed' }
```

- `running` 位 = 状态机最粗的"是否在工作";
- `todos` 快照(模型用 todo_write 维护)→ 气泡可显示"正在做:xxx"(`in_progress` 项),是 working 状态的内容增强,不是权威工作信号。

### 3.3 mux 帧类型(全量)

| 帧 type | 关键字段 | 语义 |
|---|---|---|
| `approval/requested` | approvalId, sessionId, toolName, reason | 待审批 → attention |
| `approval/resolved` | approvalId | 审批已决 |
| `question/requested` | 帧顶层 rpcId(=questionRpcId), sessionId, questions[] | 待回答 → attention |
| `question/resolved` | questionRpcId | 已回答 |
| `session/queue` | items[] | 队列长度(可显示"排队的会话") |
| `session/event` | event(见 §3.4) | 会话事件全量 |

### 3.4 session/event 的 SessionEventMap(全量,来自 `@deepseek-ai/dsh-session` 类型)

| event.type | 载荷 | 状态机用途 |
|---|---|---|
| `turn/start` | {turn} | 回合打开 → thinking 候选 |
| `turn/end` | {turn, reason} | **终态判定核心**(reason 见 §3.5) |
| `step/start` | {turn, step} | 一次模型调用开始(仍在生成,think) |
| `step/end` | {turn, step} | — |
| `user/message` | 用户消息 | — |
| `assistant/chunk` | 流式 token | —(过于高频,不消费) |
| `assistant/message` | {usage?} | —(可选:token 统计展示) |
| `tool/call` | {turn, step, callId, **name**, arguments} | **working 权威信号**(带工具名) |
| `tool/result` | {message, **error?**, meta?} | 工具结束;**error 字段 = 工具级失败** |
| `todo/write` | {todos} | 任务快照(内容增强) |
| `request/header` | header, reason | —(诊断用) |
| `request/context` | route | — |

### 3.5 TurnEndReason(六种,权威)

| kind | 语义 | 状态机映射 |
|---|---|---|
| `completed` | 正常完成 | done 窗口 + 庆祝音 |
| `error` | 回合出错 | **failed 窗口 + 失败音** |
| `max-tokens` | 超长截断 | failed(半失败) |
| `aborted` | 用户取消(含 cancel cause) | 中性,不触发情绪 |
| `interrupted` | 被打断 | 中性 |
| `blocked` | 被阻塞等待用户(批准/权限) | attention(等用户) |

### 3.6 host 帧

| 帧 type | 字段 | 用途 |
|---|---|---|
| `host/session-status` | {sessionId, running} | running 翻转即时性;重连后补轮询对齐基线 |

### 3.7 外部客户端信息边界(必须写进契约)

pet-rs 是**外部只读 HTTP/WS 客户端**,与进程内插件(kun-pet / whale-girl,挂宿主 cordis 总线)相比:

| 信号 | 进程内插件 | 外部客户端 | 替代方案 |
|---|---|---|---|
| 工具执行 | `tools/execute` 瀑布事件 | ❌ 不可达 | ✅ mux `tool/call`(更好,带工具名、全局可见) |
| 回合失败 | — | — | ✅ `turn/end reason=error/max-tokens` |
| 工具失败 | — | — | ✅ `tool/result.error` |
| 等待用户 | `approval/request` | ✅ mux approval/question 帧 | 同左 |
| **LLM 单次请求抖动** | `agent/request-error` | ❌ **不可达** | 无等价物;失败语义对齐到回合级 |
| 任务级终态 | `jobs.onJobDone`(completed/failed/killed) | ❌ 不可达 | turn 级近似;不冒充任务级记账 |

> 设计推论:宠物的"失败"表现定义为**回合失败**(turn/end error),不是任务失败;用户取消(aborted/interrupted)中性处理——对齐 whale-girl 的"killed 中性、请求错误不记账"语义。

## 4. 状态机(v1 推荐 8 态全粒度)

### 4.1 Mode 与优先级(高 → 低,表驱动)

```
1. offline   — 连接器健康探测失败(连不上 DSH)
2. attention — 存在未决 approval/question(等用户)
3. failed    — 最近有回合 failed 窗口(默认 4s,可配)
4. working   — 存在 in-flight tool(call 未 result)
5. thinking  — 存在活跃 turn 且无工具在跑(模型生成中)
6. done      — 会话刚完成,2 分钟窗口内(含"完成待查看"列表)
7. idle      — 兜底
```

瞬发用户互动(拖动 / 点击)由 UI 层覆盖,不进入状态机——窗口结束后重算底层派生状态(whale-girl 原则)。

### 4.2 事件 → 状态增量规则

| 事件 | 状态变化 |
|---|---|
| `turn/start` | session.turns++ → 进入 thinking 候选 |
| `step/start` | —(step 内模型生成,仍算 thinking) |
| `tool/call` | session.tools++;→ **working**(气泡显示工具名) |
| `tool/result`(无 error) | session.tools--;无工具后回 thinking |
| `tool/result`(有 error) | session.tools--;可触发短时"工具出错"情绪(可选) |
| `turn/end completed` | turns--;开 done 窗口;`done_sound_pending = true` |
| `turn/end error / max-tokens` | turns--;开 **failed 窗口**;`fail_sound_pending = true` |
| `turn/end aborted / interrupted` | turns--;中性 |
| `turn/end blocked` | turns--;视作等待用户(attention 候选) |
| `approval/requested` / `question/requested` | pending 表插入 → attention |
| `approval/resolved` / `question/resolved` | pending 表删除 |
| 轮询 `running: true→false` 且无工具无 turn | done 兜底(推送缺口防护;须排除刚见过 blocked 的会话) |
| `host/session-status` running 翻转 | 同上,即时版 |
| `Tick`(30s) | TTL 清理:approval/question 30 分钟、done 2 分钟 |

### 4.3 气泡文案与音效

| Mode | 气泡标题 | 气泡 body | 音效 |
|---|---|---|---|
| offline | 连不上 DSH 😢 | GUI 无响应,自动重试 | — |
| attention | 需要你确认 · N 项 | 逐条:会话「x」请求使用 y / 提问文本 | attention |
| failed | 呜…出错了 (._.) | 失败会话标题列表 | failed(新增素材) |
| working | 正在干活…(N 个会话) | 每行:「标题」+ 正在执行「工具名」;有 todos 则显示 in_progress 项 | — |
| thinking | 思考中… | 运行中会话标题列表 | — |
| done | 任务完成啦 🎉 | 完成待查看列表(2 分钟窗口) | done |
| idle | 休息中 💤 | 没有运行中的任务 | — |

多会话聚合:所有列表按会话聚合进一个可滚动气泡(沿用 pet-rs 的 bubble 设计)。

## 5. 连接器实现要点(DSH,v1)

- 复用 pet-rs 骨架:`rpc.rs`(session.list)+ `sse.rs`(WS 信封/重连)+ `tasks.rs`(轮询 2s + 双流 + tick 30s);
- **改动点**:帧解析上移——`apply_mux_frame` 从状态机挪进连接器,输出 §2 的语义事件;`session/event` 按 `event.type` 分发(注意字段名是 `type` 不是 `kind`,whale-girl 踩过此坑);
- 配置:单 endpoint(默认 `http://127.0.0.1:3080`),环境变量 `DSH_PET_URL` 优先,设置面板热切换(复用 pet-rs 的 endpoint 编辑 + IME);
- 素材/音效:沿用 `sprites.json`(palette + 80×58 字符画)+ `custom/` 覆盖目录;**新增 thinking / failed 两个 sprite 与 failed 音效**;
- 渲染:tiny-skia 合成 → softbuffer 呈现,透明置顶窗 + skip-taskbar(winit 三端适配直接复用)。

## 6. 测试策略

| 层 | 测试 |
|---|---|
| 状态机 | 单测:全事件组合 → 期望 Mode(纯逻辑无 I/O);含 TTL 过期、done 窗口、failed 窗口 |
| 连接器 | wiremock 集成:WS 正常关闭 / 部分帧 / 非 JSON / 无空格 `data:` / HTTP 5xx / 取消(pet-rs 已有 6 case,扩展 mux 全帧解析) |
| 渲染 | `--shot` 截图回归:8 状态 × 3 时间点 → PNG 像素 diff |
| 真机冒烟 | 连本机 DSH(127.0.0.1:3080)跑一个会话,人工核对状态切换时序 |

## 7. Hermes 接入(v2,已在本机实测可行)

### 7.1 通道选型(按可靠性排序)

| 通道 | 状态 | 说明 |
|---|---|---|
| **SQLite 直读(首选)** | ✅ 已验证 | `~/.hermes-web-ui/hermes-web-ui.db`(SQLite **WAL 模式**,`file:...?mode=ro` 只读打开并发安全,**零认证**) |
| HTTP API(增强) | ⚠️ 认证未解 | `http://127.0.0.1:8748`:GET /api/sessions、GET /v1/runs/{id}、GET /v1/runs/{id}/events(SSE)、POST /v1/runs/{id}/approval、GET /api/status、GET /api/discovery;要求 Bearer JWT,签名密钥在桌面壳内存(`~/.hermes-web-ui/.token` 与 `profiles/default/.model-run-token` 均实测 401)——**v2 先不依赖** |

### 7.2 SQLite → 语义事件映射(已实测)

| 表 / 字段 | 语义 | 映射 |
|---|---|---|
| `sessions.ended_at IS NULL` | 会话运行中 | ≈ DSH `running` 位 → working/thinking |
| `sessions.end_reason`(`complete` / `abort` 实测值) | 会话终态 | complete → done;abort → 中性;边沿检测 = ended_at 从 NULL → 值 |
| `sessions.last_active` / `messages` 行数增长 | 正在活动 | working 辅助信号 |
| `messages.tool_name='clarify'` 后无新消息 | 等用户回答 | ≈ DSH question/requested → attention |
| `workflow_runs.status` | 高级任务态(实测为空表) | 预留 |
| `session_usage.{model, api_calls, tokens}` | 消耗统计 | 可选展示(本机实测模型 deepseek-v4-flash) |

**坑(已踩)**:
- 时间单位不一致:`sessions`/`messages` 的时间戳是**秒**,`session_usage.created_at` 是**毫秒**;
- Hermes 会话**没有 running 位**,工作态从 `ended_at IS NULL` + `last_active` 新鲜度推断;
- hermes 的 Hermes Studio 会话与 CLI 会话共用一张表(`source` 字段区分),按需过滤。

### 7.3 多后端架构

- `Config`: `sources: Vec<{ kind: "dsh" | "hermes", endpoint | db_path, token? }>`;
- 每个源一个 `AgentSource` 实例,事件合并进同一 `PetState`(或按源拆实例再聚合);
- `Snapshot` 增加 `source` 维度;气泡按源分节(DSH 会话一段 / Hermes 会话一段);
- `offline` 按源独立判定;两源同时 attention 时合并展示。

## 8. 里程碑

| 阶段 | 内容 | 验收 |
|---|---|---|
| M1 骨架 | 仓库初始化,复用 pet-rs 三 crate;8 态状态机 + 单测 | `cargo test` 全绿 |
| M2 DSH 全粒度连接器 | mux 全帧解析 + 轮询 + host WS + TTL | 真机冒烟:thinking/working/failed 切换正确 |
| M3 UI 升级 | thinking/failed 素材、气泡文案、失败音、设置面板 | `--shot` 8 状态截图通过 |
| M4 可靠性 | 重连补拉、SSE 降级、截图回归、文档 | 断网/重启 DSH 场景无卡死 |
| M5 Hermes v2 | `hermes-source`(SQLite 轮询)+ 多后端聚合 | 双后端同屏正确 |
| M6 增强 | Hermes HTTP/SSE 通道(认证可解后);token 统计展示 | 推送降延迟生效 |

## 9. 风险与开放问题

1. **LLM 请求级错误外部不可见**:失败表现对齐回合级(error/max-tokens),不冒充任务级;若需任务级,需 DSH 侧增加对外事件(上游特性)。
2. **窗口时长需实测调参**:参考 kun-pet(celebrate 4.8s / failed 2.6s)与 whale-girl(error 4s + disappointed 6s / celebrate 6s)。
3. **多后端优先级语义**:两个源同时 attention/failed 时的合并展示规则需定义(建议:按源分区,标题计数合并)。
4. **素材版权**:自绘或可商用素材,勿复用 kun-pet 的坤坤素材(版权声明限制)。
5. **Hermes 漂移风险**:桌面壳升级可能改 token 机制 / DB schema——SQLite 读取需做表结构探测与版本检测,失败时降级 offline。
6. **性能**:mux 流的 `assistant/chunk` 高频帧必须过滤(只在连接器层做类型白名单),避免状态机热路径抖动。

## 10. ComfyUI 接入(已落地)

### 10.1 通道选型

| 通道 | 用途 |
|---|---|
| `GET /queue`(轮询基线) | `queue_running` 当前执行 + `queue_pending` 排队数 → 语义事件 |
| `GET /history/{prompt_id}`(兜底) | 轮询发现 prompt 离开 running 队列后查终态:`status.status_str` success/error |
| `ws://host:8188/ws`(推送增强) | `execution_start` / `executing`(当前节点+node_type) / `progress`(value/max) / `execution_success` / `execution_error` / `execution_interrupted` / `status`(queue_remaining) |

全部无需认证、无需装插件,只读身份与 pet-rs 一致。

### 10.2 语义事件映射

- `prompt_id` → `session_id`;一次出图 = 一个 turn(`turn=1`,连接器内 `running`/`finished` 表去重,TurnStarted/TurnEnded 各发一次);
- **Working 由轮询基线直接驱动**:prompt 一进 `queue_running` 就从 prompt 图里挑代表性节点(优先 KSampler → VAEDecode → 任意节点,兜底"执行中")发 `ToolStarted` —— ComfyUI 是确定性图执行,没有 thinking 阶段,不让宠物空转思考态;WS 节点帧(若可用)再升级成真实执行节点;
- WS 帧:`executing` 节点 → `ToolStarted/ToolEnded`(name=解析出的 class_type,图未到时先 `#<id>` 兜底,轮询抓到图后升级);`progress` 节流后以 `arguments="12/20"` 刷新气泡;
- 终态:`execution_success` → `TurnEnded Completed`;`execution_error` → `Error`;`execution_interrupted` → `Interrupted`(中性,同 aborted);WS 缺失时由 `/history` 兜底(status_str=error → Error;messages 含 execution_interrupted → Interrupted;否则 Completed);
- `queue_pending`/`queue_remaining` → `QueueChanged` 事件,Snapshot.queue_len 跨源求和显示;
- **attention 态天然缺失**(ComfyUI 无审批/提问概念),不做伪造。

> **消息路由关键事实(已对照 aki-v3.2 源码核实)**:`execution_start` / `executing` / `progress` / `execution_success` / `execution_error` / `execution_cached` 都是 `send_sync(..., server.client_id)` **只发给前端浏览器那个 socket**(`add_message` broadcast=False);浏览器开着时宠物收不到这些帧,只有 `status`(queue_remaining)是广播。所以节点级 Working、进度、成功/失败/中断都不能依赖 WS,轮询(/queue + /history)才是唯一可靠信号,WS 只是锦上添花。

### 10.3 配置

```json
"comfyui": { "enabled": true, "url": "http://127.0.0.1:8188", "poll_ms": 2000, "ws": true }
```

轮询是可靠性基线,WS 只做延迟增强;WS 断线 3s 退避重连,重连后由轮询线程负责拉齐(与双流设计一致)。测试覆盖:`/queue`/history 解析、executing 节点切换与 progress 节流、终态去重、状态机 ComfyUI 全流程、queue_len 跨源聚合。

> 实测格式注意(已对照 ComfyUI-aki-v3.2 server.py 核实):
> - `/queue` 项是**数组** `[number, prompt_id, prompt, extra_data, outputs_to_execute]`,不是对象;
> - ws `executing` 帧**只有数字 node id**、没有 node_type —— 节点类名从 `/queue` 里 prompt 图的 `id → class_type` 映射解析,轮询未赶上时先用 `#<id>` 兜底、随后升级;
> - `execution_error` / `execution_interrupted` 帧才带 node_type / exception 字段。
