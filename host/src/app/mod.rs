//! ホスト本体のアプリケーション状態と、毎フレームの処理。
//!
//! `main.rs` は起動だけを受け持ち、中身はここに置いてある。**バイナリ側ではなく
//! ライブラリに入れてある**ので、`cargo test --lib` に一緒に乗る。
//!
//! [`App`] のフィールドは private のままで、`app` の子モジュール
//! (`routing` / `plugins` / …) からだけ触れる。`impl App` は責務ごとに
//! それらのファイルへ分かれている。

mod library_ui;
mod mixer_ui;
mod monitor_ui;
mod notice;
mod plugins;
mod project_io;
mod render;
mod routing;
mod track;

use notice::{notice_window, Notice};
use plugins::LoadChoice;
use project_io::FileAction;
use track::{AudioTrackUi, Candidates, Engine, TrackAudio, TrackPlugin};

use crate::audio::transport::TransportMsg;
use crate::audio::GuiMsg;
use crate::editor_ui::{EditorCommand, EditorState};
use crate::host::MainThreadMessage;
use crate::library::{Library, Scan};
use crate::meter::Meters;
use crate::{audio, discovery, editor_ui, project};
use eframe::egui;
use library_ui::Tab;
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::time::Duration;

#[derive(Default)]
pub struct App {
    /// 「+」を押したあと、**どの段へ載せようとしているか**。
    ///
    /// `Some` の間はプラグインの窓が「選んで載せる」状態になり、
    /// 一覧の行を押すとそこへ載る。
    pending_load: Option<audio::NodeAddr>,
    /// 一覧に無いものをファイルから直接読む流れに入っているか。
    ///
    /// 形式ごとにダイアログの出し方が違うので、先に [`LoadChoice`] を決める
    /// 必要がある。**一覧から選ぶのが普通の道**で、こちらは逃げ道。
    direct_dialog: bool,
    candidates: Option<Candidates>,
    // 宣言順にドロップされるため、ストリームをインスタンスより先に止める
    engine: Option<Engine>,
    /// オーディオトラック ([`graph::AUDIO_TRACKS`](crate::audio::graph::AUDIO_TRACKS) 本)。
    /// 空のときは [`ensure_audio_tracks`](App::ensure_audio_tracks) が用意する
    audio_tracks: Vec<AudioTrackUi>,
    /// 詳細ウィンドウを出しているオーディオトラック
    detail_track: Option<usize>,
    /// オーディオトラックの一覧を出しているか。
    ///
    /// **閉じられるようにしてある。** 常設だと画面のどこかを必ず塞ぐので、
    /// 上部のボタンで出し入れする。`Default` は false なので、
    /// 起動時に開くのは [`ensure_audio_tracks`](App::ensure_audio_tracks) で決める。
    show_audio_tracks: bool,
    /// 外したが、まだノードが返ってきていないインスタンス。
    /// 返却時に正しいインスタンスへ deactivate するため、ここで生かしておく。
    /// 添字は**オーディオトラック番号**で、同じトラック内では入れた順に返る。
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
    /// プロジェクトの保存先。Ctrl+S はここへ上書きする。
    /// MIDI のインポートでは設定しない (読み込んだファイルを上書きしないため)
    project_path: Option<PathBuf>,
    /// 最後に読み書きしたフォルダ (ダイアログの初期位置)
    last_directory: Option<PathBuf>,
    /// 画面中央に出す結果通知 (閉じるまで残る)。
    ///
    /// **結果の知らせはここに一本化してある。** 以前は上部に status 行を出して
    /// いたが、同じ内容を通知にも出していたうえ、上部パネルごと畳んだため。
    notice: Option<Notice>,
    /// マスターのスペクトルとラウドネス。
    /// 中身はオーディオスレッドから流れてくるサンプルで毎フレーム更新する
    meters: Meters,
    /// 最後に見た「頭から通した回数」
    /// ([`TransportShared::pass`](crate::audio::transport::TransportShared::pass))。
    /// **増えていたら Integrated を測り直す**
    meter_pass: u64,
    /// 走査したプラグインの一覧 (`config\plugins.ron`)
    library: Library,
    /// 走査の進行。**`Some` の間は毎フレーム1ファイルずつ進む**
    scan: Option<Scan>,
    /// 今開いているファイル (進捗の表示用)
    scanning_now: Option<PathBuf>,
    /// プラグインの窓を出しているか
    show_library: bool,
    /// 一覧のどのタブを見ているか
    library_tab: Tab,
    /// 一覧の絞り込み (名前・ベンダー)
    library_filter: String,
}

