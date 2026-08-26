# Hannis Lua 脚本接入指南

> **读者**：想给宠物接入"另一个程序"的用户（无需懂 Rust、无需编译）。
> **前提**：会写 Lua（内置 Lua 5.4 解释器已打进 Hannis.exe，**用户无需安装任何运行时**）。
>
> **内置来源全部脚本化**：DSH、Hermes、MAA、ComfyUI 都已由内置 Rust 连接器迁移为
> Lua 脚本（`scripts/dsh.lua`、`scripts/hermes.lua`、`scripts/maa.lua`、
> `scripts/comfyui.lua`，随程序发布，行为与原内置版一致）——它们本身就是这份指南
> 最好的完整参考实现。新接入口只需照着写一个 `.lua` 文件。

## 1. 三步接入

1. 写一个 `.lua` 脚本（可参考 `scripts/tail_log.lua`、`scripts/process_watch.lua`）
2. 在 `config.json` 的 `scripts` 数组里注册它
3. 重启 Hannis —— 气泡出现 "From <name>"，状态随之切换

```jsonc
"scripts": [
  {
    "name": "MyGame",              // 气泡里显示的名字
    "file": "scripts/mygame.lua",  // 相对 exe 目录或绝对路径
    "poll_ms": 1000,               // 提示值,脚本可用 pet.config().poll_ms 读取
    "sandbox": false,              // true = 禁用文件/进程/网络访问(只保留纯计算 + pet API)
    "enabled": true,               // false = 不启动(托盘/设置面板可切换)
    "args": { "log": "D:\\MyGame\\game.log" }  // 任意 JSON,脚本通过 pet.config().args 读取
  }
]
```

## 2. 脚本结构

脚本在**独立线程**里运行，自己负责循环（像连接器一样轮询）：

```lua
local cfg = pet.config()          -- { name=…, poll_ms=…, args=<你的 args> }
local args = cfg.args or {}

pet.health(true)                  -- 标记本源健康(可随时改)
pet.log("info", "hello")          -- 写入 hannis.log,便于调试

while true do
  -- ...观察你的程序,把变化翻译成 pet 调用...
  pet.wait(cfg.poll_ms or 1000)   -- 必须用 pet.wait 睡眠(可被退出打断)
end
```

**脚本返回(不循环)或出错 = 该源下线(health=false)**，宠物和其他源不受影响。

## 3. pet.* API 一览

| 调用 | 作用 | 宠物表现 |
|---|---|---|
| `pet.health(ok)` | 标记本源健康/不健康 | 全部源不健康 → offline |
| `pet.poll({ {session_id=…, running=…, title=…, todos=…}, … })` | 基线快照 | 会话合并/回收 |
| `pet.session_started(id, turn)` | 会话开始 | thinking |
| `pet.session_ended(id, turn, reason)` | 会话结束;reason ∈ `completed`/`error`/`max_tokens`/`aborted`/`interrupted`/`blocked` | done / fail / attention |
| `pet.session_status(id, running)` | running 翻转(host/session-status、会话结束) | 状态机 running 标记 |
| `pet.tool_started(id, name, args?)` | 开始干活 | working |
| `pet.tool_ended(id, name, error?)` | 干活结束 | 回退 thinking |
| `pet.live_text(id, {reasoning=…, text=…, tool_name=…})` | 实时文字 | 气泡流式显示 |
| `pet.question(id, session_id, text)` / `pet.answer(id)` | 向用户提问/回答 | attention |
| `pet.approval_requested(id, session_id, tool)` / `pet.approval_resolved(id)` | 审批请求/解决(DSH events.mux) | attention |
| `pet.pending_sync()` | WS 重连后清空该源残留的审批/提问 | 防宠物卡在 attention |
| `pet.todo(id, { {content=…, status=…}, … })` | 待办列表 | 气泡任务名兜底 |
| `pet.user_message(id, text)` | 用户消息 | 气泡任务名兜底 |
| `pet.queue(n)` | 队列深度 | 气泡"队列中还有 N 个任务" |
| `pet.log(level, msg)` | 写日志 | 进 `hannis.log`(GUI 运行)或 stderr(headless) |
| `pet.wait(ms)` | 可中断睡眠 | **必须用它做轮询间隔,否则程序退不掉** |
| `pet.http(url, timeout_ms?)` | HTTP GET → `status, body` | 访问任意 HTTP 接口(沙箱禁用) |
| `pet.http_post(url, body, timeout_ms?)` | HTTP POST(JSON body)→ `status, body` | DSH `session.list/history` 这类接口(沙箱禁用) |
| `pet.ws(url, path, timeout_ms?)` → handle | 打开 WebSocket(读超时=timeout) | 推送型接口,如 DSH `/api/events.mux`(沙箱禁用) |
| `pet.ws_read(handle)` | 读一帧 | **三态返回值,见下**(沙箱禁用) |
| `pet.ws_close(handle)` | 关闭 WS 连接 | — |
| `pet.sqlite(path, sql, params?)` → 行数组(每行 {列名=值}) | 只读 SQLite 查询一次一开 | Hermes 这类 SQLite 后端(沙箱禁用;BLOB 转十六进制) |
| `pet.config()` | 返回 `{ name, poll_ms, args }` | — |

