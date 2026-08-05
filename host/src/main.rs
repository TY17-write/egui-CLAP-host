//! CLAP ミニホスト: .clap ファイルをロードして egui から鳴らすテスト用ホスト。

use clap_host_test::{audio, discovery, editor_ui, gui, host, midi, params, theme};

use audio::config::StreamAudioConfig;
use audio::transport::{TransportMsg, TransportShared};
use audio::GuiMsg;
use clack_host::prelude::*;
use crossbeam_channel::{Receiver, Sender};
use eframe::egui;
use editor_ui::{EditorCommand, EditorState};
use gui::PluginGuiManager;
use host::{MainThreadMessage, MiniHost, MiniHostMainThread, MiniHostShared};
use params::ParamUi;
use std::sync::atomic::Ordering;
use std::collections::HashSet;
use std::error::Error;
use std::ffi::CString;
use std::path::PathBuf;
use std::time::Duration;

/// 鍵盤に表示する1オクターブ+1音 (C4〜C5) ※鍵盤 UI 無効化中のため未使用
#[allow(dead_code)]
const KEYS: [(u16, &str, bool); 13] = [
    (60, "C4", false),
    (61, "C#", true),
    (62, "D", false),
    (63, "D#", true),
    (64, "E", false),
    (65, "F", false),
    (66, "F#", true),
    (67, "G", false),
    (68, "G#", true),
    (69, "A", false),
    (70, "A#", true),
    (71, "B", false),
    (72, "C5", false),
];

