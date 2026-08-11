//! VST3 エディタを開けるかだけを見る検証。音は鳴らさない。
//!
//! `vst3_smoke` が音の経路を見るのに対し、こちらは **GUI の経路だけ**を見る。
//! 本体と同じ道 (`activate_vst3_track` → `Vst3GuiManager::open`) を通すので、
//! ここで失敗するものは本体でも失敗する。
//!
//! **音源1つにつき1プロセス**にしてある。エディタの生成はクラッシュしうるし、
//! 音源どうしが同じプロセスで干渉することもあるため、まとめて回すなら
//! シェル側でループして1つずつ起動すること。
//!
//! 出力の最後の行は必ず `RESULT <状態> <詳細>` の形にしてあり、
//! 総なめの結果を機械的に集計できる。
//!
//! 使い方: cargo run -p clap-host-test --bin editor_smoke -- <path\to\plugin.vst3>

#![allow(unsafe_code)]

use clap_host_test::audio::config::StreamAudioConfig;
use clap_host_test::audio::{self};
use clap_host_test::discovery;
use clap_host_test::gui::Vst3GuiManager;
use std::error::Error;
use std::path::Path;
use std::time::{Duration, Instant};

/// エディタを開いたあと、貼り付いた中身が落ち着くまで回すメッセージポンプの時間。
///
/// 本体では winit のループが配送しているぶんをここで肩代わりする。
/// 開いた直後にプラグインから飛んでくるリサイズ要求も、この間に届く。
const PUMP: Duration = Duration::from_millis(800);

fn main() -> Result<(), Box<dyn Error>> {
    let path = std::env::args()
        .nth(1)
        .ok_or("使い方: editor_smoke <path\\to\\plugin.vst3>")?;
    let path = Path::new(&path);

    // ---- 1. 数え上げ ----
    let found = match discovery::load_vst3_file(path) {
        Ok(found) => found,
        Err(e) => return done("列挙不可", &e.to_string()),
    };
    let target = &found[0];
    println!("プラグイン: {} ({})", target.name, target.id);

    // ---- 2. 装填 (本体と同じ経路。GUI だけ見たいが、Plugin を得るには要る) ----
    let stream_config = StreamAudioConfig {
        output_channel_count: 2,
        min_buffer_size: 1,
        max_likely_buffer_size: 512,
        sample_rate: 44_100,
        sample_format: cpal::SampleFormat::F32,
    };

    let loading = Instant::now();
    let (shared, _processor) = match audio::activate_vst3_track(path, &target.id, &stream_config) {
        Ok(pair) => pair,
        Err(e) => return done("装填不可", &e.to_string()),
    };
    println!("装填: {:.2} 秒", loading.elapsed().as_secs_f64());

    // ---- 3. エディタを持つか ----
    let mut gui = Vst3GuiManager::new(&shared.lock());
    if !gui.supports_gui() {
        return done("GUI なし", "has_editor() が false");
    }

    // ---- 4. 開く ----
    // 受け手は捨てる。ここでは窓からの通知 (閉じる・リサイズ) を処理しないので、
    // 溜まっても構わない (crossbeam の unbounded なので詰まらない)。
    let (sender, _receiver) = crossbeam_channel::unbounded();
    let opening = Instant::now();
    let opened = {
        let mut plugin = shared.lock();
        gui.open(&mut plugin, &target.name, sender)
    };
    let elapsed = opening.elapsed().as_secs_f64();

    if let Err(e) = opened {
        return done("開けない", &format!("{e} ({elapsed:.2} 秒)"));
    }

    // ---- 5. 開いたあと ----
    // 貼り付いた直後はプラグインがまだ自分の大きさを直しに来る (resizeView)。
    // ポンプを回している間に受け取り、本体と同じように窓へ反映する。
    let deadline = Instant::now() + PUMP;
    while Instant::now() < deadline {
        pump_messages();
        if let Some(plugin) = shared.try_lock() {
            gui.poll_resize_request(&plugin);
        }
        std::thread::sleep(Duration::from_millis(16));
    }

    let size = {
        let plugin = shared.lock();
        plugin.get_editor_size().ok()
    };
    let detail = match size {
        Some((width, height)) => format!("{width}x{height} ({elapsed:.2} 秒)"),
        None => format!("大きさ不明 ({elapsed:.2} 秒)"),
    };

    gui.close(&mut shared.lock());
    done("開けた", &detail)
}

/// 最後の1行を決まった形で出す。総なめの集計はこの行だけを見る。
fn done(status: &str, detail: &str) -> Result<(), Box<dyn Error>> {
    println!("RESULT {status} {detail}");
    Ok(())
}

/// 溜まっているウィンドウメッセージを捌く。
///
/// 本体では winit のループが同一スレッドの全ウィンドウへ配送している
/// (`plugin_window.rs` の冒頭を参照)。ここにはそのループが無いので、
/// 同じことを自前で回す。これが無いと、貼り付けた中身が描画も応答もしないまま
/// 「開けた」と報告してしまう。
fn pump_messages() {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        DispatchMessageW, PeekMessageW, TranslateMessage, MSG, PM_REMOVE,
    };

    unsafe {
        let mut message: MSG = std::mem::zeroed();
        while PeekMessageW(&mut message, std::ptr::null_mut(), 0, 0, PM_REMOVE) != 0 {
            TranslateMessage(&message);
            DispatchMessageW(&message);
        }
    }
}
