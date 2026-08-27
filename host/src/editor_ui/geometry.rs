//! 座標変換・当たり判定・移動量の算出。
//!
//! **egui の描画には触らない純関数だけを置く。** グリッドの見た目を変えずに
//! 単体で試せるようにしてあり、エディタのテストの過半はここに掛かっている。

use super::metrics::RULER_H;
use crate::sequencer::{MidiEditor, Note};
use eframe::egui::{self, vec2, Pos2, Rect};

/// ノート右端のリサイズ判定幅
const EDGE_W: f32 = 10.0;

/// 画面座標を楽譜座標 (x = 四分音符単位, y = 段) に変換する。
/// `ppq` と `row_h` はズームで変わるので呼び出し側から渡す。
pub(super) fn to_content_pos(grid_origin: Pos2, screen: Pos2, ppq: f32, row_h: f32) -> Pos2 {
    Pos2::new(
        (screen.x - grid_origin.x) / ppq,
        (screen.y - grid_origin.y - RULER_H) / row_h,
    )
}

/// 楽譜座標を画面座標に戻す (to_content_pos の逆変換)
pub(super) fn to_screen_pos(grid_origin: Pos2, content: Pos2, ppq: f32, row_h: f32) -> Pos2 {
    Pos2::new(
        grid_origin.x + content.x * ppq,
        grid_origin.y + RULER_H + content.y * row_h,
    )
}

/// 掴んだ位置 (`anchor_quarters`) を画面上の `anchor_x` に留めるための横スクロール量。
///
/// `ScrollArea` のオフセットは「内容の左端からビューポート左端までの距離」なので、
/// **内容上での位置から、画面上でのビューポート左端からのずれを引けば**求まる。
/// 左端より手前へは送れないので 0 で止める。
pub(super) fn horizontal_offset_for_anchor(
    viewport_left: f32,
    anchor_x: f32,
    anchor_quarters: f32,
    ppq: f32,
) -> f32 {
    (anchor_quarters * ppq - (anchor_x - viewport_left)).max(0.0)
}

/// その位置が段の並んでいる範囲に入っているか。
///
/// ルーラーの上と、段の下に広げた当たり判定の余白を除く。
/// 余白は範囲選択を始めるためだけの場所なので、ここにノートを作らせない
/// (行番号は最終段に丸められるため、放っておくと離れた場所の操作で
/// 最終段にノートができてしまう)。
pub(super) fn is_inside_lanes(origin: Pos2, pos: Pos2, content_h: f32) -> bool {
    pos.y >= origin.y + RULER_H && pos.y < origin.y + content_h
}

/// 画面端の自動スクロール量を求める。
///
/// `visible` の内側 `EDGE_SCROLL_MARGIN` に `pointer` が入ると、端に近いほど速く
/// その方向へスクロールする量を返す。`vertical` が false なら横方向のみ。
///
/// 返す値は `scroll_with_delta` に渡す形式で、符号は「内容が動く向き」。
/// 例えば右端に寄せたときは先の内容を見せたいので x は負になる。
pub(super) fn edge_scroll_delta(visible: Rect, pointer: Pos2, vertical: bool) -> egui::Vec2 {
    /// 端から何ピクセルを反応領域とするか
    const EDGE_SCROLL_MARGIN: f32 = 48.0;
    /// 反応領域の最も端での速度 (ピクセル/フレーム)
    const EDGE_SCROLL_MAX_SPEED: f32 = 14.0;

    // 端からの食い込み具合 (0.0..=1.0) を速度に変換する
    let speed = |depth: f32| (depth / EDGE_SCROLL_MARGIN).clamp(0.0, 1.0) * EDGE_SCROLL_MAX_SPEED;

    let mut delta = egui::Vec2::ZERO;

    if pointer.x > visible.right() - EDGE_SCROLL_MARGIN {
        delta.x = -speed(pointer.x - (visible.right() - EDGE_SCROLL_MARGIN));
    } else if pointer.x < visible.left() + EDGE_SCROLL_MARGIN {
        delta.x = speed((visible.left() + EDGE_SCROLL_MARGIN) - pointer.x);
    }

    if vertical {
        if pointer.y > visible.bottom() - EDGE_SCROLL_MARGIN {
            delta.y = -speed(pointer.y - (visible.bottom() - EDGE_SCROLL_MARGIN));
        } else if pointer.y < visible.top() + EDGE_SCROLL_MARGIN {
            delta.y = speed((visible.top() + EDGE_SCROLL_MARGIN) - pointer.y);
        }
    }

    delta
}

