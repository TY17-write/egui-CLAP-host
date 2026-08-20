//! シーケンスエディタの egui UI。
//! 段 (レーン) 方式: デフォルト16段の横レーンがあり、ノートは置かれた段に属する。
//! 各ノートは "(半音,オクターブ)" ラベルを表示し、縦ドラッグで段の移動、
//! 右端ドラッグで音価を変更する。ピッチは選択して数値で編集する。
//!
//! 外へ出しているのは [`EditorState`] / [`EditorCommand`] / [`editor_panel`] の
//! 3つだけ。**それ以外は `pub(super)` でこのディレクトリの中に閉じている。**

mod color;
mod geometry;
mod grid;
mod gutter;
mod help;
mod history;
mod metrics;
mod shortcuts;
mod state;
mod toolbar;

pub use state::{EditorState, NoteDefaults};

use eframe::egui;
use metrics::{GUTTER_W, RULER_H};

/// エディタからメインスレッドへの指示
pub enum EditorCommand {
    /// シーケンスをオーディオスレッドに再送する
    Commit,
    Play,
    Stop,
    Seek {
        quarters: f64,
    },
    SetLoop(bool),
    /// MIDI ファイルを選んで読み込む
    ImportMidi,
    /// 保存先を選んで MIDI ファイルに書き出す
    ExportMidi,
    /// プロジェクトを開く (.ron)
    OpenProject,
    /// プロジェクトを保存する (保存先が未設定なら選ばせる)
    SaveProject,
    /// 保存先を選んでプロジェクトを保存する
    SaveProjectAs,
    /// シーケンス全体を音声ファイル (WAV) に書き出す
    ExportWav,
    /// シーケンス全体を Ogg/Opus に書き出す
    ExportOpus,
    /// 1トラック目を CeVIO のプロジェクトファイル (CCS) に書き出す
    ExportCcs,
}

