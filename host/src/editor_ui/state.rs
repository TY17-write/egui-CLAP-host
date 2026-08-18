//! エディタの UI 状態と、選択に対する編集操作。
//!
//! 描画を伴わない編集 (選択・移調・ベロシティ・コピー/ペースト) はここに集めてある。
//! 画面から呼ぶだけで試せるので、テストもここに置く。

use super::history::{EditGroup, History};
use super::metrics::{MAX_OCTAVE, MAX_PPQ, MAX_ROW_H, MIN_OCTAVE, MIN_PPQ, MIN_ROW_H, PPQ, ROW_H};
use crate::sequencer::{MidiEditor, Note};
use eframe::egui::Pos2;

/// 新規ノートに引き継ぐ設定。
/// 選択が外れても保持する必要があるため (ダブルクリックの1打目で選択が解除される)、
/// 選択インデックスとは別に持つ。
#[derive(Clone, Copy)]
pub struct NoteDefaults {
    pub semitone: i32,
    pub octave: i32,
    pub duration: f32,
    pub velocity: u8,
}

impl Default for NoteDefaults {
    fn default() -> Self {
        Self {
            semitone: 0,
            octave: 4,
            duration: 0.25,
            velocity: 100,
        }
    }
}

impl From<&Note> for NoteDefaults {
    fn from(note: &Note) -> Self {
        Self {
            semitone: note.semitone,
            octave: note.octave,
            duration: note.duration,
            velocity: note.velocity,
        }
    }
}

/// エディタの UI 状態 (プラグインのロードをまたいで保持される)
pub struct EditorState {
    pub editor: MidiEditor,
    /// スナップ幅 (四分音符単位)
    pub snap: f32,
    /// 連符モード。1 = オフ、3..=7 は N 連符
    pub tuplet: u32,
    /// ノートの左端でも音価を変更できるようにする (左端は終端を固定して頭を動かす)
    pub left_resize: bool,
    pub looping: bool,
    /// 詳細編集の対象 (選択が1個のときだけ Some)
    pub selected: Option<usize>,
    /// 選択中のノート (範囲選択で複数になる)
    pub selection: Vec<usize>,
    /// 最後に選択・編集したノートの設定 (新規ノートの初期値)
    pub last_note: NoteDefaults,
    /// 変更があり、オーディオスレッドへの再送が必要
    pub dirty: bool,
    /// アンドゥ / リドゥ履歴
    pub(super) history: History,
    /// 操作ガイドのウィンドウを開いているか
    pub(super) show_help: bool,
    /// グリッドの縦スクロール位置 (左のトラック欄を追従させるために持つ)
    pub(super) grid_scroll_y: f32,
    /// 段1つの高さ (縦ズーム)。段が少ないときに広げて操作しやすくするためのもの。
    /// MIN_ROW_H..=MAX_ROW_H に収める (set_row_h を通すこと)。
    pub(super) row_h: f32,
    /// 四分音符1つの横幅 (横ズーム)。
    /// MIN_PPQ..=MAX_PPQ に収める (set_ppq を通すこと)。
    pub(super) ppq: f32,
    /// プロジェクトの保存先 (表示用。実際のパス管理は main 側)
    pub project_path: Option<String>,
    /// 打ち込みトラックごとに、**それを鳴らすオーディオトラックの一覧**
    /// (表示用。main 側が毎フレーム更新する)。
    ///
    /// `None` は「どこからも参照されていない = 書いても鳴らない」。
    /// 音源は打ち込みトラックではなくオーディオトラックに載る。
    pub track_plugins: Vec<Option<String>>,
    /// 再生を始めたときの再生ヘッド位置。停止・終端到達でここへ戻す。
    pub(super) play_return: Option<f64>,
    /// 前フレームの再生状態 (終端に達して自分で止まったことの検出用)
    pub(super) was_playing: bool,
    /// 現在の選択を last_note に反映してよいか。
    /// 範囲選択やペーストで選ばれたノートは「ユーザーが指し示したノート」では
    /// ないため反映しない (新規ノートの設定が意図せず書き換わるのを防ぐ)。
    pub(super) track_last_note: bool,
    pub(super) drag: Option<DragState>,
    /// 中クリックドラッグの用途 (押していなければ None)
    pub(super) middle_drag: Option<MiddleDrag>,
    /// 次のフレームでグリッドに強制する横スクロール位置。
    ///
    /// 横ズームの補正に使う。**差分 (`scroll_with_delta`) では駄目**で、
    /// 理由は横ズームを反映している箇所に書いてある。
    pub(super) pending_scroll_x: Option<f32>,
    /// 一度だけ画面に入れたい再生ヘッドの位置 (停止したときに使う)
    pub(super) scroll_to_quarters: Option<f64>,
    /// 段の種別を設定する一覧を開いているトラック (閉じていれば None)
    pub(super) lane_config_track: Option<usize>,
    /// Opus 書き出しのビットレート (kbps)。メニューで選ぶ
    pub opus_bitrate_kbps: u32,
    /// 入れ替えの相手待ちになっている段 (track, lane)。
    ///
    /// 段の帯を押すとここに入り (帯が赤くなる)、次に別の段の帯を押すと入れ替わる。
    pub(super) lane_swap_source: Option<(usize, usize)>,
    /// コピー・カットしたノートの控え (先頭を 0 とした相対位置)。
    ///
    /// **OS のクリップボードは使わない。** 使うと、ノートをコピーするたびに
    /// ユーザーが他所でコピーした内容を壊してしまう。別インスタンスとの
    /// やり取りは要らないと判断したので、ここに持つ。
    pub(super) note_clipboard: Vec<Note>,
}

