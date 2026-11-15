@echo off
title Pylon
cd /d "%~dp0"

echo ========================================
echo  Pylon - one-click start
echo ========================================
echo.

REM 1. Check Rust toolchain
where cargo >nul 2>nul
if not errorlevel 1 goto :havecargo
echo [ERROR] cargo not found. Install Rust MSVC build first:
echo         https://rustup.rs/
echo Close this window, install Rust, then double-click start.bat again.
pause
exit /b 1
:havecargo

REM 2. Check MSVC linker. It is never on PATH by default, so also look
REM    for the Visual Studio C++ tools folder.
where link >nul 2>nul
if not errorlevel 1 goto :havelink
if exist "C:\Program Files (x86)\Microsoft Visual Studio\2022\BuildTools\VC\Tools\MSVC" goto :havelink
if exist "C:\Program Files\Microsoft Visual Studio\2022\BuildTools\VC\Tools\MSVC" goto :havelink
if exist "C:\Program Files (x86)\Microsoft Visual Studio\2022\Community\VC\Tools\MSVC" goto :havelink
if exist "C:\Program Files\Microsoft Visual Studio\2022\Community\VC\Tools\MSVC" goto :havelink
if exist "C:\Program Files (x86)\Microsoft Visual Studio\2022\Professional\VC\Tools\MSVC" goto :havelink
if exist "C:\Program Files\Microsoft Visual Studio\2022\Professional\VC\Tools\MSVC" goto :havelink
if exist "C:\Program Files (x86)\Microsoft Visual Studio\2022\Enterprise\VC\Tools\MSVC" goto :havelink
if exist "C:\Program Files\Microsoft Visual Studio\2022\Enterprise\VC\Tools\MSVC" goto :havelink
if exist "C:\Program Files (x86)\Microsoft Visual Studio\2019\BuildTools\VC\Tools\MSVC" goto :havelink
if exist "C:\Program Files\Microsoft Visual Studio\2019\BuildTools\VC\Tools\MSVC" goto :havelink
if exist "C:\Program Files (x86)\Microsoft Visual Studio\2019\Community\VC\Tools\MSVC" goto :havelink
if exist "C:\Program Files\Microsoft Visual Studio\2019\Community\VC\Tools\MSVC" goto :havelink
echo [ERROR] MSVC linker link.exe not found, and no Visual Studio C++ tools folder found.
echo Install Visual Studio Build Tools with the C++ workload, then try again:
echo         https://visualstudio.microsoft.com/downloads/
echo In the installer, check the workload named Desktop development with C++.
pause
exit /b 1
:havelink

REM 3. Warn if a stray Cargo.toml sits in the parent folder
if not exist "%~dp0..\Cargo.toml" goto :nowarn
echo [WARN] A stray Cargo.toml exists in the parent folder:
echo        %~dp0..\Cargo.toml
echo        If cargo complains about workspace, delete that file and run again.
echo.
:nowarn

REM 4. First run: copy the example config if none exists
if exist bridge-config.json goto :haveconfig
if not exist bridge-config.example.json goto :haveconfig
copy /y bridge-config.example.json bridge-config.json >nul
echo Created bridge-config.json. Finish the 3 steps inside the app window.
:haveconfig

REM 5. Build, then launch detached. v1.0: the exe runs with no console
REM    window - windows_subsystem - only the tray icon stays in the taskbar
REM    corner, so this CMD window closes itself right after launching.
echo Building... first build takes a while, please wait...
echo.
REM Old bridge may still be running and locking the exe; stop it first.
REM Without this, cargo fails with os error 5 when it tries to replace it.
taskkill /F /IM muse-local-bridge.exe >nul 2>nul
cargo build --release
if not errorlevel 1 goto :built
echo.
echo [FAILED] The error is printed above. Send the last 20 lines to Astro.
pause
exit /b 1
:built
REM v2.3.3: pass --config explicitly. Release builds no longer read the
REM current working directory for bridge-config.json (a controllable CWD
REM could inject an attacker's token/roots); --config is the sanctioned
REM way to point at a config that is not next to the exe.
start "" "%~dp0target\release\muse-local-bridge.exe" --config "%~dp0bridge-config.json"
exit /b 0
