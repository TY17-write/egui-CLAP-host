//! egui-CLAP-host: CLAP / VST3 をロードして egui から鳴らすマルチトラックのホスト。
//!
//! **ここは起動だけを受け持つ。** アプリの中身は
//! [`egui_clap_host::app`](egui_clap_host::app) にあり、ライブラリ側に置いてあるので
//! `cargo test --lib` に一緒に乗る。

// GUI アプリなので、リリースビルドでは起動時に黒いコンソールを出さない
// (Windows のみ効く)。引き換えに標準出力・標準エラーの行き先が無くなるため、
// デバッグビルドには付けず、cargo run でパニックのメッセージを見られるままにする。
// smoke 系のバイナリはコンソールで使うものなので、こちらには付けない。
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use egui_clap_host::{app::App, theme};

use eframe::egui;
use std::path::PathBuf;

fn main() -> eframe::Result {
    // 検証用 CLI: egui-clap-host.exe [plugin.clap] [--open-gui]
    let args: Vec<String> = std::env::args().skip(1).collect();

    // **走査の子プロセスとして起てられた場合は、ここで終わる。**
    // 窓を出さず、1ファイル開いて結果を書くだけ (egui_clap_host::subscan)。
    // 落ちてよい相手として親から呼ばれるので、GUI の初期化より先に見る
    if let Some(code) = egui_clap_host::subscan::child_main(&args) {
        std::process::exit(code);
    }

    let auto_open_gui = args.iter().any(|a| a == "--open-gui");
    let autoload_path = args
        .iter()
        .find(|a| !a.starts_with("--"))
        .map(PathBuf::from);

    let options = eframe::NativeOptions {
        // inner_size は最大化を解除したときに戻る大きさで、
        // 上部のメーター (スペクトル + ラウドネス) が入る幅にしてある。
        // これより狭いと、オーディオトラックのボタンとメーターが折り返す。
        // 起動時の最大化はここ (ViewportBuilder::with_maximized) では行わない:
        // inner_size と併用すると Windows では「最大化フラグだけ立って寸法が
        // 従来サイズのまま」になる。代わりに下でコマンドとして送る
        viewport: egui::ViewportBuilder::default().with_inner_size([1320.0, 620.0]),
        // **vsync は切る。** ON だと毎フレームの SwapBuffers が次の垂直同期まで
        // スレッドを塞ぎ (最大 16.7ms)、内蔵 GPU の古いドライバはそれを
        // ビジーウェイトで行う。同じスレッドで動くプラグインのエディタが
        // その間止まり、2コア機ではエディタがほぼ動かなくなった
        // (ホスト自身の描画は 3ms なのに、エディタのタイトルバーを掴んで
        // ホストの描画を止めると滑らかになる、という症状)。
        // 描画の間隔は App が request_repaint_after で自前に刻んでいるので、
        // 上限としての vsync は要らない。DWM が合成するので tearing も出ない
        vsync: false,
        ..Default::default()
    };
    eframe::run_native(
        "egui-CLAP-host",
        options,
        Box::new(move |cc| {
            setup_japanese_fonts(&cc.egui_ctx);
            theme::apply(&cc.egui_ctx);
            // 起動時に最大化する。コマンドはキューされ、最初のフレームの後に
            // ウィンドウへ適用される (生成時に指定しない理由は上のコメント)
            cc.egui_ctx
                .send_viewport_cmd(egui::ViewportCommand::Maximized(true));
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