/// エディタパネルを描画する。pos_quarters は現在の再生位置 (四分音符単位)。
pub fn editor_panel(
    ui: &mut egui::Ui,
    state: &mut EditorState,
    pos_quarters: f64,
    playing: bool,
) -> Vec<EditorCommand> {
    let mut commands = Vec::new();

    // 終端まで再生して自分で止まったときも、再生を始めた位置へ戻す
    if state.was_playing && !playing {
        if let Some(quarters) = state.play_return.take() {
            commands.push(EditorCommand::Seek { quarters });
            // 再生中は追従スクロールで画面が流れているので、位置と一緒に画面も戻す
            state.scroll_to_quarters = Some(quarters);
        }
    }
    state.was_playing = playing;

    toolbar::toolbar(ui, state, playing, pos_quarters, &mut commands);
    ui.separator();

    // 左下のヘルプボタン用に1行分を残してグリッドを描く
    const HELP_ROW_H: f32 = 26.0;
    let grid_height = (ui.available_height() - HELP_ROW_H).max(80.0);

    ui.horizontal_top(|ui| {
        // 左のトラック欄。グリッドとは別のスクロール領域にして、
        // 横スクロールしても隠れないようにする (縦だけグリッドに追従させる)。
        ui.vertical(|ui| {
            // 見出しはスクロールの外に出す。中に入れるとトラックが増えたときに
            // 追加・削除ボタンまでスクロールできなくなる。
            // 高さをルーラーと同じにして、下の一覧がグリッドの段と揃うようにする。
            gutter::track_gutter_header(ui, state);

            egui::ScrollArea::vertical()
                .id_salt("track_gutter")
                .auto_shrink([false, false])
                .max_height((grid_height - RULER_H).max(state.row_h))
                .max_width(GUTTER_W)
                // 縦位置はグリッドから毎フレーム上書きされるので、
                // ここでドラッグしても一瞬ずれて戻るだけになる
                .drag_to_scroll(false)
                .scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::AlwaysHidden)
                .vertical_scroll_offset(state.grid_scroll_y)
                .show(ui, |ui| {
                    gutter::track_gutter(ui, state);
                });
        });

        let mut grid_area = egui::ScrollArea::both()
            .id_salt("grid")
            .auto_shrink([false, false])
            // 左ドラッグは範囲選択に使うので、スクロールには割り当てない。
            // スクロールは中ドラッグ・ホイール・スクロールバーで行う。
            .drag_to_scroll(false)
            .max_height(grid_height);
        // 横ズームの補正だけは、この場で位置を指定する。**ppq が変わるフレームと
        // 同じフレームで横位置も決まる**ので、両者がずれて画面が揺れることがない。
        // 指定するのはズームした次のフレームだけで、普段のスクロールには触らない。
        if let Some(x) = state.pending_scroll_x.take() {
            grid_area = grid_area.horizontal_scroll_offset(x);
        }
        let grid_scroll = grid_area.show(ui, |ui| {
            grid::grid(ui, state, pos_quarters, playing, &mut commands);
        });
        // 次フレームで左欄を同じ位置に合わせる
        state.grid_scroll_y = grid_scroll.state.offset.y;
    });

    ui.horizontal(|ui| {
        if ui
            .button("? 操作ガイド")
            .on_hover_text("マウス・キーボード操作の一覧")
            .clicked()
        {
            state.show_help = !state.show_help;
        }

        ui.separator();

        // 保存 (.ron) と MIDI の入出力をまとめる。
        // 並べるとボタン列が長くなりすぎるため。
        ui.menu_button("ファイル", |ui| {
            if ui
                .button("開く…")
                .on_hover_text("プロジェクト (.ron) を読み込む (今の内容は置き換わります)")
                .clicked()
            {
                commands.push(EditorCommand::OpenProject);
                ui.close_menu();
            }
            if ui
                .button("保存 (Ctrl+S)")
                .on_hover_text("プロジェクトを保存する (保存先が未設定なら選びます)")
                .clicked()
            {
                commands.push(EditorCommand::SaveProject);
                ui.close_menu();
            }
            if ui
                .button("名前を付けて保存…")
                .on_hover_text("保存先を選んでプロジェクトを保存する")
                .clicked()
            {
                commands.push(EditorCommand::SaveProjectAs);
                ui.close_menu();
            }

            ui.separator();

            // **書き出しは「出力」へ移した。** 読み込みだけがここに残る
            if ui
                .button("MIDI インポート")
                .on_hover_text("MIDI ファイルを読み込む (今のノートは置き換わります)")
                .clicked()
            {
                commands.push(EditorCommand::ImportMidi);
                ui.close_menu();
            }
        });

        // 書き出し形式は増える見込みなのでメニューにしておく
        ui.menu_button("出力", |ui| {
            if ui
                .button("WAV ファイルとして出力")
                .on_hover_text("シーケンス全体を音声ファイルに書き出す (16bit PCM)")
                .clicked()
            {
                commands.push(EditorCommand::ExportWav);
                ui.close_menu();
            }
            // Opus はビットレートを選んでから。48kHz へ切り替えて書き出すので、
            // WAV より時間がかかることも添える。
            ui.menu_button("Opus ファイルとして出力", |ui| {
                ui.weak("48kHz で書き出します (音源を一時的に切り替えます)");
                for kbps in crate::opus::BITRATES_KBPS {
                    let label = if kbps == crate::opus::DEFAULT_BITRATE_KBPS {
                        format!("{kbps} kbps (既定)")
                    } else {
                        format!("{kbps} kbps")
                    };
                    if ui
                        .selectable_label(state.opus_bitrate_kbps == kbps, label)
                        .clicked()
                    {
                        state.opus_bitrate_kbps = kbps;
                        commands.push(EditorCommand::ExportOpus);
                        ui.close_menu();
                    }
                }
            });

            // **ここから下は音ではなく譜面の書き出し。** 上の2つ (音声) とは
            // 用途が違うので区切る
            ui.separator();

            if ui
                .button("MIDI ファイルとして出力")
                .on_hover_text(
                    "MIDI ファイルに書き出す (スウィングが乗ります。編集の保存には使わないこと)",
                )
                .clicked()
            {
                commands.push(EditorCommand::ExportMidi);
                ui.close_menu();
            }
            if ui
                .button("CCS ファイルとして出力")
                .on_hover_text("CeVIO のプロジェクトファイルに書き出す (1トラック目のみ)")
                .clicked()
            {
                commands.push(EditorCommand::ExportCcs);
                ui.close_menu();
            }
        });

        if let Some(path) = &state.project_path {
            ui.weak(format!("保存先: {path}"));
        }
    });
    help::help_window(ui.ctx(), &mut state.show_help);
    gutter::lane_config_window(ui.ctx(), state);

    shortcuts::shortcuts(ui, state, pos_quarters, playing, &mut commands);

    // ドラッグ中でなければ変更をコミットする
    if state.dirty && state.drag.is_none() {
        commands.insert(0, EditorCommand::Commit);
        state.dirty = false;
    }

    // 次フレームの「変更前の状態」を控える
    state.history.end_frame(&state.editor);

    commands
}
