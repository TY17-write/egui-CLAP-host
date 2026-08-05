@echo off
rem  CLAP mini host launcher
rem    run.bat                        build + run with test_plugin
rem    run.bat "C:\path\to\x.clap"    run with the given plugin
rem    run.bat --open-gui             test_plugin + open plugin GUI
rem    run.bat "x.clap" --open-gui    given plugin + open plugin GUI
setlocal
cd /d "%~dp0"
set "TEST_PLUGIN=target\debug\test_plugin.clap"
set "ARGS=%*"
if "%~1"=="" set "ARGS=%TEST_PLUGIN%"
if "%~1"=="--open-gui" set "ARGS=%TEST_PLUGIN% %*"
cargo build --workspace
if errorlevel 1 pause
if errorlevel 1 exit /b 1
rem a .clap file is just a renamed DLL
if exist "target\debug\test_plugin.dll" copy /y "target\debug\test_plugin.dll" "%TEST_PLUGIN%" >nul
rem --bin is required: this package also has the smoke / seq_smoke binaries
cargo run -p clap-host-test --bin clap-host-test -- %ARGS%
if errorlevel 1 pause
