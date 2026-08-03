#!/usr/bin/env bash
# ============================================================
#  rl-index Windows 交叉编译脚本（在 Linux/macOS 上运行）
#
#  前置条件:
#    1. rustup target add x86_64-pc-windows-gnu
#    2. mingw-w64 交叉编译器:
#         Ubuntu/Debian: sudo apt-get install gcc-mingw-w64-x86-64
#         macOS:         brew install mingw-w64
#
#  用法: scripts/cross-build-windows.sh
#  产物: target/x86_64-pc-windows-gnu/release/rl-index.exe
# ============================================================
set -euo pipefail
cd "$(dirname "$0")/.."

TARGET=x86_64-pc-windows-gnu

if ! rustup target list --installed | grep -q "$TARGET"; then
    echo "[*] 安装 rust target: $TARGET"
    rustup target add "$TARGET"
fi

if ! command -v x86_64-w64-mingw32-gcc >/dev/null 2>&1; then
    echo "[错误] 未找到 x86_64-w64-mingw32-gcc"
    echo "  Ubuntu/Debian: sudo apt-get install gcc-mingw-w64-x86-64"
    echo "  macOS:         brew install mingw-w64"
    exit 1
fi

echo "[1/2] 交叉编译 rl-index -> $TARGET ..."
cargo build --release --target "$TARGET" -p rustlucene-core --bin rl-index

EXE=target/$TARGET/release/rl-index.exe
echo "[2/2] 完成! 产物: $EXE ($(du -h "$EXE" | cut -f1))"
file "$EXE" 2>/dev/null || true
