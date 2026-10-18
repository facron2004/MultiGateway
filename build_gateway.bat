@echo off
chcp 65001 >nul
cd /d "%~dp0"
set "GATEWAY_DEPS=%~dp0.gateway-deps"
py -3.13 -m pip install --target "%GATEWAY_DEPS%" --upgrade -r requirements-gateway.txt
if errorlevel 1 exit /b 1
set "PYTHONPATH=%GATEWAY_DEPS%;%PYTHONPATH%"
py -3.13 -m PyInstaller --noconfirm --distpath gateway-dist --workpath gateway-build MultiGateway.spec
if errorlevel 1 exit /b 1
copy /y "gateway-dist\MultiGateway.exe" "MultiGateway.exe" >nul
if errorlevel 1 exit /b 1
echo MultiGateway.exe 构建完成。
