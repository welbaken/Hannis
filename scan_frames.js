// Scan each page: alpha stats (mean, % non-transparent) to spot blank frames.
// Usage: node scan_frames.js <dir>
const sharp = require('/home/nnn/.nvm/versions/node/v22.23.2/lib/node_modules/@deepseek-ai/dsh/node_modules/sharp');
const fs = require('fs');
const path = require('path');

const dir = process.argv[2];
const files = fs.readdirSync(dir).filter(f => f.endsWith('.webp')).sort();

(async () => {
  for (const f of files) {
    const buf = fs.readFileSync(path.join(dir, f));
    const m = await sharp(buf, { animated: true }).metadata();
    const blank = [];
    let minMean = 255, minCover = 100, maxCover = 0;
    for (let i = 0; i < m.pages; i++) {
      const raw = await sharp(buf, { page: i }).ensureAlpha().raw().toBuffer();
      let sum = 0, nonZero = 0;
      const n = raw.length / 4;
      for (let p = 3; p < raw.length; p += 4) {
        const a = raw[p];
        sum += a;
        if (a > 0) nonZero++;
      }
      const mean = sum / n;
      const cover = (nonZero / n) * 100;
      if (mean < minMean) minMean = mean;
      if (cover < minCover) minCover = cover;
      if (cover > maxCover) maxCover = cover;
      if (cover < 1) blank.push(i);
    }
    console.log(
      `${f.padEnd(22)} pages=${m.pages}  alpha-mean min=${minMean.toFixed(1)}  coverage[min=${minCover.toFixed(1)}% max=${maxCover.toFixed(1)}%]` +
      (blank.length ? `  NEAR-BLANK: ${blank.slice(0, 20).join(',')}${blank.length > 20 ? '...' : ''}` : '')
    );
  }
})().catch(e => { console.error(e); process.exit(1); });