#!/usr/bin/env node
// dshpet 绿幕 mp4 -> sprite sheet 本地网页服务。
//
// 页面:上传 mp4 -> 服务端 ffmpeg 解码 + keyer.js 去绿幕/量化/打包 ->
//       浏览器端 canvas 动画预览 -> 下载 .sheet.png / .sheet.json / QA 预览条。
// 无第三方 Node 依赖(sharp/ffmpeg 复用 tools/keyer.js 的查找逻辑,与 CLI 同一套管线)。
//
// 启动:  node web/mp4-keyer/server.js [port]     (默认 127.0.0.1:3137)
// 环境变量: PORT / FFMPEG / FFPROBE
//
// API:
//   POST /api/process             body = mp4 字节流;头: x-filename/x-name/x-key/x-dlow/x-dhigh/x-despill/x-palette
//                                 返回 { job_id }
//   GET  /api/job/<id>            { status, stage, percent, ... | done 元数据 + files }
//   GET  /api/result/<id>/<file>  下载(<name>.sheet.png / <name>.sheet.json / <name>.preview.png)
const http = require('http');
const path = require('path');
const fs = require('fs');
const crypto = require('crypto');
const keyer = require('../../tools/keyer.js');

const PORT = Number(process.env.PORT || process.argv[2] || 3137);
const HOST = process.env.HOST || '127.0.0.1';
const ROOT = __dirname;
const DATA = path.join(ROOT, 'data');
const RESULTS = path.join(DATA, 'results');
const UPLOADS = path.join(DATA, 'uploads');
const MAX_UPLOAD = 1024 * 1024 * 1024;      // 1GB
const RESULT_TTL_MS = 2 * 3600 * 1000;      // 结果保留 2 小时
const MIME = { '.png': 'image/png', '.json': 'application/json; charset=utf-8', '.html': 'text/html; charset=utf-8', '.js': 'text/javascript; charset=utf-8', '.css': 'text/css; charset=utf-8' };

fs.mkdirSync(RESULTS, { recursive: true });
fs.mkdirSync(UPLOADS, { recursive: true });

const jobs = new Map();   // id -> job
const queue = [];         // 处理队列(单并发,避免内存峰值叠加)
let running = false;

// ------------------------------------------------------------------ 工具

function sanitizeName(s) {
  const base = String(s || '')
    .replace(/\.[^.]+$/, '')               // 去掉扩展名
    .replace(/[^\w\u4e00-\u9fa5.-]+/g, '_') // 保留中文与 \w,其余折叠为 _
    .replace(/[_]+/g, '_').replace(/^[._-]+/, '')
    .slice(0, 60);
  return base || 'asset';
}

function sendJson(res, code, obj) {
  const body = JSON.stringify(obj);
  res.writeHead(code, { 'Content-Type': 'application/json; charset=utf-8', 'Content-Length': Buffer.byteLength(body), 'Cache-Control': 'no-store' });
  res.end(body);
}

function stagePercent(stage, extra) {
  switch (stage) {
    case 'probe': return 2;
    case 'decode': return 6;
    case 'key': return 8 + Math.round(62 * (extra.frame / extra.frames));
    case 'quant': return 74;
    case 'verify': return 88;
    case 'write': return 96;
    default: return 1;
  }
}

// ------------------------------------------------------------------ 处理队列

async function processJob(id) {
  const job = jobs.get(id);
  if (!job) return;
  job.status = 'processing';
  try {
    const res = await keyer.processToSheet({
      src: uploadPath(id), name: job.name, outDir: resultDir(id),
      palette: job.palette, dlow: job.dlow, dhigh: job.dhigh, despill: job.despill,
      key: job.key, previewSamples: 8,
      onStage: ev => {
        job.stage = ev.stage;
        if (ev.stage === 'key') job.frame = ev.frame, job.frames = ev.frames;
        job.percent = stagePercent(ev.stage, ev);
      },
    });
    // QA 预览条(棋盘格底)也放进结果目录
    if (res.previewFrames.length) {
      await keyer.writePreviewStrip(path.join(resultDir(id), `${job.name}.preview.png`), res.previewFrames, res.width, res.height);
    }
    const files = [
      { name: `${job.name}.sheet.png`, url: `/api/result/${id}/${encodeURIComponent(job.name)}.sheet.png` },
      { name: `${job.name}.sheet.json`, url: `/api/result/${id}/${encodeURIComponent(job.name)}.sheet.json` },
    ];
    if (res.previewFrames.length) files.push({ name: `${job.name}.preview.png`, url: `/api/result/${id}/${encodeURIComponent(job.name)}.preview.png` });
    Object.assign(job, {
      status: 'done', stage: 'done', percent: 100, error: null,
      width: res.width, height: res.height, frame_count: res.frame_count,
      frames_per_row: res.frames_per_row, sheetW: res.sheetW, sheetH: res.sheetH,
      fps: res.fps, duration: res.duration, key: res.key, palette: res.palette,
      pngBytes: res.pngBytes, jsonBytes: res.jsonBytes, elapsedMs: res.elapsedMs,
      files,
    });
  } catch (e) {
    job.status = 'error';
    job.error = String(e.message || e);
  } finally {
    try { fs.unlinkSync(uploadPath(id)); } catch { /* 已删则忽略 */ }
  }
}

function pump() {
  if (running) return;
  const id = queue.shift();
  if (!id) return;
  running = true;
  processJob(id)
    .catch(e => { const j = jobs.get(id); if (j) { j.status = 'error'; j.error = String(e.message || e); } })
    .finally(() => { running = false; pump(); });
}

