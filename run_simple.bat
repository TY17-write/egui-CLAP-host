@echo off
set "ARGS=target\debug\test_plugin.clap"
cargo run -p clap-host-test --bin clap-host-test -- %ARGS%
if errorlevel 1 pause