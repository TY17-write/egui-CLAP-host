//! 左のトラック欄と、段まわりの操作 (段の帯・段の増減・CC 段の一覧)。

use super::history::EditGroup;
use super::metrics::{GUTTER_W, ROW_H, RULER_H};
use super::state::EditorState;
use crate::sequencer::LaneKind;
use crate::swing;
use crate::theme::palette;
use crate::waltz;
use eframe::egui::{self, vec2, CornerRadius, Pos2, Rect, Sense, Stroke};

/// トラック欄2段目 (段の増減ボタン) の上端。1段目のボタンの下に置く。
///
/// トラックの高さは段数 × 段の高さで決まり、グリッドと揃える必要があるため
/// 広げられない。**入らない高さのときは1段目に並べる** (隠れると段を増やす
/// 手立てが無くなるため)。
const LANE_BUTTON_ROW_Y: f32 = 22.0;
/// 2段目の高さ。これが入らなければ1段目へ回す
const LANE_BUTTON_ROW_H: f32 = 18.0;
/// 段の帯 (全選択・入れ替え) の幅。トラック欄の右端に置く
const LANE_STRIP_W: f32 = 10.0;

/// CC 段の設定でよく使う番号。名前を出しておかないと番号だけでは分からない。
const COMMON_CCS: [(u8, &str); 6] = [
    (1, "モジュレーション"),
    (11, "エクスプレッション"),
    (64, "ペダル (サステイン)"),
    (66, "ソステヌート"),
    (67, "ソフト"),
    (7, "音量"),
];

/// 段ごとに「音符を置く段」か「CC を書く段」かを決める一覧。
///
/// 段は何段にも増えるので、左のトラック欄に並べず別窓にしている。
///
/// **CC 段では、ブロックの頭で値を送り、尻で 0 に戻す。** MIDI に「CC 無し」は
/// 無いので、書いていない区間は 0 を送ることで表す (`CC_RELEASE`)。停止・シークでも
/// 同じように戻すので、ペダルが踏みっぱなしで残ることはない。
pub(super) fn lane_config_window(ctx: &egui::Context, state: &mut EditorState) {
    let Some(track) = state.lane_config_track else {
        return;
    };
    if track >= state.editor.track_count() {
        state.lane_config_track = None;
        return;
    }

    let mut open = true;
    egui::Window::new(format!("制御段 — {}", state.editor.tracks[track].name))
        .collapsible(false)
        .resizable(false)
        .open(&mut open)
        .show(ctx, |ui| {
            // 追加・削除はここにも置く。トラック欄が狭いときは行に出ないので、
            // ここが唯一の入口になる。
            ui.horizontal(|ui| {
                ui.label("制御段:");
                cc_lane_buttons(ui, state, track);
            });
            ui.separator();

            let lanes = state.editor.lanes(track);
            let first_cc = state.editor.tracks[track].normal_lanes();
            if first_cc >= lanes {
                ui.label("このトラックに制御段はありません。");
                ui.weak("上の「+」で CC 段、「+V」でヴェロシティ段を追加できます。");
                return;
            }

            ui.label("置いたブロックの長さだけ効きます。");
            ui.weak("CC 段の値はベロシティをそのまま使います。書いていない区間は 0 です。");
            ui.separator();

            for lane in first_cc..lanes {
                let kind = state.editor.lane_kind(track, lane);
                let current = kind.cc();
                ui.horizontal(|ui| {
                    ui.label(format!("段 {}", lane + 1));

                    if kind == LaneKind::Velocity {
                        ui.colored_label(palette::PURPLE, "ヴェロシティ");
                        ui.weak("このトラックの全ての音符段に効きます")
                            .on_hover_text(
                                "ブロックは開始値から終了値へ変化します。\
                                 区間にかかった音符は、その音符が始まる位置の値になります。\
                                 ブロックの外は音符自身の値のままです。",
                            );
                    }

                    if let Some(number) = current {
                        let mut number = number;
                        if ui
                            .add(
                                egui::DragValue::new(&mut number)
                                    .range(0..=127)
                                    .speed(0.25)
                                    .prefix("CC "),
                            )
                            .changed()
                        {
                            state.history.record(EditGroup::Once);
                            state.editor.tracks[track].set_lane_cc(lane, Some(number));
                            state.dirty = true;
                        }

                        egui::ComboBox::from_id_salt(("cc_preset", track, lane))
                            .selected_text(
                                COMMON_CCS
                                    .iter()
                                    .find(|(n, _)| *n == number)
                                    .map_or("その他", |(_, name)| name),
                            )
                            .show_ui(ui, |ui| {
                                for (n, name) in COMMON_CCS {
                                    if ui
                                        .selectable_label(n == number, format!("CC{n} {name}"))
                                        .clicked()
                                    {
                                        state.history.record(EditGroup::Once);
                                        state.editor.tracks[track].set_lane_cc(lane, Some(n));
                                        state.dirty = true;
                                    }
                                }
                            });

                        // 0 が「効いていない」に当たらない CC は、書いていない区間が
                        // 無音・左端・片寄りになってしまう。黙って壊れないよう断っておく。
                        // (7 音量 / 8 バランス / 10 パン)
                        if matches!(number, 7 | 8 | 10) {
                            ui.colored_label(palette::YELLOW, "⚠ 0 が中立ではありません")
                                .on_hover_text(
                                    "書いていない区間が 0 になります。\
                                     音量やパンでは無音・左端になってしまうので、\
                                     区間を切らずに書いてください。",
                                );
                        }
                    }
                });
            }
        });

    if !open {
        state.lane_config_track = None;
    }
}

