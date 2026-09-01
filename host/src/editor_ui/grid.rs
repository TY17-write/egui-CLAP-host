//! グリッド本体 (ルーラー + ノートレーン) の描画と操作。
//!
//! 描画と当たり判定は同じ `row_h` / `ppq` で通す必要があるため、1つの関数に
//! まとめてある。算出そのものは [`geometry`](super::geometry) に出してあり、
//! ここは egui とのやり取りに専念する。

use super::color::note_fill;
use super::geometry::{
    edge_scroll_delta, hit_note, horizontal_offset_for_anchor, is_inside_lanes, move_delta,
    note_rect, note_row, resize_delta, row_to_track_lane, seek_target, to_content_pos,
    to_screen_pos, track_row_offsets, velocity_fill_rect, velocity_ramp_points, Hit,
};
use super::history::EditGroup;
use super::metrics::{PPQ_ZOOM_PER_PIXEL, ROW_ZOOM_STEP, RULER_H};
use super::state::{DragKind, DragState, EditorState, MiddleDrag, NoteDefaults};
use super::EditorCommand;
use crate::sequencer::{LaneKind, Note};
use crate::theme::palette;
use eframe::egui::{
    self, vec2, Align2, CornerRadius, CursorIcon, FontId, Pos2, Rect, Sense, Stroke,
};

/// ノートに載せる音名ラベルの文字サイズ。
/// これが入らない高さまで縮めたらラベルを省く
const NOTE_LABEL_SIZE: f32 = 11.0;

/// Alt+ホイール1ノッチあたりのベロシティ変化量
const VELOCITY_WHEEL_STEP: i32 = 4;

/// ベロシティに満たない部分 (ゴースト) の不透明度。
/// 0 にするとノートが痩せて見えるので、色相と輪郭が残る程度に薄く塗る。
const VELOCITY_GHOST_ALPHA: f32 = 0.3;

/// ホイールの回転量 (ノッチ数) を取り出す。`wanted` が真のときだけ横取りする。
///
/// ScrollArea は内容を描き終えたあとに smooth_scroll_delta を読むので、
/// ここでイベントごと消しておけば、割り当てた操作とスクロールが同時に起きない。
fn take_wheel_notches(ui: &mut egui::Ui, wanted: impl Fn(&egui::Modifiers) -> bool) -> f32 {
    ui.input_mut(|i| {
        if !wanted(&i.modifiers) {
            return 0.0;
        }
        let mut notches = 0.0;
        i.events.retain(|event| match event {
            egui::Event::MouseWheel { unit, delta, .. } => {
                notches += match unit {
                    egui::MouseWheelUnit::Line => delta.y,
                    egui::MouseWheelUnit::Point => delta.y / 50.0,
                    egui::MouseWheelUnit::Page => delta.y * 3.0,
                };
                false
            }
            _ => true,
        });
        i.smooth_scroll_delta = egui::Vec2::ZERO;
        i.raw_scroll_delta = egui::Vec2::ZERO;
        notches
    })
}

