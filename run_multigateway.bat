@echo off
chcp 65001 >nul
title MultiGateway (Rust 高性能版)
cd /d "%~dp0"
setlocal

if not exist "MultiGateway.exe" (
    echo [INFO] 未检测到 MultiGateway.exe，正在自动构建...
    cargo build --locked --release -p mg-server
    if errorlevel 1 exit /b 1
    copy /y "target\release\MultiGateway.exe" "MultiGateway.exe" >nul
)

echo =======================================================
echo   启动 MultiGateway Rust 高性能网关
echo =======================================================
echo.
"%~dp0MultiGateway.exe" --host 127.0.0.1 --port 8791 --data-dir "%~dp0gateway-data"
pause
