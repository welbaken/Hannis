#!/bin/sh
# 将本地 WSL 编译产出的 dist 同步到 Windows 侧 /mnt/d/src/dshpet/dist
# 用法: ./sync-dist.sh [目标目录]
#   默认目标: /mnt/d/src/dshpet/dist (Windows D 盘)
#   可传参覆盖,例如: ./sync-dist.sh /mnt/d/src/other/dist
#
# 规则(与 build.ps1 保持一致):
#   - 镜像整个 dist(Hannis.exe / resource/ / icon.png 等编译产物),目标端多余文件删除
#   - 排除 Hannis.old、hannis.log(本地备份/运行日志,不传播)
#   - config.json 仅当目标端不存在时复制,避免覆盖 Windows 侧手动修改的配置
set -eu

SRC="$(cd "$(dirname "$0")" && pwd)/dist"
DST="${1:-/mnt/d/src/dshpet/dist}"

if [ ! -d "$SRC" ]; then
  echo "错误: 未找到本地 dist 目录: $SRC (请先运行 ./build-wsl.sh)" >&2
  exit 1
fi
if [ ! -d "$DST" ]; then
  echo "错误: 目标目录不存在: $DST" >&2
  echo "      请确认 WSL 已挂载 Windows 盘(/mnt/d),或手动指定目标目录。" >&2
  exit 1
fi

echo "==> 同步 $SRC → $DST"
rsync -rt --delete \
  --exclude 'Hannis.old' \
  --exclude 'hannis.log' \
  --exclude 'config.json' \
  "$SRC/" "$DST/"

# config.json:仅当目标不存在时复制(与 build.ps1 相同规则)
if [ ! -e "$DST/config.json" ] && [ -f "$SRC/config.json" ]; then
  cp "$SRC/config.json" "$DST/config.json"
  echo "    已生成默认 config.json"
fi

echo "==> 同步完成,目标内容:"
ls -la "$DST"