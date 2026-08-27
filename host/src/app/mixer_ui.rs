//! オーディオトラックの窓。
//!
//! **一覧は常設**でルーティング・音量・パン・ミュート/ソロだけを扱い、
//! **プラグインの管理は詳細ウィンドウ**が受け持つ。音源を選ばせるポップアップも
//! 一連の流れなのでここに置く。

use super::notice::Notice;
use super::plugins::{file_label, LoadChoice};
use super::routing::{MAX_GAIN_DB, MIN_GAIN_DB};
use super::track::TrackPlugin;
use super::App;
use crate::audio::graph;
use crate::theme::palette;
use crate::{audio, theme};
use eframe::egui;

/// オーディオトラック一覧で、名前と送り先に取る幅。
///
/// **固定にしてある。** 中身の文字数で幅が変わると、行ごとに音量や M/S の
/// 位置がずれて押しにくくなる。入りきらない名前は端を詰めて、
/// ホバーで全体を出す。
const TRACK_NAME_W: f32 = 118.0;
const TRACK_SENDS_W: f32 = 74.0;

/// 詳細ウィンドウで、段の名前とチャンネル表記に取る幅 (理由は上と同じ)
const NODE_NAME_W: f32 = 150.0;
const NODE_CHANNELS_W: f32 = 56.0;

/// MIDI の割り当てを選ぶ一覧の高さ。
///
/// **打ち込みトラックは何本でも作れる**ので、全部並べると窓が画面の外まで
/// 伸びる。ここで切ってスクロールさせる (8本ぶんくらいが見える高さ)。
const MIDI_PICK_H: f32 = 160.0;

/// M / S / B のような1文字の切り替えの文字。
///
/// **入っているときだけ色を付ける。** 打ち込みトラック欄 (`editor_ui::gutter`)
/// と同じ流儀で、切り替えの状態を枠だけでなく色でも出す。
/// 消えているときは目立たせない ([`palette::FG_DIM`])。
fn toggle_text(label: &str, on: bool, color: egui::Color32) -> egui::RichText {
    egui::RichText::new(label)
        .size(10.0)
        .color(if on { color } else { palette::FG_DIM })
}