/// 左のトラック欄。トラックごとに名前と段の増減ボタンを、
/// グリッドの段と同じ高さで並べる (行の位置が揃うようにするため)。
/// トラック欄の見出し (常に見える位置に置く)。高さはルーラーと同じにして、
/// 下のトラック一覧の1行目がグリッドの1段目と揃うようにする。
pub(super) fn track_gutter_header(ui: &mut egui::Ui, state: &mut EditorState) {
    let (rect, _) = ui.allocate_exact_size(vec2(GUTTER_W, RULER_H), Sense::hover());
    ui.painter_at(rect)
        .rect_filled(rect, CornerRadius::ZERO, palette::BG_LIGHT);

    let mut content = ui.new_child(
        egui::UiBuilder::new()
            .max_rect(rect.shrink2(vec2(6.0, 2.0)))
            .layout(egui::Layout::left_to_right(egui::Align::Center)),
    );
    content.set_clip_rect(rect);
    content.label(
        egui::RichText::new("トラック")
            .size(11.0)
            .color(palette::FG_DIM),
    );

    if content
        .small_button("+")
        .on_hover_text("トラックを追加")
        .clicked()
    {
        state.history.record(EditGroup::Once);
        state.editor.add_track();
    }

    // ノートの入った最後のトラックと、最後の1本は消せない
    let last = state.editor.track_count() - 1;
    let removable =
        state.editor.track_count() > 1 && !state.editor.notes.iter().any(|n| n.track == last);
    let response = content.add_enabled(removable, egui::Button::new("−").small());
    if response.clicked() {
        state.history.record(EditGroup::Once);
        state.editor.remove_last_track();
    }
    if !removable {
        response.on_hover_text("最後のトラックにノートがある (またはトラックが1つ) ので消せません");
    }
}

/// 左のトラック欄。トラックごとに名前と段の増減ボタンを、
/// グリッドの段と同じ高さで並べる (行の位置が揃うようにするため)。
pub(super) fn track_gutter(ui: &mut egui::Ui, state: &mut EditorState) {
    // ScrollArea の中身は親のレイアウトを引き継ぐ (ここは横並びの中なので)。
    // 縦並びを明示しないとトラックが横に並んでしまう。
    ui.vertical(|ui| track_gutter_content(ui, state));
}

