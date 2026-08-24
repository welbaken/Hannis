#!/usr/bin/env node
// dshpet mp4 -> sprite sheet 工具(绿幕抠除 + 量化 + 打包)。
//
// 把 mp4/ 下的绿幕视频处理为与 resource/ 现有素材一致的 sprite sheet:
//   <out>/<name>.sheet.png   — 256 色调色板(索引)PNG,所有帧按网格排布
//   <out>/<name>.sheet.json  — 几何元数据(width/height/frame_count/frames_per_row)
//
// 处理管线见 tools/keyer.js(与 web/mp4-keyer 网页服务共用同一套逻辑)。
//
// 用法:
//   node tools/mp4_to_sheet.js --name lucky          # mp4/MiniMax_H3_00044_.mp4 -> resource/lucky.sheet.*
//   node tools/mp4_to_sheet.js                       # 批处理 mp4/ 下所有视频(输出名=文件名)
//   node tools/mp4_to_sheet.js --ffmpeg /path/to/ffmpeg --name lucky
//
// 选项:
//   --name <n>        输出名(默认取 mp4 基名;单文件时可用 --name 指定,如 lucky)
//   --src <dir>       mp4 目录(默认 mp4)
//   --out <dir>       sheet 输出目录(默认 resource)
//   --palette <n>     调色板颜色数,量化程度(默认 256,与内置素材一致)
//   --key r,g,b       手动键色(默认自动:全片边框像素中位数)
//   --dlow <d>        完全透明距离阈值(默认 32)
//   --dhigh <d>       完全不透明距离阈值(默认 80)
//   --despill 0|1     去绿边(半透明边缘削减 G,默认 1)
//   --preview <path>  额外输出一张 QA 预览条(棋盘格底、每 9 帧取样)
//
// 依赖:sharp(与 make_sheets.js 相同的加载方式)
const path = require('path');
const fs = require('fs');
const keyer = require('./keyer.js');

function argParse(argv) {
  const opts = { palette: 256, dlow: 32, dhigh: 80, despill: 1, name: null, src: 'mp4', out: 'resource', key: null, ffmpeg: null, preview: null };
  const args = [];
  for (let i = 0; i < argv.length; i++) {
    const a = argv[i];
    const next = () => argv[++i];
    if (a === '--name') opts.name = next();
    else if (a === '--src') opts.src = next();
    else if (a === '--out') opts.out = next();
    else if (a === '--palette') opts.palette = parseInt(next(), 10) || 256;
    else if (a === '--key') opts.key = next().split(',').map(Number);
    else if (a === '--dlow') opts.dlow = parseFloat(next());
    else if (a === '--dhigh') opts.dhigh = parseFloat(next());
    else if (a === '--despill') opts.despill = parseInt(next(), 10) ? 1 : 0;
    else if (a === '--preview') opts.preview = next();
    else if (a === '--ffmpeg') opts.ffmpeg = next();
    else if (a.startsWith('-')) throw new Error(`未知参数: ${a}`);
    else args.push(a);
  }
  return { opts, files: args };
}

async function main() {
  const { opts, files } = argParse(process.argv.slice(2));
  const srcDir = path.resolve(opts.src);
  const outDir = path.resolve(opts.out);
  if (!fs.existsSync(srcDir)) throw new Error(`mp4 目录不存在: ${srcDir}`);

  let inputs = files.length ? files.map(f => path.resolve(f))
    : fs.readdirSync(srcDir).filter(f => /\.(mp4|mov|webm|m4v)$/i.test(f)).map(f => path.join(srcDir, f));
  if (!inputs.length) throw new Error(`${srcDir} 下没有视频文件`);
  if (opts.name && inputs.length > 1) throw new Error('--name 只能配合单个输入使用');
  fs.mkdirSync(outDir, { recursive: true });

  const ffmpeg = keyer.findFfmpeg(opts.ffmpeg);
  console.log(`== mp4 -> sprite sheet(ffmpeg: ${ffmpeg}, palette=${opts.palette}, dLow=${opts.dlow}, dHigh=${opts.dhigh}, despill=${opts.despill}) ==`);

  for (const src of inputs) {
    const name = opts.name || path.basename(src).replace(/\.[^.]+$/, '');
    const res = await keyer.processToSheet({
      src, name, outDir, ffmpeg,
      palette: opts.palette, dlow: opts.dlow, dhigh: opts.dhigh, despill: opts.despill,
      key: opts.key, previewSamples: opts.preview ? 8 : 0,
      onStage: ev => {
        if (ev.stage === 'decode') console.log(`  ${name}: 解码中...`);
        else if (ev.stage === 'key' && ev.frame === 0) console.log(`  ${name}: 键色 RGB(${ev.key.join(',')})`);
        else if (ev.stage === 'key' && ev.frame % 10 === 0) console.log(`  ${name}: 去绿幕 ${ev.frame}/${ev.frames}`);
        else if (ev.stage === 'quant') console.log(`  ${name}: 调色板量化...`);
      },
    });
    console.log(`  ${name}: ${res.frame_count} 帧 (${res.width}x${res.height}) -> ${path.relative(process.cwd(), res.pngPath)} (${res.sheetW}x${res.sheetH}, ${res.frames_per_row}/row, quant p${res.palette}) 耗时 ${(res.elapsedMs / 1000).toFixed(1)}s`);
    if (opts.preview && res.previewFrames.length) {
      const p = path.resolve(opts.preview);
      await keyer.writePreviewStrip(p, res.previewFrames, res.width, res.height);
      console.log(`  preview: ${p} (${res.previewFrames.length} 帧取样)`);
    }
  }
}

main().catch(e => { console.error(e.stack || e.message); process.exit(1); });