**`pet.ws_read` 返回值（重要，三态）**：

| 返回 | 含义 | 脚本应做什么 |
|---|---|---|
| `string` | 收到一帧文本/二进制 | 解析并处理 |
| `nil` | **读超时**（连接还活着，只是这个时间窗内没新帧） | 继续循环，别当断线 |
| `false` | **连接已关闭/出错** | `pet.ws_close(handle)` + 延迟重连 |

> 只有 `false` 才代表连接死了。旧版文档写"关闭/超时都返回 nil"，那会导致脚本永远
> 检测不到 WS 掉线、审批/提问通道断了也不知道——已改为三态。

**约定**：
- `session_id` 会被自动加 `script-<序号>-` 前缀(避免与其它源串台)，你只管用自己的 id
- `question`/`approval` 的 id 也会加前缀，保证全局唯一
- 每个 `session_started` 对应一个 `session_ended`;`tool_started` 对应 `tool_ended`(否则状态机一直停在 working)
- 会话 id 按"任务"分:不同任务用不同 id 即可看到多条会话轮流显示
- `sandbox: true` 时 `http`/`http_post`/`ws`/`ws_read`/`ws_close`/`sqlite` 全部报错，且 `os`/`io` 不存在

## 4. 示例：监控任意日志(通用版)

`scripts/tail_log.lua` 已发布,`args` 里给关键词即可:

```jsonc
{
  "name": "MAA",
  "file": "scripts/tail_log.lua",
  "poll_ms": 1000,
  "args": {
    "log": "D:\\MeoAssistantArknights\\debug\\gui.log",
    "session": "maa",
    "connect": "正在连接模拟器",
    "work": "开始任务",
    "done": "Idle: false to true (called from ProcTaskChainMsg)",
    "fail": "已停止"
  }
}
```

## 4.5 随程序发布的完整示例

| 脚本 | 用途 | 亮点（照抄哪段） |
|---|---|---|
| `scripts/dsh.lua` | DSH 连接器 | **HTTP 轮询 + WebSocket + JSON 解码 + seq 增量**——最全的参考 |
| `scripts/hermes.lua` | Hermes 连接器 | **SQLite 轮询 + 增量 diff + clarify 提问检测 + WAL 副本回退** |
| `scripts/maa.lua` | MAA 日志监控 | 文件 tail + 截断检测 + 启动恢复 + attention 自动消除 |
| `scripts/comfyui.lua` | ComfyUI 出图队列 | `pet.http` + 内置最小 JSON 解码器 |
| `scripts/tail_log.lua` | 通用日志关键词监控 | 最简版,改关键词即可 |
| `scripts/process_watch.lua` | 进程监控 | `io.popen` 范例(沙箱模式不可用) |

## 5. 沙箱

`sandbox: true` 时脚本全局环境里**没有** `os`/`io`/`package`/`require`/`dofile`/`loadfile`/`load`/`debug`
——只能纯计算 + 调 pet API(不能读文件、不能开进程、不能联网、不能加载其他代码)。
默认 `false`(你的机器、你的脚本,完整权限)。

