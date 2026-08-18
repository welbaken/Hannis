# 独立桌面宠物 Windows 实施方案(dshpet-win)

> 目标:一个**独立的 Windows 桌面宠物程序**(非浏览器、非插件、非服务),单 exe、零运行时依赖、透明置顶悬浮窗。
> 本方案在 `desktop-pet-monitor-design.md`(数据面/状态机设计)之上,落地 Windows 原生实现,并补充素材播放、离线灰度、渐隐透明、头顶气泡(后端指示 + 实时模型输出)、性能与素材预处理流水线。
> 状态:方案待评审,尚未开始执行。已按评审意见修订:v2(窗口尺寸可调 / 渐隐 / 气泡后端指示 + 实时模型输出 / idle 完整重播 / 动画切换防闪烁)。

---

## 1. 需求 → 方案映射

| # | 需求 | 方案 |
|---|---|---|
| R1 | 独立且无依赖的 Windows 程序 | Rust + windows-rs,静态链接 CRT 的单 exe(约 5-8 MB),仅依赖 Windows 自带系统 DLL;免安装、免管理员、不写注册表(可选开机自启除外) |
| R2 | 悬浮在所有窗口上方 | `WS_EX_LAYERED \| WS_EX_TOPMOST \| WS_EX_TOOLWINDOW` 透明置顶窗,`UpdateLayeredWindow` 逐像素 alpha |
| R3 | 根据 dsh / hermes 状态切换宠物状态 | 复用设计文档的状态机 + 双连接器(DSH:HTTP 轮询 + SSE;Hermes:SQLite 只读轮询) |
| R4 | 使用 `resource/` 下 webp 文件 | 7 个动画文件按状态映射(见 §3),运行时解码为 RGBA 帧缓存 |
| R5 | 播放后重复最后约 1s 内容;**idle 例外:完整重播不限尾部** | 非 idle 状态:完整播一遍(73 帧/3.04s)→ 循环尾部 ~1s(24 帧,可配);idle:始终 0..72 完整循环 |
| R6 | dsh 与 hermes 都断开 → idle 第一帧 + 灰度 | offline 呈现态 = idle 第 0 帧静态图 + 灰度变换(缓存一次),透明度按渐隐/基础透明度规则 |
| R7 | 透明采用**渐隐**(无操作 5 秒后) | 交互(鼠标悬停/拖拽)重置 5s 计时;无操作 5s → 渐隐至可配目标(默认 0.15);交互 → 渐显。保留每状态基础透明度(默认 100%)作为乘数 |
| R8 | 头顶气泡:显示当前工作的是 hermes 还是 dsh + 实时模型输出(如思考内容) | 气泡头部标注后端来源;双源选择规则:优先"在线且非 idle",并列时优先 DSH;内容含实时思考/正文输出(DSH:chunk 流节流;Hermes:messages 轮询) |
| R9 | 窗口尺寸可修改 | config `display.scale` + 设置对话框数值调整 + Ctrl+滚轮快捷缩放(0.25-2.0),持久化 |
| R10 | 切换动画避免闪烁 | 交叉淡化(cross-fade)+ 目标动画首帧预解码 + 缓冲永不置空/置黑(见 §5.6) |
| R11 | 考虑性能 | §10:单动画常驻、解码缓存、缩放降采样、chunk 聚合节流、无每帧分配 |
| R12 | 后期 webp 分割等处理 | §11:开发期分割工具 → 帧 PNG + manifest,运行期优先走分割帧加载,webp 直读为兜底 |

---

## 2. 资产现状(已实测)

| 文件 | 尺寸 | 帧数 | 总时长 | 帧率 | 备注 |
|---|---|---|---|---|---|
| attention / done / fail / idle / move / think / working.webp | 576×736 | 73 | 3041 ms | ≈24 fps | 全部一致 |
| alpha | — | — | — | — | 已含透明通道(绿幕已在预处理阶段抠除,`out/_process_webp.py`) |
| 文件体积 | — | — | — | — | 每文件 10.8-12.0 MB(无损 webp),共约 77 MB |
| 循环标记 | — | — | — | — | loop=1(播放一次) |

