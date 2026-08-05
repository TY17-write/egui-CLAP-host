//! シーケンスエディタの egui UI。
//! 段 (レーン) 方式: デフォルト16段の横レーンがあり、ノートは置かれた段に属する。
//! 各ノートは "(半音,オクターブ)" ラベルを表示し、縦ドラッグで段の移動、
//! 右端ドラッグで音価を変更する。ピッチは選択して数値で編集する。

use crate::sequencer::{MidiEditor, Note, ScaleMode, TrackInfo};
use crate::theme::palette;
use eframe::egui::{
    self, Align2, Color32, CornerRadius, CursorIcon, FontId, Pos2, Rect, Sense, Stroke, vec2,
};

/// 1四分音符の横幅 (ピクセル)
const PPQ: f32 = 80.0;
/// 段の高さ
const ROW_H: f32 = 24.0;
/// ルーラーの高さ
const RULER_H: f32 = 22.0;
/// ノート右端のリサイズ判定幅
const EDGE_W: f32 = 10.0;

/// Alt+ホイール1ノッチあたりのベロシティ変化量
const VELOCITY_WHEEL_STEP: i32 = 4;

/// 指定できるオクターブの範囲
const MIN_OCTAVE: i32 = -2;
const MAX_OCTAVE: i32 = 8;

/// ベロシティに満たない部分 (ゴースト) の不透明度。
/// 0 にするとノートが痩せて見えるので、色相と輪郭が残る程度に薄く塗る。
const VELOCITY_GHOST_ALPHA: f32 = 0.3;

/// ノートの塗り色 (オクターブごとに巡回)
const NOTE_COLORS: [Color32; 6] = [
    palette::BLUE,
    palette::GREEN,
    palette::YELLOW,
    palette::PURPLE,
    palette::CYAN,
    palette::RED,
];

/// ノートの塗り色。
///
/// オクターブごとに vim-hybrid のアクセント色を巡回させつつ、半音が上がるほど
/// 次のオクターブの色へ混ぜていく。半音0で純粋なそのオクターブの色、
/// 最上位の半音でほぼ次のオクターブの色になる (オクターブ境界は色が連続する)。
fn note_fill(note: &Note, scale: ScaleMode) -> Color32 {
    let len = NOTE_COLORS.len() as i32;
    let base = NOTE_COLORS[note.octave.rem_euclid(len) as usize];
    let next = NOTE_COLORS[(note.octave + 1).rem_euclid(len) as usize];

    let steps = scale.steps_per_octave().max(1);
    let t = note.semitone.clamp(0, steps) as f32 / steps as f32;
    lerp_color(base, next, t)
}

