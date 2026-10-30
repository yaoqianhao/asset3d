@echo off
chcp 936 >nul
setlocal
cd /d "%~dp0"
title asset3d 一键打包
echo ============================================================
echo   asset3d 一键打包（Windows x64 / Release）
echo   构建 asset3d.exe / asset3d-gui.exe
echo   生成 plugin.json（UTF-8 无 BOM）与版本化 zip
echo ============================================================
echo.
powershell -NoProfile -ExecutionPolicy Bypass -File "%~dp0build-exe.ps1" %*
set "RC=%ERRORLEVEL%"
chcp 936 >nul
echo.
if not "%RC%"=="0" (
    echo [失败] 打包未完成，退出码 %RC%。请查看上方红色错误信息。
    echo.
    pause
    exit /b %RC%
)
echo [成功] 打包完成，产物见上方路径与大小。
pause
exit /b 0
