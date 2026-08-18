// Inspect animated webp assets (new-resource batch vs current resource/).
// Usage: node inspect_webp.js [dir ...]
const sharp = require('/home/nnn/.nvm/versions/node/v22.23.2/lib/node_modules/@deepseek-ai/dsh/node_modules/sharp');
const fs = require('fs');
const path = require('path');

const dirs = process.argv.slice(2).length ? process.argv.slice(2) : ['new-resource', 'resource'];

function fmtDelay(delay) {
  if (!delay || !delay.length) return 'n/a';
  const uniq = [...new Set(delay)];
  const min = Math.min(...delay), max = Math.max(...delay);
  const avg = Math.round(delay.reduce((a, b) => a + b, 0) / delay.length);
  return `n=${delay.length} avg=${avg}ms min=${min} max=${max} uniq=${uniq.slice(0, 12).join(',')}${uniq.length > 12 ? '...' : ''}`;
}

async function inspect(file) {
  const buf = fs.readFileSync(file);
  const m = await sharp(buf, { animated: true }).metadata();
  return {
    file,
    sizeMB: (buf.length / 1048576).toFixed(2),
    w: m.width, h: m.height,
    pages: m.pages,
    loop: m.loop,
    hasAlpha: m.hasAlpha,
    delay: fmtDelay(m.delay),
  };
}

(async () => {
  for (const dir of dirs) {
    console.log(`\n=== ${dir} ===`);
    const files = fs.readdirSync(dir).filter(f => f.endsWith('.webp')).sort();
    for (const f of files) {
      try {
        const r = await inspect(path.join(dir, f));
        console.log(
          `${r.file.padEnd(28)} ${r.sizeMB}MB  ${r.w}x${r.h}  pages=${r.pages}  loop=${r.loop}  alpha=${r.hasAlpha}\n` +
          `  delays: ${r.delay}`
        );
      } catch (e) {
        console.log(`${f.padEnd(28)} ERROR: ${e.message}`);
      }
    }
  }
})().catch(e => { console.error(e); process.exit(1); });