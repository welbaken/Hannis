// 绿幕 mp4 → sprite sheet 网页端逻辑:
// 上传(显示进度) -> 轮询 /api/job/<id> -> canvas 逐帧动画预览 + 网格图 + 下载链接。
'use strict';

const $ = id => document.getElementById(id);
const els = {
  dropzone: $('dropzone'), fileInput: $('fileInput'), dzText: $('dzText'), fileInfo: $('fileInfo'),
  optName: $('optName'), optPalette: $('optPalette'),
  optKeyMode: $('optKeyMode'), optKey: $('optKey'),
  optDlow: $('optDlow'), optDhigh: $('optDhigh'), optDespill: $('optDespill'),
  startBtn: $('startBtn'), barFill: $('barFill'), stageText: $('stageText'),
  resultCard: $('resultCard'),
  anim: $('anim'), playBtn: $('playBtn'), frameSlider: $('frameSlider'), frameText: $('frameText'),
  stats: $('stats'), sheetThumb: $('sheetThumb'), sheetThumbLink: $('sheetThumbLink'),
  previewStrip: $('previewStrip'),
  dlPng: $('dlPng'), dlJson: $('dlJson'), dlPreview: $('dlPreview'),
};

let selectedFile = null;
let playing = false;
let rafId = 0;
let sheetImg = null, cell = null; // {w,h,fpr,frames}
let frameMs = 42;
let curFrame = 0;
let lastTick = 0;

// ---------------------------------------------------------------- 文件选择

function pickFile(f) {
  if (!f) return;
  if (!/\.(mp4|mov|webm|m4v)$/i.test(f.name)) { setError('请选择 mp4/mov/webm/m4v 文件'); return; }
  selectedFile = f;
  els.fileInfo.textContent = `${f.name} · ${(f.size / 1048576).toFixed(1)} MB`;
  els.optName.value = els.optName.value || f.name.replace(/\.[^.]+$/, '');
  els.startBtn.disabled = false;
  setError(null);
}

els.dropzone.addEventListener('click', () => els.fileInput.click());
els.dropzone.addEventListener('keydown', e => { if (e.key === 'Enter' || e.key === ' ') { e.preventDefault(); els.fileInput.click(); } });
els.dropzone.addEventListener('dragover', e => { e.preventDefault(); els.dropzone.classList.add('drag'); });
els.dropzone.addEventListener('dragleave', () => els.dropzone.classList.remove('drag'));
els.dropzone.addEventListener('drop', e => {
  e.preventDefault();
  els.dropzone.classList.remove('drag');
  pickFile(e.dataTransfer.files[0]);
});
els.fileInput.addEventListener('change', () => pickFile(els.fileInput.files[0]));

els.optKeyMode.addEventListener('change', () => { els.optKey.disabled = els.optKeyMode.value !== 'manual'; });

// ---------------------------------------------------------------- 上传与轮询

function setError(msg) {
  let box = $('errBox');
  if (!box) {
    box = document.createElement('div');
    box.id = 'errBox';
    els.startBtn.closest('.card').appendChild(box);
  }
  box.style.display = msg ? 'block' : 'none';
  box.textContent = msg || '';
}

function setProgress(pct, text) {
  els.barFill.style.width = `${Math.max(0, Math.min(100, pct))}%`;
  if (text != null) els.stageText.textContent = text;
}

const STAGE_TEXT = {
  uploading: '上传中', queued: '排队中', probe: '读取视频信息', decode: 'ffmpeg 解码帧',
  key: '去绿幕处理中', quant: '调色板量化', verify: '校验', write: '写出结果', done: '完成',
};

function startProcessing() {
  if (!selectedFile) return;
  const key = els.optKeyMode.value === 'manual' && /^\s*\d{1,3}\s*,\s*\d{1,3}\s*,\s*\d{1,3}\s*$/.test(els.optKey.value)
    ? els.optKey.value.trim().replace(/\s+/g, '') : '';
  const headers = {
    'x-filename': selectedFile.name,
    'x-name': els.optName.value.trim() || selectedFile.name.replace(/\.[^.]+$/, ''),
    'x-palette': els.optPalette.value,
    'x-dlow': els.optDlow.value,
    'x-dhigh': els.optDhigh.value,
    'x-despill': els.optDespill.value,
  };
  if (key) headers['x-key'] = key;

  els.startBtn.disabled = true;
  resultHide();
  setProgress(0, '上传中(0%)');

  const xhr = new XMLHttpRequest();
  xhr.open('POST', '/api/process');
  for (const [k, v] of Object.entries(headers)) xhr.setRequestHeader(k, v);
  xhr.upload.onprogress = e => {
    if (e.lengthComputable) setProgress(100 * e.loaded / e.total, `上传中(${Math.round(100 * e.loaded / e.total)}%)`);
  };
  xhr.onerror = () => { setError('上传失败:请确认服务仍在运行'); resetBtn(); };
  xhr.onload = () => {
    if (xhr.status !== 202) {
      let msg = `服务端错误 (HTTP ${xhr.status})`;
      try { msg = JSON.parse(xhr.responseText).error || msg; } catch { /* ignore */ }
      setError(msg);
      resetBtn();
      return;
    }
    const { job_id } = JSON.parse(xhr.responseText);
    setProgress(1, '排队/处理中…');
    pollJob(job_id, 0);
  };
  xhr.send(selectedFile);
}

