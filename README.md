# Hannis — 独立桌面宠物(Windows)

透明置顶悬浮窗宠物,实时监控 DSH / Hermes / MAA / ComfyUI 以及任意脚本接入的程序
(DSH、Hermes、MAA、ComfyUI 均已由内置连接器迁移为 Lua 脚本,随程序发布)。
单个 `Hannis.exe`,零运行时依赖(仅 Windows 系统 DLL),免安装。

## 快速开始

```
dist/
├─ Hannis.exe         # 主程序(Windows 10 1809+ / 11)
├─ icon.png           # 托盘/窗口图标(缺失时回退为内置圆点)
├─ resource/          # 宠物素材(7 组 sprite sheet: idle/working/think/attention/done/fail/move)
└─ config.json        # 配置(不存在时自动生成默认值)
```

直接双击 `Hannis.exe` 即可。宠物出现在屏幕右下角,悬浮于所有窗口之上。托盘图标显示 `icon.png`(exe 同目录)。

## 功能

| 状态 | 触发 | 表现 |
|---|---|---|
| idle | 无活动 | idle.sheet.png 完整循环 |
| thinking | 会话生成中 | think.sheet.png,气泡实时显示思考内容 |
| working | 工具/任务执行中 | working.sheet.png,气泡显示后端(DSH/Hermes 等)+ 工具名 + todo |
| attention | 等待确认/提问(DSH 审批/提问,或 Hermes `clarify` 提问) | attention.sheet.png,气泡逐条列出 |
| done / fail | 回合完成/出错 | done/fail.sheet.png;事件后**庆祝窗口 4s 强制置顶显示**,之后按窗口期(done 10s / fail 10s)继续 |
| offline | 全部来源(DSH/Hermes/全部脚本)都断 | **idle 第 1 帧 + 灰度**(静态) |
| move | 拖动 | move.sheet.png |