## 6. 调试

- `pet.log(level, msg)` → 追加到 exe 同目录 `hannis.log`
- 脚本编译/运行错误也会写进 `hannis.log`(形如 `[lua:名字] script error: …`)
- WSL 下开发:headless 模式直接跑,错误在 stderr 可见:
  `cd app && cargo run`(读取 config.json 的 scripts 并执行)
- 脚本一改就重启 Hannis 生效(暂无热重载)

## 6.5 内置源已全部脚本化

**DSH、Hermes、MAA、ComfyUI 全部由内置 Rust 连接器迁移为 Lua 脚本**，行为与原内置
版一致。它们的线格式、状态机、坑都在 §8 线格式参考 + 脚本注释里，**以脚本为唯一
规范**（不再有 Rust 源码可查）。维护/扩展这四类接入口 = 改脚本 + 改 `config.json`
的 `args`，不需要编译。

## 6.6 设置面板参数声明（`--[hannis:set]`）

接入口设置窗口（托盘 → "接入口设置…"）会自动扫描脚本里的参数声明并生成编辑框：

```lua
--[hannis:set] 键名 | 显示标签 | 默认值
--[hannis:set] url | ComfyUI 地址(IP及端口) | http://127.0.0.1:8188
--[hannis:set] timeout_ms | 请求超时(ms) | 5000
```

- 格式：一行一条，`--[hannis:set]` 后跟 `键 | 标签 | 默认值`，`|` 分隔，均可省略
- 声明只用于生成 UI；脚本实际取值仍走 `args.键名 or 默认值`
- 保存后写回 `config.json` 的 `args` 并重启该脚本

## 7. 常见问题

| 现象 | 原因 |
|---|---|
| 气泡没有 From 我的名字 | `scripts` 里 `name` 为空或文件路径不对 |
| 脚本完全没反应 | 看 `hannis.log` 里 `[lua:<名字>]` 开头的行:`started` = 已加载;`error:` = 抓取失败;完全没有该前缀 = `scripts` 数组里没有这个条目 |
| 一直 working 不结束 | 忘了调 `tool_ended` / `session_ended` |
| 宠物显示 offline | 脚本 `pet.health(false)`;或所有源都断了 |
| 脚本卡死宠物 | 不可能——脚本在独立线程;但死循环会烧 CPU,用 `pet.wait` 做间隔 |
| 退出时程序卡住 | 脚本里用了系统睡眠或死循环没调 `pet.wait` |
| 宠物一直 attention 出不来 | 提问发了但 `pet.answer` 的 id 对不上;或 WS 掉线后没重连(见 `ws_read` 三态 + `pending_sync`) |
| 连接器好像没反应但 HTTP 是通的 | 检查 `args` 里 `poll_ms`/`history_ms` 是否被设置界面覆盖成了小值 |

## 8. Lua 常见坑（写脚本前必读）

这节是迁移 DSH/Hermes 时踩过的坑，写任何新脚本都可能遇到：

1. **`t.function` 是语法错误**——`function` 是 Lua 保留字。访问 JSON 里的
   `"function"` 字段必须 `t["function"]`（例如 Hermes 的 `tool_calls`）。
2. **`local f = function() ... f() ... end` 自引用会崩**——`local f =` 的声明在
   函数体结束才生效，函数体内 `f` 是全局。要用 `local function f()`。
3. **`pcall` 多返回值顺序**：`local ok, a, b = pcall(f)`，`a`/`b` 是 `f` 的返回值
   （不是错误消息）。曾把函数第二个返回值当错误消息用、把返回的表当集合用而踩坑。
4. **JSON `null` → Lua `nil`**；SQLite NULL → `nil`。`t.k` 取到 `nil` 和"字段不存在"
   无法区分，判空用 `x == nil or x == ""`。
5. **`string.sub` 按字节截断**——中文会截出半个字符。需要按"字符数"截断时参考
   `hermes.lua` 的 `truncate_chars`（按 UTF-8 首字节判断码点长度）。