/// 一括移動の移動量 (四分音符, 段) を求める。
///
/// 移動量は掴んだノート (targets の先頭) を基準に1度だけスナップし、全員に同じだけ
/// 適用する。個別にスナップすると選択内の相対位置が崩れるため。
/// 選択全体が範囲 (開始位置 >= 0、段 0..rows) に収まるよう移動量を抑える。
pub(super) fn move_delta(
    targets: &[(usize, Note)],
    row_offsets: &[usize],
    editor: &MidiEditor,
    dx_raw: f32,
    d_row_raw: i32,
    snap: f32,
    rows: usize,
) -> (f32, i32) {
    let Some((_, grabbed)) = targets.first() else {
        return (0.0, 0);
    };
    let snapped = ((grabbed.start_tick + dx_raw) / snap).round() * snap;
    let dx = snapped - grabbed.start_tick;

    let min_start = targets
        .iter()
        .map(|(_, n)| n.start_tick)
        .fold(f32::INFINITY, f32::min);
    let min_row = targets
        .iter()
        .map(|(_, n)| note_row(row_offsets, n))
        .min()
        .unwrap_or(0) as i32;
    let max_row = targets
        .iter()
        .map(|(_, n)| note_row(row_offsets, n))
        .max()
        .unwrap_or(0) as i32;

    // まず画面の範囲へ収め、そのうえで段の種別をまたぐぶんを削る。
    // 0 へ向かって1段ずつ戻すので、**行けるところまでは行く**
    // (CC 段が下にあるトラックでも、その手前までは動かせる)。
    let mut d_row = d_row_raw.clamp(-min_row, rows as i32 - 1 - max_row);
    while d_row != 0 && !keeps_lane_kind(targets, row_offsets, editor, d_row) {
        d_row -= d_row.signum();
    }

    (dx.max(-min_start), d_row)
}

/// その移動量で、全員が自分と同じ種別の段に着地するか。
///
/// **CC 段のノートは段から動かさない。** CC は段に割り当てた番号で意味が決まるので、
/// 別の段へ移すと黙って別の CC になってしまう。逆に、音符が CC 段へ入ると
/// 音として鳴らずに CC を送ってしまう。どちらも見た目では気付きにくいので、
/// そもそも動かせないようにする (横方向は自由)。
fn keeps_lane_kind(
    targets: &[(usize, Note)],
    row_offsets: &[usize],
    editor: &MidiEditor,
    d_row: i32,
) -> bool {
    targets.iter().all(|(_, note)| {
        let row = (note_row(row_offsets, note) as i32 + d_row).max(0) as usize;
        let (track, lane) = row_to_track_lane(row_offsets, editor, row);
        match editor.lane_cc(note.track, note.lane) {
            // CC 段のノートは、その段のまま以外を認めない
            Some(_) => track == note.track && lane == note.lane,
            // 音符は CC 段へ入れない
            None => editor.lane_cc(track, lane).is_none(),
        }
    })
}

/// 再生ヘッドの移動先 (四分音符単位) を求める。
///
/// 拍の間隔にスナップする。四分音符固定にすると 3/8 拍子のように1小節が
/// 四分音符の整数倍にならない拍子で小節線に乗らなくなるため、拍を単位にする
/// (拍は必ず小節を割り切るので、小節線は常にスナップ先に含まれる)。
/// 選択中ノートの頭がスナップ先より近ければ、そちらを優先する。
/// `free` が true なら (Alt 押下中) スナップしない。
pub(super) fn seek_target(
    raw_quarters: f32,
    beat: f32,
    selected_start: Option<f32>,
    free: bool,
) -> f64 {
    let raw = raw_quarters.max(0.0);
    if free {
        return raw as f64;
    }
    let beat = if beat > 0.0 { beat } else { 1.0 };
    let snapped = ((raw / beat).round() * beat).max(0.0);
    match selected_start {
        Some(start) if (raw - start).abs() < (raw - snapped).abs() => start as f64,
        _ => snapped as f64,
    }
}

