# Hannis 接入口指南 — Lua 脚本接入 + 宿主架构契约

> **读者**：想给宠物接入"另一个程序"的用户（无需懂 Rust、无需编译）。
> **前提**：会写 Lua（内置 Lua 5.4 解释器已打进 Hannis.exe，**用户无需安装任何运行时**）。
>
> **本文分两部分**：
> - **第一部分（§1-10）写给你**：三步接入、`pet.*` API、线格式参考、Lua 坑。
> - **第二部分（§11-16）写给 AI/开发者**：宿主内部架构与 `StateEvent` 事件契约、
>   Mode 推导、何时需要改宿主（原 `adding-connectors.md` 已并入此处并按当前
>   代码修正——新接入口**不需要**读它）。
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
    "debug": false,                // true = 把每条 pet.* 调用写进 hannis.log(排查用)
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
| `pet.config()` | 返回 `{ name, poll_ms, args, _dir }`（`_dir`=脚本所在目录,可用来定位同目录资源） | — |

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

**宿主侧容错（脚本信息有缺漏时的兜底）**：
- `pet.poll`/`pet.todo` 对缺字段宽容:条目缺 `running` 按 false、todo 缺 `status` 按 pending、
  缺 `session_id` 的条目跳过(数字 id 转字符串)——坏条目降级处理，脚本不会被类型错误炸掉
- 重复的 `session_started(id, 同一 turn 号)` 只记一次账(轮询重放/补发安全);
  `session_ended` 的 turn 号与开着的回合对不上时不会误关其它回合;
  回合收尾后同号回合重新开始是合法的新周期
- 源下线(`pet.health(false)` 或脚本退出)时,宿主清掉该源的会话运行账目
  (running/turns/tools)并清掉该源未决的审批/提问——陈旧的"运行中"与永远
  等不到 resolve 的请求不会把宠物钉死
- 会话从 `pet.poll` 快照里消失超过 60 秒,即便账目没关也会被回收(快照是
  全量基线,消失即"会话没了")

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
- **脚本启动时会回显 args**(`[lua:名字] started (N bytes) args={...}`)——配置传错
  (url 拼错、路径不对)一眼可见
- **`"debug": true` 时把每条 `pet.*` 调用写进日志**:`[lua:名字] ev: session_started
  script-0-xxx turn=1` / `tool_started …` / `live_text … r+3 t+2` / `poll N items` /
  `health true`…——确认"调用发出去了没/发成什么样",定位宠物卡状态最有效
- WSL 下开发:headless 模式直接跑,错误在 stderr 可见:
  `cd app && cargo run`(读取 config.json 的 scripts 并执行)
- 脚本一改就重启 Hannis 生效(暂无热重载)

## 6.5 内置源已全部脚本化

**DSH、Hermes、MAA、ComfyUI 全部由内置 Rust 连接器迁移为 Lua 脚本**，行为与原内置
版一致。它们的线格式、状态机、坑都在 §9 线格式参考 + 脚本注释里，**以脚本为唯一
规范**（不再有 Rust 源码可查）。维护/扩展这四类接入口 = 改脚本 + 改 `config.json`
的 `args`，不需要编译。这四个脚本同时也在**编译内出厂默认**里注册（没有
config.json 时自动生成的配置同样包含它们;目标程序不存在时对应源标记不健康,
不影响其它源）。

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
   `session_ended` → 状态机停在 working/thinking 不回来。(宿主对重复的
   `session_started` 同号重放已做幂等,但工具事件的配对仍需脚本自己保证。)
8. **`pet.wait` 是唯一的睡眠方式**——脚本线程退出依赖它的可中断切片；用系统
   睡眠/死循环会让程序退不掉。
9. **别让 HTTP/解码饿死 WS 读**——脚本是单线程的：一个 `pet.http_post` 大响应
   （纯 Lua 解码 ~0.3s/MB）会阻塞整条循环,push 帧在内核缓冲区里排队。dsh.lua
   曾因此让提问进入 attention 延迟十几秒（webui 秒到、宠物十几秒后才动）。
   对策:每次 HTTP 调用/解码之后都读一次 WS（`dsh.lua` v2.1 的 `pump_ws`）,
   并把读超时设小（100ms 量级）,让"无帧空转"足够便宜。

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

**push 延迟（实测教训）**：提问/审批只走 mux 推送（`session.history` 里拿不到
envelope rpcId,做不了配对 key）,而 `session.history` 的大响应解码会占死单线程循环
——**每个 HTTP 调用后都要读一次 WS**（`dsh.lua` v2.1 的 `pump_ws`,读超时默认
100ms）,否则 push 帧实测会延迟十几秒才被处理。mux 链路半开时服务端不报错、脚本
也收不到帧,`dsh.lua` 用周期性自愈重连兜底（有会话运行时 15s,空闲 60s;重连即重放
pending）。

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