**尾部循环参数(默认)**:单帧 41.67 ms,最后 1s ≈ 24 帧 = 索引 49..72。完整一遍 3.04 s,此后循环尾部 1 s。**idle 不走尾部循环,始终完整 0..72 循环(R5)**。

---

## 3. 技术选型

**主选:Rust + windows-rs,原生 Win32 窗口(不用 winit/softbuffer/tiny-skia)**

理由:
- **单 exe 零依赖**:`cargo build --release` + `-C target-feature=+crt-static`,产物只依赖 Windows 系统 DLL(kernel32/user32/gdi32/shell32/comctl32/winhttp/ole32 等,系统自带),无 .NET/VC 运行库/Node 等任何外部运行时。
- **透明置顶窗控制力最强**:分层窗(WS_EX_LAYERED + UpdateLayeredWindow)是逐像素 alpha 的标准做法,原生 Win32 最直接;winit 的透明窗口能力反而受限。
- **动画解码纯 Rust**:`image` crate 的 `WebPDecoder` 支持动画 WebP(帧序列 + 每帧延迟,已核实),无需 C 工具链编译 libwebp。
- **内存安全 + 迭代快**,与设计文档的 pet-rs 参考(Rust)保持一致。
- 构建机只需 rustup(MSVC 或 GNU 工具链均可,见 §13)。

**备选:C++/Win32 + libwebp + sqlite3 amalgamation**(/MT 静态链接)。体积最小(~1 MB)但需手工维护 Win32 样板与 C 源码 vendoring,开发效率低。**结论:不做,除非出现 Rust 工具链不可用的情况。**

关键依赖(全部编译期依赖,不进入产物运行时):
- `windows`(Win32 API 绑定,微软官方)
- `image`(webp 动画解码,纯 Rust)
- `winhttp`(DSH 轮询 + SSE,走系统 WinHTTP,免 TLS 依赖——本地 HTTP 即可)
- `rusqlite`(bundled,读 Hermes SQLite;构建期编译 sqlite3.c,产物仍单 exe)
- `serde_json`(config / manifest)
- 嵌入式图标用 `winres`(仅构建期)

---

## 4. 总体架构(单进程多线程)

```
┌────────────────────────────── 进程:dshpet.exe ─────────────────────────────┐
│                                                                             │
│  UI 线程(GetMessage 主循环)                                                   │
│  ├─ 分层窗 wndproc:WM_TIMER(动画步进) / WM_APP+1(状态快照) / WM_APP+2(气泡文本)│
│  ├─ 渲染:RGBA 帧缓冲 → 状态透明度 × 渐隐系数 → (灰度) → 合成气泡 → ULW         │
│  ├─ 交叉淡化:状态切换时旧帧 → 新动画首帧 blend(200ms,防闪烁)                  │
│  ├─ 渐隐计时:无操作 5s → 渐隐;悬停/拖拽 → 渐显                                │
│  ├─ 托盘 Shell_NotifyIcon + 右键菜单(设置/退出)                              │
│  └─ 设置对话框(尺寸、渐隐参数、endpoint、尾部循环参数)                          │
│                                                                             │
│  连接器线程(每源一个,可独立启停)                                                 │
│  ├─ DSH Connector: 2s session.list 轮询 + events.mux/host SSE(WinHTTP)     │
│  │     └─ assistant/chunk 消费:按会话聚合 reasoning/text 增量,节流上抛        │
│  └─ Hermes Connector: 1s SQLite 只读轮询(mode=ro,失败降级复制式读取)          │
│        │ 产出 StateEvent(语义事件,无后端字段) + 实时文本事件                   │
│        ▼                                                                     │
│  PetState 状态机(纯逻辑,单线程内消费,可单测) → Snapshot                        │
│        │ 通过 PostMessage(WM_APP+1) 推给 UI 线程                                │
│        ▼                                                                     │
│  动画播放器:状态 → 动画文件 → 帧缓存 → (完整一遍 + 尾部循环 / idle 全循环)      │
└─────────────────────────────────────────────────────────────────────────────┘
```