- **播放**:非 idle 状态播完动作一遍后,优先切换到**独立的循环动画** `resource/<state>_loop.sheet.png`(动作播放期间已预解码,切换无延迟);没有 loop 素材时自动回退为"循环尾部 ~1s"(`tail_ms`,可调);idle 完整重播。所有帧按统一时长播放(`display.frame_ms`,默认 42ms/帧,可调)
- **记住位置**:拖拽宠物到新位置后自动写入 config.json 的 `window_pos`;下次启动在屏幕内恢复该位置(不在时回退右下角锚点)
- **渐隐**:无操作 5 秒后渐隐至 70%(参数可调),鼠标悬停/拖拽即恢复;**状态切换会取消渐隐、立即恢复不透明并重新计时**(默认仅 attention 不参与渐隐,见 `fade_disabled_states`)
- **done/fail 可见性**:回合完成/出错后先强制显示 4 秒(庆祝窗口),再进入低优先级窗口期;即使 agent 立刻开始下一个回合也能看到;气泡显示**对应任务名**(会话标题→最近用户消息→todo 兜底)
- **字体/气泡缩放**:文字与气泡尺寸按系统 DPI(100%/125%/150%…)自动缩放,高分屏不偏小;可用 `bubble.font_scale` 再整体放大/缩小
- **气泡稳定性**:流式单行气泡高度固定(最多 4 行),文字滚动更新时窗口不抖动;窗口尺寸变化始终以宠物底部为锚,宠物本体不会移动
- **逐字出现**:thinking/working 的实时文字按 `bubble.type_cps`(默认 90 字/秒)逐字"打字"出现,流太快时会自动追赶到最新内容;设为 0 恢复直接显示
- **文字渲染**:气泡文字用 GDI **2× 超采样**绘制(所有光栅化在 2 倍分辨率进行,再盒式滤波缩回 1×,边缘比单次原生渲染更平滑、对比度更高),双通道提取(白字黑底提取抗锯齿覆盖率 + 彩色字黑底取预乘颜色,按 alpha "over" 合成)。成块结果按内容缓存,只在文字变化时重新光栅化(打字机逐字出现时也只在变化帧重绘)。字体为 Microsoft YaHei UI(14px × 系统 DPI × `font_scale`)。透明置顶窗口无法使用 ClearType 亚像素渲染(会有彩边),这是与网页文字观感差异的主要来源
- **资源占用**:帧内存用**紧凑格式**——调色板 PNG 素材直接按 1 字节/像素索引 + tRNS alpha 表解码(与 RGBA 解码逐字节一致),RGBA 素材在加载时量化为 ≤256 色、**逐像素 alpha 保持精确**;800×800 素材单帧从 2.4 MiB 降到 0.6–1.3 MiB(idle 100 帧 ≈ 61 MiB,加载峰值减半)。绘制采用**脏标记渲染**:只在动画帧变化/气泡文字变化/渐隐过渡/模式切换时重绘,静止时几乎零 CPU(不再以 66fps 全量重绘);每帧按 alpha 包围盒裁剪像素循环
- **气泡主题与状态色**:卡片按 `bubble.theme` 渲染——`dark` 切换深色预设(深色半透明底+亮字,游戏/深色桌面场景);左侧 4px **状态色条** + 「From X」状态色胶囊,标题在 done/fail/attention 时也随状态色(淡蓝=思考/蓝=干活/金=完成/红=出错/橙=等待);所有颜色/圆角/阴影可用 `bubble.theme` 覆盖(`#RRGGBB`,非法值自动回退预设);`acrylic: true` 尝试 **DWM 系统毛玻璃**(Win10+,实验性,失败自动回退)。气泡出现时 200ms **淡入+从宠物方向滑入**
- **气泡**:现代卡片风格——1px 浅灰微边框 + **向四周弥散的柔和阴影**(CSS `box-shadow: 0 20px 40px rgba(0,0,0,.15)` 风格,软件高斯模糊) + 半透明白底。内容分三层:标题行(状态名居左,如「思考中…」;**"From DSH/Hermes" 客户端名右对齐**)、1px 分割线、分割线下的信息流。思考时**滚动显示思考文字流(显示最新内容尾部)**,工作时显示**正在执行的工具+实际内容**(DSH 为工具参数;Hermes 为工具结果输出,该版本无参数列);**双方都休息或全部断线时不显示气泡**
- **MAA / ComfyUI 内置源已脚本化**:MAA 与 ComfyUI 不再由内置 Rust 连接器提供,改为随程序发布的 Lua 脚本(行为与原内置版一致):
  - `scripts/maa.lua` — 监控 MAA `debug/gui.log`,由 `args.log` 指定路径;`args.attention_ms` 控制资深干员 attention 保持时长(默认 3000ms);`args.stream=true`(默认)时**任务期间的用户可见日志行**([TaskQueueViewModel],如「理智作战」的 开始行动/掉落统计、公招识别结果 等)会组装成信息流显示在气泡里,直到下一次 done/fail/attention。行为:正在连接模拟器=thinking、开始任务=working、任务链完成(`Idle: false to true (called from ProcTaskChainMsg)`)=done(整条链一次,链内「完成任务」行不单独触发)、已停止=fail、…资深干员…=attention(约 `attention_ms` 后自动消除);日志清空/截断=运行边界;启动时恢复 30 分钟内的未完成链
  - `scripts/comfyui.lua` — 轮询 ComfyUI `/queue` + `/history`(经 `pet.http`,无需外部工具),有任务在跑=working、成功=done、出错=fail、队列深度进气泡;服务不可达=该源不健康
  - `scripts/dsh.lua` / `scripts/hermes.lua` — DSH 与 Hermes 连接器同样由内置 Rust 迁移为脚本(行为与原内置版一致,线格式见 `scripts-guide.md` §9)
  - 四者都由 config.json `scripts` 数组注册(默认配置已含),与其它脚本一样会写 `hannis.log` 便于排查
