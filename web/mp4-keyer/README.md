# mp4-keyer — 绿幕 mp4 → sprite sheet 网页工具

上传绿幕 mp4,自动去绿幕 + 调色板量化,生成与 dshpet `resource/` 同格式的
`.sheet.png` / `.sheet.json`,并在浏览器里预览、下载。处理管线与
`tools/mp4_to_sheet.js`(CLI)共用 `tools/keyer.js`,结果一致。

## 依赖(处理视频的那台电脑需要安装)

| 依赖 | 版本要求 | 安装方式 |
| --- | --- | --- |
| Node.js | ≥ 20.9 | Windows: [nodejs.org](https://nodejs.org) LTS 安装包;Linux: `apt install nodejs npm`(Ubuntu 24.04 自带 18,需用 nodesource/nvm 装 ≥20.9) |
| ffmpeg | 任意较新版本 | Windows: `winget install Gyan.FFmpeg`;Ubuntu/Debian: `apt install ffmpeg`;macOS: `brew install ffmpeg` |
| sharp | 随 npm 安装 | 见下 |

安装 ffmpeg 后工具会自动找到它(也支持 `--ffmpeg <路径>` / 环境变量 `FFMPEG`
显式指定)。Node 只用于起本地服务和跑 CLI;`resource/` 素材在 Windows 桌面宠物
运行期加载时**不需要** Node。

## 安装与启动

```powershell
# Windows / 任意平台,在本目录(web/mp4-keyer)执行:
npm install          # 安装 sharp(唯一 npm 依赖)

node server.js       # 启动,默认 http://127.0.0.1:3137
# 或 npm start
```

CLI 版同样只缺 sharp:在仓库根执行 `npm install sharp` 后即可用
`node tools/mp4_to_sheet.js --name lucky`。

## 使用

打开 http://127.0.0.1:3137 → 拖入 mp4 → 调整参数(可选)→ 开始处理 →
预览动画 → 下载 `.sheet.png` / `.sheet.json` 放入 `resource/`。

结果只保留 2 小时(自动清理);上传上限 1GB;处理任务单并发。

## 常见问题

- **找不到 ffmpeg**:安装后重开终端;或 `node server.js` 前设置
  `$env:FFMPEG="C:\path\to\ffmpeg.exe"`。
- **找不到 sharp**:确认当前目录是 `web/mp4-keyer` 且执行过 `npm install`。
- **端口被占**:`node server.js 8080` 或环境变量 `PORT=8080`。
