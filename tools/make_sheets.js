#!/usr/bin/env node
// dshpet sprite-sheet 打包工具(与 tools/split_webp.py 等价,Node + sharp 版)。
//
// 把 resource/<state>.webp 打包为:
//   resource/<state>.sheet.png   — 所有帧按网格排布在一张大图上
//   resource/<state>.sheet.json  — 元数据(单帧尺寸/帧数/每帧时长/tail)
//
// 运行期 anim.rs 优先按 sheet 加载:单文件一次解码,启动/切换零解码延迟,
// 同时避免拆帧方式在 resource/ 下散落几十个 PNG。
//
// 实现要点:动画帧在 sharp 中按页垂直堆叠,先把整叠解码为 raw RGBA,
// 再纯内存拷贝到网格画布,最后一次性 PNG 编码(避免逐帧解/编码)。
// 降采样(scale≠1)在整张 sheet 上做,网格几何随之等比缩放。
//
// 用法:
//   node tools/make_sheets.js                       # 全部 *.webp 打包(仓库 resource/)
//   node tools/make_sheets.js --src dist/resource   # 指定资源目录(构建脚本用)
//   node tools/make_sheets.js idle                  # 只打包 idle
//   node tools/make_sheets.js idle 0.5              # 打包时降采样(可选,默认原尺寸)
//
// 依赖:sharp(本机由 DSH 安装提供,见 mux_webp.js 同类引用方式)
const path = require('path');
const fs = require('fs');

function loadSharp() {
  try {
    return require('sharp');
  } catch {
    // 与仓库根目录 mux_webp.js 相同:DSH 自带的 sharp
    return require('/home/nnn/.nvm/versions/node/v22.23.2/lib/node_modules/@deepseek-ai/dsh/node_modules/sharp');
  }
}
const sharp = loadSharp();

const SRC = path.join(__dirname, '..', 'resource');
const MAX_SHEET_WIDTH = 8192; // 避开旧 GPU 16384 上限,同时单张 PNG 尺寸合理
const TAIL_MS = 1000;
const DEFAULT_MS = 42;

function tailInfo(durs) {
  let acc = 0, start = durs.length;
  for (let i = durs.length - 1; i >= 0; i--) {
    acc += durs[i];
    if (acc >= TAIL_MS) { start = i; break; }
  }
  if (start === durs.length) start = 0;
  return { start, end: durs.length - 1 };
}

/** 把 srcRaw(每页 fh 行)按网格拷贝进 sheet 画布。 */
function blitRows(srcRaw, sheetRaw, pages, fw, fh, fpr, sheetW) {
  const bytesPerRow = fw * 4;
  for (let i = 0; i < pages; i++) {
    const col = i % fpr;
    const row = Math.floor(i / fpr);
    const srcBase = i * fh * bytesPerRow;
    const dstBase = ((row * fh) * sheetW + col * fw) * 4;
    for (let y = 0; y < fh; y++) {
      // srcRaw.copy(sheetRaw, dstStart, srcStart, srcEnd)
      srcRaw.copy(sheetRaw, dstBase + y * sheetW * 4, srcBase + y * bytesPerRow, srcBase + (y + 1) * bytesPerRow);
    }
  }
}

/** 校验:重解码 sheet PNG,与画布原始像素逐字节比对。 */
async function verifySheet(sheetPng, dstRaw, name) {
  const back = await sharp(sheetPng).raw().toBuffer();
  if (back.length !== dstRaw.length) {
    throw new Error(`${name}: size mismatch ${back.length} != ${dstRaw.length}`);
  }
  let first = -1;
  for (let i = 0; i < dstRaw.length; i++) {
    if (back[i] !== dstRaw[i]) { first = i; break; }
  }
  if (first >= 0) {
    throw new Error(`${name}: sheet verification failed at byte ${first} (encoded ${back[first]}, expected ${dstRaw[first]})`);
  }
}

async function buildSheet(name, scale, outDir) {
  const src = path.join(outDir, `${name}.webp`);
  if (!fs.existsSync(src)) {
    console.log(`  skip ${name}: ${src} not found`);
    return;
  }
  const m = await sharp(src, { animated: true }).metadata();
  const pages = m.pages || 1;
  const fw = m.width;
  const fh = m.pageHeight || m.height;
  let durs = (m.delay && m.delay.length ? m.delay : [DEFAULT_MS]).slice(0, pages);
  while (durs.length < pages) durs.push(DEFAULT_MS);

  const fpr = Math.max(1, Math.min(pages, Math.floor(MAX_SHEET_WIDTH / fw)));
  const rows = Math.ceil(pages / fpr);
  const sheetW = fpr * fw;
  const sheetH = rows * fh;

  // 动画帧在 sharp 中按页垂直堆叠;先整体转成 PNG 再解码,避免直接在
  // animated 输入上 extract(bad extract area)。
  // 注意:png() 默认 palette 量化是有损的,必须显式 palette:false。
  const stacked = await sharp(src, { animated: true }).png({ palette: false }).toBuffer();
  const srcRaw = await sharp(stacked).raw().toBuffer();
  const dstRaw = Buffer.alloc(sheetW * sheetH * 4, 0); // 透明底
  blitRows(srcRaw, dstRaw, pages, fw, fh, fpr, sheetW);

  let sheetPng = await sharp(dstRaw, { raw: { width: sheetW, height: sheetH, channels: 4 } })
    .png({ palette: false, compressionLevel: 9 })
    .toBuffer();
  await verifySheet(sheetPng, dstRaw, name);

  let w = fw, h = fh;
  if (scale !== 1.0) {
    w = Math.max(1, Math.round(fw * scale));
    h = Math.max(1, Math.round(fh * scale));
    sheetPng = await sharp(sheetPng).resize(fpr * w, rows * h).png().toBuffer();
  }

  const meta = {
    state: name,
    width: w,
    height: h,
    frame_count: pages,
    durations_ms: durs,
    tail: tailInfo(durs),
    frames_per_row: fpr,
  };
  fs.writeFileSync(path.join(outDir, `${name}.sheet.png`), sheetPng);
  fs.writeFileSync(path.join(outDir, `${name}.sheet.json`), JSON.stringify(meta, null, 2) + '\n');
  console.log(
    `  ${name}: ${pages} frames (${w}x${h}, ${durs.reduce((a, b) => a + b, 0)}ms) ` +
    `-> ${name}.sheet.png (${fpr * w}x${rows * h}, ${fpr}/row)`
  );
}

(async () => {
  const argv = process.argv.slice(2);
  let outDir = SRC;
  const srcIdx = argv.indexOf('--src');
  if (srcIdx >= 0) {
    outDir = path.resolve(argv[srcIdx + 1]);
    argv.splice(srcIdx, 2);
  }
  const args = argv.filter(a => !a.startsWith('-'));
  const scale = args.length > 1 ? parseFloat(args[1]) || 1.0 : 1.0;
  const targets = args.length ? [args[0]] :
    fs.readdirSync(outDir).filter(f => f.endsWith('.webp')).map(f => f.slice(0, -5)).sort();
  console.log(`== 打包 ${targets.length} 个 webp 为 sprite sheet(scale=${scale}, src=${outDir}) ==`);
  for (const t of targets) {
    try {
      await buildSheet(t, scale, outDir);
    } catch (e) {
      console.error(`  ${t}: FAIL ${e.message}`);
    }
  }
})();