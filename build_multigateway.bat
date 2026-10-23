@echo off
chcp 65001 >nul
setlocal
cd /d "%~dp0"

echo =======================================================
echo   MultiGateway Tauri 2 - 一键打包便携 EXE
echo =======================================================
echo.
echo 正在构建 Tauri 桌面程序 (Rust 网关 + 系统 WebView2)...
cargo build --locked --release -p mg-server --bin MultiGateway

if %ERRORLEVEL% equ 0 (
    if not exist "dist" mkdir "dist"
    copy /y "target\release\MultiGateway.exe" "MultiGateway.exe" >nul
    if errorlevel 1 goto copy_failed
    copy /y "target\release\MultiGateway.exe" "dist\MultiGateway.exe" >nul
    if errorlevel 1 goto copy_failed
    echo.
    echo =======================================================
    echo   [构建成功]
    echo   - 根目录文件: MultiGateway.exe
    echo   - 发布包目录: dist\
    echo =======================================================
) else (
    echo.
    echo [错误] 构建失败，请检查编译日志。
    pause
    exit /b 1
)
pause
exit /b 0

:copy_failed
echo [错误] 复制 EXE 失败，请退出正在运行的 MultiGateway 后重试。
pause
exit /b 1