- 状态机与连接器骨架复用设计文档(`StateEvent` / `AgentSource` / 8 态表驱动优先级);**新增"实时文本事件"通道**(ReasoningChunk / TextChunk 聚合文本),与状态事件分开传递。
- **offline 判定(R6)**:所有已配置连接器 health 均失败 → 状态机输出 offline;任一源在线则正常聚合(离线源仅丢失其会话信息)。
- 线程间只传小消息(枚举 + 聚合文本),帧数据只在 UI 线程内部流转。

---

## 5. 窗口与渲染

### 5.1 分层窗与尺寸(R9)
- 窗口样式:`WS_POPUP | WS_EX_LAYERED | WS_EX_TOPMOST | WS_EX_TOOLWINDOW`(无任务栏、无边框、置顶)。
- 渲染:`UpdateLayeredWindow` 上传**预乘 alpha ARGB** 位图,逐像素透明,宠物轮廓外完全透出桌面。
- **尺寸可改**:`display.scale` 默认 0.5(576×736 → 288×368),范围 0.25-2.0;调整途径:① 设置对话框数值框;② Ctrl+滚轮(步进 0.05);③ config.json。持久化,重载动画帧缓存(后台重新降采样,期间用旧缓存最近邻缩放过渡)。
- DPI:`SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2)`,按监视器 DPI 缩放窗口尺寸,保证 GDI 文本清晰。

### 5.2 透明度:渐隐(R7)
- **交互定义**:鼠标指针位于宠物窗口内(WM_MOUSEMOVE 进入)或正在拖拽 = 有操作;离开窗口 = 无操作。
- **行为**:无操作持续 5s(`fade_after_sec`,可配)→ 全局 alpha 从 1.0 线性渐隐至 `fade_target`(默认 0.15,可配 0=完全消失),时长 `fade_ms`(默认 1200);鼠标进入/拖拽 → 立即渐显回 1.0。
- **合成公式**:`最终 alpha = 状态基础透明度 × 渐隐系数`。每状态基础透明度保留(默认 1.0,可配),渐隐统一作用于所有状态;`fade_disabled_states` 可配例外(如 attention 不渐隐,可选)。
- 实现:渐隐系数每帧更新,对当前帧缓冲做一次 alpha 乘法(0.42 MB @24fps 成本可忽略);渐隐与交叉淡化共用同一合成管线。
- 气泡不参与渐隐(保持可读,可配)。

### 5.3 离线灰度(R6)
- 呈现:静态 idle 第 0 帧 + 灰度化。灰度矩阵 `g = 0.2126R + 0.7152G + 0.0722B`(保留 alpha)。
- 仅在线路从"有源在线"翻转为"全部离线"时计算一次并缓存;恢复在线后按新状态正常播放(从第 0 帧完整播)。

### 5.4 气泡(R8):后端指示 + 实时模型输出
- 绘制在宠物头顶上方,窗口矩形向上扩展(宠物底部锚点不动),气泡消失时窗口缩回;尺寸随内容自适应(宽度 ≤ 窗口宽度 × 2,超长滚动)。

**后端选择规则**(气泡显示"当前工作的是谁"):
```
候选 = 在线源(offline 源不参与)
等级: L2 = 非 idle(working/thinking/attention/failed/done)
      L1 = idle(在线但空闲)
取最高等级;同等级并列 → 优先 DSH
例: DSH working + Hermes idle   → 显示 DSH
    DSH idle + Hermes working   → 显示 Hermes
    DSH working + Hermes working → 显示 DSH(并列取 DSH)
    全部离线 → offline 气泡(连接失败提示)+ 灰度 idle
```

**气泡内容**(按选定源):
- 头部:`[DSH] 正在思考…` / `[Hermes] 正在执行 tool…` + 会话标题;
- **实时模型输出**:thinking 期间显示思考内容,生成正文时显示正文(截断 + 滚动);
  - DSH:消费 `assistant/chunk` 的 `reasoning-delta` / `text-delta`,连接器按会话聚合,节流上抛(100-150ms 合并)→ 气泡按节流频率刷新;
  - Hermes:轮询 `messages` 表最新行,取 `reasoning_content`(思考)/ `display_content`(正文),增量检测变化才上抛;
- 工具信息:`tool/call` 后显示「正在执行:工具名」,Hermes 对应 `tool_name`;
- 其余状态文案沿用设计文档 §4.3(attention 待确认项、failed/done 列表等)。
- 多会话聚合,按会话分节;文本缓存——内容不变不重绘。

