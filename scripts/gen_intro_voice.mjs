#!/usr/bin/env node
/**
 * 用本地 Qwen3-TTS 服务(http://127.0.0.1:7860)为 web/intro.html 的每句台词预生成配音。
 *
 * - 台词(LINES)与朗读替换规则(SAY_MAP)直接从 web/intro.html 解析,与页面永远同一份源
 * - 产物: web/intro_voice/lineNN.wav (24kHz 16bit mono PCM)
 * - 默认使用声线库中的 "hannis"(d188bc45c3) 克隆声线
 * - 已存在的 wav 跳过(可断点续跑);--force 全部重生成
 *
 * 用法: node scripts/gen_intro_voice.mjs [--voice <voice_id>] [--force]
 */
import { readFileSync, writeFileSync, existsSync, mkdirSync, statSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const ROOT = join(dirname(fileURLToPath(import.meta.url)), "..");
const API = "http://127.0.0.1:7860";
const OUT = join(ROOT, "web", "intro_voice");
const FORCE = process.argv.includes("--force");
const VOICE_ID = process.argv.includes("--voice")
  ? process.argv[process.argv.indexOf("--voice") + 1]
  : "d188bc45c3"; // 声线库中的 "hannis"

/* —— 从 intro.html 中提取 LINES 与 SAY_MAP(与页面同一份源,避免两处维护) —— */
function block(name) {
  const html = readFileSync(join(ROOT, "web", "intro.html"), "utf8");
  const start = html.indexOf(`const ${name} = [`);
  if (start < 0) throw new Error(`intro.html 中未找到 ${name}`);
  const end = html.indexOf("\n];", start);
  if (end < 0) throw new Error(`intro.html 中 ${name} 未正常闭合`);
  const literal = html.slice(html.indexOf("[", start), end + 2);
  return new Function(`return (${literal})`)();
}
const LINES = block("LINES");
const SAY_MAP = block("SAY_MAP");
const sayText = t => SAY_MAP.reduce((s, [re, rep]) => s.replace(re, rep), t);

mkdirSync(OUT, { recursive: true });

async function genOne(i) {
  const file = join(OUT, `line${String(i).padStart(2, "0")}.wav`);
  if (!FORCE && existsSync(file) && statSync(file).size > 44) {
    console.log(`[${i + 1}/${LINES.length}] 已存在,跳过`);
    return;
  }
  const text = sayText(LINES[i]);
  for (let attempt = 1; ; attempt++) {
    try {
      const r = await fetch(`${API}/api/generate`, {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ voice_id: VOICE_ID, text, language: "Auto" }),
      });
      const j = await r.json();
      if (!r.ok || !j.audio_url) throw new Error(j.error || `HTTP ${r.status}`);
      const wav = Buffer.from(await (await fetch(API + j.audio_url)).arrayBuffer());
      const dur = ((wav.length - 44) / (j.sr * 2)).toFixed(2); // 16bit mono → 秒
      writeFileSync(file, wav);
      console.log(`[${i + 1}/${LINES.length}] ${dur}s  ${text}`);
      return;
    } catch (e) {
      if (attempt >= 3) throw e;
      console.error(`  第${i + 1}句重试 ${attempt}/2: ${e.message}`);
      await new Promise(res => setTimeout(res, 2000));
    }
  }
}

console.log(`声线: ${VOICE_ID}  共 ${LINES.length} 句  →  ${OUT}`);
for (let i = 0; i < LINES.length; i++) await genOne(i);
console.log("全部完成 ✓");
