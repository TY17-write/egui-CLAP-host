//! 左のトラック欄と、段まわりの操作 (段の帯・段の増減・CC 段の一覧)。

use super::history::EditGroup;
use super::metrics::{GUTTER_W, RULER_H};
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
    egui::Window::new(format!("段の設定 — {}", state.editor.tracks[track].name))
        .collapsible(false)
        .resizable(false)
        .open(&mut open)
        .show(ctx, |ui| {
            // **追加・削除は全部ここにも置く。** 段が1つのトラックでは行に
            // 出ないので、ここが唯一の入口になる。
            ui.horizontal(|ui| {
                ui.label("段:");
                lane_settings_buttons(ui, state, track);
            });
            ui.separator();

            let lanes = state.editor.lanes(track);
            let first_cc = state.editor.tracks[track].normal_lanes();
            if first_cc >= lanes {
                ui.label("このトラックに制御段 (CC・ヴェロシティ) はありません。");
                ui.weak(
                    "上の段の「+」で音符段、右の「+」で CC 段、\
                     「+V」でヴェロシティ段を追加できます。",
                );
                return;
            }

            ui.label("置いたブロックの長さだけ効きます。");
            ui.weak("CC 段の値はベロシティをそのまま使います。書いていない区間は 0 です。");
            ui.separator();

            // 入れ替えは行を回し終えてから行う (回している最中に段の番号が動くと、
            // 同じフレームの残りの行がずれる)
            let mut swap = None;

            for lane in first_cc..lanes {
                let kind = state.editor.lane_kind(track, lane);
                let current = kind.cc();
                ui.horizontal(|ui| {
                    // **並び替え。** 制御段どうしなので、CC とヴェロシティを
                    // またいで動かせる (種別もブロックと一緒に動く)
                    let up = ui.add_enabled(lane > first_cc, egui::Button::new("▲").small());
                    if up.clicked() {
                        swap = Some((lane, lane - 1));
                    }
                    up.on_hover_text("1つ上の制御段と入れ替える");

                    let down = ui.add_enabled(lane + 1 < lanes, egui::Button::new("▼").small());
                    if down.clicked() {
                        swap = Some((lane, lane + 1));
                    }
                    down.on_hover_text("1つ下の制御段と入れ替える");

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

            if let Some((from, to)) = swap {
                state.history.record(EditGroup::Once);
                state.editor.swap_lanes((track, from), (track, to));
                state.dirty = true;
            }
        });

    if !open {
        state.lane_config_track = None;
    }
}

/// 左のトラック欄。トラックごとに名前と段の増減ボタンを、
/// グリッドの段と同じ高さで並べる (行の位置が揃うようにするため)。
/// 子 Ui の描画範囲を `rect` に絞る。**親のクリップと必ず交差させる。**
///
/// `Ui::set_clip_rect` は今のクリップを**置き換える** (`painter_at` は交差させる)。
/// そのまま渡すと、ScrollArea が絞った範囲を広げ直してしまい、画面の外にある
/// 行までが描かれる。**実際にトラックを増やすとツールバーや下端のボタンの上に
/// トラック欄が重なって出た。**
fn clip_within(child: &mut egui::Ui, parent: &egui::Ui, rect: Rect) {
    child.set_clip_rect(rect.intersect(parent.clip_rect()));
}

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
    clip_within(&mut content, ui, rect);
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
    // **行の間に余白を入れない。**
    //
    // グリッドは段を隙間なく並べるので、ここで既定の余白 (3px) が入ると
    // **トラックが増えるほど段の位置が下へずれていく** (18本で 54px)。
    // 少ないうちは気付かないので、必ずここで 0 にしておくこと。
    ui.spacing_mut().item_spacing.y = 0.0;

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
        clip_within(&mut content, ui, rect);
        // 名前 + トグル4つ (M/S/W/V) + 印を 200px に収めるため間隔を詰める。
        // **右端は clip_rect で切られる**ので、増やすときは実際に見て確かめること
        content.spacing_mut().item_spacing.x = 2.0;

        // **名前をいちばん左に、書き換えられる形で置く。**
        //
        // 以前はトグルの右に 44px で置いていたが、それでは「ト…」としか出ず、
        // どのトラックか分からなかった。行の先頭に回して幅を稼ぐ。
        //
        // 2段目がある高さなら残り幅を名前に使い、無いときは段のボタンと
        // 同居するので詰める (隠れるより窮屈なほうがまし)。
        let two_rows = rect.height() >= LANE_BUTTON_ROW_Y + LANE_BUTTON_ROW_H;
        let name_w = if two_rows { 96.0 } else { 56.0 };
        let mut name = state.editor.tracks[track].name.clone();
        let response = content.add(
            egui::TextEdit::singleline(&mut name)
                .desired_width(name_w)
                .font(egui::FontId::proportional(11.0))
                .margin(vec2(2.0, 0.0)),
        );
        if response.changed() {
            // **1文字ごとに履歴へ積まない。** まとめて1ステップにする
            state.history.record(EditGroup::TrackName);
            state.editor.tracks[track].name = name;
            state.dirty = true;
        }
        if response.lost_focus() {
            state.history.end_group();
        }
        response.on_hover_text("トラック名 (クリックして書き換えられます)");

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

        // 段の増減は2段目へ回す。1行に詰めると収まらないうえ、
        // **通常段と CC 段で別のボタンが要る** (最下段は CC 段のことがあるので、
        // 1組では「消したい段と違うものが消える」)。
        //
        // **2段目が入らない高さのときは、ボタン1つに畳んで設定窓へ送る。**
        // 以前は同じ行に6個続けて置いていたが、名前の欄を入れたことで
        // 幅が足りなくなり、右端の「CC」が見切れた。設定窓には同じ操作が
        // 全部あるので、畳んでも手立ては失われない。
        if two_rows {
            drop(content);
            let mut second = ui.new_child(
                egui::UiBuilder::new()
                    .max_rect(Rect::from_min_max(
                        Pos2::new(rect.left() + 6.0, rect.top() + LANE_BUTTON_ROW_Y),
                        rect.right_bottom() - vec2(6.0, pad_y),
                    ))
                    .layout(egui::Layout::left_to_right(egui::Align::Min)),
            );
            clip_within(&mut second, ui, rect);
            second.spacing_mut().item_spacing.x = 2.0;
            lane_buttons(&mut second, state, track);
        } else {
            lane_settings_button(&mut content, state, track);
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
        // 相手待ちの段があるとき、音符段と制御段の間では入れ替えられない
        // (制御段どうしは CC ↔ ヴェロシティでもよい。`swap_lanes` と同じ規則)
        let selectable = match state.lane_swap_source {
            Some((source_track, source_lane)) => {
                state.editor.lane_kind(source_track, source_lane).is_note()
                    == state.editor.lane_kind(track, lane).is_note()
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

/// 段の設定窓を開くボタン1つ。**2段目が入らない高さのときに使う。**
///
/// 段が1つのトラック (既定) はこの高さになる。ボタンを6個並べる幅が無いので、
/// 全部を持っている設定窓 ([`lane_config_window`]) への入口だけを出す。
fn lane_settings_button(ui: &mut egui::Ui, state: &mut EditorState, track: usize) {
    let lanes = state.editor.lanes(track);
    let has_control = state.editor.tracks[track].normal_lanes() < lanes;
    // 制御段があるトラックは色で分かるようにする (2段目の「CC」と同じ流儀)
    let label = egui::RichText::new("段…").size(10.0).color(if has_control {
        palette::GREEN
    } else {
        palette::FG_DIM
    });
    let response = ui.add(egui::Button::new(label).small());
    if response.clicked() {
        state.lane_config_track = (state.lane_config_track != Some(track)).then_some(track);
    }
    response.on_hover_text("段の追加・削除と CC の割り当てを開く");
}

/// 段の増減ひとそろい。**トラック欄の2段目に置く。**
///
/// 設定窓には [`lane_settings_buttons`] のほうを置く (窓を開くボタンが要らないため)。
fn lane_buttons(ui: &mut egui::Ui, state: &mut EditorState, track: usize) {
    normal_lane_buttons(ui, state, track, true);
    ui.separator();
    open_lane_config_button(ui, state, track);
    cc_lane_buttons(ui, state, track);
}

/// 設定窓の中に置く段の増減。
///
/// **窓を開くボタンは出さない** (すでに開いている窓の中なので、押すと閉じてしまう)。
fn lane_settings_buttons(ui: &mut egui::Ui, state: &mut EditorState, track: usize) {
    normal_lane_buttons(ui, state, track, false);
    ui.separator();
    cc_lane_buttons(ui, state, track);
}

/// 段の設定窓を開く／閉じるボタン
fn open_lane_config_button(ui: &mut egui::Ui, state: &mut EditorState, track: usize) {
    let lanes = state.editor.lanes(track);
    let has_control = state.editor.tracks[track].normal_lanes() < lanes;
    let label = egui::RichText::new("CC").size(10.0).color(if has_control {
        palette::GREEN
    } else {
        palette::FG_DIM
    });
    let response = ui.add(egui::Button::new(label).small());
    if response.clicked() {
        state.lane_config_track = (state.lane_config_track != Some(track)).then_some(track);
    }
    response.on_hover_text("段の設定 (CC の番号など) を開く");
}

/// 通常 (音符) 段の追加・削除
fn normal_lane_buttons(ui: &mut egui::Ui, state: &mut EditorState, track: usize, label: bool) {
    let normal = state.editor.tracks[track].normal_lanes();

    if label {
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
        "いちばん下の段を削除 (制御段には触れません)"
    } else {
        "その段にノートがある (または段が1つ) ので消せません"
    });
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

    // **ヴェロシティ段だけを名指しで外す。**
    //
    // 下の「−」は最下段しか外せないので、CC 段を後から足すと
    // ヴェロシティ段が下でなくなり、先に CC 段を消すしかなくなる。
    let velocity_lane = state.editor.tracks[track].velocity_lane();
    let velocity_removable = velocity_lane.is_some_and(|lane| {
        !state
            .editor
            .notes
            .iter()
            .any(|note| note.track == track && note.lane == lane)
    });
    let response = ui.add_enabled(velocity_removable, egui::Button::new("−V").small());
    if response.clicked() {
        state.history.record(EditGroup::Once);
        state.editor.remove_velocity_lane(track);
        state.dirty = true;
    }
    response.on_hover_text(if velocity_removable {
        "ヴェロシティ段を削除 (最下段でなくても外せます)"
    } else if has_velocity {
        "その段にブロックがあるので消せません"
    } else {
        "ヴェロシティ段がありません"
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