/// グリッド本体 (ルーラー + ノートレーン)
pub(super) fn grid(
    ui: &mut egui::Ui,
    state: &mut EditorState,
    pos_quarters: f64,
    playing: bool,
    commands: &mut Vec<EditorCommand>,
) {
    // ---- Alt+ホイールで選択ノートのベロシティを変更 ----
    let notches = take_wheel_notches(ui, |m| m.alt);
    if notches != 0.0 {
        let delta = (notches * VELOCITY_WHEEL_STEP as f32).round() as i32;
        if delta != 0 && state.change_selection_velocity(delta) {
            state.history.record(EditGroup::Velocity);
            state.dirty = true;
        }
    }

    // ---- Ctrl+ホイールで段の縦幅をズーム ----
    // ホイールは画面のどこで回しても届くので、グリッドの見えている範囲
    // (ScrollArea のビューポート = クリップ矩形) にカーソルがあるときだけ拾う。
    let pointer_in_grid = ui
        .input(|i| i.pointer.hover_pos())
        .is_some_and(|pos| ui.clip_rect().contains(pos));
    // Alt が優先 (上で消費済み)。両方押されていてもズームはしない
    let zoom_notches = if pointer_in_grid {
        take_wheel_notches(ui, |m| m.command && !m.alt)
    } else {
        0.0
    };

    // 段の高さと四分音符の横幅は、このフレームの間ずっと同じ値を使う
    // (ズームの反映は次のフレーム)
    let row_h = state.row_h;
    let ppq = state.ppq;

    // 表示する行数 = 全トラックの段の合計。
    // (フェーズ1ではトラックは1本なので、そのトラックの段数と同じ)
    let display_rows = state.editor.total_rows().max(1);
    // 各トラックが何行目から始まるか (ノートの y 位置に使う)
    let row_offsets = track_row_offsets(&state.editor);

    let qpb = state.editor.quarters_per_bar();
    // 表示範囲: ノートの終端を小節に切り上げ + 余白2小節 (最低8小節)
    let total_quarters = (state.editor.length_quarters_bar_aligned() + qpb * 2.0).max(qpb * 8.0);

    // 中身を描く高さ (ルーラー + 全段)
    let content_h = RULER_H + display_rows as f32 * row_h;
    // 確保する高さは見えている範囲まで広げる。
    //
    // 段の下に隙間を残すと、そこは ScrollArea のものになってドラッグが
    // スクロールに化けてしまい、段の外から範囲選択を始められない。
    // 広げるのは当たり判定だけで、描画はすべて content_h までに留める
    // (空白に段があるように見せないため)。
    let size = vec2(total_quarters * ppq, content_h.max(ui.clip_rect().height()));
    let (response, painter) = ui.allocate_painter(size, Sense::click_and_drag());
    let origin = response.rect.min;

    let to_x = |quarters: f32| origin.x + quarters * ppq;

    // ---- 背景 ----
    painter.rect_filled(
        Rect::from_min_size(origin, vec2(size.x, RULER_H)),
        CornerRadius::ZERO,
        palette::BG_LIGHT,
    );
    painter.rect_filled(
        Rect::from_min_size(
            origin + vec2(0.0, RULER_H),
            vec2(size.x, content_h - RULER_H),
        ),
        CornerRadius::ZERO,
        palette::BG_DARK,
    );

    // ---- トラックの地色 ----
    // 1つおきに薄く敷いて、どこからどこまでが1トラックか分かるようにする
    for (track, offset) in row_offsets.iter().enumerate() {
        if track % 2 == 1 {
            let top = origin.y + RULER_H + *offset as f32 * row_h;
            let height = state.editor.lanes(track) as f32 * row_h;
            painter.rect_filled(
                Rect::from_min_size(Pos2::new(origin.x, top), vec2(size.x, height)),
                CornerRadius::ZERO,
                palette::BG.gamma_multiply(0.5),
            );
        }
    }

    // ---- 制御段の地色 ----
    // 音符段と見分けが付かないと、音高のつもりで置いた音が CC になってしまう。
    // トラックの地色より上に敷いて、どちらのトラックでも同じ色に見えるようにする。
    // **CC とヴェロシティも色で分ける** (ブロックの値の意味が違うため)。
    for (track, offset) in row_offsets.iter().enumerate() {
        for lane in 0..state.editor.lanes(track) {
            let tint = match state.editor.lane_kind(track, lane) {
                LaneKind::Note => continue,
                LaneKind::Cc(_) => palette::GREEN,
                LaneKind::Velocity => palette::PURPLE,
            };
            let top = origin.y + RULER_H + (*offset + lane) as f32 * row_h;
            painter.rect_filled(
                Rect::from_min_size(Pos2::new(origin.x, top), vec2(size.x, row_h)),
                CornerRadius::ZERO,
                tint.gamma_multiply(0.14),
            );
        }
    }

    // ---- 段の区切り線 ----
    let lane_stroke = Stroke::new(0.5_f32, palette::BG_SELECT.gamma_multiply(0.7));
    for row in 0..=display_rows {
        let y = origin.y + RULER_H + row as f32 * row_h;
        painter.line_segment(
            [Pos2::new(origin.x, y), Pos2::new(origin.x + size.x, y)],
            lane_stroke,
        );
    }

    // ---- トラックの区切り線 (段の線より目立たせる) ----
    let track_stroke = Stroke::new(1.5_f32, palette::FG_DIM);
    for offset in row_offsets.iter().skip(1) {
        let y = origin.y + RULER_H + *offset as f32 * row_h;
        painter.line_segment(
            [Pos2::new(origin.x, y), Pos2::new(origin.x + size.x, y)],
            track_stroke,
        );
    }

    // ---- 連符の補助線 ----
    // 拍線・小節線より先に描いて、重なる位置は上から塗り潰させる
    let unit = state.snap_unit();
    if state.tuplet > 1 && unit * ppq >= 6.0 {
        let tuplet_stroke = Stroke::new(0.5_f32, palette::PURPLE.gamma_multiply(0.45));
        let mut q = unit;
        while q <= total_quarters {
            let x = to_x(q);
            painter.line_segment(
                [
                    Pos2::new(x, origin.y + RULER_H),
                    Pos2::new(x, origin.y + content_h),
                ],
                tuplet_stroke,
            );
            q += unit;
        }
    }

    // ---- 拍線・小節線 ----
    let qpbeat = state.editor.quarters_per_beat();
    let beat_stroke = Stroke::new(1.0_f32, palette::BG_SELECT);
    let bar_stroke = Stroke::new(1.5_f32, palette::FG_DIM);

    let mut q = 0.0f32;
    let mut beat_index = 0u32;
    let mut bar_number = 1u32;
    while q <= total_quarters {
        let x = to_x(q);
        let is_bar = beat_index.is_multiple_of(state.editor.beats.max(1));
        let stroke = if is_bar { bar_stroke } else { beat_stroke };
        let top = if is_bar { origin.y } else { origin.y + RULER_H };
        painter.line_segment(
            [Pos2::new(x, top), Pos2::new(x, origin.y + content_h)],
            stroke,
        );
        if is_bar {
            painter.text(
                Pos2::new(x + 4.0, origin.y + RULER_H * 0.5),
                Align2::LEFT_CENTER,
                bar_number.to_string(),
                FontId::proportional(11.0),
                palette::FG_DIM,
            );
            bar_number += 1;
        }
        q += qpbeat;
        beat_index += 1;
    }

    // ---- ノート ----
    // 同じ段で重なっているノートの印 (ドラッグ中も毎フレーム追従する)
    let overlapped = state.editor.overlapping_notes();
    for (idx, note) in state.editor.notes.iter().enumerate() {
        let rect = note_rect(origin, note_row(&row_offsets, note), note, ppq, row_h);
        // CC ブロックは音高で色を変えても意味が無い。音符と取り違えないよう、
        // 段の地色と同じ緑で塗り分ける (ベロシティの塗り高さはそのまま使える)。
        let kind = state.editor.lane_kind(note.track, note.lane);
        let fill = match kind {
            LaneKind::Cc(_) => palette::GREEN,
            LaneKind::Velocity => palette::PURPLE,
            LaneKind::Note => note_fill(note, state.editor.scale),
        };
        // ベロシティは「下からの塗りの高さ」で表す。明度やアルファを直接下げると
        // ダークな背景で弱いノートが見えなくなるため、色相はそのままに
        // 満たない部分をゴーストとして残す (輪郭と音高の色は常に見える)。
        painter.rect_filled(
            rect,
            CornerRadius::same(4),
            fill.gamma_multiply(VELOCITY_GHOST_ALPHA),
        );
        // 文字を抜き色にするかの判定に使う (文字は左端から書くので、左端の高さで見る)
        let filled_top;
        if kind == LaneKind::Velocity {
            // **坂は形で見せる。** クレシェンドかデクレシェンドかを、
            // 数字を読まずに分かるようにする
            let points = velocity_ramp_points(rect, note.velocity, note.velocity_to);
            filled_top = points[0].y;
            painter.add(egui::Shape::convex_polygon(points, fill, Stroke::NONE));
        } else {
            let level = velocity_fill_rect(rect, note.velocity);
            filled_top = level.top();
            painter.rect_filled(
                level,
                if level.height() >= rect.height() - 0.5 {
                    CornerRadius::same(4)
                } else {
                    // 上辺はゴーストとの境目なので角を丸めない
                    CornerRadius {
                        nw: 0,
                        ne: 0,
                        sw: 4,
                        se: 4,
                    }
                },
                fill,
            );
        }

        // **重なりの警告は内・外の二重枠。** 同じ段のブロックは重なると
        // 隠れ合って、置いたことに気付けない。内側の明るい赤はノート色の上で、
        // 外側の濃い赤は暗い背景の上で浮く — 1本だけだと赤いノートに
        // 赤枠が沈む。選択中は外側が白枠 (あとから描く) に替わるが、
        // 内側は残るので警告は見失わない。
        if overlapped[idx] {
            painter.rect_stroke(
                rect,
                CornerRadius::same(4),
                Stroke::new(
                    2.0_f32,
                    palette::RED.lerp_to_gamma(egui::Color32::WHITE, 0.5),
                ),
                egui::StrokeKind::Inside,
            );
            painter.rect_stroke(
                rect,
                CornerRadius::same(4),
                Stroke::new(2.0_f32, palette::RED),
                egui::StrokeKind::Outside,
            );
        }

        if state.is_selected(idx) {
            painter.rect_stroke(
                rect,
                CornerRadius::same(4),
                Stroke::new(2.0_f32, palette::FG),
                egui::StrokeKind::Outside,
            );
        }

        // 縮めたときは文字がはみ出して読めないので、入る高さのときだけ書く
        if rect.width() > 26.0 && rect.height() >= NOTE_LABEL_SIZE + 2.0 {
            // 文字の位置まで実塗りが来ていれば背景色で抜き、
            // ゴーストの上に載るときは前景色にして読めるようにする
            let color = if filled_top <= rect.center().y {
                palette::BG
            } else {
                palette::FG
            };
            // 制御段では音名に意味が無いので、効く値のほうを出す
            let label = match kind {
                LaneKind::Cc(number) => format!("CC{number}={}", note.velocity.min(127)),
                // 平らなら1つ、坂なら矢印で向きが分かる形に
                LaneKind::Velocity if note.velocity == note.velocity_to => {
                    format!("V {}", note.velocity.min(127))
                }
                LaneKind::Velocity => {
                    format!("V {}→{}", note.velocity.min(127), note.velocity_to.min(127))
                }
                LaneKind::Note => note.name(),
            };
            painter.text(
                rect.left_center() + vec2(4.0, 0.0),
                Align2::LEFT_CENTER,
                label,
                FontId::proportional(NOTE_LABEL_SIZE),
                color,
            );
        }
    }

    // ---- 再生線 ----
    let playhead_x = to_x(pos_quarters as f32);
    painter.line_segment(
        [
            Pos2::new(playhead_x, origin.y),
            Pos2::new(playhead_x, origin.y + content_h),
        ],
        Stroke::new(2.0_f32, palette::RED),
    );

    // 再生中は再生線が見えるように追従スクロール
    if playing {
        let visible = ui.clip_rect();
        if playhead_x > visible.right() - 40.0 || playhead_x < visible.left() {
            ui.scroll_to_rect(
                Rect::from_min_size(Pos2::new(playhead_x, origin.y), vec2(ppq * 4.0, 1.0)),
                Some(egui::Align::Min),
            );
        }
    }

    // 停止したときは、戻った再生ヘッドを画面に入れる。
    //
    // 再生中は追従スクロールで画面が先へ流れているため、そのままだと戻った
    // 再生ヘッドが画面外に残る。**見えているときは動かさない**ので、
    // 少し再生して止めただけのときに画面が跳ねることはない。
    if let Some(quarters) = state.scroll_to_quarters.take() {
        let target_x = to_x(quarters as f32);
        let visible = ui.clip_rect();
        if target_x < visible.left() || target_x > visible.right() - 40.0 {
            ui.scroll_to_rect(
                Rect::from_min_size(Pos2::new(target_x, origin.y), vec2(ppq * 4.0, 1.0)),
                Some(egui::Align::Center),
            );
        }
    }

    // ---- 操作 ----
    let snap = state.snap_unit().max(0.03125);
    let left_resize = state.left_resize;
    let snap_floor = |q: f32| (q / snap).floor() * snap;

    // シーク位置の算出: 拍の間隔にスナップ (Alt 押下中は自由)。
    // 選択中ノートの頭が近ければそちらを優先する。
    let alt_held = ui.input(|i| i.modifiers.alt);
    let beat = state.editor.quarters_per_beat();
    let selected_start = state
        .selected
        .and_then(|i| state.editor.notes.get(i))
        .map(|n| n.start_tick.max(0.0));
    let seek_quarters = move |x: f32| -> f64 {
        let raw = ((x - origin.x) / ppq).max(0.0);
        seek_target(raw, beat, selected_start, alt_held)
    };

    // ---- 中クリックドラッグでスクロール / Ctrl 併用で横ズーム ----
    // egui のドラッグ判定はボタンを問わない (any_down) ため、
    // ここで掴んでいる間はノート編集のドラッグ処理を止める必要がある。
    let (middle_down, pointer_delta, zoom_modifier) = ui.input(|i| {
        (
            i.pointer.middle_down(),
            i.pointer.delta(),
            i.modifiers.command,
        )
    });
    if middle_down {
        if state.middle_drag.is_none() && response.contains_pointer() {
            // 用途は押した瞬間に決める (ドラッグ中に Ctrl を足しても変わらない)
            state.middle_drag = Some(if zoom_modifier {
                let anchor_quarters = ui
                    .input(|i| i.pointer.hover_pos())
                    .map_or(0.0, |pos| ((pos.x - origin.x) / ppq).max(0.0));
                MiddleDrag::ZoomHorizontally { anchor_quarters }
            } else {
                MiddleDrag::Pan
            });
            state.drag = None; // ノート編集とは排他
        }
    } else {
        state.middle_drag = None;
    }

    // 横ズームの量 (ピクセル)。実際の反映は描画のあと
    let mut zoom_pixels = 0.0;
    match state.middle_drag {
        Some(MiddleDrag::Pan) => {
            // 掴んだ位置がカーソルに追従するよう、アニメーションなしで即時スクロールする
            ui.scroll_with_delta_animation(pointer_delta, egui::style::ScrollAnimation::none());
            ui.output_mut(|o| o.cursor_icon = CursorIcon::Grabbing);
        }
        Some(MiddleDrag::ZoomHorizontally { .. }) => {
            zoom_pixels = pointer_delta.x;
            ui.output_mut(|o| o.cursor_icon = CursorIcon::ResizeHorizontal);
        }
        None => {}
    }

    // カーソル形状のフィードバック
    if state.drag.is_none() && state.middle_drag.is_none() {
        if let Some(hover) = response.hover_pos() {
            match hit_note(
                &state.editor.notes,
                &row_offsets,
                origin,
                hover,
                left_resize,
                ppq,
                row_h,
            ) {
                Some((_, Hit::ResizeRight | Hit::ResizeLeft)) => {
                    ui.output_mut(|o| o.cursor_icon = CursorIcon::ResizeHorizontal)
                }
                Some((_, Hit::Body)) => ui.output_mut(|o| o.cursor_icon = CursorIcon::Grab),
                None => {}
            }
        }
    }

    // ドラッグ開始。
    // 注意: interact_pointer_pos はドラッグ判定が成立した時点の位置なので、
    // ノート右端のような狭い領域のヒットテストには「押下位置」を使う。
    if response.drag_started() && state.middle_drag.is_none() {
        let press_pos = ui
            .input(|i| i.pointer.press_origin())
            .or_else(|| response.interact_pointer_pos());
        if let Some(pos) = press_pos {
            if pos.y < origin.y + RULER_H {
                state.drag = Some(DragState {
                    kind: DragKind::Seek,
                    targets: Vec::new(),
                    base_selection: Vec::new(),
                    origin: to_content_pos(origin, pos, ppq, row_h),
                });
            } else if let Some((idx, hit)) = hit_note(
                &state.editor.notes,
                &row_offsets,
                origin,
                pos,
                left_resize,
                ppq,
                row_h,
            ) {
                // 複数選択中のノートを掴んだら選択全体を操作する (選択は変えない)
                let bulk = state.selection.len() > 1 && state.is_selected(idx);
                let mut targets = if bulk {
                    // 掴んだノートを先頭に置く (移動量・スナップの基準になる)
                    let mut targets = vec![(idx, state.editor.notes[idx])];
                    targets.extend(
                        state
                            .selection_sorted()
                            .iter()
                            .filter(|i| **i != idx)
                            .map(|i| (*i, state.editor.notes[*i])),
                    );
                    targets
                } else {
                    state.select_single(idx);
                    vec![(idx, state.editor.notes[idx])]
                };

                // **Shift+ドラッグは複製してから動かす** (元は置いたまま)。
                // 端を掴んだときは通常どおり伸縮 (複製で伸縮を始めても押し違いにしか
                // ならない)。複製の追加と続く移動は Move の1グループ = アンドゥ1回で
                // 複製ごと戻る。動かさずに離すと元と重なったままになるが、
                // 重なり警告 (赤枠) が出るので気付ける。
                if matches!(hit, Hit::Body) && ui.input(|i| i.modifiers.shift) {
                    state.history.record(EditGroup::Move);
                    let base = state.editor.notes.len();
                    for (offset, (source, note)) in targets.iter_mut().enumerate() {
                        state.editor.notes.push(*note);
                        *source = base + offset; // 以降のドラッグは複製のほうを動かす
                    }
                    // 選択も複製側へ移す (動かしているのはそちらのため)
                    state.select_many(targets.iter().map(|(idx, _)| *idx).collect());
                    state.dirty = true;
                }

                state.drag = Some(DragState {
                    kind: match hit {
                        Hit::Body => DragKind::Move,
                        Hit::ResizeRight => DragKind::Resize { from_left: false },
                        Hit::ResizeLeft => DragKind::Resize { from_left: true },
                    },
                    targets,
                    base_selection: Vec::new(),
                    origin: to_content_pos(origin, pos, ppq, row_h),
                });
            } else {
                // 空白からのドラッグは範囲選択。Shift 押下なら既存の選択に足す。
                let base_selection = if ui.input(|i| i.modifiers.shift) {
                    state.selection_sorted()
                } else {
                    Vec::new()
                };
                state.drag = Some(DragState {
                    kind: DragKind::Marquee,
                    targets: Vec::new(),
                    base_selection,
                    origin: to_content_pos(origin, pos, ppq, row_h),
                });
            }
        }
    }

    // ドラッグ中
    if response.dragged() && state.middle_drag.is_none() {
        // 範囲選択の結果は借用が終わってから反映する
        let mut marquee_selection = None;

        if let (Some(drag), Some(pos)) = (&state.drag, response.interact_pointer_pos()) {
            // 掴んだ位置も現在位置も楽譜座標で扱う (自動スクロール中の追従のため)
            let content = to_content_pos(origin, pos, ppq, row_h);

            match drag.kind {
                DragKind::Seek => {
                    commands.push(EditorCommand::Seek {
                        quarters: seek_quarters(pos.x),
                    });
                }
                DragKind::Move => {
                    // 縦移動は「画面の行」で扱う。行はトラックをまたいで通しなので、
                    // 動かした先のトラックと段に変換して書き戻す。
                    let (dx, d_row) = move_delta(
                        &drag.targets,
                        &row_offsets,
                        &state.editor,
                        content.x - drag.origin.x,
                        (content.y - drag.origin.y).round() as i32,
                        snap,
                        display_rows,
                    );

                    for (idx, orig) in &drag.targets {
                        let mut note = *orig;
                        note.start_tick = (orig.start_tick + dx).max(0.0);
                        let row = (note_row(&row_offsets, orig) as i32 + d_row).max(0) as usize;
                        let (track, lane) = row_to_track_lane(&row_offsets, &state.editor, row);
                        note.track = track;
                        note.lane = lane;
                        state.editor.notes[*idx] = note;
                    }
                    if dx != 0.0 || d_row != 0 {
                        state.history.record(EditGroup::Move);
                    }
                }
                DragKind::Resize { from_left } => {
                    // 掴んだ端を、選択全体で同じ量だけ動かす。音価を揃え直さないので
                    // 尾揃え・頭揃えで作った相対関係がそのまま保たれる。
                    let dx =
                        resize_delta(&drag.targets, content.x - drag.origin.x, snap, from_left);

                    for (idx, orig) in &drag.targets {
                        let mut note = *orig;
                        if from_left {
                            // 頭を動かす (終端は固定)
                            note.start_tick = orig.start_tick + dx;
                            note.duration = orig.duration - dx;
                        } else {
                            // 終端を動かす (頭は固定)
                            note.duration = orig.duration + dx;
                        }
                        state.editor.notes[*idx] = note;
                    }
                    if dx != 0.0 {
                        state.history.record(EditGroup::Resize);
                    }
                }
                DragKind::Marquee => {
                    let rect =
                        Rect::from_two_pos(to_screen_pos(origin, drag.origin, ppq, row_h), pos);
                    let mut selection = drag.base_selection.clone();
                    for (idx, note) in state.editor.notes.iter().enumerate() {
                        if rect.intersects(note_rect(
                            origin,
                            note_row(&row_offsets, note),
                            note,
                            ppq,
                            row_h,
                        )) && !selection.contains(&idx)
                        {
                            selection.push(idx);
                        }
                    }
                    marquee_selection = Some(selection);

                    painter.rect_filled(
                        rect,
                        CornerRadius::same(2),
                        palette::BG_HOVER.gamma_multiply(0.35),
                    );
                    painter.rect_stroke(
                        rect,
                        CornerRadius::same(2),
                        Stroke::new(1.0_f32, palette::FG_DIM),
                        egui::StrokeKind::Inside,
                    );
                    // 選択枠はノートより後に描くので、次フレームで囲みを反映させる
                    ui.ctx().request_repaint();
                }
            }

            // ---- 画面端での自動スクロール ----
            // ノートを掴んだまま端に寄せると、その方向へスクロールし続ける。
            if !matches!(drag.kind, DragKind::Seek) {
                let vertical = matches!(drag.kind, DragKind::Move | DragKind::Marquee);
                let delta = edge_scroll_delta(ui.clip_rect(), pos, vertical);
                if delta != egui::Vec2::ZERO {
                    ui.scroll_with_delta_animation(delta, egui::style::ScrollAnimation::none());
                    // カーソルが静止していてもスクロールを続けるため再描画を要求する
                    ui.ctx().request_repaint();
                }
            }
        }

        if let Some(selection) = marquee_selection {
            state.select_many(selection);
        }
    }

    // ドラッグ終了
    if response.drag_stopped() {
        if let Some(drag) = state.drag.take() {
            if matches!(drag.kind, DragKind::Move | DragKind::Resize { .. }) {
                state.dirty = true;
            }
            state.history.end_group();
        }
    }

    // クリック (選択 / ルーラーシーク)
    if response.clicked() {
        if let Some(pos) = response.interact_pointer_pos() {
            if pos.y < origin.y + RULER_H {
                commands.push(EditorCommand::Seek {
                    quarters: seek_quarters(pos.x),
                });
            } else {
                match hit_note(
                    &state.editor.notes,
                    &row_offsets,
                    origin,
                    pos,
                    left_resize,
                    ppq,
                    row_h,
                ) {
                    // Shift+クリックは選択に追加 / 解除
                    Some((idx, _)) if ui.input(|i| i.modifiers.shift) => {
                        let mut selection = state.selection_sorted();
                        match selection.iter().position(|i| *i == idx) {
                            Some(at) => {
                                selection.remove(at);
                            }
                            None => selection.push(idx),
                        }
                        state.select_many(selection);
                    }
                    Some((idx, _)) => state.select_single(idx),
                    None => state.clear_selection(),
                }
            }
        }
    }

    // ダブルクリックで新規ノート (クリックした段に置く)
    if response.double_clicked() {
        if let Some(pos) = response.interact_pointer_pos() {
            if is_inside_lanes(origin, pos, content_h)
                && hit_note(
                    &state.editor.notes,
                    &row_offsets,
                    origin,
                    pos,
                    left_resize,
                    ppq,
                    row_h,
                )
                .is_none()
            {
                // 最後に選択・編集したノートの設定を引き継ぐ。
                // (ダブルクリックの1打目で選択が解除されるため state.selected は使えない)
                let defaults = state.last_note;
                let row = (((pos.y - origin.y - RULER_H) / row_h).floor() as i32)
                    .clamp(0, display_rows as i32 - 1) as usize;
                let (track, lane) = row_to_track_lane(&row_offsets, &state.editor, row);
                let start = snap_floor((pos.x - origin.x) / ppq).max(0.0);
                state.history.record(EditGroup::Once);
                // 連符モード中は連符1音分で置く (直前のノートの音価は引き継がない)
                let duration = if state.tuplet > 1 {
                    snap
                } else {
                    defaults.duration.max(snap)
                };
                state.editor.notes.push(Note {
                    start_tick: start,
                    duration,
                    semitone: defaults.semitone,
                    octave: defaults.octave,
                    velocity: defaults.velocity,
                    // 置いた直後は平ら。坂はツールバーで付ける
                    velocity_to: defaults.velocity,
                    track,
                    lane,
                });
                state.select_single(state.editor.notes.len() - 1);
                state.dirty = true;
            }
        }
    }

    // 右クリックで削除 (選択中のノートを指したら選択ごと消す)
    if response.secondary_clicked() {
        if let Some(pos) = response.interact_pointer_pos() {
            if let Some((idx, _)) = hit_note(
                &state.editor.notes,
                &row_offsets,
                origin,
                pos,
                left_resize,
                ppq,
                row_h,
            ) {
                state.history.record(EditGroup::Once);
                if state.is_selected(idx) {
                    state.delete_selection();
                } else {
                    state.editor.notes.remove(idx);
                    state.clear_selection();
                }
                state.dirty = true;
            }
        }
    }

    // 選択中ノートの設定を新規ノート用に控えておく。
    // 選択・ツールバーでの編集・音価ドラッグのすべてがここを通る。
    // 選択が外れたときは更新しない (直前の設定を保持したいため)。
    // 範囲選択やペーストで選ばれたノート (track_last_note = false) も対象外。
    if state.track_last_note {
        if let Some(note) = state.selected.and_then(|i| state.editor.notes.get(i)) {
            state.last_note = NoteDefaults::from(note);
        }
    }

    // ---- 縦ズームの反映 ----
    // 描画・当たり判定を1フレーム分の row_h で通したあとに変える。
    // ホイール中は毎フレーム描き直されるので、1フレームの遅れは見えない。
    if zoom_notches != 0.0 {
        // カーソルの下にある段。ズームしてもここが動かないようスクロールを合わせる
        let anchor_row = ui
            .input(|i| i.pointer.hover_pos())
            .map_or(0.0, |pos| to_content_pos(origin, pos, ppq, row_h).y);
        let grew = state.set_row_h(row_h * ROW_ZOOM_STEP.powf(zoom_notches));

        // ScrollArea は offset -= delta で動くので、伸びた分だけ負の値を渡す。
        // ルーラーの上 (anchor_row が負) で回したときは先頭を見ているので補正しない。
        if grew != 0.0 && anchor_row > 0.0 {
            ui.scroll_with_delta_animation(
                vec2(0.0, -anchor_row * grew),
                egui::style::ScrollAnimation::none(),
            );
        }
    }

    // ---- 横ズームの反映 ----
    // 縦ズームと同じく、1フレーム分の ppq で描き終えてから変える。
    //
    // **スクロールの補正は差分ではなく絶対位置で渡す。** 差分
    // (`scroll_with_delta`) だと、ppq の変更が効くフレームと補正が効くフレームが
    // 1つずれる。ドラッグ中はフレームごとの移動量が細かく揺れるので、そのずれが
    // そのまま画面の左右の揺れになって見える。掴んだ位置が画面上のどこに居るべきかは
    // 毎フレーム一意に決まるので、そこから**その場のオフセットを直接求める**。
    // こうすると ppq と横位置が同じフレームで揃い、揺れようがない。
    if let Some(MiddleDrag::ZoomHorizontally { anchor_quarters }) = state.middle_drag {
        if zoom_pixels != 0.0 {
            // 掴んだ位置が今いる画面上の x。ズームしてもここから動かさない
            let anchor_x = origin.x + anchor_quarters * ppq;
            // 左へ動かすと拡大したいので、x の変化量の符号を反転して指数に使う
            state.set_ppq(ppq * PPQ_ZOOM_PER_PIXEL.powf(-zoom_pixels));
            state.pending_scroll_x = Some(horizontal_offset_for_anchor(
                ui.clip_rect().left(),
                anchor_x,
                anchor_quarters,
                state.ppq,
            ));
        }
    }
}
