//! 上部ツールバー (再生・テンポ・拍子・音階・スナップ・ズーム・アンドゥ) と、
//! 揺らぎ (スウィング / 拍の偏り) の行。

use super::history::EditGroup;
use super::metrics::{MAX_OCTAVE, MAX_ROW_H, MIN_OCTAVE, MIN_ROW_H, PPQ, ROW_H};
use super::shortcuts::{start_playback, stop_playback};
use super::state::EditorState;
use super::EditorCommand;
use crate::sequencer::ScaleMode;
use crate::swing;
use crate::waltz;
use eframe::egui;

/// 揺らぎ (スウィング / 拍の偏り) のツールバー。**独立した行にする。**
///
/// 1行目に足していたが、**スライダー1本ぶんの幅が足りず、右端で切れて操作
/// できなくなった** (1366px の窓で「拍の偏り」のラベルだけが出てスライダーが
/// 画面外)。項目が増えるたびに同じことが起きるので、性質の似た2つを別の行に
/// 分けて幅の取り合いから外す。
///
/// ラベルとスライダーは折り返しレイアウトの**直接の子**にしておくこと。
/// `add_enabled_ui` や `scope` で包むと入れ子の Ui になり、折り返しに参加せず
/// 右端からはみ出して見えなくなる。
fn groove_toolbar(ui: &mut egui::Ui, state: &mut EditorState) {
    ui.horizontal_wrapped(|ui| {
        // 既定の 100px はツールバーには広すぎる。抜ける前に元へ戻す
        let default_slider_width = ui.spacing().slider_width;
        ui.spacing_mut().slider_width = 84.0;

        // スウィングの深さ (200BPM 時の裏拍の比)。
        // 掛けるトラックはトラック欄の「W」で選ぶ。N/4 拍子でしか効かない。
        let swing_usable = swing::applies_to(state.editor.beat_type);
        let swing_hint = if swing_usable {
            "跳ねの深さ (1.00 = 直線 / 2.00 = 三連)。掛けるトラックは左の欄の「W」で選びます"
        } else {
            "スウィングは N/4 拍子のときだけ使えます"
        };
        // 折り返しレイアウトなので、そのままだと文字の途中で改行される
        // (「ス」と「ウィング」に割れる)。1語として次の行へ送る。
        ui.add_enabled(
            swing_usable,
            egui::Label::new("スウィング").wrap_mode(egui::TextWrapMode::Extend),
        );
        let mut peak = state.editor.swing_peak_ratio;
        if ui
            .add_enabled(
                swing_usable,
                egui::Slider::new(&mut peak, swing::MIN_PEAK_RATIO..=swing::MAX_PEAK_RATIO)
                    .fixed_decimals(2),
            )
            .on_hover_text(swing_hint)
            .changed()
        {
            state.editor.swing_peak_ratio = peak;
            // 鳴り方が変わるのでシーケンスを送り直す (編集ではないので履歴には積まない)
            state.dirty = true;
        }

        ui.separator();

        // 不均等な拍 (ウィンナ・ワルツ風)。掛けるトラックはトラック欄の「V」で選ぶ。
        // 奇数の N/4 でしか効かない。
        //
        // **1.00 が中央で無効**、左へ動かすと山 (2拍目が前)、右へ動かすと谷。
        // 値だけでは向きが分からないので、説明で補う。
        let waltz_usable = waltz::applies_to(state.editor.beats, state.editor.beat_type);
        let waltz_hint = if waltz_usable {
            "拍の長さの偏り (1.00 = 均等)。1.00 未満で2拍目が前に出ます (ウィンナ・ワルツ風)。\
             掛けるトラックは左の欄の「V」で選びます"
        } else {
            "不均等な拍は奇数の N/4 拍子 (3/4・5/4・7/4…) のときだけ使えます"
        };
        ui.add_enabled(
            waltz_usable,
            egui::Label::new("拍の偏り").wrap_mode(egui::TextWrapMode::Extend),
        );
        let mut waltz_ratio = state.editor.waltz_ratio;
        if ui
            .add_enabled(
                waltz_usable,
                egui::Slider::new(&mut waltz_ratio, waltz::MIN_RATIO..=waltz::MAX_RATIO)
                    .fixed_decimals(2),
            )
            .on_hover_text(waltz_hint)
            .changed()
        {
            state.editor.waltz_ratio = waltz_ratio;
            state.dirty = true;
        }

        ui.spacing_mut().slider_width = default_slider_width;
    });
}

