# Hannis Lua 脚本接入指南

> **读者**：想给宠物接入"另一个程序"的用户（无需懂 Rust）。
> **前提**：会写 Lua（内置 Lua 5.4 解释器已打进 Hannis.exe，**用户无需安装任何运行时**）。
>
> 注：MAA 与 ComfyUI 的内置连接器已迁移为脚本（`scripts/maa.lua`、`scripts/comfyui.lua`，
> 随程序发布,行为与原内置版一致）——它们本身就是这份指南最好的完整示例。

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
    "sandbox": false,              // true = 禁用文件/进程访问(只保留 pet API)
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

脚本返回(不循环)或出错 = 该源下线(health=false),宠物和其他源不受影响。

## 3. pet.* API 一览

| 调用 | 作用 | 宠物表现 |
|---|---|---|
| `pet.health(ok)` | 标记本源健康/不健康 | 全部源不健康 → offline |
| `pet.poll({ {session_id=…, running=…, title=…, todos=…}, … })` | 基线快照 | 会话合并/回收 |
| `pet.session_started(id, turn)` | 会话开始 | thinking |
| `pet.session_ended(id, turn, reason)` | 会话结束;reason ∈ `completed`/`error`/`max_tokens`/`aborted`/`interrupted`/`blocked` | done / fail / attention |
| `pet.tool_started(id, name, args?)` | 开始干活 | working |
| `pet.tool_ended(id, name, error?)` | 干活结束 | 回退 thinking |
| `pet.live_text(id, {reasoning=…, text=…, tool_name=…})` | 实时文字 | 气泡流式显示 |
| `pet.question(id, session_id, text)` / `pet.answer(id)` | 向用户提问/回答 | attention |
| `pet.todo(id, { {content=…, status=…}, … })` | 待办列表 | 气泡任务名兜底 |
| `pet.user_message(id, text)` | 用户消息 | 气泡任务名兜底 |
| `pet.queue(n)` | 队列深度 | 气泡"队列中还有 N 个任务" |
| `pet.log(level, msg)` | 写日志 | 进 `hannis.log`(GUI 运行)或 stderr(headless) |
| `pet.wait(ms)` | 可中断睡眠 | 必须用它做轮询间隔,否则程序退不掉 |
| `pet.http(url, timeout_ms?)` | HTTP GET → `status, body` | 访问任意 HTTP 接口(沙箱禁用) |
| `pet.http_post(url, body, timeout_ms?)` | HTTP POST(JSON body)→ `status, body` | DSH `session.list/history` 这类接口(沙箱禁用) |
| `pet.ws(url, path, timeout_ms?)` → handle | 打开 WebSocket(读超时=timeout) | 推送型接口,如 DSH `/api/events.mux`(沙箱禁用) |
| `pet.ws_read(handle)` → 文本帧 \| nil | 读一帧;ping/pong 自动跳过,关闭/超时返回 nil | 配合 pet.ws;用后 `pet.ws_close(handle)` |
| `pet.ws_close(handle)` | 关闭 WS 连接 | — |
| `pet.sqlite(path, sql, params?)` → 行数组(每行 {列名=值}) | 只读 SQLite 查询一次一开 | Hermes 这类 SQLite 后端(沙箱禁用;BLOB 转十六进制) |
| `pet.config()` | 返回 `{ name, poll_ms, args }` | — |

**约定**：
- `session_id` 会被自动加 `script-<序号>-` 前缀(避免与 DSH/Hermes 串台),你只管用自己的 id
- `question` 的 id 也会加前缀,保证全局唯一
- 每个 `session_started` 对应一个 `session_ended`;`tool_started` 对应 `tool_ended`(否则状态机一直停在 working)
- 会话 id 按"任务"分:不同任务用不同 id 即可看到多条会话轮流显示

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

| 脚本 | 用途 | 亮点 |
|---|---|---|
| `scripts/maa.lua` | MAA(明日方舟小助手)日志监控 | 文件 tail + 截断检测 + 启动恢复 + attention 自动消除 + **任务期间日志信息流**(args.stream) —— 最完整的日志接入范本 |
| `scripts/comfyui.lua` | ComfyUI 出图队列 | `pet.http` + 内置最小 JSON 解码器 —— HTTP 轮询范本 |
| `scripts/tail_log.lua` | 通用日志关键词监控 | 最简版,改关键词即可 |
| `scripts/process_watch.lua` | 进程监控 | `io.popen` 范例(沙箱模式不可用) |

## 5. 沙箱

`sandbox: true` 时脚本全局环境里**没有** `os`/`io`/`package`/`require`/`dofile`/`loadfile`/`load`/`debug`
——只能纯计算 + 调 pet API(不能读文件、不能开进程、不能加载其他代码)。
默认 `false`(你的机器、你的脚本,完整权限)。

## 6. 调试

- `pet.log(level, msg)` → 追加到 exe 同目录 `hannis.log`
- 脚本编译/运行错误也会写进 `hannis.log`(形如 `[lua:名字] script error: …`)
- WSL 下开发:headless 模式直接跑,错误在 stderr 可见:
  `cd app && cargo run`(读取 config.json 的 scripts 并执行)
- 脚本一改就重启 Hannis 生效(暂无热重载)

## 6.5 现有内置源能否用脚本实现?

**能**,且三种能力已齐备(以上 API 就是为了覆盖它们而加的):

- **Hermes**:`pet.sqlite` 只读查询 + 脚本自己记增量(prev 状态用 Lua 表)+ 事件翻译 ≈ 原内置连接器(唯一简化项:WAL 副本回退不做,打开失败=该源不健康)
- **DSH**:
  - `pet.http_post` 轮询 `session.list` / `session.history`(JSON envelope,增量由脚本记 `last_seq`)
  - `pet.ws` 接 `/api/events.mux` 与 `/api/events.host` 的推送帧 → JSON 解码 → 事件翻译
  - 唯一简化项:`PendingSync`(断线重连后的审批清理)不做,断线重连即重基线
- 两者都保持内置(性能/健壮性更好),脚本能力仅作补充与扩展

## 7. 常见问题

| 现象 | 原因 |
|---|---|
| 气泡没有 From 我的名字 | `scripts` 里 `name` 为空或文件路径不对 |
| 脚本完全没反应(该源状态不出现) | 看 `hannis.log` 里 `[lua:<名字>]` 开头的行:`started` = 已加载;`error:` = 抓取失败(URL/网络/JSON);完全没有该前缀 = `scripts` 数组里没有这个条目 |
| 一直 working 不结束 | 忘了调 `tool_ended` / `session_ended` |
| 宠物显示 offline | 脚本 `pet.health(false)`(比如日志打不开);或所有源都断了 |
| 脚本卡死宠物 | 不可能——脚本在独立线程;但死循环会烧 CPU,用 `pet.wait` 做间隔 |
| 退出时程序卡住 | 脚本里用了系统睡眠或死循环没调 `pet.wait`;检查脚本 |