---

# 第二部分：宿主内部架构与 StateEvent 契约

> **读者**：AI/开发者。**何时读**：只有两类情况需要下到这一层——① 给 `pet.*`
> 增加新 API；② 排查"状态机为什么是这个行为"。新接入口请回到第一部分写 Lua。
>
> 本部分由原 `adding-connectors.md` 整合而来,所有契约描述以当前
> `app/src/state.rs` / `app/src/connectors/lua.rs` 为准（原文中已失效的
> "Rust 连接器扩展路径"与旧行为描述已删除/修正）。

## 11. 架构总览（30 秒版）

```
                 ┌─────────────────────────────────────────────┐
 上游消息源  ───► │ scripts/*.lua（每脚本独立线程 + 独立 Lua state）│
 (HTTP/WS/SQLite)│  轮询/订阅 → pet.* 调用（= StateEvent）       │
  /日志文件       └───────────────┬─────────────────────────────┘
                                  │ connectors/lua.rs 翻译
                                  ▼ mpsc channel (Sender<StateEvent>)
                 ┌─────────────────────────────────────────────┐
                 │ PetState（纯状态机,无 I/O,state.rs）          │
                 │  apply(ev) → sessions/approvals/questions…   │
                 │  snapshot() → Snapshot + Mode                │
                 └───────────────┬─────────────────────────────┘
                                 ▼
                 ┌─────────────────────────────────────────────┐
                 │ GUI（gui/mod.rs,Windows）                    │
                 │  Mode → 动画资产 / 气泡内容(bubble_text)      │
                 └─────────────────────────────────────────────┘
```

- **脚本 = 生产者**：只做一件事——把上游数据翻译成 `pet.*` 调用。**绝不碰 UI、
  绝不做状态决策**（Mode 由状态机推导）。
- **状态机 = 消费者**：`apply(ev)` 累积账目;`snapshot()` 产出快照供 GUI 每帧取用。
- `Source::Script(id)` 是唯一来源变体,id = `scripts` 数组下标;`label()` 显示名
  来自 `scripts[].name`,未注册显示 "Script N"。
- **会话生命周期**：`session_started → (live_text/tool_started/tool_ended)* →
  session_ended`。一个"回合"（turn）≈ 一次模型回答;工具是回合内的工作阶段。
- **会话表以 `session_id` 全局为键**：脚本的 id 会被宿主自动加 `script-<序号>-`
  前缀,无需自己处理命名空间。

**文件地图**：

| 文件 | 职责 |
|---|---|
| `scripts/*.lua` | **各来源连接器（主扩展方式,第一部分）** |
| `app/src/connectors/lua.rs` | Lua 脚本宿主：每脚本一线程 + `pet.*` API 翻译为 StateEvent |
| `app/src/state.rs` | `Source` / `Mode` / `StateEvent` / `PetState` / `Snapshot`（契约核心） |
| `app/src/connectors/mod.rs` | `send()` / `sleep_interruptible()` / `stop_flag()` 公共助手 |
| `app/src/http.rs` | 零依赖 HTTP/1.1 + WebSocket + SSE 客户端（`pet.http/ws` 复用） |
| `app/src/config.rs` | 配置段结构 + 默认值 |
| `app/src/bubble_text.rs` | 气泡文案（标题行 / "From X" / 内容） |
| `app/src/gui/mod.rs` | 脚本注册点与渲染 |
| `app/src/headless.rs` | 无 GUI 调试入口（同一 `cfg.scripts` 数组,行为与 GUI 一致） |

## 12. StateEvent 契约