- **窗口**:Ctrl+滚轮缩放(0.25–2.0),拖拽移动;托盘右键可切换「回避模式」「自动收起」与「退出」(其余配置走 config.json)
- **Lua 脚本接入(开放接口)**:在 config.json 的 `scripts` 数组注册任意 `.lua` 脚本即可把**任何程序**接入宠物——内嵌 Lua 5.4 已静态链接进 exe,用户只需会写 Lua、**无需安装任何运行时**。脚本在自己线程里轮询,通过 `pet.*` API 驱动状态机(与内置连接器同一套事件契约):`session_started/ended`、`session_status`、`tool_started/ended`、`live_text`、`question/answer`、`approval_requested/resolved`、`pending_sync`、`poll`、`todo`、`queue`、`health`、`log`、`wait`、`config`、`http/http_post/ws/sqlite`。脚本编译/运行出错只下线该脚本源,不影响宠物;`"sandbox": true` 可禁用文件/进程/网络访问。详见 `scripts-guide.md`(含 DSH/Hermes 线格式参考),示例:`scripts/tail_log.lua`(通用日志监控,关键词自定义)与 `scripts/process_watch.lua`(进程监控)
- **自动收起(auto-hide)**:托盘勾选「自动收起」后,idle/offline 持续超过 `auto_hide.after_sec`(默认 30 秒),宠物从原位**向下滑动**到任务栏区域(`slide_speed` px/s,默认 600,y = 屏幕高度 − `y_factor` × 窗口高度,默认 0.4)。**保持置顶**——头仍悬浮在所有窗口之上,被任务栏盖住的身体部分裁剪为透明(任务栏从透明区透出,看起来就是"身体被任务栏挡住");透明度在渐隐基础上再乘 `opacity`(默认 0.3);**鼠标点击穿透**(WS_EX_TRANSPARENT,可正常操作其下方的任务栏/桌面)。退出条件:有新消息(状态离开 idle/offline)或鼠标悬停超过 `hover_sec`(默认 3 秒,可设置)——**滑回原位**、恢复不透明度与完整绘制。纯窗口样式+位置实现,无额外权限/依赖
- **滚轮拦截**:宠物上的滚轮事件一律被吞掉,不会透传给下层应用(Ctrl+滚轮=缩放宠物)
- **回避模式(avoid)**:开启后宠物会自动躲开鼠标——光标靠近到 `avoid.distance` 范围内时,宠物快速跳开到 `avoid.shift` 像素外并保持;光标移出 `distance × hysteresis` 范围后,宠物平滑滑回原位。可在托盘右键勾选/取消,即时生效并写回 config.json
- **单实例**:重复启动会直接退出

## 配置(config.json)

文件位于 exe 同目录,首次运行自动生成默认值;修改后**重启程序生效**。完整字段:

