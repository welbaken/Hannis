// Parse RIFF structure of webp: VP8X canvas dims + first ANMF frame dims.
// Usage: node riff_webp.js <file.webp>
const fs = require('fs');
const buf = fs.readFileSync(process.argv[2]);

if (buf.toString('ascii', 0, 4) !== 'RIFF' || buf.toString('ascii', 8, 12) !== 'WEBP') {
  console.log('NOT a webp'); process.exit(1);
}
let off = 12;
let vp8x = null, anim = null, anmfCount = 0, anmfFirst = null, lastAnmf = null;
while (off + 8 <= buf.length) {
  const id = buf.toString('ascii', off, off + 4);
  const size = buf.readUInt32LE(off + 4);
  const body = off + 8;
  if (id === 'VP8X') {
    const flags = buf[body];
    const w1 = buf.readUIntLE(body + 4, 3), h1 = buf.readUIntLE(body + 7, 3);
    vp8x = { flags: '0x' + flags.toString(16), w: w1 + 1, h: h1 + 1,
             alpha: !!(flags & 0x10), anim: !!(flags & 0x02) };
  } else if (id === 'ANIM') {
    anim = { bg: buf.readUIntLE(body, 4), loop: buf.readUInt16LE(body + 4) };
  } else if (id === 'ANMF') {
    anmfCount++;
    const x = buf.readUIntLE(body, 3), y = buf.readUIntLE(body + 3, 3);
    const w = buf.readUIntLE(body + 6, 3) + 1, h = buf.readUIntLE(body + 9, 3) + 1;
    const dur = buf.readUIntLE(body + 12, 3);
    const flags = buf[body + 15];
    const e = { x, y, w, h, dur, flags: '0x' + flags.toString(16) };
    if (!anmfFirst) anmfFirst = e;
    lastAnmf = e;
  }
  off = body + size + (size % 2);
}
console.log(JSON.stringify({ vp8x, anim, anmfCount, anmfFirst, lastAnmf }, null, 2));