/// 中ボタンドラッグで何をするか。
///
/// **押し始めに決めて、離すまで変えない。**途中で Ctrl を足したり離したりしても
/// 切り替わらないので、ドラッグの最中に挙動が変わって驚くことがない。
#[derive(Clone, Copy, PartialEq, Debug)]
pub(super) enum MiddleDrag {
    /// スクロール (修飾キーなし)
    Pan,
    /// 横ズーム (Ctrl 併用)。左へ動かすと拡大、右へ動かすと縮小。
    ZoomHorizontally {
        /// 押した位置の四分音符。ズームしてもここが動かないようスクロールを合わせる。
        /// カーソルの現在位置ではなく**押した位置**を使うので、
        /// ドラッグ中に掴んだ場所が逃げていかない。
        anchor_quarters: f32,
    },
}

impl Default for EditorState {
    fn default() -> Self {
        let editor = MidiEditor::default();
        Self {
            history: History::new(&editor),
            editor,
            snap: 0.25,
            tuplet: 1,
            left_resize: false,
            looping: false,
            selected: None,
            selection: Vec::new(),
            last_note: NoteDefaults::default(),
            dirty: true, // 初回コミットのため
            show_help: false,
            grid_scroll_y: 0.0,
            row_h: ROW_H,
            ppq: PPQ,
            project_path: None,
            track_plugins: Vec::new(),
            play_return: None,
            was_playing: false,
            track_last_note: false,
            drag: None,
            middle_drag: None,
            pending_scroll_x: None,
            scroll_to_quarters: None,
            lane_config_track: None,
            opus_bitrate_kbps: crate::opus::DEFAULT_BITRATE_KBPS,
            lane_swap_source: None,
            note_clipboard: Vec::new(),
        }
    }
}

pub(super) enum DragKind {
    /// ノート本体: 横 = 移動、縦 = 段の移動
    Move,
    /// ノートの端: 音価変更。from_left なら終端を固定して頭を動かす。
    Resize { from_left: bool },
    /// ルーラー上のシーク
    Seek,
    /// 空白からのドラッグ: 範囲選択
    Marquee,
}

pub(super) struct DragState {
    pub(super) kind: DragKind,
    /// 操作対象 (インデックスとドラッグ開始時のノート)。
    /// 先頭が掴んだノートで、スナップの基準になる。範囲選択・シークでは空。
    pub(super) targets: Vec<(usize, Note)>,
    /// 範囲選択の開始時点で選ばれていたノート (Shift 押下時の追加選択用)
    pub(super) base_selection: Vec<usize>,
    /// 掴んだ位置を「楽譜上の座標」で保持する (x = 四分音符単位, y = 段)。
    ///
    /// 画面座標で持つと、自動スクロール中にカーソルを止めたときに
    /// 画面座標の差分が変わらず、ノートがカーソルから置き去りになる。
    /// グリッド原点はスクロールに追従して動くので、楽譜座標で持てば
    /// スクロールした分だけ「カーソル下の位置」が変化して自然に追従する。
    pub(super) origin: Pos2,
}

impl EditorState {
    /// 実際に使うスナップ幅 (四分音符単位)。連符モードなら連符1音分。
    ///
    /// N 連符は「スナップ幅2つ分を N 等分」する。つまり連符 N 音でスナップ2つ分に
    /// ちょうど収まる (スナップ 1/8 なら、3連符でも5連符でも四分音符1つ分に N 音)。
    pub fn snap_unit(&self) -> f32 {
        let n = self.tuplet.clamp(1, 7);
        if n <= 1 {
            return self.snap;
        }
        self.snap * 2.0 / n as f32
    }

    /// 段の高さを変える。範囲外は丸める。実際に変わった量 (新 - 旧) を返す。
    ///
    /// 戻り値はズーム後もカーソル下の段を動かさないためのスクロール補正に使う。
    pub(super) fn set_row_h(&mut self, row_h: f32) -> f32 {
        let clamped = row_h.clamp(MIN_ROW_H, MAX_ROW_H);
        let delta = clamped - self.row_h;
        self.row_h = clamped;
        delta
    }

    /// 四分音符1つの横幅を変える。範囲外は丸める。実際に変わった量 (新 - 旧) を返す。
    ///
    /// 戻り値はズーム後も掴んだ位置を動かさないためのスクロール補正に使う
    /// ([`set_row_h`](Self::set_row_h) と同じ形)。
    pub(super) fn set_ppq(&mut self, ppq: f32) -> f32 {
        let clamped = ppq.clamp(MIN_PPQ, MAX_PPQ);
        let delta = clamped - self.ppq;
        self.ppq = clamped;
        delta
    }