```jsonc
{
  // ---- Lua 脚本(所有来源都是脚本:DSH/Hermes/MAA/ComfyUI 内置版已迁移,
  //      见 scripts/ 与 scripts-guide.md)----
  "scripts": [
    {
      "name": "DSH",                            // 气泡 "From DSH"
      "file": "scripts/dsh.lua",                // DSH 连接器(session.list/history + events.mux/host)
      "poll_ms": 1000,
      "args": {
        "url": "http://127.0.0.1:3080",         // DSH 地址;env DSH_PET_URL 优先
        "poll_ms": 2000,                        // session.list 轮询间隔(ms)
        "history_ms": 1000                      // session.history 轮询间隔(ms):实时思考/输出流与回合/工具事件的刷新粒度
      }
    },
    {
      "name": "Hermes",                         // 气泡 "From Hermes"
      "file": "scripts/hermes.lua",             // Hermes 连接器(只读 SQLite 轮询)
      "poll_ms": 1000,
      "args": {
        "db_path": null,                        // Hermes 数据库路径;null=自动解析:
                                                //   env HERMES_WEB_UI_HOME(若设置) → %USERPROFILE%\.hermes-web-ui\hermes-web-ui.db
                                                //   解析失败则该源离线
        "poll_ms_active": 1000,                 // 有活跃会话时的轮询间隔(ms)——决定思考文本刷新频率
        "poll_ms_idle": 2000                    // 空闲时的轮询间隔(ms)
      }
    },
    {
      "name": "MAA",                            // 气泡 "From MAA"
      "file": "scripts/maa.lua",                // 监控 MAA gui.log
      "poll_ms": 1000,
      "args": {
        "log": "D:\\MeoAssistantArknights\\debug\\gui.log",
        "attention_ms": 3000                    // 资深干员 attention 保持时长(ms)
      }
    },
    {
      "name": "ComfyUI",                        // 气泡 "From ComfyUI"
      "file": "scripts/comfyui.lua",            // 轮询出图队列(经 pet.http)
      "poll_ms": 2000,
      "args": { "url": "http://127.0.0.1:8188" }
    }
  ],

  // ---- 自动收起(idle/offline 太久把宠物收到任务栏后;托盘右键可勾选) ----
  "auto_hide": {
    "enabled": false,         // 总开关;托盘勾选/取消即时切换,重启后保持
    "after_sec": 30,          // idle/offline 持续多少秒后收起
    "y_factor": 0.4,          // 收起位置:y = 屏幕高度 − y_factor × 窗口高度
    "opacity": 0.3,           // 收起时额外透明度(0.05–1.0,叠加在渐隐系数上)
    "hover_sec": 3,           // 鼠标悬停宠物超过该秒数即恢复
    "slide_speed": 600        // 收起/恢复滑动速度(px/s)
  },

  // ---- 自定义 Lua 接入口(开放接口,详见 scripts-guide.md;在 scripts 数组里追加即可)----
  // {
  //   "name": "MyGame",               // 气泡 "From MyGame"
  //   "file": "scripts/tail_log.lua", // 脚本路径(相对 exe 目录)
  //   "poll_ms": 1000,                // 提示值;脚本用 pet.config().poll_ms 读
  //   "sandbox": false,               // true=禁用文件/进程访问
  //   "args": { "log": "D:\\MyGame\\game.log" }  // 脚本通过 pet.config().args 读
  // },

  // ---- 显示与动画 ----
  "display": {
    "scale": 1.0,             // 宠物显示比例(0.25–2.0);Ctrl+滚轮调整后会自动写回此值
    "tail_ms": 1000,          // 非 idle 状态播放完整一遍后,循环尾部多长(ms)
    "tail_frames": null,      // 开发调参:直接指定尾部循环帧数(优先级高于 tail_ms);null=按 tail_ms 计算
    "frame_ms": 42            // 统一每帧时长(ms,1–2000):sheet 不再存每帧时长,所有帧按此值播放
  },

  // ---- 窗口位置记忆 ----
  "window_pos": {             // 拖拽宠物后自动写回;启动时恢复(越界自动夹回屏幕内)
    "x": null,                // null=未记录,使用默认右下角锚点
    "y": null
  },

  // ---- 渐隐(无操作自动变透明) ----
  "fade": {
    "fade_after_sec": 5,      // 鼠标离开宠物多少秒后开始渐隐
    "fade_target": 0.7,       // 渐隐目标透明度(0.0=完全消失,1.0=不隐)
    "fade_ms": 1200,          // 渐隐/渐显过渡时长(ms)
    "fade_disabled_states": ["attention"] // 不参与渐隐的状态,如 ["attention"] 表示等待确认时保持不透明
  },

  // ---- 各状态基础透明度(与渐隐系数相乘,默认全 1.0) ----
  // 取值范围 0.0–1.0;例如想 idle 半透明: "idle": 0.5
  "opacity": {
    "idle": 1.0, "working": 1.0, "thinking": 1.0, "attention": 1.0,
    "done": 1.0, "fail": 1.0, "move": 1.0, "offline": 1.0
  },

  // ---- 气泡 ----
  "bubble": {
    "throttle_ms": 150,       // 实时文本(思考/正文)刷新节流(ms),越小越跟手、越费 CPU
    "max_text_len": 600,      // 每条实时文本最多保留字符数,超出截断加 …
    "exempt_from_fade": true, // 气泡不随宠物渐隐(保持可读)
    "font_scale": 1.0,        // 气泡字体额外缩放(0.5–2.5,叠加系统 DPI)
    "type_cps": 90,           // 打字机效果:实时文字每秒逐字出现的速度(字符/秒);0=直接显示全部
    "theme": {                // 气泡主题:dark 换深色预设;各字段 None=预设值;颜色为 "#RRGGBB"
      "dark": false,          // 深色预设(游戏/深色桌面场景推荐)
      "acrylic": false,       // DWM 系统毛玻璃(Win10+ 实验性,失败自动回退)
      "fill": null,           // 卡片底色
      "fill_alpha": null,     // 卡片底色透明度(0-255)
      "border": null,         // 边框色
      "border_alpha": null,
      "divider": null,        // 分割线色
      "divider_alpha": null,
      "title": null,          // 标题色
      "from": null,           // "From X" 胶囊文字色(状态色,默认白字)
      "shadow_alpha": null,   // 阴影强度
      "radius": null,         // 圆角(px)
      "state_colors": null    // 如 { "working": "#4A8FE7", "done": "#E8A33D", ... }
                              // thinking/working/done/fail/attention/neutral
    }
  },

  // ---- 状态窗口期 ----
  "windows": {
    "done_sec": 10,           // 完成后保持 done 动画的秒数(低优先级,被后续工作覆盖)
    "fail_sec": 10,           // 出错后保持 fail 动画的秒数(低优先级)
    "celebrate_sec": 4        // done/fail 事件后的"庆祝窗口":强制置顶显示几秒,保证可见
  },

  // ---- 回避模式(宠物躲开鼠标;托盘右键勾选可实时开关并写回此节) ----
  "avoid": {
    "enabled": false,         // 总开关:false=关闭;托盘勾选/取消即时切换,重启后保持
    "distance": 190,          // 触发半径(px):光标进入宠物"原位矩形"周围多近开始躲
    "shift": 380,             // 躲开距离(px):宠物沿"指向鼠标的反方向"跳开的距离
    "hysteresis": 1.6,        // 返回阈值系数:光标移出 distance×hysteresis 才回原位(防抖)
    "dodge_speed": 2600,      // 躲避移动速度(px/s):大=瞬间弹开,小=缓缓挪走
    "return_speed": 700       // 回原位速度(px/s):小=悠悠滑回去,大=加速归位
  },

  // ---- 开机自启 ----
  "autostart": false          // true=写入 HKCU\Software\Microsoft\Windows\CurrentVersion\Run
}
```

