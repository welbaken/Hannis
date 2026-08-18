#!/usr/bin/env python3
"""dshpet 素材打包工具(开发期, plan §11)

把 resource/<state>.webp 打包为 sprite sheet:
  resource/<state>.sheet.png   — 所有帧按网格排布在一张大图上
  resource/<state>.sheet.json  — 元数据(单帧尺寸/帧数/每帧时长/tail)

运行期 anim.rs 优先按 sheet 加载(单文件一次解码,启动/切换零解码延迟,
同时避免旧拆帧方式在 resource/ 下散落几十个 PNG)。

循环动画:<state>_loop.webp 同样支持,输出 <state>_loop.sheet.*。

用法:
  python split_webp.py                # 全部分 webp 打包为 sheet
  python split_webp.py idle           # 只打包 idle
  python split_webp.py idle --scale 0.5   # 打包时降采样(可选,默认原尺寸)
  python split_webp.py --legacy-split     # 旧方式:拆成 frame_%03d.png + manifest.json

依赖: Pillow(Windows 上 pip install Pillow)
"""
import glob
import json
import os
import struct
import sys

SRC = os.path.join(os.path.dirname(__file__), "..", "resource")

# 网格排布时 sheet 的最大宽度限制(避开旧 GPU/解码器 16384 像素上限,
# 同时保证单张 PNG 尺寸合理)。
MAX_SHEET_WIDTH = 8192


def parse_durations(path):
    """从 WebP 容器读取 ANMF 每帧时长(ms)与帧数。"""
    data = open(path, "rb").read()
    pos, durs = 12, []
    while pos < len(data) - 8:
        cid = data[pos:pos + 4]
        size = struct.unpack("<I", data[pos + 4:pos + 8])[0]
        if cid == b"ANMF":
            durs.append(int.from_bytes(data[pos + 20:pos + 23], "little"))
        pos += 8 + size + (size & 1)
    return durs


def make_sheet(name, scale=1.0, legacy=False, out_dir=SRC):
    src = os.path.join(out_dir, f"{name}.webp")
    if not os.path.exists(src):
        print(f"  skip {name}: {src} not found")
        return
    from PIL import Image
    im = Image.open(src)
    durs = parse_durations(src)
    assert len(durs) == im.n_frames, f"{name}: {len(durs)} != {im.n_frames}"
    w, h = im.width, im.height
    if scale != 1.0:
        w, h = max(1, round(w * scale)), max(1, round(h * scale))

    frames = []
    for i in range(im.n_frames):
        im.seek(i)
        frame = im.convert("RGBA")
        if scale != 1.0:
            frame = frame.resize((w, h), Image.LANCZOS)
        frames.append(frame)

    # tail 计算(默认 tail_ms=1000)
    acc, start = 0, len(durs)
    for i, d in enumerate(reversed(durs)):
        acc += d
        if acc >= 1000:
            start = len(durs) - 1 - i
            break
    if start == len(durs):
        start = 0
    meta = {
        "state": name,
        "width": w,
        "height": h,
        "frame_count": im.n_frames,
        "durations_ms": durs,
        "tail": {"start": start, "end": im.n_frames - 1},
    }

    if legacy:
        # 旧方式:拆成目录 frame_%03d.png + manifest.json
        out_dir2 = os.path.join(out_dir, name)
        os.makedirs(out_dir2, exist_ok=True)
        for i, frame in enumerate(frames):
            frame.save(os.path.join(out_dir2, f"frame_{i:03d}.png"))
        with open(os.path.join(out_dir2, "manifest.json"), "w", encoding="utf-8") as f:
            json.dump(meta, f, ensure_ascii=False, indent=2)
        print(f"  {name}: {im.n_frames} frames -> {out_dir2}/ (legacy split)")
        return

    # sprite sheet:网格排布,frames_per_row 使宽不超过 MAX_SHEET_WIDTH
    fpr = max(1, min(im.n_frames, MAX_SHEET_WIDTH // w))
    rows = (im.n_frames + fpr - 1) // fpr
    sheet = Image.new("RGBA", (fpr * w, rows * h), (0, 0, 0, 0))
    for i, frame in enumerate(frames):
        sheet.paste(frame, ((i % fpr) * w, (i // fpr) * h))
    meta["frames_per_row"] = fpr
    sheet.save(os.path.join(out_dir, f"{name}.sheet.png"))
    with open(os.path.join(out_dir, f"{name}.sheet.json"), "w", encoding="utf-8") as f:
        json.dump(meta, f, ensure_ascii=False, indent=2)
    print(
        f"  {name}: {im.n_frames} frames ({w}x{h}) "
        f"-> {name}.sheet.png ({fpr * w}x{rows * h}, {fpr}/row)"
    )


def main():
    global SRC
    if "--src" in sys.argv:
        SRC = os.path.abspath(os.path.expanduser(sys.argv[sys.argv.index("--src") + 1]))
    legacy = "--legacy-split" in sys.argv
    args = [a for a in sys.argv[1:] if not a.startswith("--") and a != "--src"]
    scale = 1.0
    if "--scale" in sys.argv:
        scale = float(sys.argv[sys.argv.index("--scale") + 1])
    targets = args or sorted(
        os.path.basename(p)[:-5]
        for p in glob.glob(os.path.join(SRC, "*.webp"))
    )
    mode = "legacy split" if legacy else "sprite sheet"
    print(f"== 打包 {len(targets)} 个 webp 为 {mode}(src={SRC}) ==")
    for t in targets:
        make_sheet(t, scale, legacy, SRC)


if __name__ == "__main__":
    main()