### 5.5 交互与托盘
- 拖拽:WM_LBUTTONDOWN 按下 → 拖动期间播 move.webp,松开回到派生状态;拖动不进入状态机(设计文档原则);拖拽视为"有操作",重置渐隐计时。
- 双击:打开设置;托盘右键:显示/隐藏、设置、开机自启、退出。
- 单实例:命名互斥体,二次启动时唤起已有实例。
- `#![windows_subsystem = "windows"]` 无控制台窗口。

### 5.6 动画切换防闪烁(R10)
1. **缓冲永不置空**:合成缓冲区是常驻 RGBA 位图,切换期间绝不提交空/黑缓冲;ULW 每次上传的都是完整合成结果(宠物外区域天然全透明)。
2. **交叉淡化**:状态切换时,新动画第 0 帧与当前帧做 200ms alpha blend(0→1)后,新动画才开始完整播放;旧动画画面持续到 fade 完成,视觉无缝。
3. **首帧预解码**:解码线程优先产出目标动画第 0 帧;若 500ms 内未就绪,旧动画继续播放(不闪),就绪后再 cross-fade。
4. **同状态不重启**:working→working(换工具)只更新气泡,动画继续,避免跳动。
5. `WM_ERASEBKGND` 直接返回 1,无背景刷,杜绝黑色闪底。

---

## 6. 动画播放器

### 6.1 加载与缓存
- 首次进入某状态时后台线程解码该动画(73 帧 RGBA),**解码完第 0 帧立即显示**,其余帧渐进就绪;只常驻**当前动画**(约 30 MB @ 0.5 缩放,见 §10 内存账),状态切换时丢弃旧动画。
- 解码后统一降采样到显示缩放,只保留缩放后帧(峰值瞬态 ~118 MB 原生帧,可接受)。
- 帧时长取自 WebP ANMF chunk,与素材一致(41.67 ms)。

### 6.2 播放调度(R5)
```
非 idle 状态: 首次进入 → 播 0..72(完整一遍,3.04s);之后循环 49..72(尾部 ~1s,24 帧)
idle 状态:    始终 0..72 完整循环(不限尾部)
```
- 参数: `tail_ms`(默认 1000,从末尾累计帧时长求帧数)或 `tail_frames`(精确指定尾部帧数,开发期调参用),config 可配。
- 状态切换 → 交叉淡化后从第 0 帧完整播放(R10);同状态重复事件不重启动画。
- done/fail 是窗口态:窗口(默认 done 120 s / fail 4 s,可配)结束后回落到派生状态。
- 拖动 → move.webp(拖完即回,同样走交叉淡化)。

### 6.3 离线呈现
- 全部源离线 → 停掉动画,显示 idle 第 0 帧灰度静态图(§5.3),不再播放,直到任一源恢复。

---

## 7. 状态机与连接器(复用设计文档 + 实测数据)

| 项 | 来源 | 本方案落地 |
|---|---|---|
| 状态机 | 设计文档 §4 | 8 态表驱动(offline > attention > failed > working > thinking > done > idle > 拖动覆盖),纯逻辑单测 |
| DSH 连接器 | 设计文档 §3/§5 + 本机源码核查 | `session.list` 2s 轮询 + `events.mux`/`events.host` SSE GET(WinHTTP 长连接,3s 退避重连,重连后补拉);**消费 `assistant/chunk`**(已核实字段:`chunk.reasoning-delta.text` 思考增量 / `chunk.text-delta.text` 正文增量,按会话聚合、100-150ms 节流上抛);`assistant/message` 时落定最终文本;其余高频帧仍白名单丢弃 |
| Hermes 连接器 | 设计文档 §7 + **本机实测 schema** | SQLite 轮询(活跃会话时 1s,空闲 2s):`sessions.ended_at IS NULL` → 运行中;`end_reason` 边沿 → done/中性;`messages` 增量检测(`reasoning_content`/`reasoning`/`display_content`/`content`/`tool_name`/`tool_calls`)→ working 内容 + 实时思考/正文(表结构已实测确认);`sessions.source`/`agent` 用于后端标注与过滤;表结构探测失败 → 该源 offline。**db 路径**:默认按 §8 动态解析(`HERMES_WEB_UI_HOME` env → `%USERPROFILE%\.hermes-web-ui\hermes-web-ui.db`),不依赖安装位置,可配覆盖 |
| 事件合并 | 设计文档 §7.3 | 多源事件合并进同一 PetState;气泡按 §5.4 规则选源并分节 |