6. **大响应是纯 Lua 的软肋**：DSH 超大会话的 `session.history` 响应可达 **几十 MB**，
   内置 JSON 解码器解析 30MB 约 7 秒（Rust serde_json 约 50ms）。缓解：
   - 首轮基线窗口可调小（`dsh.lua` 的 `args.baseline_msgs`，默认 200）
   - 增量轮询用小窗口（`maxMessages=2`，正常只有几十~几千事件，解析 <0.1s）
   - **永远不要 `pet.log` 整个大响应**（会写爆 hannis.log）
7. **事件必须成对**：`tool_started` 没 `tool_ended`、`session_started` 没
   `session_ended` → 状态机停在 working/thinking 不回来。
8. **`pet.wait` 是唯一的睡眠方式**——脚本线程退出依赖它的可中断切片；用系统
   睡眠/死循环会让程序退不掉。

## 9. 线格式参考（DSH / Hermes）

> 这节是 DSH/Hermes 脚本解析的数据格式全集。**脚本即规范**——改脚本前先看这里和
> 脚本本身的注释。`dsh.lua` / `hermes.lua` 是完整参考实现。

### 9.1 DSH：HTTP envelope（`session.list` / `session.history`）

请求（`pet.http_post`，`Content-Type: application/json`）：

```json
{"type":"client-request","rpcId":"pet-lua-1","method":"session.list","payload":{}}
{"type":"client-request","rpcId":"pet-lua-2","method":"session.history",
 "payload":{"sessionId":"session-xxx","maxMessages":2}}
```

响应：

```json
{"type":"server-response","rpcId":"pet-lua-1","result":{"ok":true,"value":{...}}}
```

- **`session.list`** → `result.value.items[]`，每项：
  `sessionId`、`running`（bool）、`projections.values.title`（可能没有）、
  `projections.values.todos[]`（`{content, status}`，可能没有）。
- **`session.history`** → `result.value.events[]`，每项 `{"event":{"seq":N,"type":T,"data":{...}}}`。
  `seq` 全局单调递增，**按 seq 去重**（只处理 > 上次 `last_seq` 的）。
- **基线 vs 增量**：会话首次轮询用大窗口（`maxMessages=200`，重建当前状态：开着的
  回合/工具/todo/最近用户消息，**不重放实时文字**）；之后用 `maxMessages=2` 小窗口，
  只发增量。`maxMessages` 限制的是"最近 N 条消息的事件"，事件数可能仍很大。
- 会话结束的 `turn/end` 有 `data.reason.kind`，映射到 `pet.session_ended` 的 reason：
  `completed→completed`、`error→error`、`max-tokens→max_tokens`、`aborted→aborted`、
  `interrupted→interrupted`、`blocked→blocked`（注意连字符）。

**`session.history` 事件类型表**：

| type | data 要点 | 翻译成 |
|---|---|---|
| `turn/start` | `turn` | `pet.session_started` |
| `turn/end` | `turn`, `reason.kind` | `pet.session_ended` + flush live |
| `tool/call` | `callId`, `name`, `arguments`(JSON 字符串) | `pet.tool_started` + flush live |
| `tool/result` | `message.source.callId`, 有 `error` 字段=失败 | `pet.tool_ended`(按 callId 配对) |
| `todo/write` | `todos[]` | `pet.todo` |
| `assistant/chunk` | `chunk.type` ∈ `reasoning-delta`/`text-delta`(其它如 `usage`/`finish` 忽略), `chunk.text` | 累积后 `pet.live_text` |
| `user/message` | `message.content[]` 里 `type=="text"` 的 `text` 拼接 | `pet.user_message` + flush live |
| `assistant/message` / `step/end` | — | flush live（回合阶段切换点） |

### 9.2 DSH：events.mux / events.host（WebSocket 推送）

WS 帧可能一帧带多个 JSON 对象（用花括号配平拆分）。`handle_mux` 读 `payload.type`：

| payload.type | 字段 | 翻译成 |
|---|---|---|
| `session/jobs` | `jobs[]` = `{id, status}`;`status` ∈ `running`/`queued`/`completed`/`failed`… | 变 running/queued → `pet.tool_started(id,"job:<id>")`;变其它 → `pet.tool_ended` |
| `approval/requested` | `approvalId`, `toolName` | `pet.approval_requested` |
| `approval/resolved` | `approvalId` | `pet.approval_resolved` |
| `question/requested` | **envelope 的 `rpcId`** + `questions[]` = `{id, question}` | `pet.question(rpcId.."\0"..q.id, …)` |
| `question/resolved` | `questionRpcId` | `pet.answer(questionRpcId)` |

