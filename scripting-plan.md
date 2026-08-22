# 开放接口计划：Lua 脚本接入

> 状态：**待确认**。本文是先行的方案与可行性分析，确认后才开始实施。
> 目标读者：项目作者（确认决策点）与后续实现者。

## 0. 目标

让用户**尽量简单**地把任意程序的状态接入宠物，例如：

- 监控某个程序的日志文件（像 MAA 连接器那样，但关键词由用户自定义）
- 监控某个进程是否在运行
- 周期性执行一条命令并解析输出
- 读取某个 JSON/INI/文本配置或状态文件

用户只需写一个 **Lua 脚本** + 一行配置，不需要懂 Rust、不需要重新编译。

## 1. 方案选型

| 方案 | 说明 | 可行性 | 结论 |
|---|---|---|---|
| A. 内嵌 Lua（mlua + vendored Lua 5.4） | 脚本在宠物进程内的独立线程运行，通过 `pet.*` API 发事件 | **已验证**：mlua 0.10（lua54+vendored+send）在 Linux 与 Windows 交叉目标（zig 工具链）均编译通过；Lua→Rust 函数调用与脚本错误处理冒烟通过 | ✅ 推荐 |
| B. 子进程 JSON-lines 协议 | 宠物 spawn 任意语言的脚本，stdin/stdout 传 JSON 事件 | 可行，但每个脚本都要用户自备运行时（Lua/Python/Node…），且要处理进程管理 | 作为后续可选扩展 |
| C. A+B 组合 | 内嵌 Lua 为主，协议为辅 | 工作量大 | 二期再说 |

**推荐 A**：单 exe 零运行时依赖（延续项目一贯原则），Lua 脚本开箱即用；脚本运行在独立线程，卡死/崩溃不影响主程序（只影响该脚本源）。

### 1.1 可行性验证结果（已完成）

- ✅ `mlua = { version = "0.10", features = ["lua54", "vendored", "send"] }`：
  - Linux host `cargo check` 通过（增量 ~8s）
  - `x86_64-pc-windows-gnu` 交叉 `cargo check` 通过（~48s；Lua C 源码经 cc + zig 编译，与 rusqlite bundled 同一条工具链路径）
  - 功能冒烟：Lua 调 Rust 注册函数、`error()` 抛错经 `Result` 捕获不 panic
- ⚠️ 体积预估：exe 增加 ~1–2 MB（Lua 5.4 静态库 + 绑定层）。是否接受需确认（见 §7 决策点 1）

## 2. 架构设计

```
scripts/mygame.lua（用户编写）
        │ 配置注册: "scripts": [{ "name": "MyGame", "file": "scripts/mygame.lua" }]
        ▼
LuaScriptConnector（Rust, connectors/lua.rs,每脚本一条命名线程）
  │  线程内: 创建独立 Lua state → 加载脚本 → 循环执行
  │          pet.* API 调用 → 校验 → 翻译为 StateEvent
  ▼
mpsc channel ──► PetState（状态机，完全复用，零改动）
  ▼
GUI / 气泡 / 动画（"From MyGame" 标签）
```

要点：

- **每脚本一线程 + 一个独立 Lua state**：脚本之间、脚本与主程序完全隔离；`send` 特性允许 Lua 函数把事件跨线程发进 channel
- **脚本拥有自己的轮询循环**（与现有连接器同构）：`pet.wait(ms)` 提供可中断的切片睡眠（同 `sleep_interruptible`），保证退出时线程能及时终止
- **脚本错误**：任何 Lua 错误被 pcall 包裹 → 记录到 `hannis.log` + 该源 `health=false` → 脚本线程停止（或按配置自动重启，见决策点 4）。绝不影响其它源与主程序
- **Source 动态化**：`Source::Script(u16)` 新变体 + 标签注册表（`label()` 改为返回 `String`，从注册表取"From MyGame"）。影响面：`state.rs`、`bubble_text.rs`、`headless.rs` 少量改动；状态机优先级/窗口期逻辑零改动
- **headless 同步注册**：脚本连接器同样在 Linux headless 下运行——用户可以在 WSL 里直接开发调试脚本（Lua 与平台无关）

## 3. Lua API 契约（脚本可见面）

镜像现有 `StateEvent` 契约（详见 `adding-connectors.md` §2），脚本作者只需学会这一小张表：

| Lua 调用 | 对应事件 | 状态机效果 |
|---|---|---|
| `pet.health(ok)` | SourceHealth | 健康翻转；全部源不健康 → offline |
| `pet.poll({ {session_id=…, running=…, title=…}, … })` | Poll | 基线快照；running 翻转、会话回收 |
| `pet.session_started(id, turn)` | TurnStarted | thinking |
| `pet.session_ended(id, turn, "completed"/"error"/"aborted"/"blocked")` | TurnEnded | done/fail/attention |
| `pet.tool_started(id, name, args?)` | ToolStarted | working |
| `pet.tool_ended(id, name)` | ToolEnded | 回退 thinking |
| `pet.live_text(id, {reasoning=…, text=…})` | LiveText | 气泡实时文字 |
| `pet.question(id, session_id, text)` / `pet.answer(id)` | QuestionRequested/Resolved | attention |
| `pet.todo(id, { {content=…, status=…}, … })` | TodoSnapshot | 气泡任务名兜底 |
| `pet.user_message(id, text)` | UserMessage | 气泡任务名兜底 |
| `pet.queue(n)` | QueueChanged | 气泡"队列中还有 N 个任务" |
| `pet.log(level, msg)` | — | 写入 hannis.log，方便调试 |
| `pet.wait(ms)` | — | 可中断睡眠（脚本主循环用） |
| `pet.config()` | — | 返回该脚本的配置段（JSON→Lua table） |

