#!/usr/bin/env bash
# Hannis WSL 交叉编译脚本(Windows 版 build.ps1 的 Linux 等价物)
# 用法: ./build-wsl.sh
#   产物: dist/Hannis.exe(零运行时依赖,仅系统 DLL)
#   之后若需发布到 Windows 盘,运行 ./sync-dist.sh
set -eu

ROOT="$(cd "$(dirname "$0")" && pwd)"
APP="$ROOT/app"
DIST="$ROOT/dist"

# 自愈:从 Windows(NTFS) 复制过来的文件会丢失执行权限与符号链接
TOOLCHAIN_BIN="$ROOT/.rustup/toolchains/stable-x86_64-unknown-linux-gnu"
RUSTLIB_BIN="$TOOLCHAIN_BIN/lib/rustlib/x86_64-unknown-linux-gnu/bin"
chmod +x "$ROOT"/.cargo/bin/* "$TOOLCHAIN_BIN"/bin/* "$RUSTLIB_BIN"/rust-lld \
        "$RUSTLIB_BIN"/rust-objcopy "$RUSTLIB_BIN"/wasm-component-ld \
        "$RUSTLIB_BIN"/gcc-ld/* "$ROOT"/.tools/bin/* "$ROOT"/.tools/zig-x86_64-linux-0.14.1/zig 2>/dev/null || true
if [ ! -e "$RUSTLIB_BIN/gcc-ld/ld" ]; then
  ln -s ../rust-lld "$RUSTLIB_BIN/gcc-ld/ld"
fi

# 载入交叉编译环境(.cargo/.rustup 为本工程自带,无需系统安装 rust)
. "$ROOT/.tools/env.sh"

echo "==> 交叉编译 x86_64-pc-windows-gnu (release)"
(cd "$APP" && cargo build --release --target x86_64-pc-windows-gnu)

echo "==> 组装 $DIST"
rm -rf "$DIST"
mkdir -p "$DIST"
cp "$APP/target/x86_64-pc-windows-gnu/release/hannis.exe" "$DIST/Hannis.exe"
cp -r "$ROOT/resource" "$DIST/resource"
cp "$ROOT/icon.png" "$DIST/icon.png"
if [ -f "$APP/config.json" ]; then
  cp "$APP/config.json" "$DIST/config.json"
fi

echo ""
echo "构建完成: $DIST/Hannis.exe"
echo "同步到 Windows: ./sync-dist.sh"