/// 上部ツールバー
pub(super) fn toolbar(
    ui: &mut egui::Ui,
    state: &mut EditorState,
    playing: bool,
    pos_quarters: f64,
    commands: &mut Vec<EditorCommand>,
) {
    // 項目が増えて 1100px に収まらなくなったので折り返す。
    // 収まらないぶんが画面外へ消えると、そこだけ操作できなくなるため。
    ui.horizontal_wrapped(|ui| {
        if playing {
            if ui
                .button("⏹ 停止")
                .on_hover_text("停止して再生開始位置へ戻す (Space)")
                .clicked()
            {
                stop_playback(state, commands);
            }
        } else if ui
            .button("▶ 再生")
            .on_hover_text("再生ヘッドから再生 (Space) / 先頭から再生 (Shift+Space)")
            .clicked()
        {
            start_playback(state, pos_quarters, playing, None, commands);
        }

        if ui.checkbox(&mut state.looping, "ループ").changed() {
            commands.push(EditorCommand::SetLoop(state.looping));
        }

        ui.separator();

        ui.label("テンポ");
        if ui
            .add(egui::DragValue::new(&mut state.editor.tempo).range(20..=300))
            .changed()
        {
            state.dirty = true;
        }

        ui.label("拍子");
        if ui
            .add(egui::DragValue::new(&mut state.editor.beats).range(1..=16))
            .changed()
        {
            state.dirty = true;
        }
        ui.label("/");
        egui::ComboBox::from_id_salt("beat_type")
            .selected_text(state.editor.beat_type.to_string())
            .width(48.0)
            .show_ui(ui, |ui| {
                for bt in [1u32, 2, 4, 8, 16, 32] {
                    if ui
                        .selectable_value(&mut state.editor.beat_type, bt, bt.to_string())
                        .changed()
                    {
                        state.dirty = true;
                    }
                }
            });

        ui.separator();

        // 音階モード。切り替え時に範囲外になる半音を丸める。
        ui.label("音階");
        let mut scale = state.editor.scale;
        egui::ComboBox::from_id_salt("scale")
            .selected_text(scale.label())
            .width(96.0)
            .show_ui(ui, |ui| {
                for mode in [ScaleMode::Equal12, ScaleMode::BohlenPierce13] {
                    ui.selectable_value(&mut scale, mode, mode.label());
                }
            });
        if scale != state.editor.scale {
            state.history.record(EditGroup::Once);
            state.editor.scale = scale;
            let max = scale.max_semitone();
            for note in &mut state.editor.notes {
                note.semitone = note.semitone.min(max);
            }
            state.last_note.semitone = state.last_note.semitone.min(max);
            state.dirty = true;
        }

        ui.separator();

        ui.label("スナップ");
        egui::ComboBox::from_id_salt("snap")
            .selected_text(snap_label(state.snap))
            .width(56.0)
            .show_ui(ui, |ui| {
                for (label, value) in [
                    ("1/1", 4.0),
                    ("1/2", 2.0),
                    ("1/4", 1.0),
                    ("1/8", 0.5),
                    ("1/16", 0.25),
                    ("1/32", 0.125),
                ] {
                    ui.selectable_value(&mut state.snap, value, label);
                }
            });

        // 連符モード: スナップ幅と新規ノートの音価が連符1音分になる
        ui.label("連符");
        egui::ComboBox::from_id_salt("tuplet")
            .selected_text(tuplet_label(state.tuplet))
            .width(64.0)
            .show_ui(ui, |ui| {
                for n in [1u32, 3, 4, 5, 6, 7] {
                    ui.selectable_value(&mut state.tuplet, n, tuplet_label(n));
                }
            })
            .response
            .on_hover_text("スナップ幅2つ分を N 等分します (例: スナップ 1/8 の5連符なら四分音符1つに5音)");

        ui.checkbox(&mut state.left_resize, "左端音価")
            .on_hover_text("右端に加えて、ノートの左端でも音価を変更できるようにする (左端は終端を固定して頭を動かす)");

        ui.separator();

        // 段の縦幅 (縦ズーム)。段が少ないときに広げて掴みやすくするためのもの
        ui.label("段幅");
        let mut row_h = state.row_h;
        if ui
            .add(
                egui::DragValue::new(&mut row_h)
                    .range(MIN_ROW_H..=MAX_ROW_H)
                    .speed(0.5)
                    .fixed_decimals(0)
                    .suffix("px"),
            )
            .on_hover_text("段の縦幅 (グリッドの上で Ctrl+ホイールでも変えられます)")
            .changed()
        {
            state.set_row_h(row_h);
        }
        // 横ズームはドラッグでしか変えられないので、戻す口をここに置く
        // (ここが無いと、行きすぎたときに手で戻すしかない)
        if ui
            .small_button("既定")
            .on_hover_text(format!(
                "拡大率を既定に戻す (段幅 {ROW_H:.0}px / 四分音符 {PPQ:.0}px)"
            ))
            .clicked()
        {
            state.set_row_h(ROW_H);
            state.set_ppq(PPQ);
        }

        ui.separator();

        // アンドゥ / リドゥ (Ctrl+Z / Ctrl+Y)
        if ui
            .add_enabled(state.history.can_undo(), egui::Button::new("↶"))
            .on_hover_text("元に戻す (Ctrl+Z)")
            .clicked()
            && state.history.undo(&mut state.editor)
        {
            state.clear_selection();
            state.dirty = true;
        }
        if ui
            .add_enabled(state.history.can_redo(), egui::Button::new("↷"))
            .on_hover_text("やり直し (Ctrl+Y)")
            .clicked()
            && state.history.redo(&mut state.editor)
        {
            state.clear_selection();
            state.dirty = true;
        }

    });

    groove_toolbar(ui, state);

    // 選択中ノートの詳細
    let max_semitone = state.editor.scale.max_semitone();
    ui.horizontal(|ui| {
        if let Some(idx) = state.selected {
            if let Some(note) = state.editor.notes.get_mut(idx) {
                /* どうやってもUIがズレるので削除しました。→後続のラベルがあるため基本不要
                // 表示名は桁数で幅が変わる ((9,4) → (10,4)) ため、
                // 固定幅の領域に左寄せで置いて後続のウィジェットを動かさない
                let name = format!("選択中: {}", note.name());
                ui.allocate_ui_with_layout(
                    vec2(120.0, 18.0),
                    egui::Layout::left_to_right(egui::Align::Center),
                    |ui| {
                        ui.label(name);
                    },
                );
                */

                // 2桁分の幅を確保して、9→10 でレイアウトが動かないようにする
                ui.label("半音");
                let resp = ui.add_sized(
                    [44.0, 18.0],
                    egui::DragValue::new(&mut note.semitone).range(0..=max_semitone),
                );
                if resp.changed() {
                    state.history.record(EditGroup::Semitone);
                    state.dirty = true;
                }
                if resp.drag_stopped() || resp.lost_focus() {
                    state.history.end_group();
                }

                ui.label("オクターブ");
                let resp = ui.add_sized(
                    [44.0, 18.0],
                    egui::DragValue::new(&mut note.octave).range(MIN_OCTAVE..=MAX_OCTAVE),
                );
                if resp.changed() {
                    state.history.record(EditGroup::Octave);
                    state.dirty = true;
                }
                if resp.drag_stopped() || resp.lost_focus() {
                    state.history.end_group();
                }

                ui.label("ベロシティ");
                let resp = ui.add(egui::Slider::new(&mut note.velocity, 1..=127));
                if resp.changed() {
                    state.history.record(EditGroup::Velocity);
                    state.dirty = true;
                }
                if resp.drag_stopped() || resp.lost_focus() {
                    state.history.end_group();
                }

                if ui.button("削除").clicked() {
                    state.history.record(EditGroup::Once);
                    state.delete_selection();
                    state.dirty = true;
                }
            } else {
                state.clear_selection();
            }
        } else if state.selection.len() > 1 {
            ui.label(format!("{} 個選択中", state.selection.len()));
            if ui.button("まとめて削除").clicked() {
                state.history.record(EditGroup::Once);
                state.delete_selection();
                state.dirty = true;
            }
            ui.weak("Alt+ホイール: ベロシティ / ←→: 頭・尾を揃える");
        } else {
            ui.weak("操作の一覧は左下の「操作ガイド」から");
        }
    });
}

fn tuplet_label(tuplet: u32) -> &'static str {
    match tuplet {
        3 => "3連符",
        4 => "4連符",
        5 => "5連符",
        6 => "6連符",
        7 => "7連符",
        _ => "オフ",
    }
}

fn snap_label(snap: f32) -> &'static str {
    match snap {
        s if s >= 4.0 => "1/1",
        s if s >= 2.0 => "1/2",
        s if s >= 1.0 => "1/4",
        s if s >= 0.5 => "1/8",
        s if s >= 0.25 => "1/16",
        _ => "1/32",
    }
}
