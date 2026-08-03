@echo off
REM ============================================================
REM  rl-index Windows 构建脚本（在 Windows 上直接运行）
REM
REM  前置条件:
REM    1. 安装 Rust (https://rustup.rs)，默认 MSVC 工具链即可
REM    2. 安装 Visual Studio Build Tools（含 C++ 桌面开发负载，
REM       lz4-sys 需要 MSVC 编译 C 代码）
REM
REM  用法: scripts\build-windows.bat
REM  产物: target\release\rl-index.exe
REM ============================================================

setlocal
cd /d "%~dp0\.."

where cargo >nul 2>nul
if errorlevel 1 (
    echo [错误] 未找到 cargo，请先安装 Rust: https://rustup.rs
    exit /b 1
)

echo [1/2] 编译 rl-index (release)...
cargo build --release -p rustlucene-core --bin rl-index
if errorlevel 1 (
    echo [错误] 编译失败
    exit /b 1
)

echo [2/2] 完成! 可执行文件:
echo     %CD%\target\release\rl-index.exe
echo.
echo 用法示例:
echo     rl-index.exe index  C:\logs C:\idx --include "*.log"
echo     rl-index.exe search C:\idx "connection timeout" --top 5
echo     rl-index.exe stats  C:\idx
endlocal