// ------------------------------------------------------------------ 路由

function uploadPath(id) { return path.join(UPLOADS, `${id}.mp4`); }
function resultDir(id) { return path.join(RESULTS, id); }

const server = http.createServer((req, res) => {
  const u = new URL(req.url, `http://${req.headers.host || 'localhost'}`);
  const p = u.pathname;

  // 静态页面
  if (p === '/' || p === '/index.html') return serveStatic(res, path.join(ROOT, 'index.html'));
  if (p === '/client.js') return serveStatic(res, path.join(ROOT, 'client.js'));
  if (p === '/style.css') return serveStatic(res, path.join(ROOT, 'style.css'));
  if (p === '/favicon.ico') { res.writeHead(204); return res.end(); }

  // 上传 + 建任务
  if (req.method === 'POST' && p === '/api/process') {
    const id = Date.now().toString(36) + '-' + crypto.randomBytes(4).toString('hex');
    const name = sanitizeName(req.headers['x-name'] || req.headers['x-filename'] || 'asset');
    const keyH = req.headers['x-key'] || '';
    const job = {
      id, name, status: 'uploading', stage: 'upload', percent: 0, error: null,
      palette: Math.max(2, Math.min(256, parseInt(req.headers['x-palette'], 10) || 256)),
      dlow: Math.max(1, parseFloat(req.headers['x-dlow']) || 32),
      dhigh: Math.max(1, parseFloat(req.headers['x-dhigh']) || 80),
      despill: parseInt(req.headers['x-despill'], 10) ? 1 : 0,
      key: /^\d{1,3},\d{1,3},\d{1,3}$/.test(keyH) ? keyH.split(',').map(Number) : null,
      createdAt: Date.now(),
    };
    jobs.set(id, job);
    const size = parseInt(req.headers['content-length'] || '0', 10) || 0;
    if (size > MAX_UPLOAD) { sendJson(res, 413, { error: `文件过大(>${Math.round(MAX_UPLOAD / 1048576)}MB)` }); return; }
    const out = fs.createWriteStream(uploadPath(id));
    let written = 0;
    let aborted = false;
    req.on('data', d => {
      written += d.length;
      job.percent = size ? Math.min(99, Math.round(100 * written / size)) : 50;
      if (written > MAX_UPLOAD && !aborted) {
        aborted = true;
        out.destroy();
        req.destroy();
        sendJson(res, 413, { error: '文件过大(>1GB)' });
      } else if (!out.write(d)) {
        req.pause();
        out.once('drain', () => req.resume());
      }
    });
    req.on('end', () => {
      if (aborted) return;
      out.end(() => {
        job.status = 'queued';
        job.percent = 100;
        queue.push(id);
        pump();
        sendJson(res, 202, { job_id: id });
      });
    });
    req.on('error', () => { if (!aborted) { aborted = true; out.destroy(); } });
    return;
  }

  // 任务状态
  let m = p.match(/^\/api\/job\/([\w-]+)$/);
  if (m) {
    const job = jobs.get(m[1]);
    if (!job) return sendJson(res, 404, { error: '任务不存在或已过期' });
    const { id, name, status, stage, percent, error, frame, frames, width, height, frame_count, frames_per_row, sheetW, sheetH, fps, duration, key, palette, pngBytes, jsonBytes, elapsedMs, files } = job;
    return sendJson(res, 200, { id, name, status, stage, percent, error, frame, frames, width, height, frame_count, frames_per_row, sheetW, sheetH, fps, duration, key, palette, pngBytes, jsonBytes, elapsedMs, files });
  }

  // 结果下载
  m = p.match(/^\/api\/result\/([\w-]+)\/(.+)$/);
  if (m) {
    const dir = resultDir(m[1]);
    const file = path.basename(m[2]);
    const full = path.join(dir, file);
    if (!full.startsWith(dir + path.sep)) return sendJson(res, 403, { error: 'bad path' });
    if (!fs.existsSync(full)) return sendJson(res, 404, { error: '文件不存在或已过期' });
    return serveStatic(res, full);
  }

  sendJson(res, 404, { error: 'not found' });
});

function serveStatic(res, file) {
  fs.stat(file, (err, st) => {
    if (err || !st.isFile()) return sendJson(res, 404, { error: 'not found' });
    res.writeHead(200, {
      'Content-Type': MIME[path.extname(file).toLowerCase()] || 'application/octet-stream',
      'Content-Length': st.size,
      'Cache-Control': 'no-store',
    });
    fs.createReadStream(file).pipe(res);
  });
}

// 过期清理:结果/任务保留 TTL,运行中的跳过
function sweep() {
  const now = Date.now();
  for (const [id, job] of jobs) {
    if (job.status === 'uploading' || job.status === 'processing' || job.status === 'queued') continue;
    if (now - job.createdAt > RESULT_TTL_MS) {
      jobs.delete(id);
      fs.rmSync(resultDir(id), { recursive: true, force: true });
    }
  }
  for (const f of fs.readdirSync(UPLOADS)) {
    const full = path.join(UPLOADS, f);
    try { if (now - fs.statSync(full).mtimeMs > 3600_000) fs.unlinkSync(full); } catch { /* 忽略 */ }
  }
}
setInterval(sweep, 10 * 60 * 1000).unref();

server.listen(PORT, HOST, () => {
  console.log(`mp4-keyer: http://${HOST}:${PORT}  (结果保留 ${RESULT_TTL_MS / 3600000}h,上传上限 ${MAX_UPLOAD / 1048576}MB)`);
});
