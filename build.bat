@echo off
title Codex App Transfer - 构建工具
chcp 65001 >nul

:MENU
cls
echo ========================================
echo    Codex App Transfer v1.0.0 - 构建工具
echo ========================================
echo.
echo  请选择打包方式：
echo.
echo    [1] 文件夹模式       —— 启动快，日常调试用
echo    [2] 单文件 exe       —— 一个文件，绿色便携
echo    [3] ZIP 安装包       —— 解压即用
echo    [4] Setup 安装包     —— ⭐ 真正的安装程序（需装 NSIS）
echo.
echo    [Q] 退出
echo.

choice /c 1234Q /n /m "请输入选项 (1/2/3/4/Q): "
set choice=%errorlevel%

if %choice%==5 exit /b 0
if %choice%==1 set MODE=folder
if %choice%==2 set MODE=onefile
if %choice%==3 set MODE=zip
if %choice%==4 set MODE=installer
if not defined MODE goto MENU

cls
echo ========================================
echo  正在打包 (%MODE%)...
echo ========================================

cd /d "%~dp0"

REM 检查 Python
python --version >nul 2>&1
if %errorlevel% neq 0 (
    echo [错误] 未安装 Python，请先安装 Python 3.9+
    pause
    exit /b 1
)

REM 安装依赖
echo [1/3] 安装依赖...
pip install -r requirements.txt >nul 2>&1
if %errorlevel% neq 0 (
    echo [错误] 依赖安装失败
    pause
    exit /b 1
)
echo  依赖安装完成

REM 清理旧构建
if exist dist\Codex-App-Transfer rmdir /s /q dist\Codex-App-Transfer >nul 2>&1
if exist Codex-App-Transfer.zip del Codex-App-Transfer.zip >nul 2>&1
if exist Codex-App-Transfer-Setup-*.exe del Codex-App-Transfer-Setup-*.exe >nul 2>&1
if exist build rmdir /s /q build >nul 2>&1

REM 执行打包
echo [2/3] 正在打包...

if "%MODE%"=="onefile" (
    set CCDS_ONEFILE=1
    python -m PyInstaller --noconfirm --clean build.spec >nul 2>&1
    set CCDS_ONEFILE=
    if %errorlevel% equ 0 (
        echo  单文件 exe 打包成功！
    ) else (
        echo [错误] 打包失败
        pause
        exit /b 1
    )
)

if "%MODE%"=="folder" (
    set CCDS_ONEFILE=
    python -m PyInstaller --noconfirm --clean build.spec >nul 2>&1
    if %errorlevel% equ 0 (
        echo  文件夹模式打包成功！
    ) else (
        echo [错误] 打包失败
        pause
        exit /b 1
    )
)

if "%MODE%"=="zip" (
    set CCDS_ONEFILE=
    python -m PyInstaller --noconfirm --clean build.spec >nul 2>&1
    if %errorlevel% equ 0 (
        powershell Compress-Archive -Path "dist\Codex-App-Transfer\*" -DestinationPath "Codex-App-Transfer.zip" -Force >nul 2>&1
        echo  ZIP 打包成功！
    ) else (
        echo [错误] 打包失败
        pause
        exit /b 1
    )
)

if "%MODE%"=="installer" (
    REM 先打文件夹
    set CCDS_ONEFILE=
    python -m PyInstaller --noconfirm --clean build.spec >nul 2>&1
    if %errorlevel% neq 0 (
        echo [错误] PyInstaller 打包失败
        pause
        exit /b 1
    )
    echo  PyInstaller 完成，正在制作安装包...

    REM 检查 NSIS
    where makensis >nul 2>&1
    if %errorlevel% neq 0 (
        echo.
        echo [错误] 未找到 NSIS！
        echo.
        echo  请先安装 NSIS 3.0+:
        echo    https://nsis.sourceforge.io/Download
        echo.
        echo  安装后确保 makensis.exe 在 PATH 中
        echo  或手动执行: makensis -DPRODUCT_VERSION=^<x.y.z^> installer.nsi
        echo.
        pause
        exit /b 1
    )

    REM 从 backend/config.py 读取唯一版本号(单一源策略)
    for /f "tokens=*" %%v in ('python -c "from backend.config import APP_VERSION; print(APP_VERSION)"') do set "APP_VERSION=%%v"
    if not defined APP_VERSION set "APP_VERSION=0.0.0"
    makensis /DPRODUCT_VERSION=%APP_VERSION% installer.nsi >nul 2>&1
    if %errorlevel% equ 0 (
        echo  Setup 安装包制作成功！
    ) else (
        echo [错误] NSIS 打包失败
        pause
        exit /b 1
    )
)

echo.
echo [3/3] 完成！
echo ========================================
echo.
echo  输出文件：
if "%MODE%"=="folder" echo    dist\Codex-App-Transfer\
if "%MODE%"=="onefile" echo    dist\Codex-App-Transfer.exe
if "%MODE%"=="zip" echo    Codex-App-Transfer.zip
if "%MODE%"=="installer" dir /b Codex-App-Transfer-Setup-*.exe 2>nul
echo.
echo  启动后会打开 Codex App Transfer 桌面窗口
echo  如需调试浏览器模式，可执行: python main.py --browser
echo ========================================
echo.

pause
goto MENU
