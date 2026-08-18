#!/usr/bin/env python3
"""dshpet 素材分割工具(开发期, plan §11)

把 resource/<state>.webp 分割为 resource/<state>/frame_%03d.png + manifest.json,
运行期 anim.rs 优先按 manifest 加载分割帧(启动/切换零解码延迟)。

循环动画:<state>_loop.webp(单独的动作循环文件)同样支持,输出到
resource/<state>_loop/;未提供时程序自动回退为"播完动作后循环尾部 1s"。

用法:
  python split_webp.py                # 全部分割
  python split_webp.py idle           # 只分割 idle
  python split_webp.py idle --scale 0.5   # 输出时降采样(可选,默认原尺寸)

依赖: Pillow(Windows 上 pip install Pillow)
"""
import glob
import json
import os
import struct
import sys

SRC = os.path.join(os.path.dirname(__file__), "..", "resource")


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


def split_one(name, scale=1.0):
    src = os.path.join(SRC, f"{name}.webp")
    if not os.path.exists(src):
        print(f"  skip {name}: {src} not found")
        return
    from PIL import Image
    im = Image.open(src)
    durs = parse_durations(src)
    assert len(durs) == im.n_frames, f"{name}: {len(durs)} != {im.n_frames}"
    out_dir = os.path.join(SRC, name)
    os.makedirs(out_dir, exist_ok=True)
    for i in range(im.n_frames):
        im.seek(i)
        frame = im.convert("RGBA")
        if scale != 1.0:
            w, h = max(1, round(frame.width * scale)), max(1, round(frame.height * scale))
            frame = frame.resize((w, h), Image.LANCZOS)
        frame.save(os.path.join(out_dir, f"frame_{i:03d}.png"))
    # tail 计算(默认 tail_ms=1000)
    acc, start = 0, len(durs)
    for i, d in enumerate(reversed(durs)):
        acc += d
        if acc >= 1000:
            start = len(durs) - 1 - i
            break
    if start == len(durs):
        start = 0
    manifest = {
        "state": name,
        "width": im.width,
        "height": im.height,
        "frame_count": im.n_frames,
        "durations_ms": durs,
        "tail": {"start": start, "end": im.n_frames - 1},
    }
    with open(os.path.join(out_dir, "manifest.json"), "w", encoding="utf-8") as f:
        json.dump(manifest, f, ensure_ascii=False, indent=2)
    print(f"  {name}: {im.n_frames} frames -> {out_dir}/ (tail {start}..{im.n_frames-1})")


def main():
    args = [a for a in sys.argv[1:] if not a.startswith("--")]
    scale = 1.0
    if "--scale" in sys.argv:
        scale = float(sys.argv[sys.argv.index("--scale") + 1])
    targets = args or sorted(
        os.path.basename(p)[:-5]
        for p in glob.glob(os.path.join(SRC, "*.webp"))
    )
    for t in targets:
        split_one(t, scale)


if __name__ == "__main__":
    main()
