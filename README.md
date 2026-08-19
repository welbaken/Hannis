# Hannis — 独立桌面宠物(Windows)

透明置顶悬浮窗宠物,实时监控 DSH 与 Hermes 的 Agent 工作状态。
单个 `Hannis.exe`,零运行时依赖(仅 Windows 系统 DLL),免安装。

## 快速开始

```
dist/
├─ Hannis.exe         # 主程序(Windows 10 1809+ / 11)
├─ icon.png           # 托盘/窗口图标(缺失时回退为内置圆点)
├─ resource/          # 宠物素材(7 个 webp: idle/working/think/attention/done/fail/move)
└─ config.json        # 配置(不存在时自动生成默认值)
```

直接双击 `Hannis.exe` 即可。宠物出现在屏幕右下角,悬浮于所有窗口之上。托盘图标显示 `icon.png`(exe 同目录)。

## 功能

| 状态 | 触发 | 表现 |
|---|---|---|
| idle | 无活动 | idle.webp 完整循环 |
| thinking | 会话生成中 | think.webp,气泡实时显示思考内容 |
| working | 工具/任务执行中 | working.webp,气泡显示后端(DSH/Hermes)+ 工具名 + todo |
| attention | 等待确认/提问(DSH 审批/提问,或 Hermes `clarify` 提问) | attention.webp,气泡逐条列出 |
| done / fail | 回合完成/出错 | done/fail.webp;事件后**庆祝窗口 4s 强制置顶显示**,之后按窗口期(done 10s / fail 10s)继续 |
| offline | DSH 与 Hermes 都断 | **idle 第 1 帧 + 灰度**(静态) |
| move | 拖动 | move.webp |

- **播放**:非 idle 状态播完动作一遍后,优先切换到**独立的循环动画** `resource/<state>_loop.webp`(动作播放期间已预解码,切换无延迟);没有 loop 文件时自动回退为"循环尾部 ~1s"(`tail_ms`,可调);idle 完整重播
- **渐隐**:无操作 5 秒后渐隐至 70%(参数可调),鼠标悬停/拖拽即恢复;**状态切换会取消渐隐、立即恢复不透明并重新计时**(默认仅 attention 不参与渐隐,见 `fade_disabled_states`)
- **done/fail 可见性**:回合完成/出错后先强制显示 4 秒(庆祝窗口),再进入低优先级窗口期;即使 agent 立刻开始下一个回合也能看到;气泡显示**对应任务名**(会话标题→最近用户消息→todo 兜底)
- **字体/气泡缩放**:文字与气泡尺寸按系统 DPI(100%/125%/150%…)自动缩放,高分屏不偏小;可用 `bubble.font_scale` 再整体放大/缩小
- **气泡稳定性**:流式单行气泡高度固定(最多 4 行),文字滚动更新时窗口不抖动;窗口尺寸变化始终以宠物底部为锚,宠物本体不会移动
- **逐字出现**:thinking/working 的实时文字按 `bubble.type_cps`(默认 90 字/秒)逐字"打字"出现,流太快时会自动追赶到最新内容;设为 0 恢复直接显示
- **文字渲染**:气泡文字用 GDI 双通道绘制(白字黑底提取抗锯齿覆盖率 + 彩色字黑底取预乘颜色,再按 alpha "over" 合成),边缘平滑不发虚;字体为 Microsoft YaHei UI(14px × 系统 DPI × `font_scale`)。透明置顶窗口无法使用 ClearType 亚像素渲染(会有彩边),这是与网页文字观感差异的主要来源
- **气泡**:宠物头顶**单行**显示实际工作内容——思考时**滚动显示思考文字流(显示最新内容尾部)**,工作时显示**正在执行的工具+实际内容**(DSH 为工具参数;Hermes 为工具结果输出,该版本无参数列);带 [DSH]/[Hermes] 后端标记;**双方都休息或全部断线时不显示气泡**
- **窗口**:Ctrl+滚轮缩放(0.25–2.0),拖拽移动;托盘右键可切换「回避模式」与「退出」(其余配置走 config.json)
- **滚轮拦截**:宠物上的滚轮事件一律被吞掉,不会透传给下层应用(Ctrl+滚轮=缩放宠物)
- **回避模式(avoid)**:开启后宠物会自动躲开鼠标——光标靠近到 `avoid.distance` 范围内时,宠物快速跳开到 `avoid.shift` 像素外并保持;光标移出 `distance × hysteresis` 范围后,宠物平滑滑回原位。可在托盘右键勾选/取消,即时生效并写回 config.json
- **单实例**:重复启动会直接退出

## 配置(config.json)

文件位于 exe 同目录,首次运行自动生成默认值;修改后**重启程序生效**。完整字段:

```jsonc
{
  // ---- DSH 连接器 ----
  "dsh": {
    "url": "http://127.0.0.1:3080",  // DSH 地址;环境变量 DSH_PET_URL 优先于此处
    "poll_ms": 2000                  // session.list 轮询间隔(ms)
    "history_ms": 1000               // session.history 轮询间隔(ms):DSH 实时思考/输出文字流与回合/工具事件的刷新粒度
  },

  // ---- Hermes 连接器 ----
  "hermes": {
    "db_path": null,          // Hermes 数据库路径;null=自动解析:
                              //   env HERMES_WEB_UI_HOME(若设置) → %USERPROFILE%\.hermes-web-ui\hermes-web-ui.db
                              // 解析失败则该源离线(设置面板已移除,改这里或环境变量)
    "poll_ms_active": 1000,   // 有活跃会话时的轮询间隔(ms)——决定思考文本刷新频率
    "poll_ms_idle": 2000      // 空闲时的轮询间隔(ms)
  },

  // ---- 显示与动画 ----
  "display": {
    "scale": 1.0,             // 宠物显示比例(0.25–2.0);Ctrl+滚轮调整后会自动写回此值
    "tail_ms": 1000,          // 非 idle 状态播放完整一遍后,循环尾部多长(ms)
    "tail_frames": null,      // 开发调参:直接指定尾部循环帧数(优先级高于 tail_ms);null=按 tail_ms 计算
    "use_split": "auto"       // 素材加载:"auto"=存在 resource/<状态>/manifest.json 时用分割帧,否则直解 webp;
                              //   "true"=强制分割帧(缺失则报错);"false"=总是直解 webp
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
    "exempt_from_fade": true  // 气泡不随宠物渐隐(保持可读)
    "font_scale": 1.0,        // 气泡字体额外缩放(0.5–2.5,叠加系统 DPI)
    "type_cps": 90            // 打字机效果:实时文字每秒逐字出现的速度(字符/秒);0=直接显示全部
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
    "distance": 140,          // 触发半径(px):光标进入宠物"原位矩形"周围多近开始躲
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

**素材替换**:直接替换 `resource/` 下的 webp 即可(文件名即状态名);删除某个状态的素材会导致该状态空白。**循环动画**:对任意非 idle 状态提供 `resource/<state>_loop.webp`(或分割帧目录 `<state>_loop/`),动作播完一遍后自动无缝切到该循环(要求 loop 首帧衔接动作末帧、loop 自身无缝);不提供则回退尾部循环。分割帧模式见下文。

**环境变量**:
| 变量 | 作用 |
|---|---|
| `DSH_PET_URL` | 覆盖 DSH 地址(优先于 config 的 dsh.url) |
| `HERMES_WEB_UI_HOME` | 覆盖 Hermes 数据目录(优先于 config 的 hermes.db_path) |

**图标**:托盘与窗口图标读取 exe 同目录的 `icon.png`(任意尺寸,按 32×32 缩放);缺失或解码失败时回退为内置的绿色圆点图标。

**Hermes 数据通道说明**:宠物只读 Hermes 的 SQLite 数据库(`messages` 行在生成中增量写入:思考内容先行、正文随后,1s 轮询≈准流式),**不依赖 Hermes Studio 的 HTTP 网关**(8748)——Hermes 单独以 CLI 方式运行时同样可用。`hermes.db_path` 为空时自动解析数据目录。

**Hermes 等待确认检测**:Hermes 调用交互工具 `clarify`(向用户提问/给出选项)时,数据库里会先出现一条 `finish_reason=tool_calls` 的 assistant 消息,答案结果行要等用户回复后才写入——宠物把这段"提问未答"的空档识别为 **attention(等待确认)**,气泡列出问题与选项;用户回答(结果行落库)或会话结束后自动恢复。普通工具调用(非 clarify)不会误判为等待确认。

## 冒烟验证(Windows)

1. 启动 DSH(127.0.0.1:3080)与 Hermes Studio,再启动 Hannis.exe
2. 无活动 → idle 动画循环;鼠标移开后 5s 渐隐,移回恢复
3. 让任一 agent 跑一个会话 → 气泡出现 [DSH]/[Hermes] 标签、思考内容实时滚动,宠物切 thinking/working
4. 关闭 DSH 和 Hermes → 宠物变灰度静态(idle 首帧);重启任一 → 恢复动画
5. 拖拽宠物 → move.webp(切换动画不消失);Ctrl+滚轮缩放(滚轮不会影响下层应用);修改 config.json 后重启生效
6. 托盘右键勾选「回避模式」→ 鼠标靠近宠物,宠物弹开到一侧并持续躲开;鼠标移远后宠物滑回原位;再在托盘取消勾选即关闭

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
cd app && cargo test    # 66 个单测:状态机/播放调度/webp 解码/分割往返/连接器帧解析/打字机
# Linux 下可直接对真实 DSH 联调:
cd app && cargo run -- --self-test   # 素材解码自检
timeout 20 ./target/debug/hannis     # headless:打印状态切换(连接真实 DSH/Hermes)
DSH_PET_DEBUG=1 ./target/debug/hannis  # 调试:打印每个连接器事件(排查流式问题)
```

## 素材分割(可选优化)

`resource/<state>.webp`(10-12MB/个)可在开发期分割为帧 PNG,运行期加载零解码延迟:

```bash
python tools/split_webp.py        # 需要 Pillow;生成 resource/<state>/frame_*.png + manifest.json
```

存在 `resource/<state>/manifest.json` 时程序自动优先使用分割帧(`display.use_split: "auto"`)。

## 目录

```
app/            Rust 源码(lib 纯逻辑可测 + gui Windows 部分;可执行名 hannis)
tools/          split_webp.py 素材分割工具
resource/       宠物素材(webp)
icon.png        程序图标(托盘/窗口;部署时放到 exe 同目录)
out/            素材处理历史脚本(绿幕抠除等)
desktop-pet-*-plan.md / desktop-pet-monitor-design.md   设计与方案
```
