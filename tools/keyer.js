'use strict';
// dshpet 素材处理共享模块:绿幕 mp4 -> sprite sheet。
// 被 tools/mp4_to_sheet.js(CLI)与 web/mp4-keyer/server.js(网页服务)共用。
//
// 管线:ffmpeg 解码 -> 每帧去绿幕(色度距离 + 平滑过渡 + 去绿边) ->
//       按网格写入大画布 -> sharp 调色板量化(alpha 走 tRNS) -> 校验 -> 写出
// 输出与 resource/ 现有素材同构(参考 tools/make_sheets.js):
//   <name>.sheet.png  — 调色板(索引)PNG,所有帧按网格排布
//   <name>.sheet.json — 几何元数据(width/height/frame_count/frames_per_row)
const path = require('path');
const fs = require('fs');
const os = require('os');
const { spawn, spawnSync } = require('child_process');

function loadSharp() {
  const candidates = [
    () => require('sharp'),                                   // 常规:本机 npm install sharp
    () => require('/home/nnn/.nvm/versions/node/v22.23.2/lib/node_modules/@deepseek-ai/dsh/node_modules/sharp'), // 本仓库开发机:DSH 自带
    () => require(path.join(__dirname, '..', 'node_modules', 'sharp')),                    // 仓库根 npm install
    () => require(path.join(__dirname, '..', '..', 'web', 'mp4-keyer', 'node_modules', 'sharp')), // 网页工具目录 npm install
  ];
  let lastErr;
  for (const load of candidates) {
    try { return load(); } catch (e) { lastErr = e; }
  }
  throw new Error('找不到 sharp 模块。请先安装:在仓库根或 web/mp4-keyer 目录执行 `npm install`(详情见 web/mp4-keyer/README.md)。' + (lastErr ? ` (${lastErr.code || lastErr.message})` : ''));
}
const sharp = loadSharp();

const MAX_SHEET_WIDTH = 8192; // 与 make_sheets.js 保持一致
const BORDER = 5;             // 边框取样宽度(px),用于自动估算键色

// ------------------------------------------------------------------ ffmpeg 查找

function findFfmpeg(explicit) {
  const tried = [];
  const probe = cmd => {
    const r = spawnSync(cmd, ['-version'], { encoding: 'utf8', timeout: 10000 });
    return r.status === 0 && /ffmpeg version/i.test(r.stdout || '');
  };
  const candidate = explicit || process.env.FFMPEG;
  if (candidate) { tried.push(candidate); if (probe(candidate)) return candidate; }
  for (const c of ['ffmpeg', 'ffmpeg.exe']) {
    if (probe(c)) { tried.push(c); return c; }
  }
  // 本机 WinGet 安装的 Gyan ffmpeg(WSL 可直接运行 .exe)
  try {
    const roots = os.platform() === 'win32' ? ['C:\\Users'] : ['/mnt/c/Users'];
    if (fs.existsSync(roots[0])) {
      for (const u of fs.readdirSync(roots[0]).slice(0, 10)) {
        const base = path.join(roots[0], u, 'AppData', 'Local', 'Microsoft', 'WinGet', 'Packages');
        if (!fs.existsSync(base)) continue;
        for (const p of fs.readdirSync(base)) {
          if (!/^Gyan\.FFmpeg/i.test(p)) continue;
          const dir = path.join(base, p);
          for (const d of fs.readdirSync(dir)) {
            const exe = path.join(dir, d, 'bin', 'ffmpeg.exe');
            if (fs.existsSync(exe)) { tried.push(exe); if (probe(exe)) return exe; }
          }
        }
      }
    }
  } catch { /* 目录不可读则跳过 */ }
  throw new Error(
    '找不到 ffmpeg。请安装(如 `apt install ffmpeg` / `winget install Gyan.FFmpeg`)或用 --ffmpeg <路径> 指定。尝试过: ' + tried.join(', ')
  );
}

function findFfprobe(ffmpeg) {
  for (const ff of [process.env.FFPROBE, ffmpeg.replace(/ffmpeg(\.exe)?$/, 'ffprobe$1')]) {
    if (!ff) continue;
    const r = spawnSync(ff, ['-version'], { encoding: 'utf8', timeout: 10000 });
    if (r.status === 0) return ff;
  }
  return null;
}