**Hermes 读取降级路径(实测)**:WAL 库 `mode=ro` 直开在部分挂载/权限场景失败(本机实测失败);降级顺序:① `mode=ro` 直读(Windows 原生下 hermes 运行中通常可行)→ ② 复制 `db+wal+shm` 到临时目录后读(本机实测可行)→ ③ 不可读 → 该源 offline。

---

## 8. 配置(config.json,exe 同目录,无则生成默认)

```json
{
  "dsh":      { "url": "http://127.0.0.1:3080", "poll_ms": 2000 },
  "hermes":   { "db_path": null, "poll_ms_active": 1000, "poll_ms_idle": 2000 },
  "display":  { "scale": 0.5, "tail_ms": 1000, "tail_frames": null },
  "fade":     { "fade_after_sec": 5, "fade_target": 0.15, "fade_ms": 1200,
                "fade_disabled_states": [] },
  "opacity":  { "idle": 1.0, "working": 1.0, "thinking": 1.0, "attention": 1.0,
                "done": 1.0, "fail": 1.0, "move": 1.0, "offline": 1.0 },
  "bubble":   { "throttle_ms": 150, "max_text_len": 600 },
  "windows":  { "done_sec": 120, "fail_sec": 4 },
  "autostart": false
}
```

**路径解析策略(已按源码核实,不写死绝对路径)**:
- **DSH url**:默认 `http://127.0.0.1:3080`(DSH 默认端口,源码确认 `webStartup.port ?? 3080`);若本机 DSH 改了端口 → 在 config 中覆盖;环境变量 `DSH_PET_URL` 优先于 config。
- **Hermes db_path**:`null` = 自动解析:① 环境变量 `HERMES_WEB_UI_HOME`(若设置,源码确认优先)→ ② `%USERPROFILE%\.hermes-web-ui\hermes-web-ui.db`(源码确认 `resolve(homedir(), '.hermes-web-ui')`)。路径与 Hermes 程序安装位置无关,只随"运行 Hermes 的 Windows 用户账户"与上述 env 变动。解析结果为空/文件不存在 → 该源 offline 并在设置面板显示实际解析路径,便于排查。
- 设置对话框修改后写回并热生效(尺寸/渐隐/透明度立即应用;动画参数变更时重载动画)。
- 热切换 endpoint / db_path 时重启对应连接器线程。

---

## 9. 目录结构与构建

```
dshpet/
├─ desktop-pet-monitor-design.md      # 既有:数据面/状态机设计(本方案的上游)
├─ desktop-pet-windows-plan.md        # 本方案
├─ resource/                          # 素材(现有 7 个 webp;后期分割产物放 resource/<state>/)
├─ tools/
│  └─ split_webp.py                   # 开发期 webp → 帧 PNG + manifest(§11)
├─ app/                               # Rust crate(唯一可执行)
│  ├─ Cargo.toml
│  ├─ build.rs                        # winres 图标 + windows_subsystem
│  └─ src/
│     ├─ main.rs                      # 入口、单实例、托盘、消息循环
│     ├─ window.rs                    # 分层窗、ULW、拖拽、DPI、尺寸缩放
│     ├─ render.rs                    # 帧缓冲合成:透明度 / 渐隐 / 灰度 / 交叉淡化
│     ├─ bubble.rs                    # 气泡测量与绘制(GDI)+ 后端指示 + 实时文本
│     ├─ anim.rs                      # webp 加载、解码缓存、播放调度(完整+尾部/idle 全循环)
│     ├─ state.rs                     # PetState(设计文档 §4 移植)
│     ├─ config.rs                    # config.json 读写
│     ├─ settings.rs                  # 设置对话框
│     └─ connectors/
│        ├─ mod.rs                    # AgentSource trait(设计文档 §2)+ 实时文本事件
│        ├─ dsh.rs                    # 轮询 + SSE + chunk 聚合节流
│        └─ hermes.rs                 # SQLite 只读轮询 + 增量检测
└─ build.ps1                          # cargo build --release + crt-static + 拷贝产物
```

