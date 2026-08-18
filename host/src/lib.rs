//! egui-CLAP-host のライブラリ部分。
//! main.rs (GUI) と bin/smoke.rs (オフライン検証) の両方から使う。
//!
//! **GUI アプリの中身も [`app`] としてここに置いてある。** バイナリ側に残すと
//! `cargo test --lib` から外れるため。`main.rs` は起動だけを受け持つ。

pub mod app;
pub mod audio;
pub mod ccs;
pub mod discovery;
pub mod editor_ui;
pub mod gui;
pub mod host;
pub mod meter;
pub mod midi;
pub mod opus;
pub mod params;
pub mod plugin_window;
pub mod project;
pub mod sequencer;
pub mod swing;
pub mod theme;
pub mod timers;
pub mod waltz;
pub mod wav;