**关键坑（必读）**：`question/resolved` 帧**只带 envelope 的 rpcId，不带每个 question
的 id**。所以发提问时必须把 id 拼成 `rpcId .. "\0" .. itemId`（NUL 分隔），解决时用
`pet.answer(rpcId)`——状态机按前缀 `rpcId\0` 清除整批。否则提问永远清不掉，
宠物卡在 attention。这是迁移时最容易写错的一处。

host 帧（`handle_host`）：`host/session-status` → `{sessionId, running}` →
`pet.session_status`。

**WS 重连**：连上 `/api/events.mux` 后**立即调 `pet.pending_sync()`**——服务端会在
连接后重放当前仍 pending 的审批/提问，先清掉本地残留，否则断线期间已解决的请求
会把宠物卡在 attention。

### 9.3 Hermes：SQLite 轮询

只读数据库（`pet.sqlite`，`SQLITE_OPEN_READ_ONLY`）。表结构（实测）：

```
sessions(id TEXT PRIMARY KEY, title, source, agent,
         started_at, ended_at, end_reason, last_active)   -- 时间戳单位:秒
messages(id INTEGER PK AUTOINCREMENT, session_id, role, content, display_content,
         tool_name, timestamp, reasoning, reasoning_content,
         tool_calls, finish_reason, tool_call_id)
```

- **running 判定**：`ended_at IS NULL` **且** `last_active` 在 10 分钟内（否则视为
  已结束/僵尸）。查询窗口 `ended_at IS NULL OR last_active > now-2h`。
- **消息取最新一行**：`WHERE session_id=? ORDER BY id DESC LIMIT 1`。Hermes 生成时
  **同一行内容增长**（思考先行、正文随后），`id` 不变 → 用 `strip_prefix` 算增量，
  发 `pet.live_text` 的 delta；新行 → 整条发。
- `text = display_content or content`；`reasoning = reasoning_content or reasoning`。
- **工具**：`role="tool"` 的行 → `pet.tool_started/ended`，`content` 是 JSON，
  取 `output` 字段作预览；`tool_name=="clarify"` 的答案行**不算干活**（跳过）。
- **clarify 提问**（→ attention）：assistant 行 `finish_reason` 含 `tool_calls` 且
  `tool_calls` JSON 数组里有 `{"function":{"name":"clarify",...}}` → `pet.question`。
  用户回答后 Hermes 写一行 `role="tool", tool_name="clarify", tool_call_id=<callId>` →
  `pet.answer`。会话结束也要清掉未答问题。
- **WAL 副本回退**：某些挂载点直接只读打开失败（WAL/shm 权限）→ 把 `db`+`-wal`+`-shm`
  复制到临时目录再打开（`hermes.lua` 的 `open_db` 已实现；沙箱模式跳过）。

### 9.4 环境变量（由脚本读取）

| 变量 | 作用 | 在哪个脚本 |
|---|---|---|
| `DSH_PET_URL` | 覆盖 DSH 地址（优先于 `args.url`） | `dsh.lua` |
| `HERMES_WEB_UI_HOME` | 覆盖 Hermes 数据目录（优先于 `args.db_path`） | `hermes.lua` |

## 10. 常见问题（线格式/调试补充）

| 现象 | 原因 |
|---|---|
| `session.history ok=false` | 请求 payload 里 `sessionId` 不是字符串（常见：把数组下标当 id 传了）；或会话不存在 |
| 工具名/会话 id 带 `job:`/`script-` 前缀 | 正常，那是命名空间前缀 |
| 大响应后脚本卡几秒 | 纯 Lua JSON 解析大响应较慢；把 `baseline_msgs` 调小 |
| 审批/提问偶尔不出现 | WS 掉线未重连——检查是否对 `ws_read` 的 `false` 做了重连、连上后是否 `pending_sync` |