构建要点:
- `RUSTFLAGS="-C target-feature=+crt-static"`(MSVC 或 GNU 工具链均支持,产物不依赖 VCRUNTIME/msvcrt 之外的运行库)。
- 产物单文件 `dshpet.exe` + 旁挂 `resource/` 与 `config.json`(素材可替换、可删除部分状态)。
- 交付物 = 解压即用的文件夹(或单 zip),无安装器。

---

## 10. 性能设计(R11)

| 项 | 措施 | 量级(实测基准) |
|---|---|---|
| 内存 | 只常驻当前状态一个动画;0.5 缩放后 73 帧 ≈ 30 MB;切换时丢弃旧动画 | 稳态 ~30-40 MB |
| 解码 CPU | 动画解码只在状态首次进入时发生(后台线程);解码完首帧即可显示 | 单动画解码 ~0.5-2 s,不阻塞 UI |
| 渲染 | 每帧一次 memcpy + ULW 上传,288×368×4 ≈ 0.42 MB @ 24 fps ≈ 10 MB/s | CPU 占用可忽略 |
| 渐隐/透明度/灰度 | 状态切换或系数变化时一次性合成,不在每帧热路径(渐隐期间系数逐帧更新,成本 = 一次 0.42 MB alpha 乘法) | 可忽略 |
| 交叉淡化 | 仅切换时 200ms 内做帧混合 | 可忽略 |
| 气泡 | 位图缓存,仅内容变化时重绘;实时文本经连接器节流(150ms)后上抛,重绘 ≤ ~7 fps | 文本重排只在节流边界 |
| 推送热路径 | `assistant/chunk` 在连接器层聚合(而非每 token 上抛),节流后状态机/UI 输入频率 ≤ ~7 Hz;无关高频帧仍白名单丢弃 | 状态机输入频率 ≤ 秒级 |
| 动画帧分配 | 解码期一次性分配帧池,播放期零分配、零解码 | 播放期 CPU ≈ 0 |
| 后期(§12) | 分割帧加载可跳过 webp 解码,启动/切换延迟趋近于 0 | — |

---

## 11. WebP 分割预处理流水线(R12,后期里程碑)

> 背景:webp 单文件 10-12 MB、73 帧无损,运行期解码有 0.5-2 s 延迟;分割后运行期直接读帧,启动即显。

**开发期工具 `tools/split_webp.py`**(复用现有 `out/` 脚本的容器解析逻辑):
- 输入 `resource/<state>.webp` → 输出 `resource/<state>/frame_000.png … frame_072.png` + `manifest.json`。
- 也支持只分割尾部帧区间(裁剪大文件),或输出无损 raw RGBA(加载最快)。

**manifest.json 格式**:
```json
{
  "state": "idle", "width": 576, "height": 736,
  "frame_count": 73, "fps": 24.0,
  "durations_ms": [42, 42, ...],      // 73 项,来自 ANMF
  "tail": { "start": 49, "end": 72 }  // 按 tail_ms=1000 自动计算;idle 忽略
}
```

**运行期加载策略**(anim.rs):
1. 优先:存在 `resource/<state>/manifest.json` → 按 manifest 流式加载 PNG/raw 帧(可只加载当前需要的区间,甚至尾部循环只驻留 24 帧,常驻内存可降到 ~10 MB);
2. 兜底:直接解码 `<state>.webp`(当前方案);
3. 配置开关 `use_split: auto|true|false`。

**收益**:启动/切换零延迟、内存更低、可逐帧调参尾部循环、素材替换只需重跑工具。此阶段同时实测并调优 `tail_frames` 具体值。

---

## 12. 里程碑与验收