/// 一括リサイズで、掴んだ端を動かす量 (四分音符) を求める。
///
/// 掴んだノート (targets の先頭) の端がグリッドに乗るようスナップし、その差分を
/// 選択全体に同じだけ適用する。音価を揃え直さないので、尾揃え・頭揃えで作った
/// 相対関係がドラッグ後も保たれる。
/// どのノートも音価が snap を下回らず、頭が 0 より前に出ないよう量を抑える。
pub(super) fn resize_delta(
    targets: &[(usize, Note)],
    dx_raw: f32,
    snap: f32,
    from_left: bool,
) -> f32 {
    let Some((_, grabbed)) = targets.first() else {
        return 0.0;
    };
    let snap_round = |q: f32| (q / snap).round() * snap;

    let min_duration = targets
        .iter()
        .map(|(_, n)| n.duration)
        .fold(f32::INFINITY, f32::min);

    if from_left {
        // 頭を動かす (終端は固定): 音価は dx だけ縮む
        let dx = snap_round(grabbed.start_tick + dx_raw) - grabbed.start_tick;
        let min_start = targets
            .iter()
            .map(|(_, n)| n.start_tick)
            .fold(f32::INFINITY, f32::min);
        dx.max(-min_start).min(min_duration - snap)
    } else {
        // 終端を動かす (頭は固定): 音価は dx だけ伸びる
        let end = grabbed.end_tick();
        let dx = snap_round(end + dx_raw) - end;
        dx.max(snap - min_duration)
    }
}

/// 各トラックの段が画面の何行目から始まるかを求める。
/// 画面の行は「トラック0の段0..n, トラック1の段0..m, ...」と上から並ぶ。
pub(super) fn track_row_offsets(editor: &MidiEditor) -> Vec<usize> {
    let mut offsets = Vec::with_capacity(editor.tracks.len());
    let mut row = 0;
    for info in &editor.tracks {
        offsets.push(row);
        row += info.lanes.max(1);
    }
    offsets
}

/// ノートが画面の何行目に描かれるか
pub(super) fn note_row(offsets: &[usize], note: &Note) -> usize {
    offsets.get(note.track).copied().unwrap_or(0) + note.lane
}

/// 画面の行番号を (トラック, 段) に戻す
pub(super) fn row_to_track_lane(
    offsets: &[usize],
    editor: &MidiEditor,
    row: usize,
) -> (usize, usize) {
    for track in (0..offsets.len()).rev() {
        if row >= offsets[track] {
            let lane = (row - offsets[track]).min(editor.lanes(track).saturating_sub(1));
            return (track, lane);
        }
    }
    (0, 0)
}

/// ノート矩形のうち、ベロシティ分を実塗りする部分 (下側) を求める。
/// ベロシティ127で矩形全体、小さいほど下に薄く残る。
pub(super) fn velocity_fill_rect(rect: Rect, velocity: u8) -> Rect {
    let level = velocity.min(127) as f32 / 127.0;
    let height = rect.height() * level;
    Rect::from_min_max(
        Pos2::new(rect.left(), rect.bottom() - height),
        Pos2::new(rect.right(), rect.bottom()),
    )
}

/// ヴェロシティ段のブロックの塗り (坂)。左が開始値、右が終了値の高さになる。
///
/// **矩形ではなく四角形で返す。** クレシェンドかデクレシェンドかを、
/// 数字を読まずに形で分かるようにするため。
pub(super) fn velocity_ramp_points(rect: Rect, from: u8, to: u8) -> Vec<Pos2> {
    let top_of = |value: u8| {
        let level = value.min(127) as f32 / 127.0;
        rect.bottom() - rect.height() * level
    };
    vec![
        Pos2::new(rect.left(), top_of(from)),
        Pos2::new(rect.right(), top_of(to)),
        Pos2::new(rect.right(), rect.bottom()),
        Pos2::new(rect.left(), rect.bottom()),
    ]
}