impl App {
    /// 起動時に自動ロードする音源を指定して作る (検証用 CLI)
    pub fn with_autoload(autoload: Option<(PathBuf, bool)>) -> Self {
        let mut app = Self {
            autoload,
            ..Default::default()
        };
        // 走査したプラグインの記録は起動時に1回だけ読む
        app.load_library();
        app
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // 起動時の自動ロード (検証用 CLI)
        if let Some((path, open_gui)) = self.autoload.take() {
            // 拡張子で形式を見分ける (CLI なので聞き返す相手がいない)
            let kind = match path.extension().and_then(|ext| ext.to_str()) {
                Some(ext) if ext.eq_ignore_ascii_case("vst3") => project::PluginKind::Vst3,
                _ => project::PluginKind::Clap,
            };
            let found = match kind {
                project::PluginKind::Clap => {
                    discovery::load_clap_file(&path).map(|(_entry, plugins)| plugins)
                }
                project::PluginKind::Vst3 => discovery::load_vst3_file(&path),
            };

            match found {
                Ok(plugins) => {
                    // 検証用 CLI なので、オーディオトラック1 の先頭に載せて
                    // 打ち込みトラック1 を鳴らす形に決め打ちする
                    let addr = audio::NodeAddr { track: 1, at: 0 };
                    self.candidates = Some(Candidates {
                        kind,
                        path,
                        plugins,
                        target_track: addr,
                    });
                    self.instantiate(0, addr);
                    self.set_midi_track(1, Some(0));
                    if open_gui {
                        if let Some(track) = self
                            .audio_tracks
                            .get_mut(1)
                            .and_then(|slot| slot.nodes.first_mut())
                        {
                            let name = track.name.clone();
                            let opened = match &mut track.plugin {
                                TrackPlugin::Clap(clap) => clap.gui.as_mut().map(|gui| {
                                    gui.open(
                                        &mut clap.instance.plugin_handle(),
                                        &name,
                                        clap.sender.clone(),
                                    )
                                }),
                                TrackPlugin::Vst3(vst3) => {
                                    let mut plugin = vst3.plugin.lock();
                                    Some(vst3.gui.open(&mut plugin, &name, vst3.sender.clone()))
                                }
                            };
                            if let Some(Err(e)) = opened {
                                self.error = Some(format!("GUI を開けません: {e}"));
                            }
                        }
                    }
                }
                Err(e) => self.error = Some(format!("自動ロード失敗: {e}")),
            }
        }

        // 差し替えで外した音源をここで停止・解放する
        self.drain_retired();

        // プラグインからのメインスレッド要求 & GUI ウィンドウイベントを処理
        for track in self
            .audio_tracks
            .iter_mut()
            .flat_map(|slot| slot.nodes.iter_mut())
        {
            let clap = match &mut track.plugin {
                TrackPlugin::Clap(clap) => clap,
                TrackPlugin::Vst3(vst3) => {
                    // エディタを閉じているときは音源に触らない
                    // (触るぶんだけオーディオスレッドと取り合いになる)
                    if !vst3.gui.is_open && vst3.receiver.is_empty() {
                        continue;
                    }
                    // ここで待ってはいけない。毎フレーム待ちに行くと、オーディオ
                    // スレッドが手放すたびに横取りする形になり、次のブロックが
                    // 落ちるのが常態化する。取れなければ次のフレームでよい。
                    let Some(mut plugin) = vst3.plugin.try_lock() else {
                        continue;
                    };
                    while let Ok(msg) = vst3.receiver.try_recv() {
                        match msg {
                            MainThreadMessage::PluginWindowClosed => vst3.gui.close(&mut plugin),
                            MainThreadMessage::PluginWindowResized { width, height } => {
                                vst3.gui.on_user_resized(&mut plugin, width, height)
                            }
                            // CLAP 拡張から来るものなので VST3 では届かない
                            MainThreadMessage::RunOnMainThread
                            | MainThreadMessage::GuiRequestResized { .. }
                            | MainThreadMessage::GuiClosed => {}
                        }
                    }
                    // プラグイン発のリサイズ要求はこちらから取りに行く
                    vst3.gui.poll_resize_request(&plugin);
                    // VSTGUI のエディタは Linux でこれを回さないと描画されない
                    // (Windows では何もしない)
                    plugin.service_run_loop();
                    continue;
                }
            };
            while let Ok(msg) = clap.receiver.try_recv() {
                match msg {
                    MainThreadMessage::RunOnMainThread => {
                        clap.instance.call_on_main_thread_callback()
                    }
                    MainThreadMessage::GuiRequestResized { new_size } => {
                        if let Some(gui) = &mut clap.gui {
                            gui.on_plugin_request_resize(new_size);
                        }
                    }
                    MainThreadMessage::GuiClosed | MainThreadMessage::PluginWindowClosed => {
                        if let Some(gui) = &mut clap.gui {
                            gui.close(&mut clap.instance.plugin_handle());
                        }
                    }
                    MainThreadMessage::PluginWindowResized { width, height } => {
                        if let Some(gui) = &mut clap.gui {
                            gui.on_user_resized(&mut clap.instance.plugin_handle(), width, height);
                        }
                    }
                }
            }

            // プラグインが登録したタイマーを駆動する (GUI 描画などに必要)
            let timer = clap
                .instance
                .access_handler(|mt| mt.timer_support.get().map(|ext| (mt.timers.clone(), ext)));
            if let Some((timers, timer_ext)) = timer {
                timers.tick_timers(&timer_ext, &mut clap.instance.plugin_handle());
            }
        }

        // エラーは通知へ寄せる (上部の状態行は畳んだ)
        if let Some(error) = self.error.take() {
            self.notice = Some(Notice::error("エラー", error));
        }

        // 音源の形式とプラグインの選択。**画面中央のポップアップで出す。**
        // 上部に行として出していたときは、押す場所が画面の隅で遠かった
        let chosen_kind = self.load_choice_popup(ctx);
        self.plugin_choice_popup(ctx);

        // マスターの出力を取り込む。**描く前に済ませる**
        // (同じフレームの中で、取り込んだ値をそのまま出すため)。
        // エンジンが無い間は溜めたものを捨てて、止まった絵が残らないようにする。
        let dt = ctx.input(|i| i.stable_dt);
        match self.engine.as_mut() {
            Some(engine) => {
                let rate = engine.config.sample_rate;
                // **頭から通し直していたら Integrated を測り直す** (再生開始・ループ)。
                // 取り込む前に戻すので、前の周の音がリングに数ブロック残っていれば
                // 新しい周に混じる。積算の窓は 400ms なので、この 20ms 程度は響かない。
                let pass = engine.transport_shared.pass.load(Ordering::Relaxed);
                if pass != self.meter_pass {
                    self.meter_pass = pass;
                    self.meters.restart_integrated();
                }
                self.meters.drain(&mut engine.monitor, rate, dt);
            }
            None => self.meters.reset(),
        }

        // **オーディオトラックの窓を呼び戻す口と、マスターのメーターの帯。**
        // 窓は閉じられるので、閉じたあとに開き直す口が要る
        // (音源を載せる唯一の入口なので、行き止まりにできない)。
        // メーターは閉じられない — 常に見えているほうがよいという判断。
        self.ensure_audio_tracks();
        egui::TopBottomPanel::top("audio_track_bar").show(ctx, |ui| {
            ui.horizontal(|ui| {
                let loaded = self
                    .audio_tracks
                    .iter()
                    .filter(|track| !track.nodes.is_empty())
                    .count();
                ui.toggle_value(&mut self.show_audio_tracks, "オーディオトラック")
                    .on_hover_text("音源とエフェクトの管理・ルーティング");
                ui.toggle_value(&mut self.show_library, "プラグイン")
                    .on_hover_text("フォルダを走査してプラグインを一覧する");
                if loaded == 0 {
                    ui.weak("音源が1つも載っていません");
                } else {
                    ui.weak(format!("{loaded} 本に音源が載っています"));
                }

                ui.separator();
                self.master_meters(ui);
            });
        });

        egui::CentralPanel::default().show(ctx, |ui| {
            // 音源の操作はオーディオトラックの窓へ移した (下の `audio_track_windows`)
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
                                        // チェーンの何段目か。この UI を戻すときは
                                        // 段を選べるようにすること
                                        node: 0,
                                        id: param.id,
                                        value: param.value,
                                    });
                                }
                            }
                        });
                }
                */
            }

            if self.audio_tracks.iter().all(|track| track.nodes.is_empty()) {
                ui.label(
                    "音源が未ロードです (「オーディオトラック」の窓から載せてください)。\
                     ロードしなくてもシーケンスの編集はできます。",
                );
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

            // 打ち込みトラック欄に出す情報。**どのオーディオトラックが
            // その打ち込みを見ているか**を集める。誰も見ていなければ鳴らないので、
            // 欄に赤枠を出す材料になる
            self.editor.track_plugins = (0..self.editor.editor.track_count())
                .map(|midi_track| {
                    let users: Vec<String> = self
                        .audio_tracks
                        .iter()
                        .enumerate()
                        .filter(|(_, slot)| slot.midi_track == Some(midi_track))
                        .map(|(index, slot)| format!("{index}: {}", slot.label()))
                        .collect();
                    (!users.is_empty()).then(|| users.join(" / "))
                })
                .collect();

            let commands =
                editor_ui::editor_panel(ui, &mut self.editor, self.pos_quarters, playing);

            // ファイルダイアログは self 全体を触るので、ループを抜けてから実行する
            let mut file_action = None;
            let mut load_plugin_track = None;

            for command in commands {
                match command {
                    EditorCommand::ImportMidi => file_action = Some(FileAction::ImportMidi),
                    EditorCommand::ExportMidi => file_action = Some(FileAction::ExportMidi),
                    EditorCommand::OpenProject => file_action = Some(FileAction::OpenProject),
                    EditorCommand::SaveProject => file_action = Some(FileAction::SaveProject),
                    EditorCommand::SaveProjectAs => file_action = Some(FileAction::SaveProjectAs),
                    EditorCommand::ExportWav => file_action = Some(FileAction::ExportWav),
                    EditorCommand::ExportOpus => file_action = Some(FileAction::ExportOpus),
                    EditorCommand::ExportCcs => file_action = Some(FileAction::ExportCcs),
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
                                let end_sample = (self.editor.editor.length_quarters_bar_aligned()
                                    as f64
                                    * spq) as u64;
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
                            | EditorCommand::OpenProject
                            | EditorCommand::SaveProject
                            | EditorCommand::SaveProjectAs
                            | EditorCommand::ExportWav
                            | EditorCommand::ExportOpus
                            | EditorCommand::ExportCcs => continue,
                        };
                        let _ = engine.producer.push(msg);
                    }
                }
            }

            match file_action {
                Some(FileAction::ImportMidi) => self.import_midi(),
                Some(FileAction::ExportMidi) => self.export_midi(),
                Some(FileAction::OpenProject) => self.open_project(),
                Some(FileAction::SaveProject) => self.save_project(false),
                Some(FileAction::SaveProjectAs) => self.save_project(true),
                Some(FileAction::ExportWav) => self.export_wav(),
                Some(FileAction::ExportOpus) => self.export_opus(),
                Some(FileAction::ExportCcs) => self.export_ccs(),
                None => {}
            }

            // オーディオトラックの窓 (一覧は常設、詳細は選んだ1本だけ)
            if let Some(addr) = self.audio_track_windows(ui.ctx()) {
                load_plugin_track = Some(addr);
            }

            // 詳細ウィンドウの「+」。**走査した一覧から選んでもらう。**
            // ファイルから直接読む道も残してあるが、そちらは一覧の中のボタンから
            if let Some(track) = load_plugin_track {
                self.pending_load = Some(track);
                self.candidates = None;
                self.show_library = true;
                // 先頭の段は音源、それ以降はエフェクトを見たいことが多い
                self.library_tab = if track.at == 0 {
                    Tab::Instrument
                } else {
                    Tab::Effect
                };
            }

            // 形式が決まったらダイアログを開く。
            // 新しく読み込めたときだけ装填する (キャンセルでは何もしない)
            if let Some(chosen) = chosen_kind {
                self.direct_dialog = false;
                // **やめたときは段の指定を残す。** 一覧へ戻るだけにする
                if chosen != LoadChoice::Cancel {
                    let Some(track) = self.pending_load.take() else {
                        return;
                    };
                    let opened = match chosen {
                        LoadChoice::Clap => self.open_clap_dialog(track),
                        LoadChoice::Vst3Bundle => self.open_vst3_dialog(track, true),
                        LoadChoice::Vst3File => self.open_vst3_dialog(track, false),
                        LoadChoice::Cancel => false,
                    };
                    if opened {
                        // 候補が1つだけならそのまま載せる (選択の手間を省く)
                        let single = self
                            .candidates
                            .as_ref()
                            .is_some_and(|candidates| candidates.plugins.len() == 1);
                        if single {
                            self.instantiate(0, track);
                            self.candidates = None;
                            // 一覧から選んだときと同じく、載せたら閉じる
                            self.show_library = false;
                        }
                    }
                }
            }
        });

        // プラグインの一覧。**走査は1フレームに1ファイルずつ進める**
        // (別スレッドにできないので、描く前に1つ進めて進捗へ反映する)
        self.step_scan();
        self.library_window(ctx);

        // 結果通知は最前面に出したいので、パネルを描いたあとに重ねる
        notice_window(ctx, &mut self.notice);

        // 鍵盤の離鍵検出などのために定期的に再描画する。
        //
        // **間隔は実際のフレーム時間より長くすること。** eframe は「期限を過ぎた
        // 再描画要求」を見つけると `ControlFlow::Poll` に落とす
        // (eframe `native/run.rs` の `check_redraw_requests`)。vsync 60Hz の
        // フレームは約 16.7ms なので、16ms を要求すると**毎回すでに期限切れ**になり、
        // ループが一度も待機状態に入らなくなる。
        //
        // そうなると winit がメッセージ配送を捌ききれない。winit は
        // `RedrawRequested` を配送するたびに配送ループを打ち切る作りのため
        // (`interrupt_msg_dispatch`)、待機に入らない限りキューが常に捌き残る。
        // 割を食うのは**同じスレッドに貼り付いたプラグインのエディタ**で、
        // 単発のクリックは通るのにホバーやドラッグが効かなくなる
        // (Native Instruments の音源で発覚。経緯は docs/archive/vst3_host_plan.md のフェーズ7)。
        //
        // エディタを開いていないときは誰も割を食わないので、そのまま滑らかに保つ。
        let interval = if self.any_editor_open() { 33 } else { 16 };
        ctx.request_repaint_after(Duration::from_millis(interval));
    }
}
