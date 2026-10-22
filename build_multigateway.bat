@echo off
chcp 65001 >nul
setlocal
cd /d "%~dp0"

echo =======================================================
echo   MultiGateway Rust - 一键打包 Release EXE
echo =======================================================
echo.
echo 正在执行 Release 极速构建 (LTO + Strip + Windows 图标内嵌)...
cargo build --locked --release -p mg-server

if %ERRORLEVEL% equ 0 (
    if not exist "dist" mkdir "dist"
    copy /y "target\release\MultiGateway.exe" "MultiGateway.exe" >nul
    copy /y "target\release\MultiGateway.exe" "dist\MultiGateway.exe" >nul
    echo.
    echo =======================================================
    echo   [构建成功]
    echo   - 根目录文件: MultiGateway.exe
    echo   - 发布包目录: dist\
    echo =======================================================
) else (
    echo.
    echo [错误] 构建失败，请检查编译日志。
)
pause