| 阶段 | 内容 | 验收 |
|---|---|---|
| M1 骨架 | Rust 工程、分层置顶窗、动画播放器(完整+尾部循环、idle 全循环)、config、托盘、拖拽 | 单 exe 运行,7 个动画均正常播放、尾部循环无跳变,`cargo test` 绿 |
| M2 状态机+连接器 | PetState 移植 + 单测;DSH 连接器(轮询+SSE+chunk 聚合);Hermes 连接器(SQLite 轮询+降级路径) | 连本机 DSH 跑会话,thinking/working/done/fail 切换正确;Hermes 会话同屏正确 |
| M3 气泡+实时输出 | 气泡渲染;后端选择规则;DSH 思考/正文实时显示;Hermes 思考/正文轮询显示 | 双源同时活动时气泡按规则选源;思考内容实时可见;气泡刷新 ≤ 150ms 粒度 |
| M4 渐隐+尺寸+设置 | 无操作 5s 渐隐/悬停渐显;窗口尺寸可调;设置对话框持久化 | 渐隐时序正确、无闪烁;缩放即时生效 |
| M5 防闪烁+可靠性 | 交叉淡化、首帧预解码;SSE 重连/降级轮询、单实例、开机自启 | 快速连续切换状态无闪烁、无黑底;重启 DSH、断网、长挂(≥24h)无卡死;内存/CPU 稳态达标(§10) |
| M6 性能+素材流水线 | split_webp.py + manifest 加载器 + 内存/帧率实测 | 切换状态 <100 ms 出首帧;常驻内存 ≤ 30 MB;尾帧参数定稿 |

> 评审变更:原计划 M6 的"Hermes HTTP/SSE 实时通道"**不采用**——8748 网关依赖 Hermes Studio,Hermes 单独 CLI 使用时不可靠;Hermes 实时性维持 SQLite 增量轮询(1s,已验证 messages 行生成中增量写入)。

---

## 13. 构建环境

- 推荐:Windows 本机 + rustup(MSVC 工具链,`cargo build --release`);无 VS Build Tools 时用 `x86_64-pc-windows-gnu`(MinGW-w64)亦可(仅 rusqlite bundled 需要 C 编译器,image 解码为纯 Rust)。
- 当前机器(WSL 挂载 /mnt/d)可交叉编译 GNU 目标用于快速迭代,正式产物在 Windows 本机构建验证。
- 运行要求:Windows 10 1809+ / Windows 11,无需任何安装。

---

## 14. 风险与开放问题

1. **image crate 动画帧时长**:已确认支持动画 WebP 帧序列,仍需冒烟验证 ANMF 时长与素材一致(不一致则逐帧 41.67 ms 兜底;备选 `webp-animation`(libwebp 绑定,需 C 工具链))。
2. **Hermes 流式粒度未验证**:`messages` 行是否在生成过程中增量更新(实时思考)需真机实测;若非增量 → 降级为"消息完成时显示",或后续 Hermes HTTP SSE(认证可解后)补实时性。
3. **GDI 文本在分层窗无 ClearType**:气泡文字用灰度抗锯齿,低分屏略糊;可选升级 DirectWrite(系统 DLL,不破坏零依赖)。
4. **无损 webp 解码延迟**:0.5-2 s/动画 → 已用"首帧即显 + 渐进解码 + 交叉淡化"缓解,终极方案是 §11 分割加载。
5. **Hermes DB 漂移**(设计文档 §9-5 沿用):表结构探测失败 → 该源降级 offline,不崩溃;本机 schema 已实测存档(§7)。
6. **多源同时 attention/failed 的合并展示**(设计文档 §9-3 沿用):按源分节 + 标题计数合并;气泡选源按 §5.4 规则。
7. **渐隐与 attention 冲突**:等待用户确认时渐隐可能降低存在感 → 提供 `fade_disabled_states` 例外配置,默认仍统一渐隐。
8. **素材版权**:沿用现有已抠图素材,新增素材需自绘或可商用。
9. **尾部循环的观感**:24 帧循环点是否顺滑需真机目检;若跳变明显,可在分割阶段对尾帧做交叉淡化或在素材侧补拍循环帧(开发期可调参数解决)。
10. **路径变动面**:DSH 端口被改、Hermes 换用户账户/设置 `HERMES_WEB_UI_HOME`、或 Hermes 数据目录被迁移 → 连接器按 §8 自动解析 + config 覆盖兜底;解析不到时该源 offline 并在设置面板展示实际解析路径,避免"黑盒连不上"。