fn track_gutter_content(ui: &mut egui::Ui, state: &mut EditorState) {
    for track in 0..state.editor.track_count() {
        let lanes = state.editor.lanes(track);
        // グリッドの段と行の位置が揃うよう、縦ズームに追従させる
        let height = lanes as f32 * state.row_h;
        let (rect, _) = ui.allocate_exact_size(vec2(GUTTER_W, height), Sense::hover());

        // トラックの区切りが分かるように枠と背景を敷く
        let painter = ui.painter_at(rect);
        painter.rect_filled(rect, CornerRadius::ZERO, palette::BG_LIGHT);
        painter.line_segment(
            [rect.left_top(), rect.right_top()],
            Stroke::new(1.5_f32, palette::FG_DIM),
        );

        // 段の帯 (トラック欄といちばん左の小節の間)。段ごとに1本ずつ。
        lane_strips(ui, state, track, rect);

        // 段が1つ (高さ = 段1つ分) でも収まるよう、名前とボタンは1行に並べる。
        // 縦に縮めたときは余白から先に削る (ボタンの高さは変えられないため)
        let pad_y = (rect.height() * 0.1).min(2.0);
        let mut content = ui.new_child(
            egui::UiBuilder::new()
                // 右端は段の帯にあけておく
                .max_rect(
                    rect.shrink2(vec2(6.0, pad_y))
                        .with_max_x(rect.right() - LANE_STRIP_W - 4.0),
                )
                .layout(egui::Layout::left_to_right(egui::Align::Min)),
        );
        content.set_clip_rect(rect);
        // トグル4つ (M/S/W/V) + 名前 + ボタン3つを 200px に収めるため間隔を詰める。
        // **右端は clip_rect で切られる**ので、増やすときは実際に見て確かめること
        content.spacing_mut().item_spacing.x = 2.0;

        // ミュート / ソロ。編集ではないのでアンドゥ履歴には積まない。
        // 変更したらシーケンスを送り直す (鳴らす/止めるの切り替えのため)。
        let muted = state.editor.tracks[track].muted;
        let soloed = state.editor.tracks[track].soloed;
        if content
            .add(egui::SelectableLabel::new(
                muted,
                egui::RichText::new("M").size(10.0).color(if muted {
                    palette::RED
                } else {
                    palette::FG_DIM
                }),
            ))
            .on_hover_text("ミュート")
            .clicked()
        {
            state.editor.tracks[track].muted = !muted;
            state.dirty = true;
        }
        if content
            .add(egui::SelectableLabel::new(
                soloed,
                egui::RichText::new("S").size(10.0).color(if soloed {
                    palette::YELLOW
                } else {
                    palette::FG_DIM
                }),
            ))
            .on_hover_text("ソロ (どれか1つでもソロなら、それ以外は鳴りません)")
            .clicked()
        {
            state.editor.tracks[track].soloed = !soloed;
            state.dirty = true;
        }

        // スウィング。伴奏は正確な拍のまま、ソロだけ跳ねさせる使い方をするので
        // トラックごとに持つ。深さはツールバーの「スウィング」で全体設定。
        let swinging = state.editor.tracks[track].swing;
        let swing_enabled = swing::applies_to(state.editor.beat_type);
        let response = content.add_enabled(
            swing_enabled,
            egui::SelectableLabel::new(
                swinging,
                egui::RichText::new("W").size(10.0).color(if swinging {
                    palette::CYAN
                } else {
                    palette::FG_DIM
                }),
            ),
        );
        if response.clicked() {
            state.editor.tracks[track].swing = !swinging;
            state.dirty = true;
        }
        response.on_hover_text(if swing_enabled {
            "スウィング (跳ねの深さはツールバーで設定)"
        } else {
            "スウィングは N/4 拍子のときだけ使えます"
        });

        // 不均等な拍 (ウィンナ = Vienna の V)。スウィングと併用できる。
        // 偏りの強さはツールバーの「拍の偏り」で全体設定。
        let waltzing = state.editor.tracks[track].waltz;
        let waltz_enabled = waltz::applies_to(state.editor.beats, state.editor.beat_type);
        let response = content.add_enabled(
            waltz_enabled,
            egui::SelectableLabel::new(
                waltzing,
                egui::RichText::new("V").size(10.0).color(if waltzing {
                    palette::CYAN
                } else {
                    palette::FG_DIM
                }),
            ),
        );
        if response.clicked() {
            state.editor.tracks[track].waltz = !waltzing;
            state.dirty = true;
        }
        response.on_hover_text(if waltz_enabled {
            "不均等な拍 / ウィンナ・ワルツ風 (偏りはツールバーで設定)"
        } else {
            "不均等な拍は奇数の N/4 拍子のときだけ使えます"
        });

        // 狭いときは名前の枠を詰める (右のボタンを見切れさせないため)
        let name_w = if rect.height() >= LANE_BUTTON_ROW_Y + LANE_BUTTON_ROW_H {
            44.0
        } else {
            32.0
        };
        let name = state.editor.tracks[track].name.clone();
        content.allocate_ui(vec2(name_w, ROW_H - 4.0), |ui| {
            ui.add(
                egui::Label::new(egui::RichText::new(name).size(11.0).color(palette::FG))
                    .truncate(),
            );
        });

        // **このトラックを鳴らすオーディオトラックがあるか。**
        //
        // 音源は打ち込みトラックではなくオーディオトラックに載る。ここを見ている
        // オーディオトラックが1つも無ければ、**書いても鳴らない**ので印を出す
        // (割り当ては「オーディオトラック」の窓で行う)。
        let users = state.track_plugins.get(track).cloned().flatten();
        let label = egui::RichText::new("♪").size(11.0).color(match &users {
            Some(_) => palette::GREEN,
            None => palette::RED,
        });
        let response = content.add(egui::Label::new(label));
        response.on_hover_text(match &users {
            Some(names) => format!("鳴らすオーディオトラック: {names}"),
            None => "どのオーディオトラックからも参照されていません (鳴りません)。\
                     「オーディオトラック」の窓で割り当ててください"
                .to_string(),
        });

        // 段の増減は2段目へ回す。1行に詰めると 200px に収まらないうえ、
        // **通常段と CC 段で別のボタンが要る** (最下段は CC 段のことがあるので、
        // 1組では「消したい段と違うものが消える」)。
        //
        // ただし2段目が入らない高さのときは、同じ行に続けて置く。
        // **隠れると段を増やす手立てが無くなる**ので、窮屈なほうがまだよい
        // (段が1つのトラックは既定でこの高さになる)。
        if rect.height() >= LANE_BUTTON_ROW_Y + LANE_BUTTON_ROW_H {
            drop(content);
            let mut second = ui.new_child(
                egui::UiBuilder::new()
                    .max_rect(Rect::from_min_max(
                        Pos2::new(rect.left() + 6.0, rect.top() + LANE_BUTTON_ROW_Y),
                        rect.right_bottom() - vec2(6.0, pad_y),
                    ))
                    .layout(egui::Layout::left_to_right(egui::Align::Min)),
            );
            second.set_clip_rect(rect);
            second.spacing_mut().item_spacing.x = 2.0;
            lane_buttons(&mut second, state, track, false);
        } else {
            lane_buttons(&mut content, state, track, true);
        }
    }
}