**优先级说明**:实际透明度 = 状态 `opacity` × 渐隐系数;渐隐系数在"鼠标离开超过 fade_after_sec"后从 1.0 向 `fade_target` 过渡。`offline` 状态固定显示 idle 第 1 帧的灰度图(透明度仍按上式)。

**素材替换**:直接替换 `resource/` 下的 sprite sheet 即可(`<状态>.sheet.png` + `<状态>.sheet.json`,文件名即状态名);删除某个状态的素材会导致该状态空白。PNG 格式**两种都支持**:调色板(索引)PNG 更省内存(1 字节/像素 + tRNS alpha),RGBA PNG 会在加载时自动量化为 ≤256 色(alpha 无损)——建议用 `tools/make_sheets.js` 的 `--palette 256` 参数生成,效果与内置素材一致。**循环动画**:对任意非 idle 状态提供 `resource/<state>_loop.sheet.*`,动作播完一遍后自动无缝切到该循环(要求 loop 首帧衔接动作末帧、loop 自身无缝);不提供则回退尾部循环。sheet 由 `tools/make_sheets.js`(或 `tools/split_webp.py`)从源 webp 生成;运行期不再加载 webp。

**环境变量**（由 DSH/Hermes 脚本读取，见 `scripts-guide.md` §9.4）:
| 变量 | 作用 |
|---|---|
| `DSH_PET_URL` | 覆盖 DSH 地址(优先于 `scripts` 里 DSH 的 `args.url`) |
| `HERMES_WEB_UI_HOME` | 覆盖 Hermes 数据目录(优先于 `scripts` 里 Hermes 的 `args.db_path`) |

**图标**:托盘与窗口图标读取 exe 同目录的 `icon.png`(任意尺寸,按 32×32 缩放);缺失或解码失败时回退为内置的绿色圆点图标。

**Hermes 数据通道说明**:Hermes 连接器(现为 `scripts/hermes.lua`)只读 Hermes 的
SQLite 数据库(`messages` 行在生成中增量写入:思考内容先行、正文随后,1s 轮询≈准流式),
**不依赖 Hermes Studio 的 HTTP 网关**(8748)——Hermes 单独以 CLI 方式运行时同样可用。
`args.db_path` 为空时自动解析数据目录(env `HERMES_WEB_UI_HOME` → 用户主目录)。

**Hermes 等待确认检测**:Hermes 调用交互工具 `clarify`(向用户提问/给出选项)时,数据库里会先出现一条 `finish_reason=tool_calls` 的 assistant 消息,答案结果行要等用户回复后才写入——宠物把这段"提问未答"的空档识别为 **attention(等待确认)**,气泡列出问题与选项;用户回答(结果行落库)或会话结束后自动恢复。普通工具调用(非 clarify)不会误判为等待确认。