/// 2色の線形補間 (t=0 で a、t=1 で b)
fn lerp_color(a: Color32, b: Color32, t: f32) -> Color32 {
    let t = t.clamp(0.0, 1.0);
    let mix = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t).round() as u8;
    Color32::from_rgb(
        mix(a.r(), b.r()),
        mix(a.g(), b.g()),
        mix(a.b(), b.b()),
    )
}

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
    history: History,
    /// 操作ガイドのウィンドウを開いているか
    show_help: bool,
    /// MIDI の保存先 (表示用。実際のパス管理は main 側)
    pub midi_path: Option<String>,
    /// 再生を始めたときの再生ヘッド位置。停止・終端到達でここへ戻す。
    play_return: Option<f64>,
    /// 前フレームの再生状態 (終端に達して自分で止まったことの検出用)
    was_playing: bool,
    /// 現在の選択を last_note に反映してよいか。
    /// 範囲選択やペーストで選ばれたノートは「ユーザーが指し示したノート」では
    /// ないため反映しない (新規ノートの設定が意図せず書き換わるのを防ぐ)。
    track_last_note: bool,
    drag: Option<DragState>,
    /// 中クリックドラッグでスクロール中か
    middle_panning: bool,
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
            midi_path: None,
            play_return: None,
            was_playing: false,
            track_last_note: false,
            drag: None,
            middle_panning: false,
        }
    }
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

    fn is_selected(&self, idx: usize) -> bool {
        self.selection.contains(&idx)
    }

    /// 1つだけを選ぶ (ユーザーが直接指したノート。last_note に反映する)
    fn select_single(&mut self, idx: usize) {
        self.selection.clear();
        self.selection.push(idx);
        self.selected = Some(idx);
        self.track_last_note = true;
    }

    /// 範囲選択・ペーストによる選択。last_note は更新しない。
    fn select_many(&mut self, idxs: Vec<usize>) {
        self.selected = if idxs.len() == 1 { Some(idxs[0]) } else { None };
        self.selection = idxs;
        self.track_last_note = false;
    }

    fn clear_selection(&mut self) {
        self.selection.clear();
        self.selected = None;
        self.track_last_note = false;
    }

    /// 選択中のノートを昇順・重複なしで返す
    fn selection_sorted(&self) -> Vec<usize> {
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
    fn align_selection_starts(&mut self) -> bool {
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

    /// 読み込んだシーケンスで中身を置き換える (アンドゥで戻せる)。
    /// テンポと拍子はファイルに入っていたときだけ反映する。
    pub fn replace_sequence(
        &mut self,
        notes: Vec<Note>,
        tempo: Option<u32>,
        time_signature: Option<(u32, u32)>,
    ) {
        self.history.record(EditGroup::Once);
        self.editor.notes = notes;
        // 読み込んだノートが全部見えるようにトラックと段を用意する
        self.editor.tracks.truncate(1);
        self.editor.ensure_capacity_for_notes();
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
    fn transpose_selection(&mut self, steps: i32) -> bool {
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
    fn change_selection_velocity(&mut self, delta: i32) -> bool {
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
    fn align_selection_ends(&mut self) -> bool {
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
    fn delete_selection(&mut self) -> bool {
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

    /// 選択中のノートをクリップボード用テキストにする。
    /// 開始位置は先頭ノートを 0 とした相対値にする (貼り付け先で再生ヘッドに合わせるため)。
    fn copy_selection(&self) -> Option<String> {
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
        Some(notes_to_text(&notes))
    }

    /// ノート列を quarters の位置を先頭として貼り付け、貼った分を選択する。
    fn paste_notes(&mut self, notes: &[Note], quarters: f32) -> bool {
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

/// クリップボードテキストの1行目に置く目印
const CLIPBOARD_HEADER: &str = "clap-host-test notes v1";

/// ノート列をクリップボード用テキストにする。
/// 1行1ノートで "開始,音価,半音,オクターブ,ベロシティ,段,トラック"。
fn notes_to_text(notes: &[Note]) -> String {
    let mut text = String::from(CLIPBOARD_HEADER);
    for n in notes {
        text.push('\n');
        text.push_str(&format!(
            "{},{},{},{},{},{},{}",
            n.start_tick, n.duration, n.semitone, n.octave, n.velocity, n.lane, n.track
        ));
    }
    text
}

/// クリップボードテキストを読む。この形式でなければ None (他アプリのテキストは無視)。
fn notes_from_text(text: &str) -> Option<Vec<Note>> {
    let mut lines = text.lines();
    if lines.next()?.trim() != CLIPBOARD_HEADER {
        return None;
    }

    let mut notes = Vec::new();
    for line in lines {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let f: Vec<&str> = line.split(',').collect();
        // 7列が現行。トラックの無い6列 (古い形式) はトラック0として読む
        if f.len() != 6 && f.len() != 7 {
            return None;
        }
        notes.push(Note {
            start_tick: f[0].parse().ok()?,
            duration: f[1].parse().ok()?,
            semitone: f[2].parse().ok()?,
            octave: f[3].parse().ok()?,
            velocity: f[4].parse().ok()?,
            lane: f[5].parse().ok()?,
            track: match f.get(6) {
                Some(track) => track.parse().ok()?,
                None => 0,
            },
        });
    }
    (!notes.is_empty()).then_some(notes)
}

/// アンドゥ1ステップ分の状態。
/// 音階モードは半音の値を書き換えるので一緒に戻す必要がある。
/// トラック構成 (段数の増減) も元に戻せるように含める。
#[derive(Clone, PartialEq)]
struct Snapshot {
    notes: Vec<Note>,
    tracks: Vec<TrackInfo>,
    scale: ScaleMode,
}

impl Snapshot {
    fn capture(editor: &MidiEditor) -> Self {
        Self {
            notes: editor.notes.clone(),
            tracks: editor.tracks.clone(),
            scale: editor.scale,
        }
    }

    fn restore(&self, editor: &mut MidiEditor) {
        editor.tracks = self.tracks.clone();
        editor.notes = self.notes.clone();
        editor.scale = self.scale;
    }
}

/// 連続した操作を1ステップにまとめるための区分。
/// 同じ区分が続く間 (ドラッグ中など) は履歴を積み増さない。
#[derive(Clone, Copy, PartialEq, Eq)]
enum EditGroup {
    /// 1回で完結する操作 (追加・削除・ペーストなど)
    Once,
    Move,
    Resize,
    Semitone,
    Octave,
    Velocity,
}

/// 履歴の上限 (これを超えたら古いものから捨てる)
const HISTORY_LIMIT: usize = 200;

/// アンドゥ / リドゥ履歴。
///
/// 「このフレームが始まる前の状態」を毎フレーム控えておき、変更が起きた時点で
/// それを1ステップとして積む。各操作の側で「変更前」を用意しなくて済む。
struct History {
    /// 今フレームの開始時点の状態
    baseline: Snapshot,
    undo: Vec<Snapshot>,
    redo: Vec<Snapshot>,
    /// まとめ中の操作区分
    group: Option<EditGroup>,
}

impl History {
    fn new(editor: &MidiEditor) -> Self {
        Self {
            baseline: Snapshot::capture(editor),
            undo: Vec::new(),
            redo: Vec::new(),
            group: None,
        }
    }

    /// 変更が起きたことを記録する。ドラッグのように毎フレーム呼ばれても、
    /// 同じ区分が続く間は1ステップにまとまる。
    fn record(&mut self, group: EditGroup) {
        if self.group == Some(group) {
            return;
        }
        // 同一フレーム内に2回積むと差分ゼロのステップができるので弾く
        if self.undo.last() != Some(&self.baseline) {
            self.undo.push(self.baseline.clone());
            if self.undo.len() > HISTORY_LIMIT {
                self.undo.remove(0);
            }
        }
        self.redo.clear();
        self.group = (group != EditGroup::Once).then_some(group);
    }

    /// まとめを打ち切る (ドラッグ終了・入力欄からフォーカスが外れたとき)
    fn end_group(&mut self) {
        self.group = None;
    }

    fn can_undo(&self) -> bool {
        !self.undo.is_empty()
    }

    fn can_redo(&self) -> bool {
        !self.redo.is_empty()
    }

    fn undo(&mut self, editor: &mut MidiEditor) -> bool {
        let Some(prev) = self.undo.pop() else {
            return false;
        };
        self.redo.push(Snapshot::capture(editor));
        prev.restore(editor);
        // 戻した直後の状態が次の「変更前」になる
        self.baseline = prev;
        self.group = None;
        true
    }

    fn redo(&mut self, editor: &mut MidiEditor) -> bool {
        let Some(next) = self.redo.pop() else {
            return false;
        };
        self.undo.push(Snapshot::capture(editor));
        next.restore(editor);
        self.baseline = next;
        self.group = None;
        true
    }

    /// フレームの終わりに、次フレームの「変更前」を控える
    fn end_frame(&mut self, editor: &MidiEditor) {
        if self.baseline.notes != editor.notes
            || self.baseline.tracks != editor.tracks
            || self.baseline.scale != editor.scale
        {
            self.baseline = Snapshot::capture(editor);
        }
    }
}

enum DragKind {
    /// ノート本体: 横 = 移動、縦 = 段の移動
    Move,
    /// ノートの端: 音価変更。from_left なら終端を固定して頭を動かす。
    Resize { from_left: bool },
    /// ルーラー上のシーク
    Seek,
    /// 空白からのドラッグ: 範囲選択
    Marquee,
}

struct DragState {
    kind: DragKind,
    /// 操作対象 (インデックスとドラッグ開始時のノート)。
    /// 先頭が掴んだノートで、スナップの基準になる。範囲選択・シークでは空。
    targets: Vec<(usize, Note)>,
    /// 範囲選択の開始時点で選ばれていたノート (Shift 押下時の追加選択用)
    base_selection: Vec<usize>,
    /// 掴んだ位置を「楽譜上の座標」で保持する (x = 四分音符単位, y = 段)。
    ///
    /// 画面座標で持つと、自動スクロール中にカーソルを止めたときに
    /// 画面座標の差分が変わらず、ノートがカーソルから置き去りになる。
    /// グリッド原点はスクロールに追従して動くので、楽譜座標で持てば
    /// スクロールした分だけ「カーソル下の位置」が変化して自然に追従する。
    origin: Pos2,
}

/// 画面座標を楽譜座標 (x = 四分音符単位, y = 段) に変換する
fn to_content_pos(grid_origin: Pos2, screen: Pos2) -> Pos2 {
    Pos2::new(
        (screen.x - grid_origin.x) / PPQ,
        (screen.y - grid_origin.y - RULER_H) / ROW_H,
    )
}

/// 楽譜座標を画面座標に戻す (to_content_pos の逆変換)
fn to_screen_pos(grid_origin: Pos2, content: Pos2) -> Pos2 {
    Pos2::new(
        grid_origin.x + content.x * PPQ,
        grid_origin.y + RULER_H + content.y * ROW_H,
    )
}

/// エディタからメインスレッドへの指示
pub enum EditorCommand {
    /// シーケンスをオーディオスレッドに再送する
    Commit,
    Play,
    Stop,
    Seek { quarters: f64 },
    SetLoop(bool),
    /// MIDI ファイルを選んで読み込む
    ImportMidi,
    /// 保存先を選んで MIDI ファイルに書き出す
    ExportMidi,
    /// MIDI として保存する (保存先が未設定なら選ばせる)
    SaveMidi,
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
        }
    }
    state.was_playing = playing;

    toolbar(ui, state, playing, pos_quarters, &mut commands);
    ui.separator();

    // 左下のヘルプボタン用に1行分を残してグリッドを描く
    const HELP_ROW_H: f32 = 26.0;
    let grid_height = (ui.available_height() - HELP_ROW_H).max(80.0);
    egui::ScrollArea::both()
        .auto_shrink([false, false])
        .max_height(grid_height)
        .show(ui, |ui| {
            grid(ui, state, pos_quarters, playing, &mut commands);
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

        if ui
            .button("MIDI インポート")
            .on_hover_text("MIDI ファイルを読み込む (今のノートは置き換わります)")
            .clicked()
        {
            commands.push(EditorCommand::ImportMidi);
        }
        if ui
            .button("MIDI エクスポート")
            .on_hover_text("保存先を選んで MIDI ファイルに書き出す")
            .clicked()
        {
            commands.push(EditorCommand::ExportMidi);
        }
        if let Some(path) = &state.midi_path {
            ui.weak(format!("保存先: {path}"));
        }
    });
    help_window(ui.ctx(), &mut state.show_help);

    shortcuts(ui, state, pos_quarters, playing, &mut commands);

    // ドラッグ中でなければ変更をコミットする
    if state.dirty && state.drag.is_none() {
        commands.insert(0, EditorCommand::Commit);
        state.dirty = false;
    }

    // 次フレームの「変更前の状態」を控える
    state.history.end_frame(&state.editor);

    commands
}

/// 操作ガイドのウィンドウ。
/// ホストのウィンドウが小さいと内容が入りきらないので、縦スクロールを付けて
/// 高さを画面内に収める (見切れると下の項目が読めなくなるため)。
fn help_window(ctx: &egui::Context, open: &mut bool) {
    let screen = ctx.screen_rect();
    let max_height = (screen.height() - 72.0).max(160.0);

    egui::Window::new("操作ガイド")
        .open(open)
        .resizable(true)
        .vscroll(true)
        .default_pos([32.0, 32.0])
        .max_height(max_height)
        .show(ctx, |ui| {
            help_section(
                ui,
                "マウス",
                &[
                    ("ダブルクリック", "ノートを置く (直前のノートの設定を引き継ぐ)"),
                    ("ドラッグ", "移動 (縦に動かすと段が変わる)"),
                    ("右端ドラッグ", "音価を変更 (頭は固定)"),
                    ("左端ドラッグ", "音価を変更 (終端を固定。「左端音価」ON のとき)"),
                    ("空白をドラッグ", "範囲選択 (Shift で選択に追加)"),
                    ("Shift+クリック", "選択に追加 / 選択から外す"),
                    ("右クリック", "削除 (選択中のノートなら選択ごと)"),
                    ("中ドラッグ", "スクロール"),
                    ("Alt+ホイール", "選択中ノートのベロシティを増減"),
                    ("ルーラーをクリック", "再生ヘッドを移動 (拍にスナップ / Alt で自由)"),
                ],
            );

            help_section(
                ui,
                "キーボード",
                &[
                    ("Space", "再生ヘッドから再生 / 停止 (停止すると開始位置へ戻る)"),
                    ("Shift+Space", "先頭から再生"),
                    ("Delete", "選択中のノートを削除"),
                    ("↑ / ↓", "選択中のノートを半音上げ / 下げ (上限でオクターブへ繰り上がる)"),
                    ("Shift+↑ / ↓", "選択中のノートをオクターブ上げ / 下げ"),
                    ("←", "選択の頭を、いちばん早いノートに揃える"),
                    ("→", "選択の尾を、いちばん遅い終端に揃える (音価を伸ばす)"),
                    ("Ctrl+C / X / V", "コピー / カット / 貼り付け (貼り付けは再生ヘッド位置)"),
                    ("Ctrl+Z / Ctrl+Y", "元に戻す / やり直し"),
                    ("Ctrl+S", "MIDI として保存 (保存先が未設定なら選ぶ)"),
                ],
            );

            help_section(
                ui,
                "複数選択中",
                &[
                    ("ドラッグ", "まとめて移動"),
                    ("端ドラッグ", "掴んだ端を全員同じだけ伸縮 (音価の相対関係は保つ)"),
                    ("Alt+ホイール", "全員のベロシティをまとめて増減"),
                ],
            );

            help_section(
                ui,
                "表示",
                &[
                    ("ノートの色", "音高 (オクターブごとの色 + 半音でのグラデーション)"),
                    ("塗りの高さ", "ベロシティ (薄い部分は最大値までの残り)"),
                    ("連符", "スナップ幅2つ分を N 等分する (1/8 の5連符なら四分音符に5音)"),
                ],
            );
        });
}

/// 操作ガイドの1セクション (キー = 説明 の2列)
fn help_section(ui: &mut egui::Ui, title: &str, rows: &[(&str, &str)]) {
    ui.strong(title);
    egui::Grid::new(title)
        .num_columns(2)
        .spacing([16.0, 4.0])
        .show(ui, |ui| {
            for (key, description) in rows {
                ui.label(egui::RichText::new(*key).color(palette::YELLOW));
                ui.label(*description);
                ui.end_row();
            }
        });
    ui.add_space(10.0);
}

/// 再生を始める。`from` を渡すとその位置へ移動してから再生する。
/// 停止したときに戻れるよう、開始時点の再生ヘッド位置を覚えておく。
fn start_playback(
    state: &mut EditorState,
    pos_quarters: f64,
    playing: bool,
    from: Option<f64>,
    commands: &mut Vec<EditorCommand>,
) {
    // すでに再生中なら、最初に再生を始めた位置を保ったままにする。
    // 止まっているときは常に今の位置を覚え直す (プラグイン未ロードで
    // 再生が始まらなかったときなどに、古い位置が残らないように)。
    if !playing {
        state.play_return = Some(pos_quarters.max(0.0));
    }
    if let Some(quarters) = from {
        commands.push(EditorCommand::Seek { quarters });
    }
    commands.push(EditorCommand::Play);
}

/// 停止して、再生を始めた位置へ再生ヘッドを戻す
fn stop_playback(state: &mut EditorState, commands: &mut Vec<EditorCommand>) {
    commands.push(EditorCommand::Stop);
    if let Some(quarters) = state.play_return.take() {
        commands.push(EditorCommand::Seek { quarters });
    }
}

/// このフレームに押されたショートカット
#[derive(Default)]
struct Shortcuts {
    copy: bool,
    cut: bool,
    /// 貼り付けられたテキスト (自作フォーマットとは限らない)
    pasted: Option<String>,
    undo: bool,
    redo: bool,
    delete: bool,
    /// 頭を揃える (←)
    align_head: bool,
    /// 尾を揃える (→)
    align_tail: bool,
    /// 移調量 (半音単位。↑↓ で ±1、Shift+↑↓ で ±1オクターブ)
    transpose: i32,
    /// MIDI として保存する (Ctrl+S)
    save: bool,
}

/// キーボードショートカット (再生 / 移調 / コピー / ペースト / 削除 / アンドゥ)
fn shortcuts(
    ui: &mut egui::Ui,
    state: &mut EditorState,
    pos_quarters: f64,
    playing: bool,
    commands: &mut Vec<EditorCommand>,
) {
    use egui::{Key, Modifiers};

    // 数値入力欄にフォーカスがあるときは横取りしない。
    // ドラッグ中も無効にする (アンドゥでノートが消えると掴んでいる対象がなくなるため)。
    if ui.ctx().wants_keyboard_input() || state.drag.is_some() {
        return;
    }

    // Space: 再生ヘッドから再生 / 停止、Shift+Space: 先頭から再生。
    // ボタンなどにフォーカスがあるときは、そちらの操作キーなので横取りしない。
    //
    // 注意: consume_key は「パターンが要求する修飾キーが押されているか」しか見ない。
    // Modifiers::NONE は Shift 併用でも一致してしまうので、必ず修飾キー付きを
    // 先に消費すること (先に Space を消すと Shift+Space が届かなくなる)。
    if ui.ctx().memory(|m| m.focused()).is_none() {
        let (from_start, toggle) = ui.input_mut(|i| {
            let from_start = i.consume_key(Modifiers::SHIFT, Key::Space);
            let toggle = i.consume_key(Modifiers::NONE, Key::Space);
            (from_start, toggle)
        });
        if from_start {
            if playing {
                commands.push(EditorCommand::Stop);
            }
            start_playback(state, pos_quarters, playing, Some(0.0), commands);
        } else if toggle {
            if playing {
                stop_playback(state, commands);
            } else {
                start_playback(state, pos_quarters, playing, None, commands);
            }
        }
    }

    // Ctrl+C / X / V は egui-winit がキーイベントではなく Copy/Cut/Paste イベントに
    // 変換するため、キーではなくイベントとして拾う (Paste は OS クリップボードに
    // 文字列があるときだけ飛んでくるので、コピー時に必ずテキストを書き出しておく)。
    let per_octave = state.editor.scale.steps_per_octave();
    let keys = ui.input_mut(|i| {
        let mut keys = Shortcuts::default();
        i.events.retain(|event| match event {
            egui::Event::Copy => {
                keys.copy = true;
                false
            }
            egui::Event::Cut => {
                keys.cut = true;
                false
            }
            egui::Event::Paste(text) => {
                keys.pasted = Some(text.clone());
                false
            }
            _ => true,
        });

        // 修飾キーの多い組み合わせから先に消費する (Modifiers::NONE や COMMAND は
        // Shift 併用でも一致するため、Ctrl+Z を先に消すと Ctrl+Shift+Z が届かない)
        keys.redo = i.consume_key(Modifiers::COMMAND | Modifiers::SHIFT, Key::Z)
            || i.consume_key(Modifiers::COMMAND, Key::Y);
        keys.undo = i.consume_key(Modifiers::COMMAND, Key::Z);

        // Shift+↑↓ はオクターブ、修飾なしの ↑↓ は半音
        if i.consume_key(Modifiers::SHIFT, Key::ArrowUp) {
            keys.transpose += per_octave;
        }
        if i.consume_key(Modifiers::SHIFT, Key::ArrowDown) {
            keys.transpose -= per_octave;
        }
        if i.consume_key(Modifiers::NONE, Key::ArrowUp) {
            keys.transpose += 1;
        }
        if i.consume_key(Modifiers::NONE, Key::ArrowDown) {
            keys.transpose -= 1;
        }

        keys.save = i.consume_key(Modifiers::COMMAND, Key::S);
        keys.delete = i.consume_key(Modifiers::NONE, Key::Delete);
        keys.align_head = i.consume_key(Modifiers::NONE, Key::ArrowLeft);
        keys.align_tail = i.consume_key(Modifiers::NONE, Key::ArrowRight);
        keys
    });
    let Shortcuts {
        copy,
        cut,
        pasted,
        undo,
        redo,
        delete,
        align_head,
        align_tail,
        transpose,
        save,
    } = keys;

    if save {
        commands.push(EditorCommand::SaveMidi);
    }

    // ↑↓: 選択中のノートを移調する (半音の上限を超えるとオクターブへ繰り上がる)
    if transpose != 0 && state.transpose_selection(transpose) {
        state.history.record(EditGroup::Semitone);
        state.dirty = true;
    }

    // 左キー: 頭を最も開始が早いノートに揃える (ノートごと移動)。
    // 右キー: 尾を最も終了が遅いノートに揃える (開始位置はそのまま音価を伸ばす)。
    // 変更が起きたときだけ履歴に積む (履歴は「フレーム開始時点」を積むので順序は問わない)。
    if align_head && state.align_selection_starts() {
        state.history.record(EditGroup::Once);
        state.dirty = true;
    }
    if align_tail && state.align_selection_ends() {
        state.history.record(EditGroup::Once);
        state.dirty = true;
    }

    if copy || cut {
        if let Some(text) = state.copy_selection() {
            ui.ctx().copy_text(text);
        }
    }

    if cut && !state.selection.is_empty() {
        state.history.record(EditGroup::Once);
        state.delete_selection();
        state.dirty = true;
    }

    // 貼り付け位置は再生ヘッド。自分の形式でないテキストは無視する。
    if let Some(notes) = pasted.as_deref().and_then(notes_from_text) {
        state.history.record(EditGroup::Once);
        state.paste_notes(&notes, pos_quarters.max(0.0) as f32);
        state.dirty = true;
    }

    if delete && !state.selection.is_empty() {
        state.history.record(EditGroup::Once);
        state.delete_selection();
        state.dirty = true;
    }

    // アンドゥ/リドゥでノートの並びが変わるので選択は解除する
    if undo && state.history.undo(&mut state.editor) {
        state.clear_selection();
        state.dirty = true;
    }
    if redo && state.history.redo(&mut state.editor) {
        state.clear_selection();
        state.dirty = true;
    }
}

/// 上部ツールバー
fn toolbar(
    ui: &mut egui::Ui,
    state: &mut EditorState,
    playing: bool,
    pos_quarters: f64,
    commands: &mut Vec<EditorCommand>,
) {
    ui.horizontal(|ui| {
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

/// グリッド本体 (ルーラー + ノートレーン)
fn grid(
    ui: &mut egui::Ui,
    state: &mut EditorState,
    pos_quarters: f64,
    playing: bool,
    commands: &mut Vec<EditorCommand>,
) {
    // ---- Alt+ホイールで選択ノートのベロシティを変更 ----
    // ScrollArea は内容を描き終えたあとに smooth_scroll_delta を読むので、
    // ここでイベントごと消しておけばベロシティ変更とスクロールが同時に起きない。
    let notches = ui.input_mut(|i| {
        if !i.modifiers.alt {
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
    });
    if notches != 0.0 {
        let delta = (notches * VELOCITY_WHEEL_STEP as f32).round() as i32;
        if delta != 0 && state.change_selection_velocity(delta) {
            state.history.record(EditGroup::Velocity);
            state.dirty = true;
        }
    }

    // 表示する行数 = 全トラックの段の合計。
    // (フェーズ1ではトラックは1本なので、そのトラックの段数と同じ)
    let display_rows = state.editor.total_rows().max(1);
    // 各トラックが何行目から始まるか (ノートの y 位置に使う)
    let row_offsets = track_row_offsets(&state.editor);

    let qpb = state.editor.quarters_per_bar();
    // 表示範囲: ノートの終端を小節に切り上げ + 余白2小節 (最低8小節)
    let total_quarters = (state.editor.length_quarters_bar_aligned() + qpb * 2.0).max(qpb * 8.0);

    let size = vec2(
        total_quarters * PPQ,
        RULER_H + display_rows as f32 * ROW_H,
    );
    let (response, painter) = ui.allocate_painter(size, Sense::click_and_drag());
    let origin = response.rect.min;

    let to_x = |quarters: f32| origin.x + quarters * PPQ;

    // ---- 背景 ----
    painter.rect_filled(
        Rect::from_min_size(origin, vec2(size.x, RULER_H)),
        CornerRadius::ZERO,
        palette::BG_LIGHT,
    );
    painter.rect_filled(
        Rect::from_min_size(origin + vec2(0.0, RULER_H), vec2(size.x, size.y - RULER_H)),
        CornerRadius::ZERO,
        palette::BG_DARK,
    );

    // ---- 段の区切り線 ----
    let lane_stroke = Stroke::new(0.5, palette::BG_SELECT.gamma_multiply(0.7));
    for row in 0..=display_rows {
        let y = origin.y + RULER_H + row as f32 * ROW_H;
        painter.line_segment(
            [Pos2::new(origin.x, y), Pos2::new(origin.x + size.x, y)],
            lane_stroke,
        );
    }

    // ---- 連符の補助線 ----
    // 拍線・小節線より先に描いて、重なる位置は上から塗り潰させる
    let unit = state.snap_unit();
    if state.tuplet > 1 && unit * PPQ >= 6.0 {
        let tuplet_stroke = Stroke::new(0.5, palette::PURPLE.gamma_multiply(0.45));
        let mut q = unit;
        while q <= total_quarters {
            let x = to_x(q);
            painter.line_segment(
                [
                    Pos2::new(x, origin.y + RULER_H),
                    Pos2::new(x, origin.y + size.y),
                ],
                tuplet_stroke,
            );
            q += unit;
        }
    }

    // ---- 拍線・小節線 ----
    let qpbeat = state.editor.quarters_per_beat();
    let beat_stroke = Stroke::new(1.0, palette::BG_SELECT);
    let bar_stroke = Stroke::new(1.5, palette::FG_DIM);

    let mut q = 0.0f32;
    let mut beat_index = 0u32;
    let mut bar_number = 1u32;
    while q <= total_quarters {
        let x = to_x(q);
        let is_bar = beat_index % state.editor.beats.max(1) == 0;
        let stroke = if is_bar { bar_stroke } else { beat_stroke };
        let top = if is_bar { origin.y } else { origin.y + RULER_H };
        painter.line_segment(
            [Pos2::new(x, top), Pos2::new(x, origin.y + size.y)],
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
    for (idx, note) in state.editor.notes.iter().enumerate() {
        let rect = note_rect(origin, note_row(&row_offsets, note), note);
        let fill = note_fill(note, state.editor.scale);
        // ベロシティは「下からの塗りの高さ」で表す。明度やアルファを直接下げると
        // ダークな背景で弱いノートが見えなくなるため、色相はそのままに
        // 満たない部分をゴーストとして残す (輪郭と音高の色は常に見える)。
        painter.rect_filled(
            rect,
            CornerRadius::same(4),
            fill.gamma_multiply(VELOCITY_GHOST_ALPHA),
        );
        let level = velocity_fill_rect(rect, note.velocity);
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

        if state.is_selected(idx) {
            painter.rect_stroke(
                rect,
                CornerRadius::same(4),
                Stroke::new(2.0, palette::FG),
                egui::StrokeKind::Outside,
            );
        }

        if rect.width() > 26.0 {
            // 文字の位置まで実塗りが来ていれば背景色で抜き、
            // ゴーストの上に載るときは前景色にして読めるようにする
            let color = if level.top() <= rect.center().y {
                palette::BG
            } else {
                palette::FG
            };
            painter.text(
                rect.left_center() + vec2(4.0, 0.0),
                Align2::LEFT_CENTER,
                note.name(),
                FontId::proportional(11.0),
                color,
            );
        }
    }

    // ---- 再生線 ----
    let playhead_x = to_x(pos_quarters as f32);
    painter.line_segment(
        [
            Pos2::new(playhead_x, origin.y),
            Pos2::new(playhead_x, origin.y + size.y),
        ],
        Stroke::new(2.0, palette::RED),
    );

    // 再生中は再生線が見えるように追従スクロール
    if playing {
        let visible = ui.clip_rect();
        if playhead_x > visible.right() - 40.0 || playhead_x < visible.left() {
            ui.scroll_to_rect(
                Rect::from_min_size(Pos2::new(playhead_x, origin.y), vec2(PPQ * 4.0, 1.0)),
                Some(egui::Align::Min),
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
        let raw = ((x - origin.x) / PPQ).max(0.0);
        seek_target(raw, beat, selected_start, alt_held)
    };

    // ---- 中クリックドラッグでスクロール ----
    // egui のドラッグ判定はボタンを問わない (any_down) ため、
    // パン中はノート編集のドラッグ処理を止める必要がある。
    let (middle_down, pointer_delta) = ui.input(|i| (i.pointer.middle_down(), i.pointer.delta()));
    if middle_down {
        if !state.middle_panning && response.contains_pointer() {
            state.middle_panning = true;
            state.drag = None; // ノート編集とは排他
        }
    } else {
        state.middle_panning = false;
    }

    if state.middle_panning {
        // 掴んだ位置がカーソルに追従するよう、アニメーションなしで即時スクロールする
        ui.scroll_with_delta_animation(pointer_delta, egui::style::ScrollAnimation::none());
        ui.output_mut(|o| o.cursor_icon = CursorIcon::Grabbing);
    }

    // カーソル形状のフィードバック
    if state.drag.is_none() && !state.middle_panning {
        if let Some(hover) = response.hover_pos() {
            match hit_note(&state.editor.notes, &row_offsets, origin, hover, left_resize) {
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
    if response.drag_started() && !state.middle_panning {
        let press_pos = ui
            .input(|i| i.pointer.press_origin())
            .or_else(|| response.interact_pointer_pos());
        if let Some(pos) = press_pos {
            if pos.y < origin.y + RULER_H {
                state.drag = Some(DragState {
                    kind: DragKind::Seek,
                    targets: Vec::new(),
                    base_selection: Vec::new(),
                    origin: to_content_pos(origin, pos),
                });
            } else if let Some((idx, hit)) =
                hit_note(&state.editor.notes, &row_offsets, origin, pos, left_resize)
            {
                // 複数選択中のノートを掴んだら選択全体を操作する (選択は変えない)
                let bulk = state.selection.len() > 1 && state.is_selected(idx);
                let targets = if bulk {
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
                state.drag = Some(DragState {
                    kind: match hit {
                        Hit::Body => DragKind::Move,
                        Hit::ResizeRight => DragKind::Resize { from_left: false },
                        Hit::ResizeLeft => DragKind::Resize { from_left: true },
                    },
                    targets,
                    base_selection: Vec::new(),
                    origin: to_content_pos(origin, pos),
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
                    origin: to_content_pos(origin, pos),
                });
            }
        }
    }

    // ドラッグ中
    if response.dragged() && !state.middle_panning {
        // 範囲選択の結果は借用が終わってから反映する
        let mut marquee_selection = None;

        if let (Some(drag), Some(pos)) = (&state.drag, response.interact_pointer_pos()) {
            // 掴んだ位置も現在位置も楽譜座標で扱う (自動スクロール中の追従のため)
            let content = to_content_pos(origin, pos);

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
                    let dx = resize_delta(
                        &drag.targets,
                        content.x - drag.origin.x,
                        snap,
                        from_left,
                    );

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
                    let rect = Rect::from_two_pos(to_screen_pos(origin, drag.origin), pos);
                    let mut selection = drag.base_selection.clone();
                    for (idx, note) in state.editor.notes.iter().enumerate() {
                        if rect.intersects(note_rect(origin, note_row(&row_offsets, note), note))
                            && !selection.contains(&idx)
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
                        Stroke::new(1.0, palette::FG_DIM),
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
                match hit_note(&state.editor.notes, &row_offsets, origin, pos, left_resize) {
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
            if pos.y >= origin.y + RULER_H
                && hit_note(&state.editor.notes, &row_offsets, origin, pos, left_resize).is_none()
            {
                // 最後に選択・編集したノートの設定を引き継ぐ。
                // (ダブルクリックの1打目で選択が解除されるため state.selected は使えない)
                let defaults = state.last_note;
                let row = (((pos.y - origin.y - RULER_H) / ROW_H).floor() as i32)
                    .clamp(0, display_rows as i32 - 1) as usize;
                let (track, lane) = row_to_track_lane(&row_offsets, &state.editor, row);
                let start = snap_floor((pos.x - origin.x) / PPQ).max(0.0);
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
            if let Some((idx, _)) =
                hit_note(&state.editor.notes, &row_offsets, origin, pos, left_resize)
            {
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
}

/// 画面端の自動スクロール量を求める。
///
/// `visible` の内側 `EDGE_SCROLL_MARGIN` に `pointer` が入ると、端に近いほど速く
/// その方向へスクロールする量を返す。`vertical` が false なら横方向のみ。
///
/// 返す値は `scroll_with_delta` に渡す形式で、符号は「内容が動く向き」。
/// 例えば右端に寄せたときは先の内容を見せたいので x は負になる。
fn edge_scroll_delta(visible: Rect, pointer: Pos2, vertical: bool) -> egui::Vec2 {
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
fn move_delta(
    targets: &[(usize, Note)],
    row_offsets: &[usize],
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

    (
        dx.max(-min_start),
        d_row_raw.clamp(-min_row, rows as i32 - 1 - max_row),
    )
}

/// 再生ヘッドの移動先 (四分音符単位) を求める。
///
/// 拍の間隔にスナップする。四分音符固定にすると 3/8 拍子のように1小節が
/// 四分音符の整数倍にならない拍子で小節線に乗らなくなるため、拍を単位にする
/// (拍は必ず小節を割り切るので、小節線は常にスナップ先に含まれる)。
/// 選択中ノートの頭がスナップ先より近ければ、そちらを優先する。
/// `free` が true なら (Alt 押下中) スナップしない。
fn seek_target(raw_quarters: f32, beat: f32, selected_start: Option<f32>, free: bool) -> f64 {
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
fn resize_delta(targets: &[(usize, Note)], dx_raw: f32, snap: f32, from_left: bool) -> f32 {
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
fn track_row_offsets(editor: &MidiEditor) -> Vec<usize> {
    let mut offsets = Vec::with_capacity(editor.tracks.len());
    let mut row = 0;
    for info in &editor.tracks {
        offsets.push(row);
        row += info.lanes.max(1);
    }
    offsets
}

/// ノートが画面の何行目に描かれるか
fn note_row(offsets: &[usize], note: &Note) -> usize {
    offsets.get(note.track).copied().unwrap_or(0) + note.lane
}

/// 画面の行番号を (トラック, 段) に戻す
fn row_to_track_lane(offsets: &[usize], editor: &MidiEditor, row: usize) -> (usize, usize) {
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
fn velocity_fill_rect(rect: Rect, velocity: u8) -> Rect {
    let level = velocity.min(127) as f32 / 127.0;
    let height = rect.height() * level;
    Rect::from_min_max(
        Pos2::new(rect.left(), rect.bottom() - height),
        Pos2::new(rect.right(), rect.bottom()),
    )
}

/// ノートの表示矩形を計算する。`row` は画面の行番号 (トラックの段を通しで数えた値)。
fn note_rect(origin: Pos2, row: usize, note: &Note) -> Rect {
    Rect::from_min_size(
        Pos2::new(
            origin.x + note.start_tick * PPQ,
            origin.y + RULER_H + row as f32 * ROW_H + 2.0,
        ),
        vec2((note.duration * PPQ - 1.0).max(4.0), ROW_H - 4.0),
    )
}

/// ノートのどこを掴んだか
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Hit {
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
fn hit_note(
    notes: &[Note],
    offsets: &[usize],
    origin: Pos2,
    pos: Pos2,
    left_resize: bool,
) -> Option<(usize, Hit)> {
    for (idx, note) in notes.iter().enumerate().rev() {
        let rect = note_rect(origin, note_row(offsets, note), note);
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

    fn viewport() -> Rect {
        Rect::from_min_max(Pos2::new(0.0, 0.0), Pos2::new(1000.0, 400.0))
    }

    fn pitched(semitone: i32, octave: i32) -> Note {
        Note {
            start_tick: 0.0,
            duration: 1.0,
            semitone,
            octave,
            velocity: 100,
            track: 0,
            lane: 0,
        }
    }

    /// テスト用: トラック1本 (段は十分にある) の行オフセット
    fn single_track_rows() -> Vec<usize> {
        vec![0]
    }

    /// 半音0はそのオクターブの色そのもの、半音が上がるほど次のオクターブの色に寄る
    #[test]
    fn note_fill_blends_toward_next_octave() {
        let scale = ScaleMode::Equal12;
        let base = NOTE_COLORS[4 % NOTE_COLORS.len()];
        let next = NOTE_COLORS[5 % NOTE_COLORS.len()];

        assert_eq!(note_fill(&pitched(0, 4), scale), base, "半音0は混ざらないこと");

        let dist = |c: Color32, to: Color32| {
            (c.r() as i32 - to.r() as i32).abs()
                + (c.g() as i32 - to.g() as i32).abs()
                + (c.b() as i32 - to.b() as i32).abs()
        };
        let low = note_fill(&pitched(3, 4), scale);
        let high = note_fill(&pitched(11, 4), scale);
        assert!(
            dist(high, next) < dist(low, next),
            "半音が高いほど次のオクターブの色に近いこと: {low:?} / {high:?}"
        );
        // 境界の連続性: 最上位半音は次オクターブの半音0に届く手前で止まる
        assert_ne!(high, next, "オクターブ境界が潰れないこと");
    }

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

    /// 一括移動: 掴んだノートを基準にスナップし、全員が同じだけ動くこと
    #[test]
    fn bulk_move_keeps_relative_positions() {
        // 0.1 だけずれた位置にあるノートも、掴んだノートと同じ移動量で動く
        let targets = vec![(0, placed(1.0, 2)), (1, placed(1.1, 3))];
        let (dx, d_lane) = move_delta(&targets, &single_track_rows(), 0.6, 1, 0.25, 16);
        assert_eq!(dx, 0.5, "掴んだノートが 1.5 に来るようスナップされること");
        assert_eq!(d_lane, 1);
    }

    /// 一括移動: 選択の端が範囲外に出ないよう移動量が抑えられること
    #[test]
    fn bulk_move_clamps_to_bounds() {
        let targets = vec![(0, placed(1.0, 1)), (1, placed(0.5, 15))];
        // 左へ大きく動かしても、先頭のノートが 0 未満にならない分しか動かない
        let (dx, _) = move_delta(&targets, &single_track_rows(), -5.0, 0, 0.25, 16);
        assert_eq!(dx, -0.5);
        // 下へ動かしても、最下段のノートが 15 段目に留まる
        let (_, d_lane) = move_delta(&targets, &single_track_rows(), 0.0, 5, 0.25, 16);
        assert_eq!(d_lane, 0);
        // 上へ動かすときは最上段のノートが 0 段目で止まる
        let (_, d_lane) = move_delta(&targets, &single_track_rows(), 0.0, -5, 0.25, 16);
        assert_eq!(d_lane, -1);
    }

    /// コピーは相対位置を保ち、ペーストは再生ヘッド位置を先頭にすること
    #[test]
    fn copy_paste_anchors_at_playhead() {
        let mut state = EditorState::default();
        state.editor.notes = vec![placed(2.0, 1), placed(2.5, 3), placed(9.0, 0)];
        state.select_many(vec![0, 1]);

        // 実際の経路と同じく、クリップボードのテキストを経由して貼り付ける
        let text = state.copy_selection().expect("コピーできること");
        let notes = notes_from_text(&text).expect("読み戻せること");
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
        assert_eq!(state.editor.notes[1].start_tick, 0.5, "選択外は動かないこと");

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
        assert_eq!(state.editor.notes[0].start_tick, 2.0, "開始位置は動かさないこと");
        assert_eq!(state.editor.notes[0].duration, 1.5, "音価を伸ばして尾を揃える");
        assert_eq!(state.editor.notes[0].end_tick(), 3.5, "選択内の最遅の終端に揃う");
        assert_eq!(state.editor.notes[2].duration, 0.5, "基準ノートは変わらない");
        assert_eq!(state.editor.notes[1].duration, 0.5, "選択外は変わらないこと");

        // 揃っていれば何もしない
        assert!(!state.align_selection_ends());
        // 1個だけの選択でも何も起きない
        state.select_many(vec![1]);
        assert!(!state.align_selection_ends());
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
        assert_eq!(hit_note(&notes, &single_track_rows(), origin, left_pos, false), Some((0, Hit::Body)));
        assert_eq!(
            hit_note(&notes, &single_track_rows(), origin, right_pos, false),
            Some((0, Hit::ResizeRight))
        );

        // ON: 左端が左リサイズになり、右端は引き続き右リサイズ
        assert_eq!(
            hit_note(&notes, &single_track_rows(), origin, left_pos, true),
            Some((0, Hit::ResizeLeft))
        );
        assert_eq!(
            hit_note(&notes, &single_track_rows(), origin, right_pos, true),
            Some((0, Hit::ResizeRight))
        );
    }

    fn pitched_at(semitone: i32, octave: i32) -> Note {
        Note {
            semitone,
            octave,
            ..placed(0.0, 0)
        }
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
        assert_eq!(seek_target(1.49, 0.5, Some(1.2), false), 1.5, "遠ければ拍が優先");

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
        let ends: Vec<f32> = targets
            .iter()
            .map(|(_, n)| n.end_tick() + dx)
            .collect();
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

    /// クリップボードのテキストが値を保ったまま往復すること
    #[test]
    fn clipboard_text_round_trips() {
        let notes = vec![
            Note {
                start_tick: 0.0,
                duration: 0.125,
                semitone: 11,
                octave: -2,
                velocity: 1,
                track: 0,
                lane: 0,
            },
            Note {
                start_tick: 1.75,
                duration: 2.5,
                semitone: 3,
                octave: 8,
                velocity: 127,
                track: 2,
                lane: 17,
            },
        ];
        let restored = notes_from_text(&notes_to_text(&notes)).expect("読み戻せること");
        assert_eq!(restored, notes);
    }

    /// 他アプリでコピーしたテキストは無視すること
    #[test]
    fn clipboard_text_rejects_foreign_content() {
        assert!(notes_from_text("hello world").is_none());
        assert!(notes_from_text(CLIPBOARD_HEADER).is_none(), "ノートが無ければ無効");
        assert!(
            notes_from_text(&format!("{CLIPBOARD_HEADER}\n0,0.5,0,4,100")).is_none(),
            "列が足りない行は無効"
        );
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
        assert_eq!(state.editor.notes[0].start_tick, 1.0, "ドラッグ全体が1回で戻ること");
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

    /// 音階モードでステップ数が変わっても半音の相対位置で混ざること
    #[test]
    fn note_fill_uses_scale_steps() {
        let eq = note_fill(&pitched(6, 4), ScaleMode::Equal12); // 6/12 = 0.5
        let bp = note_fill(&pitched(6, 4), ScaleMode::BohlenPierce13); // 6/13 ≈ 0.46
        assert_ne!(eq, bp, "ステップ数の違いが混色比に反映されること");
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