/// 段ごとの帯。押すとその段のノートを全選択し、続けて別の段を押すと入れ替える。
///
/// トラック欄の右端 (= いちばん左の小節との境) に縦棒として並べる。グリッドの
/// 段と同じ高さなので、行の位置がそのまま揃う。
///
/// **押した段は赤くなる (相手待ち)。** そこで別の段を押すと入れ替わり、同じ段を
/// もう一度押すと取り消す。入れ替えは種別が同じ段どうしでしかできないので、
/// 選べない相手はその場で分かるよう暗く描く。
fn lane_strips(ui: &mut egui::Ui, state: &mut EditorState, track: usize, track_rect: Rect) {
    let row_h = state.row_h;
    let painter = ui.painter_at(track_rect);

    for lane in 0..state.editor.lanes(track) {
        let rect = Rect::from_min_size(
            Pos2::new(
                track_rect.right() - LANE_STRIP_W,
                track_rect.top() + lane as f32 * row_h,
            ),
            vec2(LANE_STRIP_W, row_h),
        );
        let response = ui.interact(
            rect,
            ui.id().with(("lane_strip", track, lane)),
            Sense::click(),
        );

        let armed = state.lane_swap_source == Some((track, lane));
        // 相手待ちの段があるとき、種別が違えば入れ替えられない
        let selectable = match state.lane_swap_source {
            Some((source_track, source_lane)) => {
                state.editor.lane_cc(source_track, source_lane).is_some()
                    == state.editor.lane_cc(track, lane).is_some()
            }
            None => true,
        };

        let color = if armed {
            palette::RED
        } else if !selectable {
            palette::FG_DIM.gamma_multiply(0.4)
        } else if response.hovered() {
            palette::GREEN
        } else {
            palette::GREEN.gamma_multiply(0.55)
        };
        painter.rect_filled(rect.shrink2(vec2(2.0, 1.0)), CornerRadius::same(2), color);

        // 選択を変えると相手待ちは解除される (`select_many` を参照) ので、
        // **待ち状態にするのは選択したあと**。
        if response.clicked() {
            match state.lane_swap_source {
                // もう一度押したら取り消す
                Some(source) if source == (track, lane) => state.lane_swap_source = None,
                Some(source) => {
                    state.history.record(EditGroup::Once);
                    if state.editor.swap_lanes(source, (track, lane)) {
                        state.dirty = true;
                    }
                    state.select_lane(track, lane);
                    state.lane_swap_source = None;
                }
                None => {
                    state.select_lane(track, lane);
                    state.lane_swap_source = Some((track, lane));
                }
            }
        }

        response.on_hover_text(if armed {
            "入れ替える相手の段を押してください (もう一度押すと取り消し)"
        } else if selectable {
            "この段のノートを全選択 (続けて別の段を押すと入れ替え)"
        } else {
            "音符段と CC 段は入れ替えられません"
        });
    }
}

