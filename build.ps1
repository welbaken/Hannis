# Hannis Windows 本机构建脚本(MSVC 或 GNU 工具链均可)
# 用法: powershell -ExecutionPolicy Bypass -File build.ps1
$ErrorActionPreference = "Stop"

Set-Location $PSScriptRoot

if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
    Write-Error "cargo 未找到,请先安装 rustup: https://rustup.rs"
}

# 静态链接 CRT,产物不依赖 VCRUNTIME/msvcrt 之外的运行库
$env:RUSTFLAGS = "-C target-feature=+crt-static"

cargo build --release
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }

$exe = "app\target\release\hannis.exe"
$dist = "dist"

New-Item -ItemType Directory -Force -Path $dist | Out-Null
Copy-Item $exe (Join-Path $dist "hannis.exe") -Force
if (Test-Path "resource") {
    if (Test-Path (Join-Path $dist "resource")) { Remove-Item (Join-Path $dist "resource") -Recurse -Force }
    Copy-Item "resource" $dist -Recurse
}
if (Test-Path "icon.png") {
    Copy-Item "icon.png" $dist -Force
}
if (-not (Test-Path (Join-Path $dist "config.json"))) {
    Copy-Item "app\config.json" $dist -ErrorAction SilentlyContinue
}

Write-Host ""
Write-Host "构建完成: $dist\hannis.exe (解压即用)"
Write-Host "运行: 双击 dist\hannis.exe"