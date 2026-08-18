// Contact-sheet preview of assets: 3 frames per animation, one composite.
// Usage: node preview_sheet.js <dir> <out-png>
const sharp = require('/home/nnn/.nvm/versions/node/v22.23.2/lib/node_modules/@deepseek-ai/dsh/node_modules/sharp');
const fs = require('fs');
const path = require('path');

const dir = process.argv[2];
const out = process.argv[3];
const files = fs.readdirSync(dir).filter(f => f.endsWith('.webp')).sort();
const TH = 170, PAD = 14, LABEL_H = 24, COLS = 3;
const W = PAD * (COLS + 1) + TH * COLS;
const H = LABEL_H + TH + PAD;
const sheetW = W, sheetH = H * files.length;

(async () => {
  const canvas = Buffer.alloc(sheetW * sheetH * 4);
  for (let y = 0; y < sheetH; y++) for (let x = 0; x < sheetW; x++) {
    const on = ((x / 10 | 0) + (y / 10 | 0)) % 2 === 0;
    const v = on ? 0xf0 : 0xd8;
    const p = (y * sheetW + x) * 4;
    canvas[p] = v; canvas[p + 1] = v; canvas[p + 2] = v; canvas[p + 3] = 255;
  }
  const layers = [];
  for (let ri = 0; ri < files.length; ri++) {
    const f = files[ri];
    for (let y = ri * H; y < ri * H + LABEL_H; y++) {
      for (let x = 0; x < W; x++) {
        const p = (y * sheetW + x) * 4;
        canvas[p] = 45; canvas[p + 1] = 45; canvas[p + 2] = 45; canvas[p + 3] = 255;
      }
    }
    const buf = fs.readFileSync(path.join(dir, f));
    const m = await sharp(buf, { animated: true }).metadata();
    const idxs = [0, Math.floor(m.pages / 2), m.pages - 1];
    for (let k = 0; k < COLS; k++) {
      const thumb = await sharp(buf, { page: idxs[k] })
        .resize(TH, TH, { fit: 'contain', background: { r: 0, g: 0, b: 0, alpha: 0 } })
        .png()
        .toBuffer();
      layers.push({ input: thumb, left: PAD + k * (TH + PAD), top: ri * H + LABEL_H + PAD / 2 });
    }
  }
  const sheet = await sharp(canvas, { raw: { width: sheetW, height: sheetH, channels: 4 } })
    .composite(layers)
    .png()
    .toBuffer();
  fs.writeFileSync(out, sheet);
  console.log('wrote', out, sheet.length, 'bytes;', files.length, 'rows; sheet', sheetW, 'x', sheetH);
})().catch(e => { console.error(e); process.exit(1); });