// ------------------------------------------------------------------ 元数据

async function probeMeta(ffmpeg, src) {
  const ffprobe = findFfprobe(ffmpeg);
  if (ffprobe) {
    const r = spawnSync(ffprobe, [
      '-v', 'error', '-select_streams', 'v:0',
      '-show_entries', 'stream=width,height,r_frame_rate,nb_frames:format=duration',
      '-of', 'json', src,
    ], { encoding: 'utf8', timeout: 20000 });
    if (r.status === 0) {
      try {
        const j = JSON.parse(r.stdout);
        const s = j.streams && j.streams[0];
        if (s && s.width && s.height) {
          const [num, den] = String(s.r_frame_rate || '').split('/').map(Number);
          return {
            width: s.width, height: s.height,
            fps: num && den ? num / den : 24,
            nb_frames: s.nb_frames ? Number(s.nb_frames) : null,
            duration: j.format ? Number(j.format.duration) : null,
          };
        }
      } catch { /* 解析失败则走正则回退 */ }
    }
  }
  // 回退:从 ffmpeg -i 的 stderr 解析 "Video: ... 800x800"
  const r = spawnSync(ffmpeg, ['-i', src], { encoding: 'utf8', timeout: 20000 });
  const m = /Video:.*?(\d{2,5})x(\d{2,5})/.exec(r.stderr || '');
  if (!m) throw new Error(`无法从 ${src} 读取视频尺寸(ffprobe 不可用且 -i 输出无法解析)`);
  return { width: +m[1], height: +m[2], fps: 24, nb_frames: null, duration: null };
}

// ------------------------------------------------------------------ 去绿幕

/** 亮去绿幕:d = |RGB - 键色|;d<=dLow 全透明,d>=dHigh 全不透明,中间平滑过渡;
 * 半透明边缘按 alpha 削减 G 通道残留(去绿边)。对 pixels(每像素4字节)原地修改。 */
function keyFrame(pixels, key, dLow, dHigh, despill) {
  const [kr, kg, kb] = key;
  const d2lo = dLow * dLow, d2hi = dHigh * dHigh, span = dHigh - dLow;
  for (let i = 0; i < pixels.length; i += 4) {
    const r = pixels[i], g = pixels[i + 1], b = pixels[i + 2];
    const dr = r - kr, dg = g - kg, db = b - kb;
    const d2 = dr * dr + dg * dg + db * db;
    if (d2 <= d2lo) {
      pixels[i + 3] = 0;
      continue;
    }
    let a;
    if (d2 >= d2hi) {
      a = 255;
    } else {
      // smoothstep 渐变,避免台阶感
      const t = (Math.sqrt(d2) - dLow) / span;
      a = Math.round(255 * t * t * (3 - 2 * t));
    }
    if (despill && a > 0 && g > Math.max(r, b)) {
      // 半透明处残留的绿色按 alpha 削减:全透明 -> 无绿,全不透明 -> 原色
      const excess = g - Math.max(r, b);
      pixels[i + 1] = Math.min(255, Math.max(r, b) + Math.round(excess * a / 255));
    }
    pixels[i + 3] = a;
  }
}

/** 从 buffer 的边框像素计算键色中位数(直方图法)。 */
function borderKey(samples, w, h) {
  const hist = [new Array(256).fill(0), new Array(256).fill(0), new Array(256).fill(0)];
  for (const px of samples) {
    for (let y = 0; y < h; y++) {
      const first = (y * w + 0) * 4, last = (y * w + w - 1) * 4;
      if (y < BORDER || y >= h - BORDER) {
        for (let x = 0; x < w; x++) {
          const i = (y * w + x) * 4;
          hist[0][px[i]]++; hist[1][px[i + 1]]++; hist[2][px[i + 2]]++;
        }
      } else {
        hist[0][px[first]]++; hist[1][px[first + 1]]++; hist[2][px[first + 2]]++;
        hist[0][px[last]]++; hist[1][px[last + 1]]++; hist[2][px[last + 2]]++;
      }
    }
  }
  return hist.map(h => {
    let total = 0;
    for (const v of h) total += v;
    let cum = 0;
    for (let i = 0; i < 256; i++) { cum += h[i]; if (cum >= total / 2) return i; }
    return 0;
  });
}

