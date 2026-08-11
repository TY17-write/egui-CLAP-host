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
use clap_host_test::audio::vst3::SharedPlugin;
use clap_host_test::audio::{self};
use clap_host_test::discovery;
use clap_host_test::gui::Vst3GuiManager;
use clap_host_test::host::MainThreadMessage;
use std::error::Error;
use std::path::Path;
use std::time::{Duration, Instant};

/// エディタを開いたあと、貼り付いた中身が落ち着くまで回すメッセージポンプの時間。
///
/// 本体では winit のループが配送しているぶんをここで肩代わりする。
/// 開いた直後にプラグインから飛んでくるリサイズ要求も、この間に届く。
const PUMP: Duration = Duration::from_millis(800);

/// 本体と同じブロック長・サンプルレート (`--busy` の間隔もここから決まる)
const BLOCK_FRAMES: usize = 512;
const SAMPLE_RATE: f64 = 44_100.0;

fn main() -> Result<(), Box<dyn Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    // --hold: 窓を閉じるまで開いたままにして、実際に触れるかを人が確かめるためのもの。
    //
    // **本体との違いはイベントループだけ**にしてある。ここには winit も egui も無く、
    // 素の PeekMessage ループが回っているだけなので、こちらで触れて本体で触れないなら
    // 原因は本体のループ側にある。逆に両方で触れないなら貼り付け方の側にある。
    let hold = args.iter().any(|arg| arg == "--hold");
    // --busy: 本体のオーディオスレッドと同じ間隔で音源を掴み続ける。
    //
    // `--hold` だけだと音源は誰も回していないが、本体では CPAL のコールバックが
    // 毎ブロック `try_lock` して `process_audio` を呼んでいる。**本体で触れず
    // `--hold` で触れる**なら、残っている差はここなので、それを足して試せるようにする。
    let busy = args.iter().any(|arg| arg == "--busy");
    // --egui: 本体と同じく eframe の窓を並べる。**エディタが入力を取りこぼす問題は
    // これでしか再現しない**ので、回帰を見るときはこちらを使う。
    let with_egui = args.iter().any(|arg| arg == "--egui");
    let egui_options = EguiOptions {
        // --repaint-ms N: 再描画を要求する間隔。**フレーム時間以下にすると壊れる**
        // (16 で再現、21 以上で正常。理由は `main.rs` の `update` 末尾)
        repaint_ms: value_after(&args, "--repaint-ms").unwrap_or(33),
    };
    let path = args
        .iter()
        .find(|arg| !arg.starts_with("--"))
        .ok_or("使い方: editor_smoke <path\\to\\plugin.vst3> [--hold]")?;
    let path = Path::new(path);

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
        max_likely_buffer_size: BLOCK_FRAMES as u32,
        sample_rate: SAMPLE_RATE as u32,
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

    // eframe を並べる場合は、本体と同じ順番 (窓が先、エディタは最初のフレームで開く)
    // にしたいので、ここから先は eframe 側へ渡してしまう。
    if with_egui {
        return run_with_egui(shared, gui, target.name.clone(), egui_options);
    }

    // ---- 4. 開く ----
    let (sender, receiver) = crossbeam_channel::unbounded();
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

    // 寸法は `Vst3GuiManager::open` が開いた時点で1行出している (本体と同じ形)。
    if hold {
        if busy {
            println!("--busy: オーディオスレッドを模した処理を並走させます。");
            spawn_audio_like_thread(shared.clone());
        }
        println!("\n--hold: 窓を閉じるまで開いたままにします。実際に触って確かめてください。");
        hold_open(&shared, &mut gui, &receiver);
    }

    let detail = match size {
        Some((width, height)) => format!("{width}x{height} ({elapsed:.2} 秒)"),
        None => format!("大きさ不明 ({elapsed:.2} 秒)"),
    };

    gui.close(&mut shared.lock());
    done("開けた", &detail)
}