/// 段の増減 (通常段と CC 段で別々)。トラック欄の2段目に置く。
///
/// `compact` のときは、同じ行に続けて置くので見出しと区切りを省く。
fn lane_buttons(ui: &mut egui::Ui, state: &mut EditorState, track: usize, compact: bool) {
    let lanes = state.editor.lanes(track);
    let normal = state.editor.tracks[track].normal_lanes();

    // ---- 通常段 ----
    if !compact {
        ui.label(egui::RichText::new("段").size(10.0).color(palette::FG_DIM));
    }
    if ui
        .small_button("+")
        .on_hover_text("段を追加 (CC 段より上に入ります)")
        .clicked()
    {
        state.history.record(EditGroup::Once);
        state.editor.add_lane(track);
    }

    // 通常段の最下段。ノートがあるとき、通常段が1つのときは消せない
    let removable = normal > 1
        && !state
            .editor
            .notes
            .iter()
            .any(|note| note.track == track && note.lane + 1 == normal);
    let response = ui.add_enabled(removable, egui::Button::new("−").small());
    if response.clicked() {
        state.history.record(EditGroup::Once);
        state.editor.remove_last_normal_lane(track);
    }
    response.on_hover_text(if removable {
        "いちばん下の段を削除 (CC 段には触れません)"
    } else {
        "その段にノートがある (または段が1つ) ので消せません"
    });

    if !compact {
        ui.separator();
    }

    // ---- CC 段 ----
    let has_cc = normal < lanes;
    let label = egui::RichText::new("CC").size(10.0).color(if has_cc {
        palette::GREEN
    } else {
        palette::FG_DIM
    });
    let response = ui.add(egui::Button::new(label).small());
    if response.clicked() {
        state.lane_config_track = (state.lane_config_track != Some(track)).then_some(track);
    }
    response.on_hover_text("CC 段の番号の一覧を開く");

    // 狭いときは CC の増減を行に出さない。**押せるが見切れる**より、
    // 一覧 (上の「CC」ボタン) にまとめたほうが扱える。一覧側にも同じ操作がある。
    if !compact {
        cc_lane_buttons(ui, state, track);
    }
}

/// 制御段 (CC・ヴェロシティ) の追加・削除。
/// トラック欄の2段目と、一覧の両方から使う。
fn cc_lane_buttons(ui: &mut egui::Ui, state: &mut EditorState, track: usize) {
    let lanes = state.editor.lanes(track);
    let has_control = state.editor.tracks[track].normal_lanes() < lanes;

    if ui
        .small_button("+")
        .on_hover_text("CC 段を最下段に追加 (ペダル CC64。番号は一覧で変えられます)")
        .clicked()
    {
        state.history.record(EditGroup::Once);
        // 既定はペダル。いちばん使ううえ、0 が「離す」で自然に効く
        state.editor.add_cc_lane(track, 64);
        state.lane_config_track = Some(track);
        state.dirty = true;
    }

    // **トラックに1本まで。** すでにあれば押せない
    let has_velocity = state.editor.tracks[track].velocity_lane().is_some();
    let response = ui.add_enabled(!has_velocity, egui::Button::new("+V").small());
    if response.clicked() {
        state.history.record(EditGroup::Once);
        state.editor.add_velocity_lane(track);
        state.lane_config_track = Some(track);
        state.dirty = true;
    }
    response.on_hover_text(if has_velocity {
        "このトラックにはすでにヴェロシティ段があります"
    } else {
        "ヴェロシティ段を最下段に追加 (クレシェンド・デクレシェンド用)"
    });

    let removable = has_control
        && !state
            .editor
            .notes
            .iter()
            .any(|note| note.track == track && note.lane + 1 == lanes);
    let response = ui.add_enabled(removable, egui::Button::new("−").small());
    if response.clicked() {
        state.history.record(EditGroup::Once);
        state.editor.remove_last_control_lane(track);
        state.dirty = true;
    }
    response.on_hover_text(if removable {
        "いちばん下の制御段を削除"
    } else if has_control {
        "その段にブロックがあるので消せません"
    } else {
        "制御段がありません"
    });
}
