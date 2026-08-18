const sharp = require('/home/nnn/.nvm/versions/node/v22.23.2/lib/node_modules/@deepseek-ai/dsh/node_modules/sharp');
const fs = require('fs');

const CANVAS_W = 576, CANVAS_H = 736;
const BG = Buffer.from([0xff, 0xff, 0xff, 0xff]);
const LOOP = 1;
const VP8X_FLAGS = 0x12; // alpha(0x10) + animation(0x02), mirrors source files

function parseChunks(buf) {
  const chunks = [];
  let off = 12;
  while (off + 8 <= buf.length) {
    const id = buf.slice(off, off + 4).toString('ascii');
    const size = buf.readUInt32LE(off + 4);
    chunks.push({ id, size, full: buf.slice(off, off + 8 + size) });
    off += 8 + size + (size % 2);
  }
  return chunks;
}

function buildAnmf(x, y, w, h, dur, flags, payload) {
  const hdr = Buffer.alloc(16);
  hdr.writeUIntLE(x, 0, 3);
  hdr.writeUIntLE(y, 3, 3);
  // ANMF stores width/height MINUS ONE (VP8X canvas fields do too); the
  // image-webp decoder adds 1 back, so writing the raw size makes every
  // frame one pixel too large and it rejects the file ("Frame outside
  // image"). This bit fail_loop.webp + attention_loop.webp in the 10:28 run.
  hdr.writeUIntLE(w - 1, 6, 3);
  hdr.writeUIntLE(h - 1, 9, 3);
  hdr.writeUIntLE(dur, 12, 3);
  hdr.writeUInt8(flags, 15);
  const data = Buffer.concat([hdr, payload]);
  const pad = data.length % 2 ? Buffer.from([0]) : Buffer.alloc(0);
  const head = Buffer.alloc(8);
  head.write('ANMF', 0, 'ascii');
  head.writeUInt32LE(data.length, 4);
  return Buffer.concat([head, data, pad]);
}

async function encodeFramePng(pngPath) {
  const single = await sharp(pngPath).webp({ quality: 80 }).toBuffer();
  const chunks = parseChunks(single);
  const vp8x = chunks.filter(c => c.id === 'VP8X');
  if (vp8x.length !== 1) throw new Error('unexpected single-frame layout: ' + pngPath);
  // frame payload mirrors source: full ALPH + VP8 chunks (no VP8X inside ANMF)
  // libvips rejects ANMF payloads whose ALPH chunk has an odd size (it walks
  // payload chunks with RIFF 2-byte alignment). ALPH compressed data tolerates
  // a trailing zero byte (pixel-verified), so pad odd ALPH chunks.
  const out = [];
  for (const c of chunks.filter(c => c.id !== 'VP8X')) {
    if (c.id === 'ALPH' && c.size % 2 === 1) {
      const padded = Buffer.alloc(c.full.length + 1);
      c.full.copy(padded);
      padded.writeUInt32LE(c.size + 1, 4);
      padded[padded.length - 1] = 0;
      out.push(padded);
    } else {
      out.push(c.full);
    }
  }
  return Buffer.concat(out);
}

async function mux(frames, delays) {
  const payloads = [];
  for (const f of frames) payloads.push(await encodeFramePng(f));
  const anmfs = payloads.map((p, i) => buildAnmf(0, 0, CANVAS_W, CANVAS_H, delays[i], 0x02, p));
  const vp8x = Buffer.alloc(18);
  vp8x.write('VP8X', 0, 'ascii');
  vp8x.writeUInt32LE(10, 4);
  vp8x[8] = VP8X_FLAGS;
  vp8x.writeUIntLE(CANVAS_W - 1, 12, 3);
  vp8x.writeUIntLE(CANVAS_H - 1, 15, 3);
  const anim = Buffer.alloc(14);
  anim.write('ANIM', 0, 'ascii');
  anim.writeUInt32LE(6, 4);
  BG.copy(anim, 8);
  anim.writeUInt16LE(LOOP, 12);
  const body = Buffer.concat([vp8x, anim, ...anmfs]);
  const riff = Buffer.alloc(12);
  riff.write('RIFF', 0, 'ascii');
  riff.writeUInt32LE(body.length + 4, 4);
  riff.write('WEBP', 8, 'ascii');
  return Buffer.concat([riff, body]);
}

(async () => {
  const targets = [
    { name: 'attention_loop', dir: 'resource/attention_loop_frames', delaySrc: 'resource/attention_loop.webp' },
    { name: 'fail_loop', dir: 'resource/fail_loop_frames', delaySrc: 'resource/fail_loop.webp' },
    { name: 'think_loop', dir: 'resource/think_loop_frames', delaySrc: null },
  ];
  for (const t of targets) {
    const frames = fs.readdirSync(t.dir).filter(f => f.endsWith('.png')).sort();
    let delays;
    if (t.delaySrc) {
      const m = await sharp(t.delaySrc, { animated: true }).metadata();
      if (m.delay.length !== frames.length) throw new Error(t.name + ': delay length mismatch');
      delays = m.delay;
    } else {
      // think: 21 curated frames think_037..057 = last 21 frames of trimmed think.webp (57)
      const m = await sharp('resource/think.webp', { animated: true }).metadata();
      delays = m.delay.slice(36, 36 + frames.length);
      if (delays.length !== frames.length) throw new Error('think delay slice mismatch');
    }
    const out = await mux(frames.map(f => t.dir + '/' + f), delays);
    const tmp = 'resource/.tmp_' + t.name + '.webp';
    fs.writeFileSync(tmp, out);
    const m = await sharp(tmp, { animated: true }).metadata();
    console.log(t.name, 'pages', m.pages, 'dims', m.width + 'x' + m.height,
      'delay[0..2]', m.delay.slice(0, 3).join(','), 'delayLast', m.delay[m.delay.length - 1],
      'size', out.length);
    for (const k of [0, Math.floor(frames.length / 2), frames.length - 1]) {
      const single = await sharp(t.dir + '/' + frames[k]).webp({ quality: 80 }).toBuffer();
      const a = await sharp(single).raw().toBuffer();
      const b = await sharp(tmp, { page: k }).raw().toBuffer();
      console.log('  frame', k, 'pixel-match', a.equals(b));
    }
  }
})().catch(e => { console.error(e); process.exit(1); });