## 冒烟验证(Windows)

1. 启动 DSH(127.0.0.1:3080)与 Hermes Studio,再启动 Hannis.exe
2. 无活动 → idle 动画循环;鼠标移开后 5s 渐隐,移回恢复
3. 让任一 agent 跑一个会话 → 气泡出现 [DSH]/[Hermes] 标签、思考内容实时滚动,宠物切 thinking/working
4. 关闭 DSH 和 Hermes → 宠物变灰度静态(idle 首帧);重启任一 → 恢复动画(各来源均为 Lua 脚本,单源断线只让该源下线)
5. 拖拽宠物 → move.sheet.png(切换动画不消失);**拖拽松手后位置写入 config 的 window_pos,重启后恢复**;Ctrl+滚轮缩放(滚轮不会影响下层应用);修改 config.json 后重启生效
6. 托盘右键勾选「回避模式」→ 鼠标靠近宠物,宠物弹开到一侧并持续躲开;鼠标移远后宠物滑回原位;再在托盘取消勾选即关闭
7. (MAA)启动 MAA 跑任务链 → 宠物跟随后台:正在连接模拟器=think,开始任务=working,任务链完成=done(整条链一次),已停止=fail;任务管理器确认 idle 时内存 ≈80MB、CPU 基本归零
8. (接入口设置)托盘 → "接入口设置…" → 每个脚本一行(启停药丸 + 参数编辑框,来自脚本内 `--[hannis:set]` 声明),保存后写回 config.json 并热重启对应脚本

## 构建

### Windows 本机(推荐,需 rustup + MSVC 或 GNU 工具链)

```powershell
cd app
cargo build --release
# 产物: target\release\hannis.exe
# 运行前把 resource\、icon.png 和 config.json 放在 exe 同目录
```

### WSL 交叉编译(本仓库 .tools 已内置工具链)

```bash
source .tools/env.sh
cd app
cargo build --release --target x86_64-pc-windows-gnu   # 或见 build.ps1 等价命令
```

### 测试

```bash
cd app && cargo test    # 单测:状态机/播放调度/sheet 加载/脚本连接器/气泡文本/打字机
# Linux 下可直接对真实 DSH 联调(config.json 的 scripts 会全部启动):
cd app && cargo run -- --self-test   # 素材解码自检
timeout 20 ./target/debug/hannis     # headless:打印状态切换(连接真实 DSH/Hermes)
DSH_PET_DEBUG=1 ./target/debug/hannis  # 调试:打印每个脚本源的事件(排查流式问题)
```

## 素材打包

运行期**只加载 sprite sheet**(`resource/<state>.sheet.png` + `.sheet.json`,单文件一次解码、启动零延迟)。
sheet 由源 webp 生成:

```bash
node tools/make_sheets.js            # 需要 sharp;全部 *.webp 打包为 sheet
python tools/split_webp.py           # 等价工具(Pillow);生成 resource/<state>.sheet.*
node tools/mp4_to_sheet.js --name lucky   # 绿幕 mp4 直接产出 sheet(去绿幕+量化,核心在 tools/keyer.js)
node web/mp4-keyer/server.js         # 网页版:上传 mp4 -> 预览/下载(默认 http://127.0.0.1:3137)
```

sheet.json 只含几何字段(`width`/`height`/`frame_count`/`frames_per_row`);每帧时长统一由
`display.frame_ms` 决定,不再逐帧写入 JSON。

## 目录

```
app/            Rust 源码(lib 纯逻辑可测 + gui Windows 部分;可执行名 hannis)
tools/          make_sheets.js / split_webp.py 素材打包工具(webp -> sprite sheet)
                mp4_to_sheet.js + keyer.js 绿幕 mp4 -> sprite sheet(CLI,与网页共用管线)
web/mp4-keyer/  绿幕 mp4 转 sprite sheet 的本地网页工具(上传/预览/下载,node server.js)
resource/       宠物素材(sprite sheet)
scripts/        随程序发布的 Lua 示例脚本(maa/comfyui/tail_log/process_watch)
icon.png        程序图标(托盘/窗口;部署时放到 exe 同目录)
out/            素材处理历史脚本(绿幕抠除等)
adding-connectors.md   给程序添加新消息来源(连接器)的接入指南
```