    pub(super) fn is_selected(&self, idx: usize) -> bool {
        self.selection.contains(&idx)
    }

    /// 1つだけを選ぶ (ユーザーが直接指したノート。last_note に反映する)
    pub(super) fn select_single(&mut self, idx: usize) {
        self.selection.clear();
        self.selection.push(idx);
        self.selected = Some(idx);
        self.track_last_note = true;
        self.lane_swap_source = None;
    }

    /// その段のノートを全部選ぶ。
    ///
    /// 段の帯を押したときの選択。ユーザーが個々のノートを指したわけではないので、
    /// 新規ノートの設定 (`last_note`) には引き継がない。
    pub(super) fn select_lane(&mut self, track: usize, lane: usize) {
        let idxs = self
            .editor
            .notes
            .iter()
            .enumerate()
            .filter(|(_, note)| note.track == track && note.lane == lane)
            .map(|(idx, _)| idx)
            .collect();
        self.select_many(idxs);
    }

    /// 範囲選択・ペーストによる選択。last_note は更新しない。
    ///
    /// **入れ替えの相手待ちは解除する** ([`clear_selection`](Self::clear_selection) を参照)。
    /// 段の帯を押した直後だけは待ち状態にしたいので、**呼び出し側がこれを呼んだ
    /// あとに** `lane_swap_source` を立てること。
    pub(super) fn select_many(&mut self, idxs: Vec<usize>) {
        self.selected = if idxs.len() == 1 { Some(idxs[0]) } else { None };
        self.selection = idxs;
        self.track_last_note = false;
        self.lane_swap_source = None;
    }

    /// 選択を解除する。
    ///
    /// **段の入れ替えの相手待ちも一緒に解く。** 帯の赤は「その段を選んでいる」印
    /// なので、選択が消えたのに赤いままだと、何もない場所を押したあとに別の段を
    /// 押しただけで入れ替わってしまう。
    pub(super) fn clear_selection(&mut self) {
        self.selection.clear();
        self.selected = None;
        self.track_last_note = false;
        self.lane_swap_source = None;
    }

    /// 選択中のノートを昇順・重複なしで返す
    pub(super) fn selection_sorted(&self) -> Vec<usize> {
        let mut idxs: Vec<usize> = self
            .selection
            .iter()
            .copied()
            .filter(|i| *i < self.editor.notes.len())
            .collect();
        idxs.sort_unstable();
        idxs.dedup();
        idxs
    }

    /// 選択中のノートの頭を、最も早いノートに揃える。動いたら true。
    pub(super) fn align_selection_starts(&mut self) -> bool {
        let idxs = self.selection_sorted();
        if idxs.len() < 2 {
            return false;
        }
        let head = idxs
            .iter()
            .map(|i| self.editor.notes[*i].start_tick)
            .fold(f32::INFINITY, f32::min);

        let mut changed = false;
        for idx in idxs {
            if self.editor.notes[idx].start_tick != head {
                self.editor.notes[idx].start_tick = head;
                changed = true;
            }
        }
        changed
    }

    /// 読み込んだプロジェクトで丸ごと置き換える (アンドゥで戻せる)。
    ///
    /// 中身は `project::from_str` が検証済みなので、ここでは丸めない。
    pub fn replace_project(&mut self, editor: MidiEditor) {
        self.history.record(EditGroup::Once);
        self.editor = editor;
        // 保存されたノートが全部見えるようにトラックと段を確保する
        self.editor.ensure_capacity_for_notes();
        self.clear_selection();
        self.dirty = true;
    }

    /// 読み込んだシーケンスで中身を置き換える (アンドゥで戻せる)。
    /// テンポと拍子はファイルに入っていたときだけ反映する。
    pub fn replace_sequence(
        &mut self,
        notes: Vec<Note>,
        tempo: Option<u32>,
        time_signature: Option<(u32, u32)>,
        lane_ccs: &[(usize, usize, u8)],
    ) {
        self.history.record(EditGroup::Once);
        self.editor.notes = notes;
        // 読み込んだノートが全部見えるようにトラックと段を用意する
        self.editor.tracks.truncate(1);
        self.editor.ensure_capacity_for_notes();
        // 段が揃ってから種別を付ける (先に付けると段が伸びたときに位置がずれる)
        for (track, lane, number) in lane_ccs {
            if let Some(info) = self.editor.tracks.get_mut(*track) {
                info.set_lane_cc(*lane, Some(*number));
            }
        }
        if let Some(tempo) = tempo {
            self.editor.tempo = tempo.clamp(20, 300);
        }
        if let Some((beats, beat_type)) = time_signature {
            self.editor.beats = beats.clamp(1, 16);
            // 対応している分母だけ受け付ける
            if matches!(beat_type, 1 | 2 | 4 | 8 | 16 | 32) {
                self.editor.beat_type = beat_type;
            }
        }
        // 音階モードで使えない半音が入らないようにする
        let max_semitone = self.editor.scale.max_semitone();
        for note in &mut self.editor.notes {
            note.semitone = note.semitone.clamp(0, max_semitone);
        }
        self.clear_selection();
        self.dirty = true;
    }