/// 本体と同じ形 (eframe の窓 + 毎フレーム処理) でエディタを開く。
///
/// **`--hold` との違いをイベントループと同居する窓だけに絞る**ためのもの。
/// 毎フレームの処理は `main.rs` の VST3 の腕と同じ順番・同じ呼び方にしてある。
fn run_with_egui(
    shared: SharedPlugin,
    gui: Vst3GuiManager,
    name: String,
    options: EguiOptions,
) -> Result<(), Box<dyn Error>> {
    struct SmokeApp {
        shared: SharedPlugin,
        gui: Vst3GuiManager,
        sender: crossbeam_channel::Sender<MainThreadMessage>,
        receiver: crossbeam_channel::Receiver<MainThreadMessage>,
        name: String,
        opened: bool,
        options: EguiOptions,
    }

    impl eframe::App for SmokeApp {
        fn update(&mut self, ctx: &eframe::egui::Context, _frame: &mut eframe::Frame) {
            // 本体と同じく、窓が立ち上がったあとにエディタを開く
            if !self.opened {
                self.opened = true;
                let mut plugin = self.shared.lock();
                if let Err(e) = self.gui.open(&mut plugin, &self.name, self.sender.clone()) {
                    println!("RESULT 開けない {e}");
                }
            }

            eframe::egui::CentralPanel::default().show(ctx, |ui| {
                ui.heading("editor_smoke --egui");
                ui.label("プラグインの窓を触ってみてください。");
                ui.label(format!("再描画の間隔: {} ms", self.options.repaint_ms));
            });

            // ---- ここから下は main.rs の VST3 の腕と同じ ----
            if let Some(mut plugin) = self.shared.try_lock() {
                while let Ok(message) = self.receiver.try_recv() {
                    match message {
                        MainThreadMessage::PluginWindowClosed => self.gui.close(&mut plugin),
                        MainThreadMessage::PluginWindowResized { width, height } => {
                            self.gui.on_user_resized(&mut plugin, width, height)
                        }
                        _ => {}
                    }
                }
                self.gui.poll_resize_request(&plugin);
            }
            ctx.request_repaint_after(Duration::from_millis(self.options.repaint_ms));
        }
    }

    let (sender, receiver) = crossbeam_channel::unbounded();
    let app = SmokeApp {
        shared,
        gui,
        sender,
        receiver,
        name,
        opened: false,
        options,
    };

    eframe::run_native(
        "editor_smoke --egui",
        eframe::NativeOptions::default(),
        Box::new(|_cc| Ok(Box::new(app))),
    )?;
    Ok(())
}

/// `--egui` の設定
#[derive(Debug, Clone, Copy)]
struct EguiOptions {
    /// 再描画を要求する間隔 (ミリ秒)。`--repaint-ms N`
    repaint_ms: u64,
}

/// `--flag N` の N を取り出す
fn value_after(args: &[String], flag: &str) -> Option<u64> {
    let index = args.iter().position(|arg| arg == flag)?;
    args.get(index + 1)?.parse().ok()
}

/// 本体のオーディオスレッドと同じ掴み方で音源を回し続ける。
///
/// CPAL のコールバックがやっていること (`Vst3Processor::process`) のうち、
/// **ミューテックスの取り合いに関わる部分だけ**を真似る。1ブロック 512 フレーム、
/// 44.1kHz なので約 11.6ms ごと。取れなければそのブロックを飛ばすところも同じ。
fn spawn_audio_like_thread(shared: SharedPlugin) {
    use vst3_host::audio::AudioBuffers;

    std::thread::spawn(move || {
        let mut buffers = AudioBuffers::new(0, 2, BLOCK_FRAMES, SAMPLE_RATE);
        let interval = Duration::from_secs_f64(BLOCK_FRAMES as f64 / SAMPLE_RATE);
        loop {
            if let Some(mut plugin) = shared.try_lock() {
                buffers.clear();
                let _ = plugin.process_audio(&mut buffers);
            }
            std::thread::sleep(interval);
        }
    });
}

/// 窓を閉じられるまで、メッセージを配送し続ける。
///
/// **本体の毎フレーム処理と同じことだけをする** (`main.rs` の VST3 の腕を参照)。
/// 音源は `try_lock` でしか触らず、取れなければ次に回す。
fn hold_open(
    shared: &SharedPlugin,
    gui: &mut Vst3GuiManager,
    receiver: &crossbeam_channel::Receiver<MainThreadMessage>,
) {
    loop {
        pump_messages();

        let Some(mut plugin) = shared.try_lock() else {
            std::thread::sleep(Duration::from_millis(16));
            continue;
        };
        while let Ok(message) = receiver.try_recv() {
            match message {
                MainThreadMessage::PluginWindowClosed => {
                    gui.close(&mut plugin);
                    println!("窓が閉じられました。");
                    return;
                }
                MainThreadMessage::PluginWindowResized { width, height } => {
                    gui.on_user_resized(&mut plugin, width, height)
                }
                _ => {}
            }
        }
        gui.poll_resize_request(&plugin);
        drop(plugin);

        std::thread::sleep(Duration::from_millis(16));
    }
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