impl App {
    /// 音源の形式を選ばせるポップアップ。
    ///
    /// ダイアログの出し方が形式で違うので先に決めてもらう
    /// (.clap はファイル、.vst3 はバンドルディレクトリと単体ファイルの2通り)。
    ///
    /// **画面中央に出す。** 上部に行として出していたときは、押す場所が
    /// 画面の隅にあって遠かった。
    pub(super) fn load_choice_popup(&mut self, ctx: &egui::Context) -> Option<LoadChoice> {
        // **普通は一覧から選ぶ。** ここへ来るのは「ファイルから直接…」を
        // 押したときだけ
        if !self.direct_dialog {
            return None;
        }
        let track = self.pending_load?;
        let mut chosen = None;

        egui::Window::new("音源を読み込む")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
            .show(ctx, |ui| {
                ui.set_max_width(360.0);
                ui.label(format!(
                    "オーディオトラック {} の {} 段目に読み込みます。",
                    track.track,
                    track.at + 1
                ));
                ui.add_space(6.0);
                ui.weak("形式によってダイアログの出し方が違うので、先に選んでください");
                ui.add_space(8.0);

                if ui
                    .add_sized([340.0, 26.0], egui::Button::new("CLAP (.clap ファイル)"))
                    .clicked()
                {
                    chosen = Some(LoadChoice::Clap);
                }
                if ui
                    .add_sized([340.0, 26.0], egui::Button::new("VST3 (.vst3 フォルダ)"))
                    .on_hover_text("現行の標準。フォルダ選択が開きます")
                    .clicked()
                {
                    chosen = Some(LoadChoice::Vst3Bundle);
                }
                if ui
                    .add_sized(
                        [340.0, 26.0],
                        egui::Button::new("VST3 (.vst3 単体ファイル)"),
                    )
                    .on_hover_text(
                        "フォルダになっていない古い形式。\
                         バンドルの中の DLL を直接指すのにも使えます",
                    )
                    .clicked()
                {
                    chosen = Some(LoadChoice::Vst3File);
                }

                ui.add_space(6.0);
                ui.separator();
                if ui.button("やめる").clicked() {
                    chosen = Some(LoadChoice::Cancel);
                }
                ui.weak("(Esc でも閉じます)");
            });

        if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            chosen = Some(LoadChoice::Cancel);
        }
        chosen
    }

    /// 1つのファイルに複数入っていたときの選択ポップアップ。
    /// 1つだけのファイルは選ばせずにそのまま載るので出さない。
    pub(super) fn plugin_choice_popup(&mut self, ctx: &egui::Context) {
        let Some(candidates) = self
            .candidates
            .as_ref()
            .filter(|candidates| candidates.plugins.len() > 1)
        else {
            return;
        };
        let target = candidates.target_track;
        let file = file_label(&candidates.path);
        let plugins: Vec<(String, String)> = candidates
            .plugins
            .iter()
            .map(|plugin| (plugin.name.clone(), plugin.id.clone()))
            .collect();

        let mut chosen = None;
        let mut cancel = false;

        egui::Window::new("プラグインを選ぶ")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
            .show(ctx, |ui| {
                ui.set_max_width(420.0);
                ui.label(format!(
                    "オーディオトラック {} の {} 段目 — {file}",
                    target.track,
                    target.at + 1
                ));
                ui.weak("このファイルには複数のプラグインが入っています");
                ui.add_space(8.0);

                for (index, (name, id)) in plugins.iter().enumerate() {
                    if ui
                        .add_sized([400.0, 26.0], egui::Button::new(format!("▶ {name}")))
                        .on_hover_text(id)
                        .clicked()
                    {
                        chosen = Some(index);
                    }
                }

                ui.add_space(6.0);
                ui.separator();
                if ui.button("やめる").clicked() {
                    cancel = true;
                }
                ui.weak("(Esc でも閉じます)");
            });

        if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            cancel = true;
        }
        if let Some(index) = chosen {
            self.instantiate(index, target);
            self.candidates = None;
            // 一覧から選んだときと同じく、載せたら閉じる
            self.show_library = false;
        } else if cancel {
            self.candidates = None;
        }
    }

    /// オーディオトラックの窓を出す。
    ///
    /// 一覧は常設で**ルーティング・音量・パン・ミュート/ソロ**だけを扱い、
    /// **プラグインの管理は詳細ウィンドウ**が受け持つ。
    /// 戻り値は「音源を載せたい段」(押されたときだけ)。
    pub(super) fn audio_track_windows(&mut self, ctx: &egui::Context) -> Option<audio::NodeAddr> {
        self.ensure_audio_tracks();
        let mixer = self.mixer();
        let midi_tracks = self.editor.editor.track_count();

        let mut open_detail = None;
        let mut toggle_send = None;
        let mut mixer_changed = false;

        // **初期位置は右上。** 既定 (左上) だとエディタのツールバーに重なる。
        // `default_pos` なので、動かしたあとはその場所を覚える
        const LIST_W: f32 = 430.0;
        let right_top = {
            let screen = ctx.screen_rect();
            egui::pos2(screen.right() - LIST_W - 12.0, screen.top() + 12.0)
        };

        let mut list_open = self.show_audio_tracks;
        egui::Window::new("オーディオトラック")
            .default_width(LIST_W)
            .default_pos(right_top)
            .open(&mut list_open)
            .show(ctx, |ui| {
                ui.weak("0 はマスター。ここの出力がそのまま最終出力になる");
                ui.add_space(4.0);

                egui::ScrollArea::vertical().show(ui, |ui| {
                    for index in 0..graph::AUDIO_TRACKS {
                        // **マスターへ辿り着けないトラックは鳴らない。**
                        // 「繋がっていなければ鳴らない」を仕様にしたぶん、
                        // 画面で分かるようにする必要がある
                        let silent = !mixer.routing.reaches_master(index);
                        let frame = if silent {
                            egui::Frame::NONE
                                .stroke(egui::Stroke::new(1.0_f32, theme::palette::RED))
                                .inner_margin(2.0)
                        } else {
                            egui::Frame::NONE.inner_margin(2.0)
                        };

                        frame.show(ui, |ui| {
                            ui.horizontal(|ui| {
                                let track = &mut self.audio_tracks[index];
                                ui.monospace(format!("{index:>2}"));

                                // **幅を固定する。** 名前の長さで後ろのつまみが
                                // 動くと、行ごとに押す位置が変わって使いづらい
                                let name = track.label().to_string();
                                if ui
                                    .add_sized(
                                        [TRACK_NAME_W, 20.0],
                                        egui::Button::new(egui::RichText::new(&name).size(12.0))
                                            .truncate(),
                                    )
                                    .on_hover_text(format!("{name} (クリックで詳細)"))
                                    .clicked()
                                {
                                    open_detail = Some(index);
                                }

                                // 送り先。マスターは送り側にならないので中身は出さない。
                                //
                                // **どちらの枝も同じ大きさを確保する。** `allocate_ui`
                                // は中身に合わせて縮むので、ボタンの幅 (「→ 0」と
                                // 「→ 0,3」で違う) に引きずられて後ろの列がずれる。
                                // `set_min_width` で下限を揃える
                                let sends_size = egui::vec2(TRACK_SENDS_W, 20.0);
                                let layout = egui::Layout::left_to_right(egui::Align::Center);
                                ui.allocate_ui_with_layout(sends_size, layout, |ui| {
                                    ui.set_min_width(TRACK_SENDS_W);
                                    if index == graph::MASTER {
                                        return;
                                    }
                                    let label = if track.sends.is_empty() {
                                        "→ なし".to_string()
                                    } else {
                                        let list: Vec<String> =
                                            track.sends.iter().map(|to| to.to_string()).collect();
                                        format!("→ {}", list.join(","))
                                    };
                                    let sends = &track.sends;
                                    ui.menu_button(label, |ui| {
                                        for target in 0..graph::AUDIO_TRACKS {
                                            if target == index {
                                                continue;
                                            }
                                            let mut on = sends.contains(&target);
                                            if ui.checkbox(&mut on, format!("{target}")).changed() {
                                                toggle_send = Some((index, target));
                                            }
                                        }
                                    })
                                    .response
                                    .on_hover_text("送り先 (複数選ぶと足し合わさる)");
                                });

                                // 音量 (dB)。下限まで下げると無音
                                if ui
                                    .add(
                                        egui::DragValue::new(&mut track.gain_db)
                                            .speed(0.1)
                                            .range(MIN_GAIN_DB..=MAX_GAIN_DB)
                                            .fixed_decimals(1)
                                            .suffix(" dB")
                                            .custom_formatter(|value, _| {
                                                if value as f32 <= MIN_GAIN_DB {
                                                    "-∞".to_string()
                                                } else {
                                                    format!("{value:+.1}")
                                                }
                                            }),
                                    )
                                    .on_hover_text("音量 (0 dB が等倍。下限まで下げると無音)")
                                    .changed()
                                {
                                    mixer_changed = true;
                                }
                                if ui
                                    .add(
                                        egui::DragValue::new(&mut track.pan)
                                            .speed(0.01)
                                            .range(-1.0..=1.0)
                                            .prefix("P"),
                                    )
                                    .on_hover_text("パン (-1 左 〜 1 右)")
                                    .changed()
                                {
                                    mixer_changed = true;
                                }

                                // **色は打ち込みトラック欄と揃える。**
                                // 同じ意味の切り替えが窓ごとに違う見た目だと、
                                // どちらが入っているのか読み取れない
                                // 文字は先に作る (切り替えへ渡す借用と重ならないように)
                                let mute_text = toggle_text("M", track.muted, palette::RED);
                                if ui
                                    .toggle_value(&mut track.muted, mute_text)
                                    .on_hover_text("ミュート (このトラックを鳴らさない)")
                                    .changed()
                                {
                                    mixer_changed = true;
                                }
                                let solo_text = toggle_text("S", track.soloed, palette::YELLOW);
                                if ui
                                    .toggle_value(&mut track.soloed, solo_text)
                                    .on_hover_text("ソロ。マスターへ至る経路は残る")
                                    .changed()
                                {
                                    mixer_changed = true;
                                }

                                if silent {
                                    ui.colored_label(theme::palette::RED, "鳴りません")
                                        .on_hover_text("マスターへ辿り着けません");
                                }
                            });
                        });
                    }
                });
            });

        self.show_audio_tracks = list_open;

        if let Some((from, to)) = toggle_send {
            if let Err(problems) = self.toggle_send(from, to) {
                self.notice = Some(Notice::error("その繋ぎ方はできません", problems));
            }
        }
        if mixer_changed {
            self.push_routing();
        }
        if let Some(index) = open_detail {
            self.detail_track = Some(index);
        }

        self.audio_track_detail(ctx, midi_tracks)
    }

    /// 選んでいる1本の詳細 (チェーンの管理)
    fn audio_track_detail(
        &mut self,
        ctx: &egui::Context,
        midi_tracks: usize,
    ) -> Option<audio::NodeAddr> {
        let index = self.detail_track?;
        if index >= self.audio_tracks.len() {
            self.detail_track = None;
            return None;
        }

        let mut add_node = None;
        let mut remove = None;
        let mut moved = None;
        let mut bypass = None;
        let mut midi_toggle = None;
        let mut gui_error = None;
        let mut open = true;

        // プラグインの窓の所有者 (窓の中では self を借りられないので先に取る)
        let owner = self.main_window;

        // 打ち込みトラックの名前 (同上)
        let midi_names: Vec<String> = self
            .editor
            .editor
            .tracks
            .iter()
            .take(midi_tracks)
            .map(|info| info.name.clone())
            .collect();

        // 一覧の左隣に出す (一覧は右上なので、重ならない位置)
        const DETAIL_W: f32 = 420.0;
        let beside_list = {
            let screen = ctx.screen_rect();
            egui::pos2(
                (screen.right() - 430.0 - DETAIL_W - 24.0).max(screen.left() + 12.0),
                screen.top() + 12.0,
            )
        };

        egui::Window::new(format!("オーディオトラック {index}"))
            .default_width(DETAIL_W)
            .default_pos(beside_list)
            .open(&mut open)
            .show(ctx, |ui| {
                // ---- MIDI の割り当て ----
                // マスターは音を受けるだけなので、打ち込みを割り当てても意味がない
                if index != graph::MASTER {
                    let current = self.audio_tracks[index].midi;
                    ui.horizontal(|ui| {
                        ui.label("MIDI:");
                        if current.is_empty() {
                            ui.weak("未割り当て (音源を鳴らすには割り当てが要ります)");
                        } else {
                            let names: Vec<&str> = current
                                .iter()
                                .map(|track| {
                                    midi_names.get(track).map(String::as_str).unwrap_or("?")
                                })
                                .collect();
                            ui.label(names.join(" / "));
                        }
                    });

                    // **複数選べる。** ドラムをキックとハイハットで別の打ち込みに
                    // 書き、音源1つで受ける、といった使い方のため。
                    //
                    // **番号ではなく名前で選ぶ。** 番号だけだと、どれがどのパートか
                    // 覚えていないと選べない。トラックが増えると縦に伸びるので、
                    // 高さを決めてスクロールさせる (窓が画面外まで伸びないように)。
                    egui::ScrollArea::vertical()
                        .id_salt(("midi_pick", index))
                        .max_height(MIDI_PICK_H)
                        .auto_shrink([false, true])
                        .show(ui, |ui| {
                            for track in 0..midi_tracks {
                                let selected = current.contains(track);
                                // いっぱいのときは、外すほうだけ押せる
                                let can_click = selected || !current.is_full();
                                let name = midi_names.get(track).map(String::as_str).unwrap_or("?");
                                // 番号も残す。名前を空にしても選べなくならないように
                                let label = format!("{}  {name}", track + 1);
                                if ui
                                    .add_enabled(
                                        can_click,
                                        egui::SelectableLabel::new(selected, label),
                                    )
                                    .clicked()
                                {
                                    midi_toggle = Some(track);
                                }
                            }
                        });
                    if current.is_full() {
                        ui.weak(format!(
                            "受けられるのは {} 本までです",
                            graph::MAX_MIDI_SOURCES
                        ));
                    }
                    ui.separator();
                }

                // ---- チェーン ----
                let count = self.audio_tracks[index].nodes.len();
                if count == 0 {
                    ui.weak("何も刺さっていません");
                }
                for at in 0..count {
                    let node = &mut self.audio_tracks[index].nodes[at];
                    ui.horizontal(|ui| {
                        ui.monospace(format!("{}.", at + 1));
                        // 幅を固定して、名前の長さでボタンの位置が動かないようにする
                        let name = node.name.clone();
                        ui.add_sized(
                            [NODE_NAME_W, 20.0],
                            egui::Label::new(&name).truncate().halign(egui::Align::LEFT),
                        )
                        .on_hover_text(&name);
                        // **どこでステレオが潰れるかを見えるように**
                        ui.add_sized(
                            [NODE_CHANNELS_W, 20.0],
                            egui::Label::new(
                                egui::RichText::new(format!(
                                    "{}→{}ch",
                                    node.channels.0, node.channels.1
                                ))
                                .weak(),
                            ),
                        );

                        let mut bypassed = node.bypassed;
                        // **赤にはしない。** 赤は「鳴らない」に取ってあり、
                        // バイパスは音が通る (素通しになるだけ)。
                        // 打ち込み欄の W / V と同じ「この切り替えが入っている」の水色
                        let bypass_text = toggle_text("B", bypassed, palette::CYAN);
                        if ui
                            .toggle_value(&mut bypassed, bypass_text)
                            .on_hover_text(
                                "バイパス (この段の音を捨てて素通し)。\
                                 処理自体は続けるので、戻したときに続きから鳴ります",
                            )
                            .changed()
                        {
                            bypass = Some((at, bypassed));
                        }
                        if ui.add_enabled(at > 0, egui::Button::new("▲")).clicked() {
                            moved = Some((at, at - 1));
                        }
                        if ui
                            .add_enabled(at + 1 < count, egui::Button::new("▼"))
                            .clicked()
                        {
                            moved = Some((at, at + 1));
                        }
                        if ui.button("✕").on_hover_text("この段を外す").clicked() {
                            remove = Some(at);
                        }

                        // プラグイン独自 GUI
                        let name = node.name.clone();
                        match &mut node.plugin {
                            TrackPlugin::Clap(clap) => {
                                if let Some(gui) = &mut clap.gui {
                                    if gui.supports_gui() {
                                        if !gui.is_open {
                                            if ui.button("エディタ").clicked() {
                                                if let Err(e) = gui.open(
                                                    &mut clap.instance.plugin_handle(),
                                                    &name,
                                                    clap.sender.clone(),
                                                    owner,
                                                ) {
                                                    gui_error =
                                                        Some(format!("GUI を開けません: {e}"));
                                                }
                                            }
                                        } else if ui.button("閉じる").clicked() {
                                            gui.close(&mut clap.instance.plugin_handle());
                                        }
                                    }
                                }
                            }
                            TrackPlugin::Vst3(vst3) => {
                                ui.weak("VST3");
                                if vst3.gui.supports_gui() {
                                    if !vst3.gui.is_open {
                                        if ui.button("エディタ").clicked() {
                                            let mut plugin = vst3.plugin.lock();
                                            if let Err(e) = vst3.gui.open(
                                                &mut plugin,
                                                &name,
                                                vst3.sender.clone(),
                                                owner,
                                            ) {
                                                gui_error = Some(format!("GUI を開けません: {e}"));
                                            }
                                        }
                                    } else if ui.button("閉じる").clicked() {
                                        let mut plugin = vst3.plugin.lock();
                                        vst3.gui.close(&mut plugin);
                                    }
                                }
                            }
                        }
                    });
                }

                ui.add_space(4.0);
                if ui
                    .add_enabled(
                        count < graph::MAX_NODES,
                        egui::Button::new("＋ 音源 / エフェクトを足す"),
                    )
                    .clicked()
                {
                    add_node = Some(audio::NodeAddr {
                        track: index,
                        at: count,
                    });
                }
                ui.weak("上から順に通ります。入力を持たない段 (音源) は、そこまでの音を捨てます");
            });

        if !open {
            self.detail_track = None;
        }
        if let Some(midi_track) = midi_toggle {
            // ボタン側で止めてあるので、ここへは来ない想定。
            // 黙って効かないより、理由が出るほうがよい
            if !self.toggle_midi_source(index, midi_track) {
                gui_error = Some(format!(
                    "打ち込みを受けられるのは1トラックにつき {} 本までです",
                    graph::MAX_MIDI_SOURCES
                ));
            }
        }
        if let Some((at, bypassed)) = bypass {
            self.set_bypassed(audio::NodeAddr { track: index, at }, bypassed);
        }
        if let Some((from, to)) = moved {
            self.move_node(index, from, to);
        }
        if let Some(at) = remove {
            self.remove_node(audio::NodeAddr { track: index, at });
        }
        if gui_error.is_some() {
            self.error = gui_error;
        }
        add_node
    }
}
