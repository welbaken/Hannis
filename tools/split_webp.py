#!/usr/bin/env python3
"""dshpet 素材打包工具(开发期, plan §11)

把 resource/<state>.webp 打包为 sprite sheet:
  resource/<state>.sheet.png   — 所有帧按网格排布在一张大图上
  resource/<state>.sheet.json  — 元数据(单帧尺寸/帧数/网格列数)

运行期 anim.rs 优先按 sheet 加载(单文件一次解码,启动/切换零解码延迟,
同时避免旧拆帧方式在 resource/ 下散落几十个 PNG)。每帧时长不再写入
JSON,播放统一使用 config 的 display.frame_ms(默认 42ms/帧)。

循环动画:<state>_loop.webp 同样支持,输出 <state>_loop.sheet.*。

用法:
  python split_webp.py                # 全部分 webp 打包为 sheet
  python split_webp.py idle           # 只打包 idle
  python split_webp.py idle --scale 0.5   # 打包时降采样(可选,默认原尺寸)

依赖: Pillow(Windows 上 pip install Pillow)
"""
import glob
import json
import os
import sys

SRC = os.path.join(os.path.dirname(__file__), "..", "resource")

# 网格排布时 sheet 的最大宽度限制(避开旧 GPU/解码器 16384 像素上限,
# 同时保证单张 PNG 尺寸合理)。
MAX_SHEET_WIDTH = 8192


def make_sheet(name, scale=1.0, out_dir=SRC):
    src = os.path.join(out_dir, f"{name}.webp")
    if not os.path.exists(src):
        print(f"  skip {name}: {src} not found")
        return
    from PIL import Image
    im = Image.open(src)
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

    # 元数据只保留加载器需要的几何字段;每帧时长统一走 config display.frame_ms。
    meta = {
        "width": w,
        "height": h,
        "frame_count": im.n_frames,
    }

    # sprite sheet:网格排布,frames_per_row 使宽不超过 MAX_SHEET_WIDTH
    fpr = max(1, min(im.n_frames, MAX_SHEET_WIDTH // w))
    rows = (im.n_frames + fpr - 1) // fpr
    meta["frames_per_row"] = fpr
    sheet = Image.new("RGBA", (fpr * w, rows * h), (0, 0, 0, 0))
    for i, frame in enumerate(frames):
        sheet.paste(frame, ((i % fpr) * w, (i // fpr) * h))
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
    args = [a for a in sys.argv[1:] if not a.startswith("--") and a != "--src"]
    scale = 1.0
    if "--scale" in sys.argv:
        scale = float(sys.argv[sys.argv.index("--scale") + 1])
    targets = args or sorted(
        os.path.basename(p)[:-5]
        for p in glob.glob(os.path.join(SRC, "*.webp"))
    )
    print(f"== 打包 {len(targets)} 个 webp 为 sprite sheet(src={SRC}) ==")
    for t in targets:
        make_sheet(t, scale, SRC)


if __name__ == "__main__":
    main()