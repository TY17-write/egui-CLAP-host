//! egui-CLAP-host: CLAP / VST3 をロードして egui から鳴らすマルチトラックのホスト。
//!
//! **ここは起動だけを受け持つ。** アプリの中身は
//! [`egui_clap_host::app`](egui_clap_host::app) にあり、ライブラリ側に置いてあるので
//! `cargo test --lib` に一緒に乗る。

// GUI アプリなので、起動時に黒いコンソールを出さない (Windows のみ効く)。
// 引き換えに標準出力・標準エラーの行き先が無くなるため、cargo run で
// パニックのメッセージを見たいときは一時的に外すこと。
// smoke 系のバイナリはコンソールで使うものなので、こちらには付けない。
//#![windows_subsystem = "windows"]

use egui_clap_host::{app::App, theme};

use eframe::egui;
use std::path::PathBuf;

fn main() -> eframe::Result {
    // 検証用 CLI: egui-clap-host.exe [plugin.clap] [--open-gui]
    let args: Vec<String> = std::env::args().skip(1).collect();
    let auto_open_gui = args.iter().any(|a| a == "--open-gui");
    let autoload_path = args
        .iter()
        .find(|a| !a.starts_with("--"))
        .map(PathBuf::from);

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_inner_size([1100.0, 580.0]),
        ..Default::default()
    };
    eframe::run_native(
        "egui-CLAP-host",
        options,
        Box::new(move |cc| {
            setup_japanese_fonts(&cc.egui_ctx);
            theme::apply(&cc.egui_ctx);
            Ok(Box::new(App::with_autoload(
                autoload_path.map(|p| (p, auto_open_gui)),
            )))
        }),
    )
}

/// Windows のシステムフォントから日本語フォントを読み込む
fn setup_japanese_fonts(ctx: &egui::Context) {
    let candidates = [
        r"C:\Windows\Fonts\meiryo.ttc",
        r"C:\Windows\Fonts\YuGothM.ttc",
        r"C:\Windows\Fonts\msgothic.ttc",
    ];

    let mut fonts = egui::FontDefinitions::default();
    for path in candidates {
        let Ok(data) = std::fs::read(path) else {
            continue;
        };
        fonts
            .font_data
            .insert("japanese".into(), egui::FontData::from_owned(data).into());
        for family in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
            fonts
                .families
                .get_mut(&family)
                .unwrap()
                .push("japanese".into());
        }
        break;
    }
    ctx.set_fonts(fonts);
}