fn main() -> eframe::Result {
    // 検証用 CLI: clap-host-test.exe [plugin.clap] [--open-gui]
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
        "CLAP ミニホスト",
        options,
        Box::new(move |cc| {
            setup_japanese_fonts(&cc.egui_ctx);
            theme::apply(&cc.egui_ctx);
            Ok(Box::new(App {
                autoload: autoload_path.map(|p| (p, auto_open_gui)),
                ..Default::default()
            }))
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

/// ロード済みの .clap ファイル (プラグイン選択待ち)
struct Candidates {
    path: PathBuf,
    entry: PluginEntry,
    plugins: Vec<discovery::FoundPlugin>,
    /// この候補から選んだプラグインを載せるトラック
    target_track: usize,
}

/// オーディオエンジン (出力ストリーム1本と、それを操作する口)。
/// トラックより長生きし、音源はメッセージで出し入れする。
struct Engine {
    _stream: cpal::Stream,
    producer: rtrb::Producer<GuiMsg>,
    /// オーディオスレッドから返ってきた音源 (メインスレッドで deactivate する)。
    /// どのインスタンスへ返すか分かるようトラック番号が付く。
    retired: rtrb::Consumer<(usize, Box<audio::TrackProcessor>)>,
    /// 再生位置・再生中フラグの共有
    transport_shared: TransportShared,
    /// 全プラグイン共通のストリーム構成
    config: StreamAudioConfig,
}

/// トラック1本にロードされた音源
struct TrackAudio {
    name: String,
    instance: PluginInstance<MiniHost>,
    receiver: Receiver<MainThreadMessage>,
    sender: Sender<MainThreadMessage>,
    #[allow(dead_code)] // パラメータ UI 無効化中
    params: Vec<ParamUi>,
    #[allow(dead_code)] // 鍵盤 UI 無効化中
    pressed_keys: HashSet<u16>,
    /// プラグイン独自 GUI の管理 (gui 拡張がない場合は None)
    gui: Option<PluginGuiManager>,
}

impl Drop for TrackAudio {
    fn drop(&mut self) {
        // インスタンス破棄前にプラグイン GUI を確実に閉じる
        if let Some(gui) = &mut self.gui {
            gui.close(&mut self.instance.plugin_handle());
        }
    }
}

/// 描画ループの中でファイルダイアログを開けないので、種類だけ持ち帰る
enum FileAction {
    Import,
    Export,
    Save,
}

/// 表示用のファイル名 (取れなければパス全体)
fn file_label(path: &std::path::Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

#[derive(Default)]
struct App {
    candidates: Option<Candidates>,
    // 宣言順にドロップされるため、ストリームをインスタンスより先に止める
    engine: Option<Engine>,
    /// トラックごとの音源 (エディタのトラック数に合わせて伸ばす)
    tracks: Vec<Option<TrackAudio>>,
    /// 差し替えで外したが、まだ処理器が返ってきていないインスタンス。
    /// 返却時に正しいインスタンスへ deactivate するため、ここで生かしておく。
    retiring: std::collections::VecDeque<(usize, TrackAudio)>,
    error: Option<String>,
    /// 起動時に自動ロードする .clap ファイル (パス, GUI も開くか)
    autoload: Option<(PathBuf, bool)>,
    /// シーケンスエディタの状態 (プラグインのロードをまたいで保持)
    editor: EditorState,
    /// 再生ヘッドの位置 (四分音符単位)。
    /// プラグイン未ロード時はここが本体で、ロード中はトランスポートの位置を写す。
    /// 再生できない状態でも、貼り付け位置などの編集操作に再生ヘッドを使うため。
    pos_quarters: f64,
    /// MIDI の保存先。Ctrl+S はここへ上書きする。
    /// インポートでは設定しない (読み込んだファイルを上書きしないため)
    midi_path: Option<PathBuf>,
    /// 最後に読み書きしたフォルダ (ダイアログの初期位置)
    last_directory: Option<PathBuf>,
    /// 保存・読み込みの結果メッセージ
    status: Option<String>,
}

impl App {
    /// .clap を選ばせて候補を読み込む。
    /// 戻り値は「候補を新しく読み込めたか」。キャンセルや失敗では false を返し、
    /// 前回の候補には触れない (キャンセルで前のプラグインが載らないようにするため)。
    fn open_file_dialog(&mut self, target_track: usize) -> bool {
        let picked = rfd::FileDialog::new()
            .add_filter("CLAP プラグイン", &["clap"])
            .pick_file();

        // キャンセルされたら何もしない (前回の候補もそのまま残す)
        let Some(path) = picked else { return false };

        match discovery::load_clap_file(&path) {
            Ok((entry, plugins)) => {
                self.error = None;
                self.candidates = Some(Candidates {
                    path,
                    entry,
                    plugins,
                    target_track,
                });
                true
            }
            Err(e) => {
                self.error = Some(format!("ロード失敗: {e}"));
                false
            }
        }
    }

    /// MIDI ファイルを選んで読み込む (今のシーケンスは置き換わる)
    fn import_midi(&mut self) {
        let mut dialog = rfd::FileDialog::new().add_filter("MIDI ファイル", &["mid", "midi"]);
        if let Some(directory) = self.dialog_directory() {
            dialog = dialog.set_directory(directory);
        }
        let Some(path) = dialog.pick_file() else { return };
        self.last_directory = path.parent().map(PathBuf::from);

        let bytes = match std::fs::read(&path) {
            Ok(bytes) => bytes,
            Err(e) => {
                self.error = Some(format!("読み込めません: {e}"));
                return;
            }
        };

        match midi::from_bytes(&bytes, self.editor.editor.scale) {
            Ok(imported) => {
                let count = imported.notes.len();
                let name = file_label(&path);
                self.editor
                    .replace_sequence(imported.notes, imported.tempo, imported.time_signature);
                // 読み込み元を保存先にはしない (Ctrl+S で元ファイルを上書きしないため)
                self.midi_path = None;
                self.editor.midi_path = None;
                self.error = None;
                self.status = Some(format!("{name} から {count} 個のノートを読み込みました"));
            }
            Err(e) => self.error = Some(e),
        }
    }

    /// MIDI ファイルへ書き出す。
    /// `ask` が false なら、保存先が決まっていれば黙って上書きする (Ctrl+S)。
    fn export_midi(&mut self, ask: bool) {
        let path = if ask || self.midi_path.is_none() {
            let mut dialog = rfd::FileDialog::new()
                .add_filter("MIDI ファイル", &["mid"])
                .set_file_name("sequence.mid");
            if let Some(directory) = self.dialog_directory() {
                dialog = dialog.set_directory(directory);
            }
            let Some(path) = dialog.save_file() else { return };
            path
        } else {
            self.midi_path.clone().unwrap_or_default()
        };
        // 拡張子を省略されたときは .mid を補う
        let path = if path.extension().is_none() {
            path.with_extension("mid")
        } else {
            path
        };

        let bytes = match midi::to_bytes(&self.editor.editor) {
            Ok(bytes) => bytes,
            Err(e) => {
                self.error = Some(e);
                return;
            }
        };

        match std::fs::write(&path, bytes) {
            Ok(()) => {
                let name = file_label(&path);
                self.set_midi_path(path);
                self.error = None;
                self.status = Some(format!("保存しました: {name}"));
            }
            Err(e) => self.error = Some(format!("保存できません: {e}")),
        }
    }

    /// 保存先を覚える (エディタ側には表示用のファイル名だけ渡す)
    fn set_midi_path(&mut self, path: PathBuf) {
        self.last_directory = path.parent().map(PathBuf::from);
        self.editor.midi_path = Some(file_label(&path));
        self.midi_path = Some(path);
    }

    /// ファイルダイアログを開くフォルダ
    fn dialog_directory(&self) -> Option<PathBuf> {
        self.midi_path
            .as_ref()
            .and_then(|path| path.parent())
            .map(PathBuf::from)
            .or_else(|| self.last_directory.clone())
    }

    /// トラック数をエディタに合わせる (足りなければ空きで埋める)
    fn ensure_track_slots(&mut self, track: usize) {
        let needed = (track + 1).max(self.editor.editor.track_count());
        while self.tracks.len() < needed {
            self.tracks.push(None);
        }
    }

    /// 選んだプラグインを指定トラックに載せる
    fn instantiate(&mut self, plugin_index: usize, track: usize) {
        // 最初のロード時にストリームを用意する
        if self.engine.is_none() {
            match start_engine() {
                Ok(engine) => self.engine = Some(engine),
                Err(e) => {
                    self.error = Some(format!("オーディオを開始できません: {e}"));
                    return;
                }
            }
        }
        let Some(engine) = &mut self.engine else {
            return;
        };
        let Some(candidates) = &self.candidates else {
            return;
        };
        let plugin = &candidates.plugins[plugin_index];

        match instantiate_plugin(&candidates.entry, &plugin.id, &plugin.name, &engine.config) {
            Ok((audio_track, processor)) => {
                self.error = None;
                let _ = engine.producer.push(GuiMsg::SetTrack { track, processor });

                // 未ロード中に動かした再生ヘッドの位置を引き継ぐ
                let spq = self
                    .editor
                    .editor
                    .samples_per_quarter(engine.config.sample_rate as f64);
                let sample = (self.pos_quarters * spq).max(0.0) as u64;
                let _ = engine
                    .producer
                    .push(GuiMsg::Transport(TransportMsg::Seek { sample }));

                self.ensure_track_slots(track);
                // 前の音源は、処理器が返ってくるまで生かしておく
                if let Some(previous) = self.tracks[track].take() {
                    self.retiring.push_back((track, previous));
                }
                self.tracks[track] = Some(audio_track);
                // 新しいプラグインにシーケンスを送り直す
                self.editor.dirty = true;
            }
            Err(e) => self.error = Some(format!("インスタンス化失敗: {e}")),
        }
    }
}

/// 出力ストリームを1本だけ作る (最初に音源をロードするときに一度だけ呼ぶ)
fn start_engine() -> Result<Engine, Box<dyn Error>> {
    let (producer, consumer) = rtrb::RingBuffer::new(256);
    // 外した音源をメインスレッドへ返す口 (オーディオスレッドで解放しないため)
    let (retired_producer, retired) = rtrb::RingBuffer::new(8);
    let transport_shared = TransportShared::new();

    let (stream, config) =
        audio::start_engine(consumer, retired_producer, transport_shared.clone())?;

    Ok(Engine {
        _stream: stream,
        producer,
        retired,
        transport_shared,
        config,
    })
}

/// プラグインをインスタンス化して、指定のストリーム構成で鳴らせる状態にする。
/// 戻り値の処理器は呼び出し側がエンジンへ送る。
fn instantiate_plugin(
    entry: &PluginEntry,
    plugin_id: &str,
    plugin_name: &str,
    stream_config: &StreamAudioConfig,
) -> Result<(TrackAudio, Box<audio::TrackProcessor>), Box<dyn Error>> {
    let host_info = HostInfo::new(
        "CLAP Mini Host",
        "clap-host-test",
        "https://example.com",
        "0.1.0",
    )?;
    let plugin_id_cstr = CString::new(plugin_id)?;
    let (sender, receiver) = crossbeam_channel::unbounded();

    let mut instance = PluginInstance::<MiniHost>::new(
        |_| MiniHostShared::new(sender.clone()),
        |shared| MiniHostMainThread::new(shared),
        entry,
        &plugin_id_cstr,
        &host_info,
    )?;

    let processor = audio::activate_track(&mut instance, stream_config)?;
    let params = params::read_params(&mut instance);

    // gui 拡張があれば GUI マネージャを用意する
    let gui_ext = instance.access_handler(|mt| mt.gui);
    let gui = gui_ext.map(|ext| PluginGuiManager::new(ext, &mut instance.plugin_handle()));

    Ok((
        TrackAudio {
            name: plugin_name.to_string(),
            instance,
            receiver,
            sender,
            params,
            pressed_keys: HashSet::new(),
            gui,
        },
        processor,
    ))
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // 起動時の自動ロード (検証用 CLI)
        if let Some((path, open_gui)) = self.autoload.take() {
            match discovery::load_clap_file(&path) {
                Ok((entry, plugins)) => {
                    self.candidates = Some(Candidates {
                        path,
                        entry,
                        plugins,
                        target_track: 0,
                    });
                    self.instantiate(0, 0);
                    if open_gui {
                        if let Some(Some(track)) = self.tracks.get_mut(0) {
                            if let Some(gui) = &mut track.gui {
                                if let Err(e) = gui.open(
                                    &mut track.instance.plugin_handle(),
                                    &track.name,
                                    track.sender.clone(),
                                ) {
                                    self.error = Some(format!("GUI を開けません: {e}"));
                                }
                            }
                        }
                    }
                }
                Err(e) => self.error = Some(format!("自動ロード失敗: {e}")),
            }
        }

        // オーディオスレッドから返ってきた音源をここで停止・解放する
        // (オーディオスレッドで解放してはいけないため)。
        // 差し替えで外したインスタンスが待っていればそちらへ、
        // 無ければ今そのトラックに載っているインスタンスへ返す。
        if let Some(engine) = &mut self.engine {
            while let Ok((track, processor)) = engine.retired.pop() {
                let stopped = processor.into_stopped();
                let waiting = self
                    .retiring
                    .iter()
                    .position(|(index, _)| *index == track);

                match waiting {
                    Some(at) => {
                        if let Some((_, mut old)) = self.retiring.remove(at) {
                            old.instance.deactivate(stopped);
                            // old はここで破棄される (GUI も閉じられる)
                        }
                    }
                    None => {
                        if let Some(Some(current)) = self.tracks.get_mut(track) {
                            current.instance.deactivate(stopped);
                        }
                    }
                }
            }
        }

        // プラグインからのメインスレッド要求 & GUI ウィンドウイベントを処理
        for track in self.tracks.iter_mut().flatten() {
            while let Ok(msg) = track.receiver.try_recv() {
                match msg {
                    MainThreadMessage::RunOnMainThread => {
                        track.instance.call_on_main_thread_callback()
                    }
                    MainThreadMessage::GuiRequestResized { new_size } => {
                        if let Some(gui) = &mut track.gui {
                            gui.on_plugin_request_resize(new_size);
                        }
                    }
                    MainThreadMessage::GuiClosed | MainThreadMessage::PluginWindowClosed => {
                        if let Some(gui) = &mut track.gui {
                            gui.close(&mut track.instance.plugin_handle());
                        }
                    }
                    MainThreadMessage::PluginWindowResized { width, height } => {
                        if let Some(gui) = &mut track.gui {
                            gui.on_user_resized(&mut track.instance.plugin_handle(), width, height);
                        }
                    }
                }
            }

            // プラグインが登録したタイマーを駆動する (GUI 描画などに必要)
            let timer = track
                .instance
                .access_handler(|mt| mt.timer_support.map(|ext| (mt.timers.clone(), ext)));
            if let Some((timers, timer_ext)) = timer {
                timers.tick_timers(&timer_ext, &mut track.instance.plugin_handle());
            }
        }

        egui::CentralPanel::default().show(ctx, |ui| {
            // ヘッダー行 (状態表示)
            ui.horizontal(|ui| {
                ui.heading("CLAP ミニホスト");
                ui.separator();
                // .clap のロードはトラック欄の「♪」から行うので、ここでは出さない
                /*
                if ui.button(".clap ファイルを開く…").clicked() {
                    // ヘッダーからのロードはトラック1へ
                    self.open_file_dialog(0);
                }
                */
                if let Some(error) = &self.error {
                    ui.colored_label(egui::Color32::RED, error);
                }
                if let Some(status) = &self.status {
                    ui.weak(status);
                }
            });

            // プラグイン選択。1つの .clap に複数入っていて、まだ選んでいないときだけ出す
            // (1つだけのファイルはトラック欄の「♪」でそのまま載るので出さない)。
            let mut instantiate_index = None;
            if let Some(candidates) = self
                .candidates
                .as_ref()
                .filter(|candidates| candidates.plugins.len() > 1)
            {
                ui.horizontal_wrapped(|ui| {
                    ui.label(format!(
                        "トラック {} に読み込むプラグインを選択 ({}):",
                        candidates.target_track + 1,
                        file_label(&candidates.path)
                    ));
                    for (i, plugin) in candidates.plugins.iter().enumerate() {
                        if ui
                            .button(format!("▶ {} ({})", plugin.name, plugin.id))
                            .clicked()
                        {
                            instantiate_index = Some(i);
                        }
                    }
                });
            }
            if let Some(i) = instantiate_index {
                let track = self.candidates.as_ref().map_or(0, |c| c.target_track);
                self.instantiate(i, track);
                // 選び終わったら候補を片付ける (選択行を残さない)
                self.candidates = None;
            }

            // ロード済みプラグインの操作 UI (トラックごと)
            let mut gui_error = None;
            for (index, track) in self.tracks.iter_mut().enumerate() {
                let Some(track) = track else { continue };
                ui.horizontal(|ui| {
                    ui.label(format!("トラック {}: {}", index + 1, track.name));

                    // プラグイン独自 GUI の開閉ボタン
                    if let Some(gui) = &mut track.gui {
                        if gui.supports_gui() {
                            if !gui.is_open {
                                let label = if gui.is_floating() {
                                    "エディタを開く (floating)"
                                } else {
                                    "エディタを開く"
                                };
                                if ui.button(label).clicked() {
                                    if let Err(e) = gui.open(
                                        &mut track.instance.plugin_handle(),
                                        &track.name,
                                        track.sender.clone(),
                                    ) {
                                        gui_error = Some(format!("GUI を開けません: {e}"));
                                    }
                                }
                            } else if ui.button("エディタを閉じる").clicked() {
                                gui.close(&mut track.instance.plugin_handle());
                            }
                        }
                    }
                });
            }
            if gui_error.is_some() {
                self.error = gui_error;
            }
            if !self.tracks.iter().flatten().count() == 0 {
                ui.add_space(8.0);
            }
            {

                // パラメータ汎用エディタ (一時的に無効化中)
                /*
                if loaded.params.is_empty() {
                    ui.label("(このプラグインには表示可能なパラメータがありません)");
                } else {
                    egui::ScrollArea::vertical()
                        .max_height((ui.available_height() - 160.0).max(60.0))
                        .show(ui, |ui| {
                            for param in &mut loaded.params {
                                let mut slider =
                                    egui::Slider::new(&mut param.value, param.min..=param.max)
                                        .text(&param.name);
                                if param.is_stepped {
                                    slider = slider.step_by(1.0);
                                }
                                if ui.add(slider).changed() {
                                    let _ = loaded.producer.push(GuiMsg::ParamValue {
                                        track: 0,
                                        id: param.id,
                                        value: param.value,
                                    });
                                }
                            }
                        });
                }
                */

                // 鍵盤 (一時的に無効化中)
                /*
                ui.add_space(12.0);
                ui.separator();
                ui.label("鍵盤 (クリックで発音):");
                ui.horizontal(|ui| {
                    for (key, label, is_black) in KEYS {
                        let color = if is_black {
                            egui::Color32::from_gray(40)
                        } else {
                            egui::Color32::from_gray(230)
                        };
                        let text_color = if is_black {
                            egui::Color32::WHITE
                        } else {
                            egui::Color32::BLACK
                        };
                        let button = egui::Button::new(
                            egui::RichText::new(label).color(text_color).size(11.0),
                        )
                        .fill(color)
                        .min_size(egui::vec2(34.0, 90.0));

                        let response = ui.add(button);
                        let is_down = response.is_pointer_button_down_on();
                        let was_down = loaded.pressed_keys.contains(&key);

                        if is_down && !was_down {
                            loaded.pressed_keys.insert(key);
                            let _ = loaded.producer.push(GuiMsg::NoteOn {
                                track: 0,
                                key,
                                velocity: 0.8,
                            });
                        } else if !is_down && was_down {
                            loaded.pressed_keys.remove(&key);
                            let _ = loaded
                                .producer
                                .push(GuiMsg::NoteOff { track: 0, key });
                        }
                    }
                });
                */
            }

            if self.tracks.iter().flatten().count() == 0 {
                ui.label("音色となるプラグインが未ロードです (左のトラック欄の「♪」から .clap を選択)。ロードしなくてもシーケンスの編集はできます。");
                ui.add_space(8.0);
            }

            // シーケンスエディタ (残りの領域全体を使う)。
            // プラグインは音色として使うだけなので、未ロードでも編集できるようにする。
            ui.separator();
            let sample_rate = self
                .engine
                .as_ref()
                .map_or(48_000.0, |engine| engine.config.sample_rate as f64);
            let spq = self.editor.editor.samples_per_quarter(sample_rate);
            // エンジンがあればトランスポートの位置が本体。無いときは
            // 自前で覚えている位置を使う (シークだけは効くようにする)。
            let playing = match &self.engine {
                Some(engine) => {
                    self.pos_quarters =
                        engine.transport_shared.pos.load(Ordering::Relaxed) as f64 / spq;
                    engine.transport_shared.playing.load(Ordering::Relaxed)
                }
                None => false,
            };

            // トラック欄に出す音源名を渡す
            self.editor.track_plugins = (0..self.editor.editor.track_count())
                .map(|track| {
                    self.tracks
                        .get(track)
                        .and_then(|slot| slot.as_ref())
                        .map(|audio| audio.name.clone())
                })
                .collect();

            let commands =
                editor_ui::editor_panel(ui, &mut self.editor, self.pos_quarters, playing);

            // ファイルダイアログは self 全体を触るので、ループを抜けてから実行する
            let mut file_action = None;
            let mut load_plugin_track = None;

            for command in commands {
                match command {
                    EditorCommand::ImportMidi => file_action = Some(FileAction::Import),
                    EditorCommand::ExportMidi => file_action = Some(FileAction::Export),
                    EditorCommand::SaveMidi => file_action = Some(FileAction::Save),
                    EditorCommand::LoadPlugin { track } => load_plugin_track = Some(track),
                    // エンジンが無いときは送り先がないので、再生ヘッドの移動だけ
                    // 自前で処理する。ロード時に位置とシーケンスを送り直す。
                    EditorCommand::Seek { quarters } if self.engine.is_none() => {
                        self.pos_quarters = quarters.max(0.0);
                    }
                    command => {
                        let Some(engine) = &mut self.engine else {
                            continue;
                        };
                        let msg = match command {
                            EditorCommand::Commit => {
                                // トラックごとに分けて送る (音源が別々のため)
                                let end_sample =
                                    (self.editor.editor.length_quarters_bar_aligned() as f64 * spq)
                                        as u64;
                                for track in 0..self.editor.editor.track_count() {
                                    // ミュート/ソロで鳴らさないトラックは空にして送る
                                    // (再生中でも即座に止まる)
                                    let events = if self.editor.editor.is_audible(track) {
                                        self.editor
                                            .editor
                                            .to_events_for_track(track, sample_rate)
                                            .into_boxed_slice()
                                    } else {
                                        Vec::new().into_boxed_slice()
                                    };
                                    let _ = engine.producer.push(GuiMsg::Transport(
                                        TransportMsg::SetSequence {
                                            track,
                                            events,
                                            end_sample,
                                        },
                                    ));
                                }
                                continue;
                            }
                            EditorCommand::Play => GuiMsg::Transport(TransportMsg::Play),
                            EditorCommand::Stop => GuiMsg::Transport(TransportMsg::Stop),
                            EditorCommand::Seek { quarters } => {
                                GuiMsg::Transport(TransportMsg::Seek {
                                    sample: (quarters * spq).max(0.0) as u64,
                                })
                            }
                            EditorCommand::SetLoop(enabled) => {
                                GuiMsg::Transport(TransportMsg::SetLoop { enabled })
                            }
                            // ファイル操作はループの外で処理済み
                            EditorCommand::ImportMidi
                            | EditorCommand::ExportMidi
                            | EditorCommand::SaveMidi
                            | EditorCommand::LoadPlugin { .. } => continue,
                        };
                        let _ = engine.producer.push(msg);
                    }
                }
            }

            match file_action {
                Some(FileAction::Import) => self.import_midi(),
                Some(FileAction::Export) => self.export_midi(true),
                Some(FileAction::Save) => self.export_midi(false),
                None => {}
            }

            // トラック欄からの音源ロード (ダイアログを開く)。
            // 新しく読み込めたときだけ装填する (キャンセルでは何もしない)
            if let Some(track) = load_plugin_track {
                if self.open_file_dialog(track) {
                    // 候補が1つだけならそのまま載せる (選択の手間を省く)
                    let single = self
                        .candidates
                        .as_ref()
                        .is_some_and(|candidates| candidates.plugins.len() == 1);
                    if single {
                        self.instantiate(0, track);
                    }
                }
            }
        });

        // 鍵盤の離鍵検出などのために定期的に再描画する
        ctx.request_repaint_after(Duration::from_millis(16));
    }
}