/// ノートの表示矩形を計算する。`row` は画面の行番号 (トラックの段を通しで数えた値)。
///
/// 段の上下に空ける余白は高さの 1/12 (既定の 24px で従来どおり上下2px)。
/// 固定値にすると、縮めたときに矩形が潰れ、広げたときに隙間が目立たなくなる。
pub(super) fn note_rect(origin: Pos2, row: usize, note: &Note, ppq: f32, row_h: f32) -> Rect {
    let margin = row_h / 12.0;
    Rect::from_min_size(
        Pos2::new(
            origin.x + note.start_tick * ppq,
            origin.y + RULER_H + row as f32 * row_h + margin,
        ),
        vec2((note.duration * ppq - 1.0).max(4.0), row_h - margin * 2.0),
    )
}

/// ノートのどこを掴んだか
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Hit {
    /// 本体 (移動)
    Body,
    /// 右端: 頭を固定して終端を動かす
    ResizeRight,
    /// 左端: 終端を固定して頭を動かす (左端音価モードが ON のときだけ)
    ResizeLeft,
}

/// pos にあるノートと、掴んだ位置の種別を返す。
///
/// 右端は常にリサイズ領域。`left_resize` が true のときは左端も加わる。
/// 短いノートでは左右の判定領域が重なってしまうため、両方有効なときは
/// ノートの中央で分け合う (右端を優先して左端が掴めなくなるのを防ぐ)。
pub(super) fn hit_note(
    notes: &[Note],
    offsets: &[usize],
    origin: Pos2,
    pos: Pos2,
    left_resize: bool,
    ppq: f32,
    row_h: f32,
) -> Option<(usize, Hit)> {
    for (idx, note) in notes.iter().enumerate().rev() {
        let rect = note_rect(origin, note_row(offsets, note), note, ppq, row_h);
        let split = rect.center().x;

        let right_from = if left_resize {
            (rect.right() - EDGE_W).max(split)
        } else {
            rect.right() - EDGE_W
        };
        let right = Rect::from_min_max(
            Pos2::new(right_from, rect.top()),
            Pos2::new(rect.right() + 2.0, rect.bottom()),
        );
        if right.contains(pos) {
            return Some((idx, Hit::ResizeRight));
        }

        if left_resize {
            let left = Rect::from_min_max(
                Pos2::new(rect.left() - 2.0, rect.top()),
                Pos2::new((rect.left() + EDGE_W).min(split), rect.bottom()),
            );
            if left.contains(pos) {
                return Some((idx, Hit::ResizeLeft));
            }
        }

        if rect.contains(pos) {
            return Some((idx, Hit::Body));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::editor_ui::metrics::{MIN_PPQ, MIN_ROW_H, PPQ, ROW_H};

    fn viewport() -> Rect {
        Rect::from_min_max(Pos2::new(0.0, 0.0), Pos2::new(1000.0, 400.0))
    }

    fn placed(start: f32, lane: usize) -> Note {
        Note {
            start_tick: start,
            duration: 0.5,
            semitone: 0,
            octave: 4,
            velocity: 100,
            velocity_to: 100,
            track: 0,
            lane,
        }
    }

    /// テスト用: トラック1本 (段は十分にある) の行オフセット
    fn single_track_rows() -> Vec<usize> {
        vec![0]
    }

    /// 段が16ある1トラックだけのエディタ (全部が音符段)
    fn single_track_editor() -> MidiEditor {
        let mut editor = MidiEditor::default();
        editor.tracks[0].lanes = 16;
        editor
    }

    /// 段が16の1トラックで、下2段 (14,15) を CC 段にしたエディタ
    fn editor_with_cc_lanes() -> MidiEditor {
        let mut editor = single_track_editor();
        editor.tracks[0].set_lane_cc(14, Some(64));
        editor.tracks[0].set_lane_cc(15, Some(1));
        editor
    }

    /// 一括移動: 掴んだノートを基準にスナップし、全員が同じだけ動くこと
    #[test]
    fn bulk_move_keeps_relative_positions() {
        // 0.1 だけずれた位置にあるノートも、掴んだノートと同じ移動量で動く
        let targets = vec![(0, placed(1.0, 2)), (1, placed(1.1, 3))];
        let (dx, d_lane) = move_delta(
            &targets,
            &single_track_rows(),
            &single_track_editor(),
            0.6,
            1,
            0.25,
            16,
        );
        assert_eq!(dx, 0.5, "掴んだノートが 1.5 に来るようスナップされること");
        assert_eq!(d_lane, 1);
    }

    /// 音符は CC 段へ入れないこと。**手前までは動ける。**
    #[test]
    fn notes_stop_above_the_cc_lanes() {
        let editor = editor_with_cc_lanes();
        let targets = vec![(0, placed(1.0, 10))];

        // 下へ大きく動かしても、CC 段の1つ上 (13段目) で止まる
        let (_, d_lane) = move_delta(&targets, &single_track_rows(), &editor, 0.0, 5, 0.25, 16);
        assert_eq!(d_lane, 3, "13段目まで (10 + 3) しか下がらないこと");

        // 上方向は普通に動ける
        let (_, d_lane) = move_delta(&targets, &single_track_rows(), &editor, 0.0, -4, 0.25, 16);
        assert_eq!(d_lane, -4);
    }

    /// CC 段のノートは、その段から動かせないこと (横だけ動く)
    #[test]
    fn cc_notes_cannot_leave_their_lane() {
        let editor = editor_with_cc_lanes();
        let targets = vec![(0, placed(1.0, 14))];

        for requested in [-1, 1, -5, 5] {
            let (dx, d_lane) = move_delta(
                &targets,
                &single_track_rows(),
                &editor,
                0.5,
                requested,
                0.25,
                16,
            );
            assert_eq!(d_lane, 0, "段を移動できないこと (要求 {requested})");
            assert_eq!(dx, 0.5, "横方向は動かせること");
        }
    }

    /// 一括移動: 選択の端が範囲外に出ないよう移動量が抑えられること
    #[test]
    fn bulk_move_clamps_to_bounds() {
        let targets = vec![(0, placed(1.0, 1)), (1, placed(0.5, 15))];
        // 左へ大きく動かしても、先頭のノートが 0 未満にならない分しか動かない
        let (dx, _) = move_delta(
            &targets,
            &single_track_rows(),
            &single_track_editor(),
            -5.0,
            0,
            0.25,
            16,
        );
        assert_eq!(dx, -0.5);
        // 下へ動かしても、最下段のノートが 15 段目に留まる
        let (_, d_lane) = move_delta(
            &targets,
            &single_track_rows(),
            &single_track_editor(),
            0.0,
            5,
            0.25,
            16,
        );
        assert_eq!(d_lane, 0);
        // 上へ動かすときは最上段のノートが 0 段目で止まる
        let (_, d_lane) = move_delta(
            &targets,
            &single_track_rows(),
            &single_track_editor(),
            0.0,
            -5,
            0.25,
            16,
        );
        assert_eq!(d_lane, -1);
    }

    /// ノートを作れるのは段の中だけで、範囲選択用に広げた余白では作らないこと。
    /// (行番号は最終段に丸められるので、余白を許すと最終段にノートが湧く)
    #[test]
    fn notes_are_created_only_inside_the_lanes() {
        let origin = Pos2::ZERO;
        let content_h = RULER_H + ROW_H * 2.0; // 2段

        assert!(
            !is_inside_lanes(origin, Pos2::new(0.0, RULER_H - 1.0), content_h),
            "ルーラーの上では作らないこと"
        );
        assert!(is_inside_lanes(
            origin,
            Pos2::new(0.0, RULER_H + 1.0),
            content_h
        ));
        assert!(
            is_inside_lanes(origin, Pos2::new(0.0, content_h - 1.0), content_h),
            "最終段の下端までは作れること"
        );
        assert!(
            !is_inside_lanes(origin, Pos2::new(0.0, content_h + 1.0), content_h),
            "段の下の余白では作らないこと"
        );

        // 原点がずれても同じ判定になること (スクロール中でも成り立つ)
        let moved = Pos2::new(30.0, 100.0);
        assert!(is_inside_lanes(
            moved,
            moved + vec2(0.0, RULER_H + 1.0),
            content_h
        ));
        assert!(!is_inside_lanes(
            moved,
            moved + vec2(0.0, content_h + 1.0),
            content_h
        ));
    }

    /// 横ズームの補正: 掴んだ位置が画面上で動かないこと。
    ///
    /// ここが狂うとズーム中に画面が左右に揺れる。**倍率を変えても、掴んだ位置の
    /// 画面座標は変わらない**ことを、拡大・縮小の両方で確かめる。
    #[test]
    fn horizontal_zoom_keeps_the_anchor_on_screen() {
        let viewport_left = 200.0;
        let anchor_quarters = 12.0;
        let ppq = 40.0;

        // 掴んだ位置がビューポート左端から 150px のところに見えている状態
        let anchor_x = viewport_left + 150.0;

        for new_ppq in [ppq * 2.0, ppq * 0.5, ppq * 8.0, ppq] {
            let offset =
                horizontal_offset_for_anchor(viewport_left, anchor_x, anchor_quarters, new_ppq);
            // オフセットから逆算した画面上の位置が元と一致すること
            let origin_x = viewport_left - offset;
            let shown_x = origin_x + anchor_quarters * new_ppq;
            assert!(
                (shown_x - anchor_x).abs() < 0.001,
                "ppq={new_ppq} で掴んだ位置が動いた ({shown_x} != {anchor_x})"
            );
        }
    }

    /// 左端より手前へは送らないこと (ScrollArea が受け付けないため)
    #[test]
    fn horizontal_zoom_offset_never_goes_negative() {
        // 内容上の位置より画面上のずれのほうが大きい = 左端が見えている状態
        let offset = horizontal_offset_for_anchor(0.0, 500.0, 1.0, 10.0);
        assert_eq!(offset, 0.0);
    }

    /// ノート矩形が段の高さに追従し、上下の余白も一緒に伸び縮みすること
    #[test]
    fn note_rect_follows_the_row_height() {
        let origin = Pos2::new(0.0, 0.0);
        let note = placed(0.0, 1); // 2段目

        // 既定の 24px では従来どおり上下2px の余白
        let normal = note_rect(origin, 1, &note, PPQ, ROW_H);
        assert_eq!(normal.top(), RULER_H + ROW_H + 2.0);
        assert_eq!(normal.height(), ROW_H - 4.0);

        // 倍に広げれば位置も高さも倍 (余白も倍なので比率は変わらない)
        let zoomed = note_rect(origin, 1, &note, PPQ, ROW_H * 2.0);
        assert_eq!(zoomed.top(), RULER_H + ROW_H * 2.0 + 4.0);
        assert_eq!(zoomed.height(), (ROW_H - 4.0) * 2.0);
        assert_eq!(
            zoomed.width(),
            normal.width(),
            "縦ズームでは横幅が変わらないこと"
        );

        // 下限まで縮めても矩形は残ること (固定余白だと潰れてしまう)
        assert!(note_rect(origin, 0, &note, PPQ, MIN_ROW_H).height() > 0.0);
    }

    /// ノート矩形が横ズームに追従し、縦は変わらないこと
    #[test]
    fn note_rect_follows_the_horizontal_zoom() {
        let origin = Pos2::new(0.0, 0.0);
        let note = placed(2.0, 0); // 2拍目から

        let normal = note_rect(origin, 0, &note, PPQ, ROW_H);
        let zoomed = note_rect(origin, 0, &note, PPQ * 2.0, ROW_H);

        assert_eq!(zoomed.left(), normal.left() * 2.0, "位置も倍になること");
        // 幅は「音価 × ppq − 1px」なので、倍にすると 1px 分だけ端数が出る
        assert_eq!(zoomed.width(), normal.width() * 2.0 + 1.0);
        assert_eq!(zoomed.top(), normal.top(), "縦位置は変わらないこと");
        assert_eq!(zoomed.height(), normal.height(), "高さは変わらないこと");

        // 下限まで縮めても矩形は残ること
        assert!(note_rect(origin, 0, &note, MIN_PPQ, ROW_H).width() > 0.0);
    }

    /// 当たり判定も段の高さに追従すること (広げた段の中央で掴めること)
    #[test]
    fn hit_note_follows_the_row_height() {
        let origin = Pos2::new(0.0, 0.0);
        let notes = vec![placed(0.0, 1)]; // 2段目
        let rows = single_track_rows();
        let tall = ROW_H * 3.0;

        // 広げた2段目の中央
        let inside = Pos2::new(4.0, RULER_H + tall * 1.5);
        assert_eq!(
            hit_note(&notes, &rows, origin, inside, false, PPQ, tall),
            Some((0, Hit::Body))
        );
        // 同じ点も、既定の高さでは2段目の外なので当たらない
        assert_eq!(
            hit_note(&notes, &rows, origin, inside, false, PPQ, ROW_H),
            None
        );
    }

    /// 左端音価が ON でも、右端は従来どおりリサイズ領域であること。
    /// 短いノートで左右が食い合わないよう、中央で分かれること。
    #[test]
    fn hit_note_edges_split_at_center() {
        let origin = Pos2::new(0.0, 0.0);
        // 0.25拍 = 20px 幅のノート (既定の音価)。矩形は x 0..19
        let notes = vec![Note {
            duration: 0.25,
            ..placed(0.0, 0)
        }];
        let y = RULER_H + ROW_H * 0.5;
        let left_pos = Pos2::new(2.0, y);
        let right_pos = Pos2::new(17.0, y);

        // OFF: 左端は本体扱い (移動)、右端はリサイズ
        assert_eq!(
            hit_note(
                &notes,
                &single_track_rows(),
                origin,
                left_pos,
                false,
                PPQ,
                ROW_H
            ),
            Some((0, Hit::Body))
        );
        assert_eq!(
            hit_note(
                &notes,
                &single_track_rows(),
                origin,
                right_pos,
                false,
                PPQ,
                ROW_H
            ),
            Some((0, Hit::ResizeRight))
        );

        // ON: 左端が左リサイズになり、右端は引き続き右リサイズ
        assert_eq!(
            hit_note(
                &notes,
                &single_track_rows(),
                origin,
                left_pos,
                true,
                PPQ,
                ROW_H
            ),
            Some((0, Hit::ResizeLeft))
        );
        assert_eq!(
            hit_note(
                &notes,
                &single_track_rows(),
                origin,
                right_pos,
                true,
                PPQ,
                ROW_H
            ),
            Some((0, Hit::ResizeRight))
        );
    }

    /// ベロシティの塗りは下端を基準に、127で矩形全体を埋めること
    #[test]
    fn velocity_fill_grows_from_the_bottom() {
        let rect = Rect::from_min_max(Pos2::new(10.0, 100.0), Pos2::new(30.0, 120.0));

        let full = velocity_fill_rect(rect, 127);
        assert_eq!(full, rect, "最大ベロシティでは全体が実塗り");

        let half = velocity_fill_rect(rect, 64);
        assert_eq!(half.bottom(), rect.bottom(), "下端は矩形と揃うこと");
        assert!(
            (half.height() - rect.height() * 64.0 / 127.0).abs() < 1e-4,
            "高さがベロシティに比例すること: {}",
            half.height()
        );

        // 最小ベロシティでも幅は保たれる (ノートの存在が消えない)
        let min = velocity_fill_rect(rect, 1);
        assert_eq!(min.width(), rect.width());
        assert!(min.height() > 0.0);
    }

    /// シークは拍にスナップすること。
    /// 3/8 拍子 (1小節 = 1.5拍分の四分音符) でも小節線に乗ること。
    #[test]
    fn seek_snaps_to_beats_so_bar_lines_are_reachable() {
        // 4/4: 拍 = 四分音符
        assert_eq!(seek_target(2.2, 1.0, None, false), 2.0);
        assert_eq!(seek_target(2.6, 1.0, None, false), 3.0);

        // 3/8: 拍 = 8分音符 (0.5)。小節線は 1.5 / 3.0 に来る
        assert_eq!(seek_target(1.4, 0.5, None, false), 1.5, "小節線に乗ること");
        assert_eq!(seek_target(2.9, 0.5, None, false), 3.0);

        // Alt 押下中はスナップしない
        assert_eq!(seek_target(1.4, 0.5, None, true), 1.4f32 as f64);

        // 選択中ノートの頭が近ければそちらを優先する
        assert_eq!(seek_target(1.42, 0.5, Some(1.4), false), 1.4f32 as f64);
        assert_eq!(
            seek_target(1.49, 0.5, Some(1.2), false),
            1.5,
            "遠ければ拍が優先"
        );

        // 負にならないこと
        assert_eq!(seek_target(-3.0, 1.0, None, false), 0.0);
    }

    /// 一括リサイズは全員の端を同じ量だけ動かし、音価の相対関係を保つこと。
    /// (尾揃えの結果が右端ドラッグで壊れないことの担保)
    #[test]
    fn resize_moves_every_edge_by_the_same_amount() {
        // 尾揃え後を想定: 開始が違い、終端が 3.5 に揃っている
        let targets = vec![
            (
                0,
                Note {
                    duration: 1.5,
                    ..placed(2.0, 0)
                },
            ),
            (
                1,
                Note {
                    duration: 3.0,
                    ..placed(0.5, 1)
                },
            ),
        ];

        // 掴んだノート (先頭) の終端 3.5 を 4.0 へ → 全員 +0.5
        let dx = resize_delta(&targets, 0.5, 0.25, false);
        assert_eq!(dx, 0.5);
        let ends: Vec<f32> = targets.iter().map(|(_, n)| n.end_tick() + dx).collect();
        assert_eq!(ends, vec![4.0, 4.0], "終端が揃ったままであること");

        // 縮めすぎても、いちばん短いノートが snap を下回らない
        let dx = resize_delta(&targets, -10.0, 0.25, false);
        assert_eq!(dx, 0.25 - 1.5);
    }

    /// 左端ドラッグは頭を同じ量だけ動かし、終端は動かさないこと
    #[test]
    fn left_resize_moves_heads_and_keeps_ends() {
        let targets = vec![
            (
                0,
                Note {
                    duration: 1.0,
                    ..placed(1.0, 0)
                },
            ),
            (
                1,
                Note {
                    duration: 2.0,
                    ..placed(0.5, 1)
                },
            ),
        ];

        // 掴んだノートの頭 1.0 を 0.5 へ → 全員 -0.5 (音価は +0.5、終端は不変)
        let dx = resize_delta(&targets, -0.5, 0.25, true);
        assert_eq!(dx, -0.5);
        let ends: Vec<f32> = targets
            .iter()
            .map(|(_, n)| (n.start_tick + dx) + (n.duration - dx))
            .collect();
        assert_eq!(ends, vec![2.0, 2.5], "終端は動かないこと");

        // 左に振り切っても、いちばん頭が早いノートが 0 より前に出ない
        let dx = resize_delta(&targets, -5.0, 0.25, true);
        assert_eq!(dx, -0.5);

        // 右に振り切っても、いちばん短いノートが snap を下回らない
        let dx = resize_delta(&targets, 10.0, 0.25, true);
        assert_eq!(dx, 1.0 - 0.25);
    }

    #[test]
    fn no_edge_scroll_in_the_middle() {
        let delta = edge_scroll_delta(viewport(), Pos2::new(500.0, 200.0), true);
        assert_eq!(delta, egui::Vec2::ZERO);
    }

    /// 右端では先の内容を見せたいので x は負、左端では正になる
    #[test]
    fn edge_scroll_direction() {
        let right = edge_scroll_delta(viewport(), Pos2::new(995.0, 200.0), true);
        assert!(right.x < 0.0, "右端では x が負になること: {right:?}");
        assert_eq!(right.y, 0.0);

        let left = edge_scroll_delta(viewport(), Pos2::new(5.0, 200.0), true);
        assert!(left.x > 0.0, "左端では x が正になること: {left:?}");

        let bottom = edge_scroll_delta(viewport(), Pos2::new(500.0, 398.0), true);
        assert!(bottom.y < 0.0, "下端では y が負になること: {bottom:?}");
    }

    /// 端に近いほど速くなること
    #[test]
    fn edge_scroll_accelerates_toward_edge() {
        let shallow = edge_scroll_delta(viewport(), Pos2::new(960.0, 200.0), true);
        let deep = edge_scroll_delta(viewport(), Pos2::new(999.0, 200.0), true);
        assert!(
            deep.x < shallow.x,
            "端に近いほど速いこと: 浅 {shallow:?} / 深 {deep:?}"
        );
    }

    /// 音価変更 (Resize) では縦スクロールしない
    #[test]
    fn edge_scroll_horizontal_only_when_requested() {
        let delta = edge_scroll_delta(viewport(), Pos2::new(500.0, 398.0), false);
        assert_eq!(delta.y, 0.0);
    }
}