// ------------------------------------------------------------------ 校验

/** 与 make_sheets.js 的 verifySheet 相同口径:量化容忍,只查可见像素。 */
function verifySheet(sheetPng, dstRaw, name) {
  return sharp(sheetPng).raw().toBuffer().then(back => {
    if (back.length !== dstRaw.length) {
      throw new Error(`${name}: sheet size mismatch ${back.length} != ${dstRaw.length}`);
    }
    let n = 0, sum = 0, maxD = 0, bad = 0;
    for (let p = 0; p < dstRaw.length; p += 4) {
      if (dstRaw[p + 3] === 0) continue;
      n++;
      let d = 0;
      for (let c = 0; c < 4; c++) {
        const dc = Math.abs(back[p + c] - dstRaw[p + c]);
        if (dc > d) d = dc;
      }
      sum += d;
      if (d > 64) bad++;
    }
    if (maxD > 200 || (n && sum / n > 12) || bad * 20 > n) {
      throw new Error(
        `${name}: quantized sheet verification failed maxD=${maxD} avg=${n ? (sum / n).toFixed(1) : 0} bad(>64)=${bad}/${n}`
      );
    }
  });
}

// ------------------------------------------------------------------ 主流程

/**
 * 处理一个视频为 sprite sheet 并写出 <name>.sheet.png/.sheet.json。
 * @param {object} o
 *   src      视频文件路径
 *   name     输出名(安全字符,调用方负责清洗)
 *   outDir   输出目录
 *   palette  调色板颜色数(0 = 无损 RGBA)
 *   dlow     完全透明距离阈值(默认 32)
 *   dhigh    完全不透明距离阈值(默认 80)
 *   despill  去绿边(默认 1)
 *   key      [r,g,b] 手动键色;缺省自动(全片边框像素中位数)
 *   ffmpeg   显式 ffmpeg 路径;缺省自动查找
 *   onStage  (event) => void;event = {stage, ...},stage ∈
 *            probe|decode|key|quant|verify|write
 *   previewSamples  收集 N 张(等距)已去绿幕帧副本,经返回值 previewFrames 取回
 * @returns 处理结果摘要(含写出路径与 size)与(可选)previewFrames
 */