function resetBtn() { els.startBtn.disabled = !selectedFile; }

function pollJob(id, tick) {
  fetch(`/api/job/${id}`)
    .then(r => r.json().then(j => ({ ok: r.ok, j })))
    .then(({ ok, j }) => {
      if (!ok || (j.status !== 'queued' && j.status !== 'processing' && j.status !== 'uploading')) {
        if (j.status === 'error') { setError(`处理失败:${j.error}`); setProgress(0, '失败'); resetBtn(); return; }
        if (j.status === 'done') { renderResult(j); resetBtn(); return; }
        setError('任务状态异常'); resetBtn(); return;
      }
      const pct = j.percent || 1;
      const stage = STAGE_TEXT[j.stage] || j.stage;
      const detail = j.stage === 'key' ? ` (${j.frame}/${j.frames} 帧)` : '';
      setProgress(pct, `${stage}${detail} · ${Math.round(pct)}%`);
      setTimeout(() => pollJob(id, tick + 1), 350);
    })
    .catch(() => setTimeout(() => pollJob(id, tick + 1), 1000));
}

// ---------------------------------------------------------------- 结果展示

function resultHide() {
  els.resultCard.hidden = true;
  stopAnim();
}

function fmtMB(n) { return (n / 1048576).toFixed(2) + ' MB'; }

function renderResult(j) {
  stopAnim();
  els.resultCard.hidden = false;
  setProgress(100, '完成');

  els.stats.textContent =
    `帧数 ${j.frame_count} · 帧尺寸 ${j.width}×${j.height} · 网格 ${j.frames_per_row}/行 (${j.sheetW}×${j.sheetH}) · ` +
    `fps ${j.fps ? (+j.fps).toFixed(2) : '—'} · 键色 RGB(${(j.key || []).join(',')}) · 调色板 ${j.palette} 色 · ` +
    `PNG ${fmtMB(j.pngBytes)} · 耗时 ${(j.elapsedMs / 1000).toFixed(1)}s`;

  const png = j.files.find(f => f.name.endsWith('.sheet.png'));
  const json = j.files.find(f => f.name.endsWith('.sheet.json'));
  const prev = j.files.find(f => f.name.endsWith('.preview.png'));
  els.dlPng.href = png.url; els.dlPng.setAttribute('download', png.name);
  els.dlJson.href = json.url; els.dlJson.setAttribute('download', json.name);
  els.sheetThumbLink.href = png.url;
  els.sheetThumb.src = png.url;
  if (prev) { els.previewStrip.src = prev.url; els.dlPreview.href = prev.url; els.dlPreview.setAttribute('download', prev.name); els.dlPreview.hidden = false; }
  else els.dlPreview.hidden = true;

  frameMs = j.fps ? Math.round(1000 / j.fps) : 42;
  cell = { w: j.width, h: j.height, fpr: j.frames_per_row, frames: j.frame_count };
  els.frameSlider.max = j.frame_count - 1;
  els.frameSlider.value = 0;
  curFrame = 0;
  updateFrameText();

  sheetImg = new Image();
  sheetImg.onload = () => { drawFrame(curFrame); play(); };
  sheetImg.src = png.url;
}

function updateFrameText() {
  els.frameText.textContent = `${curFrame + 1}/${cell.frames} · ${frameMs}ms/帧`;
  els.frameSlider.value = curFrame;
}

function drawFrame(i) {
  if (!sheetImg || !cell) return;
  const { w, h, fpr, frames } = cell;
  const col = i % fpr, row = Math.floor(i / fpr);
  const ctx = els.anim.getContext('2d');
  const W = els.anim.width, H = els.anim.height;
  drawChecker(ctx, W, H);
  ctx.drawImage(sheetImg, col * w, row * h, w, h, 0, 0, W, H);
  curFrame = i;
  updateFrameText();
}

let checker = null;
function drawChecker(ctx, W, H) {
  if (!checker) {
    checker = document.createElement('canvas');
    checker.width = checker.height = 16;
    const c = checker.getContext('2d');
    c.fillStyle = '#22262f'; c.fillRect(0, 0, 16, 16);
    c.fillStyle = '#2c3140'; c.fillRect(0, 0, 8, 8); c.fillRect(8, 8, 8, 8);
  }
  ctx.fillStyle = ctx.createPattern(checker, 'repeat');
  ctx.fillRect(0, 0, W, H);
}

function play() {
  if (playing || !cell) return;
  playing = true;
  els.playBtn.textContent = '⏸';
  lastTick = performance.now();
  const tick = now => {
    if (!playing) return;
    if (now - lastTick >= frameMs) {
      lastTick = now;
      drawFrame((curFrame + 1) % cell.frames);
    }
    rafId = requestAnimationFrame(tick);
  };
  rafId = requestAnimationFrame(tick);
}
function stopAnim() {
  playing = false;
  cancelAnimationFrame(rafId);
  els.playBtn.textContent = '▶';
}
els.playBtn.addEventListener('click', () => (playing ? stopAnim() : play()));
els.frameSlider.addEventListener('input', () => drawFrame(+els.frameSlider.value));

els.startBtn.addEventListener('click', startProcessing);