会话 id 命名空间：连接器自动加 `script-<id>-` 前缀，防止与内置源串台（沿用现有约定）。

## 4. 配置与示例

```jsonc
// config.json 新增段
"scripts": [
  {
    "name": "MyGame",                // 气泡 "From MyGame"
    "file": "scripts/mygame.lua",    // 相对 exe 目录
    "poll_ms": 1000,                 // 可选,脚本也可自己 pet.wait
    "args": { "log": "D:\\MyGame\\game.log", "keywords": { "start": "开始任务", "done": "完成" } }
  }
]
```

示例脚本（随项目发布两个，作为模板）：

1. **`tail_log.lua`**（通用日志监控，约 30 行）：读取 `args.log` 的追加内容，按 `args.keywords` 把行映射为 working/done/fail——MAA 连接器的场景可以原样套用到任何游戏/工具
2. **`process_watch.lua`**（进程监控，约 15 行）：周期性检查某进程是否存在（`os.execute` + `tasklist` 或文件锁），存在 → working，消失 → done

## 5. 安全与健壮性

| 风险 | 处理 |
|---|---|
| 脚本死循环/卡死 | 每脚本独立线程，只烧自己的 CPU；文档要求主循环用 `pet.wait`（可中断）；二期可加"事件间隔 watchdog" |
| 脚本崩溃/语法错误 | pcall 包裹 → 记日志 + 该源下线，其余不受影响；可配置自动重启（决策点 4） |
| 脚本滥用系统权限（os.execute 等） | 默认开放完整标准库（用户自己的机器、自己的脚本）；每脚本可选 `"sandbox": true`——用 mlua 自定义 globals，只注入白名单库（去掉 os.execute/io.popen/loadfile 等），API 仍完整 |
| 事件风暴（海量 LiveText） | 状态机已有 8000 字符 cap；channel 无界（与现有连接器同级风险）；文档约定按块聚合 |
| 中文路径/编码 | Lua 字符串是字节串，Windows 中文路径无碍；脚本文件要求 UTF-8 |
| 与内置源冲突 | 会话 id 自动加前缀；`Source::Script` 参与同一套健康/优先级语义 |

## 6. 风险与问题清单（需要你知晓）

1. **体积**：exe +1~2 MB（可接受？）
2. **新依赖**：mlua（打破"尽量不加 crate"惯例，但这是功能型依赖，成熟度高）
3. **`Source::label()` 返回类型变化**：`&'static str` → `String`，波及气泡/headless 打印的少量调用点（编译期可穷尽）
4. **气泡源平局规则**：`select_bubble_source` 现有规则是"活跃优先、平局 DSH 赢"。多个脚本同时活跃时谁显示？建议：活跃脚本按注册顺序，内置源保持现有优先（决策点 2）
5. **脚本线程无法强制中断**（同进程内）：真正卡死的脚本只能等下次重启宠物才失效；用进程级协议（方案 B）才能强杀——二期再议
6. **Lua 5.4 vs LuaJIT**：默认 5.4（vendored 稳定）；JIT 对 Windows 交叉编译更挑工具链，不默认启用

## 7. 待确认决策点

1. **方案**：内嵌 Lua（推荐）？还是要连子进程协议一起做（工作量大一倍）？
2. **平局规则**：多个源同时活跃时气泡优先顺序？（建议：内置源 > 活跃脚本，脚本间按注册顺序）
3. **沙箱默认值**：脚本默认完整权限（推荐，简单），还是默认沙箱？
4. **脚本崩溃行为**：停止（推荐，简单可预期）还是自动重启（加复杂度）？
5. **体积 +1~2 MB**：接受？

## 8. 实施步骤（确认后执行）

| 里程碑 | 内容 | 验证 |
|---|---|---|
| M1 | 依赖（mlua lua54+vendored+send）；`Source::Script` + 标签注册表；`label()` 改 String | `cargo test --lib`；双目标 `cargo check` |
| M2 | `connectors/lua.rs`：脚本线程/Lua state/`pet.*` 全 API + 校验 + 错误处理 | 单测：样例脚本 → 期望事件序列（对照真实连接器测试） |
| M3 | 配置 `scripts` 段 + 生命周期（启停/健康/日志）+ headless 注册 | headless 跑示例脚本冒烟 |
| M4 | 示例脚本 `tail_log.lua` / `process_watch.lua` + `scripts-guide.md`（面向非 Rust 用户） | 用真实日志跑通 |
| M5 | 回归 + 构建 + 冒烟 | `build-wsl.sh` + Windows 真机 |

## 9. 范围外（本计划不做）

- 图形化脚本编辑器 / 脚本市场
- 子进程 JSON 协议（二期候选）
- 脚本热重载（二期候选）
