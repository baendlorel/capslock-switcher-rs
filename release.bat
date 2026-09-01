@echo off
setlocal
rem 发布脚本:构建 release 版本,并复制一份带版本号的 exe
rem 用法:在项目根目录执行 release.bat

cargo build --release || exit /b 1

rem 从 cargo pkgid 输出(url#version 或 url#name@version)中提取版本号
for /f "delims=" %%i in ('cargo pkgid') do set PKGID=%%i
for %%a in ("%PKGID:#=" "%") do set VER=%%~a
set VER=%VER:*@=%

copy /y target\release\capslock-switcher-rs.exe target\release\capslock-switcher-rs-v%VER%.exe >nul || exit /b 1
echo.
echo 发布完成: target\release\capslock-switcher-rs-v%VER%.exe
endlocal