async function processToSheet(o) {
  const palette = o.palette == null ? 256 : o.palette;
  const dlow = o.dlow == null ? 32 : o.dlow;
  const dhigh = o.dhigh == null ? 80 : o.dhigh;
  const despill = o.despill == null ? 1 : o.despill;
  const emit = (ev) => { try { o.onStage && o.onStage(ev); } catch { /* 回调异常不中断处理 */ } };

  const t0 = Date.now();
  emit({ stage: 'probe' });
  const ffmpeg = o.ffmpeg || findFfmpeg();
  const meta = await probeMeta(ffmpeg, o.src);
  const { width: w, height: h, fps } = meta;
  const frameBytes = w * h * 4;

  // 1) ffmpeg 流式解码为原始 RGBA
  emit({ stage: 'decode' });
  const child = spawn(ffmpeg, ['-v', 'error', '-i', o.src, '-map', '0:v:0', '-an', '-sn',
    '-f', 'rawvideo', '-pix_fmt', 'rgba', 'pipe:1']);
  let stderr = '';
  const frames = [];
  let buf = Buffer.alloc(0);
  await new Promise((resolve, reject) => {
    child.stderr.on('data', d => { stderr += d; });
    child.on('error', reject);
    child.on('close', code => {
      if (code !== 0) reject(new Error(`ffmpeg 退出码 ${code}: ${stderr.slice(0, 400)}`));
      else resolve();
    });
    child.stdout.on('data', d => {
      buf = Buffer.concat([buf, d]);
      while (buf.length >= frameBytes) {
        frames.push(buf.subarray(0, frameBytes));
        buf = buf.subarray(frameBytes);
      }
    });
  });
  if (!frames.length) throw new Error(`${o.src}: 解码出 0 帧`);
  const nFrames = frames.length;

  // 2) 键色:全片边框像素中位数(子采样,最多 24 帧样本足够)
  const key = o.key || borderKey(
    frames.filter((_, i) => i % Math.max(1, Math.floor(frames.length / 24)) === 0), w, h);

  // 3) 去绿幕并写入网格画布
  const fpr = Math.max(1, Math.min(nFrames, Math.floor(MAX_SHEET_WIDTH / w)));
  const rows = Math.ceil(nFrames / fpr);
  const sheetW = fpr * w, sheetH = rows * h;
  const sheetRaw = Buffer.alloc(sheetW * sheetH * 4, 0); // 透明底
  const rowBytes = w * 4;
  const previewFrames = [];
  const previewStep = o.previewSamples ? Math.max(1, Math.floor(nFrames / o.previewSamples)) : 0;
  for (let i = 0; i < frames.length; i++) {
    const px = frames[i];
    emit({ stage: 'key', frame: i, frames: nFrames, key });
    keyFrame(px, key, dlow, dhigh, despill);
    if (previewStep && i % previewStep === 0) previewFrames.push(Buffer.from(px));
    const col = i % fpr, row = Math.floor(i / fpr);
    const dstBase = ((row * h) * sheetW + col * w) * 4;
    for (let y = 0; y < h; y++) {
      px.copy(sheetRaw, dstBase + y * sheetW * 4, y * rowBytes, (y + 1) * rowBytes);
    }
  }
  frames.length = 0; // 释放原始帧

  // 4) 调色板量化(alpha 走 tRNS,与内置素材同款:无抖动)
  emit({ stage: 'quant' });
  const pngOpts = palette > 0
    ? { palette: true, colours: palette, dither: 0 }
    : { palette: false, compressionLevel: 9 };
  const sheetPng = await sharp(sheetRaw, { raw: { width: sheetW, height: sheetH, channels: 4 } })
    .png(pngOpts).toBuffer();
  emit({ stage: 'verify' });
  await verifySheet(sheetPng, sheetRaw, o.name);
  sheetRaw.fill(0);

  // 5) 写出
  emit({ stage: 'write' });
  fs.mkdirSync(o.outDir, { recursive: true });
  const pngPath = path.join(o.outDir, `${o.name}.sheet.png`);
  const jsonPath = path.join(o.outDir, `${o.name}.sheet.json`);
  const json = JSON.stringify({
    width: w, height: h, frame_count: nFrames, frames_per_row: fpr,
  }, null, 2) + '\n';
  fs.writeFileSync(pngPath, sheetPng);
  fs.writeFileSync(jsonPath, json);

  return {
    width: w, height: h, frame_count: nFrames, frames_per_row: fpr,
    sheetW, sheetH, fps: meta.fps, duration: meta.duration, key, palette,
    pngPath, jsonPath, pngBytes: sheetPng.length, jsonBytes: Buffer.byteLength(json),
    elapsedMs: Date.now() - t0,
    previewFrames,
  };
}

/** 棋盘格底 QA 预览条:已去绿幕的帧拼成一行,透明处画棋盘格。 */
async function writePreviewStrip(pathOut, frames, w, h) {
  const cell = 16;
  const bufs = await Promise.all(frames.map(px => {
    const src = Buffer.from(px);
    for (let y = 0; y < h; y++) {
      for (let x = 0; x < w; x++) {
        const i = (y * w + x) * 4;
        if (src[i + 3] < 128) {
          const on = ((x / cell | 0) + (y / cell | 0)) % 2 === 0;
          src[i] = on ? 235 : 190; src[i + 1] = on ? 235 : 190; src[i + 2] = on ? 235 : 190;
          src[i + 3] = 255;
        }
      }
    }
    return sharp(src, { raw: { width: w, height: h, channels: 4 } }).png().toBuffer();
  }));
  await sharp({
    create: { width: w * bufs.length, height: h, channels: 4, background: { r: 0, g: 0, b: 0, alpha: 1 } },
  }).composite(bufs.map((b, i) => ({ input: b, left: i * w, top: 0 }))).png().toFile(pathOut);
  return pathOut;
}

module.exports = {
  sharp, findFfmpeg, findFfprobe, probeMeta,
  keyFrame, borderKey, verifySheet,
  processToSheet, writePreviewStrip,
};