    /// 選択中のノートを半音単位で移調する。変わったら true。
    ///
    /// 半音とオクターブを「通し番号」に直して増減するので、半音が上限を超えると
    /// 自動的にオクターブへ繰り上がる (12平均律なら (11,4) の1つ上は (0,5)、
    /// ボーレン・ピアースなら (12,4) の1つ上が (0,5))。オクターブ単位の移調は
    /// steps に1オクターブ分のステップ数を渡せばよい。
    /// 選択の一部が範囲外に出る場合は、全員が収まるところまで移調量を抑える
    /// (音程の関係を崩さないため)。
    pub(super) fn transpose_selection(&mut self, steps: i32) -> bool {
        let idxs = self.selection_sorted();
        if idxs.is_empty() || steps == 0 {
            return false;
        }
        let per_octave = self.editor.scale.steps_per_octave().max(1);
        let position = |note: &Note| note.octave * per_octave + note.semitone;

        let lowest = idxs
            .iter()
            .map(|i| position(&self.editor.notes[*i]))
            .min()
            .unwrap_or(0);
        let highest = idxs
            .iter()
            .map(|i| position(&self.editor.notes[*i]))
            .max()
            .unwrap_or(0);
        let steps = steps
            .max(MIN_OCTAVE * per_octave - lowest)
            .min(MAX_OCTAVE * per_octave + per_octave - 1 - highest);
        if steps == 0 {
            return false;
        }

        for idx in idxs {
            let note = &mut self.editor.notes[idx];
            let moved = position(note) + steps;
            note.octave = moved.div_euclid(per_octave);
            note.semitone = moved.rem_euclid(per_octave);
        }
        true
    }

    /// 選択中のノートのベロシティを delta だけ増減する (1..=127 に収める)。
    /// 変わったら true。
    pub(super) fn change_selection_velocity(&mut self, delta: i32) -> bool {
        let mut changed = false;
        for idx in self.selection_sorted() {
            let note = &mut self.editor.notes[idx];
            let velocity = (note.velocity as i32 + delta).clamp(1, 127) as u8;
            if note.velocity != velocity {
                note.velocity = velocity;
                changed = true;
            }
        }
        changed
    }

    /// 選択中のノートの尾を、最も終了が遅いノートに揃える。変わったら true。
    /// 開始位置は動かさず、音価を伸ばして終端を合わせる。
    pub(super) fn align_selection_ends(&mut self) -> bool {
        let idxs = self.selection_sorted();
        if idxs.len() < 2 {
            return false;
        }
        let tail = idxs
            .iter()
            .map(|i| self.editor.notes[*i].end_tick())
            .fold(f32::NEG_INFINITY, f32::max);

        let mut changed = false;
        for idx in idxs {
            let note = &mut self.editor.notes[idx];
            let duration = tail - note.start_tick;
            if duration > 0.0 && note.duration != duration {
                note.duration = duration;
                changed = true;
            }
        }
        changed
    }

    /// 選択中のノートを削除する。削除したら true。
    pub(super) fn delete_selection(&mut self) -> bool {
        let idxs = self.selection_sorted();
        if idxs.is_empty() {
            return false;
        }
        // インデックスがずれないよう後ろから消す
        for idx in idxs.iter().rev() {
            self.editor.notes.remove(*idx);
        }
        self.clear_selection();
        true
    }

    /// 選択中のノートを控えの形にする。
    /// 開始位置は先頭ノートを 0 とした相対値にする (貼り付け先で再生ヘッドに合わせるため)。
    pub(super) fn copy_selection(&self) -> Option<Vec<Note>> {
        let mut notes: Vec<Note> = self
            .selection_sorted()
            .iter()
            .map(|i| self.editor.notes[*i])
            .collect();
        if notes.is_empty() {
            return None;
        }
        let base = notes
            .iter()
            .map(|n| n.start_tick)
            .fold(f32::INFINITY, f32::min);
        for note in &mut notes {
            note.start_tick -= base;
        }
        notes.sort_by(|a, b| a.start_tick.total_cmp(&b.start_tick));
        Some(notes)
    }