| 事件 | 字段 | 何时发 | 状态机的处理（当前行为） |
|---|---|---|---|
| `Poll` | `source, items: Vec<SessionItem>, ok` | 轮询式来源的**全量基线快照** | `ok=false` 整体忽略（不得借失败快照冲账）。`ok=true`：合并 title/todos、running 统一落账；**消失回收**：快照里不见且距上次出现 ≥60s 的会话直接回收（即便账目未关——快照是全量基线,消失即"会话没了"）;其余不见的会话按"不活跃且过 done 窗"回收 |
| `SessionStatus` | `source, session_id, running` | 推送式 running 翻转（轮询式可不发,Poll 已含） | 与 Poll 的 running 落账同一套逻辑（见下） |
| `TurnStarted` | `source, session_id, turn: u64` | 会话开始新回合 | `turns += 1; running = true; waiting_user = false`。**幂等**：turn>0 时同号重放只记一次账（按会话内打开回合集合判重）;回合收尾后同号再来是合法新周期 |
| `TurnEnded` | `source, session_id, turn, reason: TurnEndReason` | 回合结束。`reason ∈ {Completed, Error, MaxTokens, Aborted, Interrupted, Blocked}` | turn>0 且回合开着才 `turns -= 1`（重复/乱序 end 不会误关其它回合;turn==0 保持旧的饱和递减）;`running=false`;**清空该回合的工具账目**（丢 `tool/result` 帧不泄漏 Working）;记录 `last_end`;`Blocked` → `waiting_user`;`turns==0` 清空 live 文本;`Completed`/失败置声音沿 |
| `ToolStarted` | `source, session_id, name, arguments?` | 工具调用开始 | 记入 tools（→ **Working**）;记录开始时间（气泡"谁在干活显示谁"的判据）;**清空该会话 live 文本**（新工作阶段） |
| `ToolEnded` | `source, session_id, name, error` | 工具调用结束 | 移出 tools |
| `TodoSnapshot` | `source, session_id, todos` | todo 列表变化 | 覆盖保存（done/fail 气泡的任务名兜底） |
| `LiveText` | `source, session_id, reasoning?/text?/tool_name?` | 模型实时输出,**增量追加** | `Some` 字段追加进对应缓冲（各字段 8000 字符封顶）;`tool_name` 为 Some 时替换。**不要整段重发**——会重复累积 |
| `UserMessage` | `source, session_id, text` | 用户消息 | trim 后截 120 存 `last_user_text`（任务名兜底：标题→最近用户消息→第一个非 pending todo） |
| `ApprovalRequested` / `ApprovalResolved` | `source, id, session_id, tool` / `source, id` | 审批请求/结果 | 未决 → **Attention**;TTL 30 分钟（Tick 兜底）;`id` 全局唯一（脚本自动加前缀） |
| `QuestionRequested` / `QuestionResolved` | `source, id, session_id, text` / `source, id` | 向用户提问/回答 | 同上;Resolved 按 `id\0` 前缀清整批（DSH 一个 rpcId 可挂多个 question） |
| `PendingSync` | — | WS 重连后（服务端即将重放仍 pending 的请求） | 清空该源全部未决审批/提问。**脚本线程退出时宿主也会自动补发**——死人无法 resolve |
| `SourceHealth` | `source, healthy` | **只在健康状态翻转时发**（不要每轮都发） | 记录;全部已知源 down → **Offline**;**false 同时清空该源会话的运行账目**（running/turns/tools）——源下线后陈旧"运行中"不再驱动模式 |
| `QueueChanged` | `source, pending` | 队列深度变化 | 快照 `queue_len` = 各源求和 |
| `Tick` | — | GUI 每帧心跳 | 清理过期审批/提问（30 分钟 TTL;headless 调试驱动不发 Tick） |

**running 落账**（Poll 与 SessionStatus 共用,`state.rs` 的 `set_session_running`）：

- `running=true`：清防抖,会话活跃。
- `running=false` 且有回合/工具且非 `waiting_user`：进入 **3s 防抖**（`DONE_FALLBACK_DEBOUNCE_MS`）,持续 not-running 才清账回 Idle。**绝不补记"已完成"**——完成只认真实 `TurnEnded(Completed)`（旧版"补记完成"在服务器抖动下反复误报,已废除,见测试 `running_false_clears_ledger_never_fakes_done`）。

`SessionItem`（Poll 的 items 元素）：

```rust
pub struct SessionItem {
    pub session_id: String,
    pub running: bool,
    pub title: Option<String>,
    pub todos: Option<Vec<TodoItem>>, // TodoItem { content, status }
}
```

## 13. Mode 推导优先级（当前实现）

```
Offline(所有已知源 unhealthy) > Attention(未决审批/提问)
> Failed(庆祝窗 celebrate_sec=4s) > Done(庆祝窗)
> Failed(保持窗 fail_sec=10s)   > Working(有工具开着)
> Thinking(turns>0 或 running)  > Done(保持窗 done_sec=10s) > Idle
```

- **庆祝窗**：done/fail 事件后的强制可见期——即使下一回合立刻开始也能看到动画
  （低优先级窗口期在其后才生效）。
- `Move` 不在状态机里：GUI 在"拖拽且底层是 Idle"时叠加的覆盖层。
- **气泡源选择**（`select_bubble_source`）：只在健康源里选;活跃（running/turns/
  tools/waiting_user 任一）> 空闲;平局取注册序最小的脚本（BTreeMap 序,DSH 注册
  在首位即"平局 DSH 赢"）。
- 窗口值全部来自 config：`windows.done_sec` / `fail_sec` / `celebrate_sec`。

## 14. 何时需要改宿主（Rust）

**主路径永远是 Lua（第一部分）**。只有两种情况动这一层：

### 14.1 给 `pet.*` 增加新 API