    /// ノート列を quarters の位置を先頭として貼り付け、貼った分を選択する。
    pub(super) fn paste_notes(&mut self, notes: &[Note], quarters: f32) -> bool {
        if notes.is_empty() {
            return false;
        }
        let max_semitone = self.editor.scale.max_semitone();
        let first = self.editor.notes.len();
        for note in notes {
            let mut note = *note;
            note.start_tick = (quarters + note.start_tick).max(0.0);
            // 別の音階モードでコピーされたノートが範囲外にならないようにする
            note.semitone = note.semitone.clamp(0, max_semitone);
            self.editor.notes.push(note);
        }
        // 貼り付け先のトラック・段が足りなければ広げる
        self.editor.ensure_capacity_for_notes();
        self.select_many((first..self.editor.notes.len()).collect());
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sequencer::ScaleMode;

    fn placed(start: f32, lane: usize) -> Note {
        Note {
            start_tick: start,
            duration: 0.5,
            semitone: 0,
            octave: 4,
            velocity: 100,
            track: 0,
            lane,
        }
    }

    fn pitched_at(semitone: i32, octave: i32) -> Note {
        Note {
            semitone,
            octave,
            ..placed(0.0, 0)
        }
    }

    /// 選択が変わったら、段の入れ替えの相手待ちも解けること。
    ///
    /// **解けないと、何もない場所を押したあとに別の段の帯を押しただけで
    /// 入れ替わってしまう** (帯は赤く残ったままなので気付けない)。
    #[test]
    fn changing_the_selection_disarms_the_lane_swap() {
        let mut state = EditorState::default();
        state.editor.notes = vec![placed(0.0, 0)];

        for change in [
            "clear_selection",
            "select_single",
            "select_many",
            "select_lane",
        ] {
            state.lane_swap_source = Some((0, 0));
            match change {
                "clear_selection" => state.clear_selection(),
                "select_single" => state.select_single(0),
                "select_many" => state.select_many(vec![0]),
                _ => state.select_lane(0, 0),
            }
            assert_eq!(state.lane_swap_source, None, "{change} で解けること");
        }
    }

    /// CC 段のブロックをコピーして貼り付けられること。
    ///
    /// 貼り付け先は元と同じ段でなければならない (別の段に落ちると、
    /// 黙って別の CC になるか、音符として鳴ってしまう)。
    #[test]
    fn cc_blocks_can_be_copied_and_pasted() {
        let mut state = EditorState::default();
        state.editor.tracks[0].lanes = 1;
        state.editor.add_cc_lane(0, 64); // 段1 が CC
        state.editor.notes = vec![Note {
            lane: 1,
            velocity: 100,
            ..placed(1.0, 1)
        }];
        state.select_many(vec![0]);

        let copied = state.copy_selection().expect("コピーできること");
        assert_eq!(copied.len(), 1);
        assert_eq!(copied[0].lane, 1, "段を保つこと");

        assert!(state.paste_notes(&copied, 4.0));
        assert_eq!(state.editor.notes.len(), 2);
        let pasted = state.editor.notes[1];
        assert_eq!(pasted.start_tick, 4.0);
        assert_eq!(pasted.lane, 1, "CC 段に貼られること");
        assert_eq!(pasted.velocity, 100, "CC 値を保つこと");
        assert_eq!(
            state.editor.lane_cc(pasted.track, pasted.lane),
            Some(64),
            "貼り付け先が CC 段のままであること"
        );
    }

    /// コピーは相対位置を保ち、ペーストは再生ヘッド位置を先頭にすること
    #[test]
    fn copy_paste_anchors_at_playhead() {
        let mut state = EditorState::default();
        state.editor.notes = vec![placed(2.0, 1), placed(2.5, 3), placed(9.0, 0)];
        state.select_many(vec![0, 1]);

        // 実際の経路と同じく、控えを経由して貼り付ける
        let notes = state.copy_selection().expect("コピーできること");
        state.paste_notes(&notes, 4.0);

        assert_eq!(state.editor.notes.len(), 5);
        // 先頭が再生ヘッド、2つ目は元の間隔 (0.5) を保つ。段はそのまま。
        assert_eq!(state.editor.notes[3].start_tick, 4.0);
        assert_eq!(state.editor.notes[3].lane, 1);
        assert_eq!(state.editor.notes[4].start_tick, 4.5);
        assert_eq!(state.editor.notes[4].lane, 3);
        // 貼り付けた分が選択される
        assert_eq!(state.selection, vec![3, 4]);
        // 範囲選択・ペーストの選択は新規ノートの設定に引き継がない
        assert!(!state.track_last_note);
    }

    /// ← キー: 選択中のノートの頭が、最も早いノートに揃うこと
    #[test]
    fn align_selection_starts_to_earliest() {
        let mut state = EditorState::default();
        state.editor.notes = vec![placed(2.0, 0), placed(0.5, 1), placed(3.0, 2)];
        state.select_many(vec![0, 2]);

        assert!(state.align_selection_starts());
        assert_eq!(state.editor.notes[0].start_tick, 2.0, "選択内の最小は 2.0");
        assert_eq!(state.editor.notes[2].start_tick, 2.0);
        assert_eq!(
            state.editor.notes[1].start_tick, 0.5,
            "選択外は動かないこと"
        );

        // 揃っていれば何もしない (無駄なアンドゥ履歴を作らないため)
        assert!(!state.align_selection_starts());
        // 1個だけの選択でも何も起きない
        state.select_many(vec![1]);
        assert!(!state.align_selection_starts());
    }

    /// → キー: 開始位置はそのままに、音価を伸ばして尾が揃うこと
    #[test]
    fn align_selection_ends_to_latest() {
        let mut state = EditorState::default();
        // placed の音価は 0.5 なので終端は 2.5 / 1.0 / 3.5
        state.editor.notes = vec![placed(2.0, 0), placed(0.5, 1), placed(3.0, 2)];
        state.select_many(vec![0, 2]);

        assert!(state.align_selection_ends());
        assert_eq!(
            state.editor.notes[0].start_tick, 2.0,
            "開始位置は動かさないこと"
        );
        assert_eq!(
            state.editor.notes[0].duration, 1.5,
            "音価を伸ばして尾を揃える"
        );
        assert_eq!(
            state.editor.notes[0].end_tick(),
            3.5,
            "選択内の最遅の終端に揃う"
        );
        assert_eq!(
            state.editor.notes[2].duration, 0.5,
            "基準ノートは変わらない"
        );
        assert_eq!(
            state.editor.notes[1].duration, 0.5,
            "選択外は変わらないこと"
        );

        // 揃っていれば何もしない
        assert!(!state.align_selection_ends());
        // 1個だけの選択でも何も起きない
        state.select_many(vec![1]);
        assert!(!state.align_selection_ends());
    }

    /// 縦ズームは上下限で頭打ちになり、実際に変わった量を返すこと
    #[test]
    fn row_zoom_is_clamped() {
        let mut state = EditorState::default();
        assert_eq!(state.row_h, ROW_H, "既定は従来の段の高さ");

        assert_eq!(state.set_row_h(ROW_H * 2.0), ROW_H, "変わった量を返すこと");
        assert_eq!(state.row_h, ROW_H * 2.0);

        // 上限・下限を越えたら丸める
        state.set_row_h(MAX_ROW_H * 10.0);
        assert_eq!(state.row_h, MAX_ROW_H);
        state.set_row_h(0.0);
        assert_eq!(state.row_h, MIN_ROW_H);
        // 頭打ちのあとは何も変わらない (スクロール補正を打たないため)
        assert_eq!(state.set_row_h(-100.0), 0.0);
    }

    /// 横ズームは上下限で頭打ちになり、実際に変わった量を返すこと
    #[test]
    fn horizontal_zoom_is_clamped() {
        let mut state = EditorState::default();
        assert_eq!(state.ppq, PPQ, "既定は従来の横幅");

        assert_eq!(state.set_ppq(PPQ * 2.0), PPQ, "変わった量を返すこと");
        assert_eq!(state.ppq, PPQ * 2.0);

        // 上限・下限を越えたら丸める
        state.set_ppq(MAX_PPQ * 10.0);
        assert_eq!(state.ppq, MAX_PPQ);
        state.set_ppq(0.0);
        assert_eq!(state.ppq, MIN_PPQ);
        // 頭打ちのあとは何も変わらない (スクロール補正を打たないため)
        assert_eq!(state.set_ppq(-100.0), 0.0);
    }

    /// Ctrl+中ドラッグ 1px あたりの倍率が、実用的な速さになっていること。
    ///
    /// 小さすぎると下限から上限まで何往復も必要になり、大きすぎると
    /// 少し動かしただけで飛ぶ。画面幅ぶん動かして全域を通り抜けるくらいが目安。
    #[test]
    fn horizontal_zoom_speed_covers_the_range_in_one_sweep() {
        use super::super::metrics::PPQ_ZOOM_PER_PIXEL;

        // 900px は 1100px 幅のウィンドウでグリッドに使える程度の距離
        let sweep = PPQ_ZOOM_PER_PIXEL.powf(900.0);
        let full_range = MAX_PPQ / MIN_PPQ;
        assert!(
            sweep > full_range,
            "画面幅ぶん動かしても全域を通れない (倍率 {sweep:.0} / 必要 {full_range:.0})"
        );
        // 逆に速すぎないこと。少し動かしただけで飛ぶと、狙った倍率で止められない
        let nudge = PPQ_ZOOM_PER_PIXEL.powf(10.0);
        assert!(nudge < 1.5, "10px で {nudge:.2} 倍は速すぎる");
    }

    /// ↑↓ の半音移調は、半音の上限を超えるとオクターブへ繰り上がること (12平均律)
    #[test]
    fn transpose_carries_into_the_octave() {
        let mut state = EditorState::default(); // 12平均律 (半音 0..=11)
        state.editor.notes = vec![pitched_at(11, 4), pitched_at(0, 4)];
        state.select_many(vec![0, 1]);

        assert!(state.transpose_selection(1));
        assert_eq!(
            (state.editor.notes[0].semitone, state.editor.notes[0].octave),
            (0, 5),
            "(11,4) の1つ上は (0,5)"
        );
        assert_eq!(
            (state.editor.notes[1].semitone, state.editor.notes[1].octave),
            (1, 4)
        );

        // 下げると元に戻る (繰り下がりも同じ規則)
        assert!(state.transpose_selection(-1));
        assert_eq!(
            (state.editor.notes[0].semitone, state.editor.notes[0].octave),
            (11, 4)
        );

        // さらに下げると (0,4) が (11,3) になる
        state.transpose_selection(-1);
        assert_eq!(
            (state.editor.notes[1].semitone, state.editor.notes[1].octave),
            (11, 3)
        );
    }

    /// ボーレン・ピアース (13ステップ) でも同じ規則で繰り上がること
    #[test]
    fn transpose_carries_in_bohlen_pierce() {
        let mut state = EditorState::default();
        state.editor.scale = ScaleMode::BohlenPierce13; // 半音 0..=12
        state.editor.notes = vec![pitched_at(12, 4)];
        state.select_many(vec![0]);

        assert!(state.transpose_selection(1));
        assert_eq!(
            (state.editor.notes[0].semitone, state.editor.notes[0].octave),
            (0, 5),
            "(12,4) の1つ上は (0,5)"
        );

        // Shift+↑ 相当 (1オクターブ = 13ステップ)
        assert!(state.transpose_selection(13));
        assert_eq!(
            (state.editor.notes[0].semitone, state.editor.notes[0].octave),
            (0, 6)
        );
    }

    /// Shift+↑↓ のオクターブ移調と、範囲の頭打ち
    #[test]
    fn transpose_clamps_to_the_octave_range() {
        let mut state = EditorState::default();
        state.editor.notes = vec![pitched_at(0, 7), pitched_at(3, 4)];
        state.select_many(vec![0, 1]);

        // 1オクターブ上げ: どちらも範囲内
        assert!(state.transpose_selection(12));
        assert_eq!(state.editor.notes[0].octave, 8);
        assert_eq!(state.editor.notes[1].octave, 5);

        // さらに上げようとしても、上限のノートが (11,8) を超えないところで止まる。
        // 音程の関係を崩さないよう、選択全体が同じ量だけ抑えられる。
        state.transpose_selection(12);
        assert_eq!(
            (state.editor.notes[0].semitone, state.editor.notes[0].octave),
            (11, 8),
            "上限まで上がって止まること"
        );
        assert_eq!(
            (state.editor.notes[1].semitone, state.editor.notes[1].octave),
            (2, 6),
            "他のノートも同じ量だけ動くこと"
        );

        // 上限に張り付いていれば何も起きない
        assert!(!state.transpose_selection(1));
    }

    /// Alt+ホイールのベロシティ変更は選択全体に効き、1..=127 に収まること
    #[test]
    fn velocity_wheel_changes_selection_within_range() {
        let mut state = EditorState::default();
        state.editor.notes = vec![
            Note {
                velocity: 100,
                ..placed(0.0, 0)
            },
            Note {
                velocity: 20,
                ..placed(1.0, 1)
            },
            Note {
                velocity: 64,
                ..placed(2.0, 2)
            },
        ];
        state.select_many(vec![0, 1]);

        assert!(state.change_selection_velocity(8));
        assert_eq!(state.editor.notes[0].velocity, 108);
        assert_eq!(state.editor.notes[1].velocity, 28);
        assert_eq!(state.editor.notes[2].velocity, 64, "選択外は変わらないこと");

        // 上限・下限で止まる
        state.change_selection_velocity(100);
        assert_eq!(state.editor.notes[0].velocity, 127);
        state.change_selection_velocity(-1000);
        assert_eq!(state.editor.notes[0].velocity, 1, "0 にはならないこと");
        assert_eq!(state.editor.notes[1].velocity, 1);

        // 全員が端に張り付いていれば変化なし
        assert!(!state.change_selection_velocity(-5));
    }

    /// 連符モードのスナップ幅: N 音でスナップ2つ分にちょうど収まること
    #[test]
    fn tuplet_fits_n_notes_in_two_snaps() {
        let mut state = EditorState::default();
        state.snap = 0.5; // 1/8 → 2つ分 = 四分音符1つ

        state.tuplet = 1;
        assert_eq!(state.snap_unit(), 0.5, "オフなら素通し");

        for n in [3u32, 4, 5, 6, 7] {
            state.tuplet = n;
            let span = state.snap_unit() * n as f32;
            assert!(
                (span - 1.0).abs() < 1e-6,
                "{n}連符 {n} 音が四分音符1つ分になること: {span}"
            );
        }

        // 代表値の確認 (8分3連 = 1/3拍、5連符 = 0.2拍)
        state.tuplet = 3;
        assert!((state.snap_unit() - 1.0 / 3.0).abs() < 1e-6);
        state.tuplet = 5;
        assert!((state.snap_unit() - 0.2).abs() < 1e-6);
    }

    /// コピーした控えは、貼り付けても値を保ったまま残ること。
    ///
    /// テキスト化を挟まなくなったので値の往復は自明だが、**何度でも貼れること**は
    /// 実装しだいで壊れる (控えを取り出したまま戻し忘れる)。
    #[test]
    fn copied_notes_can_be_pasted_more_than_once() {
        let mut state = EditorState::default();
        state.editor.notes = vec![placed(2.0, 1), placed(2.5, 3)];
        state.select_many(vec![0, 1]);

        let copied = state.copy_selection().expect("コピーできること");
        assert_eq!(copied.len(), 2);
        assert_eq!(copied[0].start_tick, 0.0, "先頭が 0 の相対位置になること");
        assert_eq!(copied[1].start_tick, 0.5);

        state.paste_notes(&copied, 4.0);
        state.paste_notes(&copied, 8.0);

        assert_eq!(state.editor.notes.len(), 6);
        assert_eq!(state.editor.notes[2].start_tick, 4.0);
        assert_eq!(state.editor.notes[4].start_tick, 8.0);
        assert_eq!(copied[0].start_tick, 0.0, "控えは貼っても変わらないこと");
    }

    /// 貼り付け時に、音階モードで使えない半音を丸めること
    #[test]
    fn paste_clamps_semitone_to_scale() {
        let mut state = EditorState::default(); // 12平均律 (半音 0..=11)
        let notes = vec![Note {
            semitone: 12, // B-P でコピーされた13音目
            ..placed(0.0, 0)
        }];
        state.paste_notes(&notes, 0.0);
        assert_eq!(state.editor.notes[0].semitone, 11);
    }

    /// 一括削除でインデックスがずれず、選ばれていないノートが残ること
    #[test]
    fn delete_selection_removes_only_selected() {
        let mut state = EditorState::default();
        state.editor.notes = vec![placed(0.0, 0), placed(1.0, 1), placed(2.0, 2)];
        state.select_many(vec![0, 2]);

        assert!(state.delete_selection());
        assert_eq!(state.editor.notes.len(), 1);
        assert_eq!(state.editor.notes[0].start_tick, 1.0);
        assert!(state.selection.is_empty());
    }

    /// クリックでの単体選択だけが last_note の追従対象になること
    #[test]
    fn only_direct_selection_tracks_last_note() {
        let mut state = EditorState::default();
        state.editor.notes = vec![placed(0.0, 0), placed(1.0, 1)];

        state.select_single(1);
        assert!(state.track_last_note);
        assert_eq!(state.selected, Some(1));

        // 範囲選択は1個でも追従しない (詳細編集はできるようにする)
        state.select_many(vec![0]);
        assert_eq!(state.selected, Some(0), "1個なら詳細編集の対象にはなること");
        assert!(!state.track_last_note);
    }

    /// ドラッグ中の連続した変更が1ステップにまとまり、アンドゥ/リドゥで往復すること
    #[test]
    fn undo_groups_a_drag_into_one_step() {
        let mut state = EditorState::default();
        state.editor.notes = vec![placed(1.0, 0)];
        state.history.end_frame(&state.editor); // ドラッグ開始前のフレーム境界

        // 1回のドラッグを2フレーム分
        for start in [1.5, 2.0] {
            state.editor.notes[0].start_tick = start;
            state.history.record(EditGroup::Move);
            state.history.end_frame(&state.editor);
        }
        state.history.end_group(); // ドラッグ終了

        assert!(state.history.can_undo());
        assert!(state.history.undo(&mut state.editor));
        assert_eq!(
            state.editor.notes[0].start_tick, 1.0,
            "ドラッグ全体が1回で戻ること"
        );
        assert!(!state.history.can_undo());

        assert!(state.history.redo(&mut state.editor));
        assert_eq!(state.editor.notes[0].start_tick, 2.0);
    }

    /// 単発操作は毎回1ステップになり、新しい操作でリドゥが捨てられること
    #[test]
    fn undo_handles_discrete_edits() {
        let mut state = EditorState::default();
        state.history.end_frame(&state.editor);

        for start in [0.0, 1.0] {
            state.history.record(EditGroup::Once);
            state.editor.notes.push(placed(start, 0));
            state.history.end_frame(&state.editor);
        }
        assert_eq!(state.editor.notes.len(), 2);

        state.history.undo(&mut state.editor);
        assert_eq!(state.editor.notes.len(), 1);
        assert!(state.history.can_redo());

        // アンドゥ後に別の操作をしたらリドゥは無効になる
        state.history.record(EditGroup::Once);
        state.editor.notes.push(placed(5.0, 0));
        state.history.end_frame(&state.editor);
        assert!(!state.history.can_redo());

        state.history.undo(&mut state.editor);
        assert_eq!(state.editor.notes.len(), 1);
        state.history.undo(&mut state.editor);
        assert!(state.editor.notes.is_empty());
    }

    /// 音階モードの変更 (半音の丸め) もアンドゥで戻ること
    #[test]
    fn undo_restores_scale_mode() {
        let mut state = EditorState::default();
        state.editor.notes = vec![Note {
            semitone: 11,
            ..placed(0.0, 0)
        }];
        state.history.end_frame(&state.editor);

        state.history.record(EditGroup::Once);
        state.editor.scale = ScaleMode::BohlenPierce13;
        state.history.end_frame(&state.editor);

        state.history.undo(&mut state.editor);
        assert_eq!(state.editor.scale, ScaleMode::Equal12);
        assert_eq!(state.editor.notes[0].semitone, 11);
    }
}