1. `app/src/connectors/lua.rs` 的 `build_pet_api`：加一个 `lua.create_function`,
   把参数翻译成 `send(&tx, StateEvent::…)`。注意：session_id/question id 要过
   `sid()`/前缀处理（会话 id 加 `script-N-`,question/approval id 加前缀保证全局唯一）。
2. `scripts-guide.md` §3 的 API 表同步补一行。
3. 补一个 `pet_api_*` 风格单测（临时脚本 → 断言事件序列,参考现有
   `poll_and_todo_convert_tables` 等）。
4. 验证：`cargo test --lib` + `cargo check --target x86_64-pc-windows-gnu`
   （工具链:`source .tools/env.sh`）。

### 14.2 调整状态机行为

`state.rs` 用注入时钟（`now_ms`）做全事件组合测试。改行为前先补**回归测试**锁住
语义（现有防回归样例:`running_false_clears_ledger_never_fakes_done`、
`running_flap_does_not_fake_done`、`vanished_active_session_reaped_after_grace`、
`duplicate_turn_start_is_idempotent` 等）。

### 14.3 已废除路径（不要走）

为单个来源写 Rust 连接器（`connectors/xxx.rs`）、给 `Source` 枚举加变体、在
`config.rs` 加 `XxxConfig` 段——`Source` 已收敛为 `Script(u16)`,新来源一律走
Lua 脚本 + `scripts[].args`,不需要编译。

### 14.4 宿主运行时契约（脚本线程生命周期）

- 每脚本独立线程 + 独立 Lua 5.4 state;互相隔离,一个脚本出错只下线自己。
- 加载失败/编译错误/运行错误/主动 `return`（不循环）→ 该源 `health(false)` +
  自动 `PendingSync` 清残留审批/提问;其余源与 GUI 不受影响。
- `name`/`poll_ms`/`args`/`sandbox`/`enabled`/`debug` 全部 per-script;沙箱移除
  `os/io/package/require/dofile/loadfile/load/debug` 并禁用网络/SQLite API。

## 15. 宿主侧约定与坑

1. **线程模型**：生产者（脚本）绝不调用 GUI/渲染;GUI 线程只消费事件。
2. **退出**：脚本循环必须用 `pet.wait`(可中断切片);Rust 侧线程循环查 `stop`。
3. **幂等与乱序**：宿主已对 turn 重放幂等、Poll 基线幂等合并;来源侧仍应自行做
   增量去重（seq / prev diff）,把重放风暴挡在源头。
4. **LiveText 聚合**：不要每 chunk 发一条事件——自行聚合(dsh 每轮聚合 chunk、
   hermes 行级 delta),channel 与 8000 字符 cap 都不是为逐 chunk 发送设计的。
5. **session_id**：宿主自动加 `script-N-` 前缀;同一脚本内不同任务用不同 id 即可
   看到多条会话轮流显示。
6. **健康语义**：翻转才发;`false` 会被宿主拿来清账——瞬时抖动不要刷 `false`。
7. **体积**：优先复用 `http.rs`(HTTP/1.1 + WebSocket + SSE 自带);新依赖慎重
   （exe 有体积预算）。
8. **素材/动画**：新来源不需要新动画——`Mode::asset()` 只按模式映射。

## 16. 参考索引

| 想抄什么 | 看哪里 |
|---|---|
| Lua 接入主路径 | 本文第一部分 + `scripts/dsh.lua`（最全范本） |
| 连接器骨架 / 健康翻转 / 轮询循环 | `scripts/dsh.lua` / `scripts/comfyui.lua` 主循环 |
| 增量 diff 记忆 | `scripts/hermes.lua` 的 `prev` 表 |
| WS 流式 + 重连 + pending_sync + push 抽水 | `scripts/dsh.lua` 的 mux/host 段 |
| seq 增量 / 基线重建 | `scripts/dsh.lua` 的 `apply_history` |
| 仅队列来源(无会话) | `scripts/comfyui.lua` |
| 本地文件 tail + 截断/清空检测 | `scripts/maa.lua` |
| pet.* API 宿主(加新 API) | `connectors/lua.rs` 的 `build_pet_api` |
| 事件消费语义 | `state.rs` 的 `PetState::apply` |
| running 落账 / 防抖清账 | `state.rs` 的 `set_session_running` |
| 消失回收 / 快照合并 | `state.rs` 的 `PetState::apply`(Poll 分支) |
| 模式推导 | `state.rs` 的 `mode()` |
| 气泡源选择 / 平局规则 | `state.rs` 的 `select_bubble_source` |
| 气泡文案 | `bubble_text.rs` 的 `plain_bubble` / `live_parts` |
| HTTP/WS/SSE 客户端 | `http.rs` |
