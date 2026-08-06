//! シーケンサーのデータモデル。
//! 時間の単位はすべて「四分音符 = 1.0」の実数 (tick と呼ぶ)。

use crate::swing;

/// 音階モード。1オクターブを何ステップに分けるかを決める。
///
/// ホストが変えるのは MIDI ノート番号だけで、実際の音高は
/// プラグイン側の音律設定 (Scala の .scl ファイルなど) が決める。
#[derive(Clone, Copy, PartialEq, Eq, Debug, serde::Serialize, serde::Deserialize)]
pub enum ScaleMode {
    /// 12平均律。半音 0..=11。
    Equal12,
    /// ボーレン・ピアース音階。トライターブ (3:1) を13等分。半音 0..=12。
    BohlenPierce13,
}

/// ボーレン・ピアースの基準となるキー番号。
/// (半音3, オクターブ4) にあたり、12平均律の E♭4 と同じノート番号。
const BOHLEN_PIERCE_REFERENCE_KEY: f64 = 63.0;

/// 基準キーの周波数 (Hz)。12平均律の E♭4 (440 / √2) と同じ高さ。
const BOHLEN_PIERCE_REFERENCE_HZ: f64 = 311.127;

impl ScaleMode {
    /// 1オクターブ (ボーレン・ピアースではトライターブ) あたりのステップ数
    pub fn steps_per_octave(self) -> i32 {
        match self {
            ScaleMode::Equal12 => 12,
            ScaleMode::BohlenPierce13 => 13,
        }
    }

    /// 半音として指定できる最大値
    pub fn max_semitone(self) -> i32 {
        self.steps_per_octave() - 1
    }

    /// (半音, オクターブ) に対応する MIDI ノート番号。
    /// 0..=127 に収まらない値もそのまま返す (範囲の判定は呼び出し側)。
    ///
    /// (半音0, オクターブ4) が常に 60 (中央ハ) になるよう基準を取る。
    pub fn key_number(self, semitone: i32, octave: i32) -> i32 {
        60 + (octave - 4) * self.steps_per_octave() + semitone
    }

    /// この音律での実際の周波数 (Hz)。
    ///
    /// 再生時はノート番号をプラグインに渡すだけで、音高はプラグイン側の音律設定
    /// (Scala の .scl など) が決める。この関数は「ホストが想定している音高」を
    /// 返すもので、周波数そのものを書き出す形式 (CCS の LogF0 など) で使う。
    ///
    /// キー番号が 1 増えると音律の 1 ステップぶん上がるので、基準点との差だけで決まる。
    pub fn frequency(self, semitone: i32, octave: i32) -> f64 {
        let key = self.key_number(semitone, octave) as f64;
        match self {
            // A4 (キー69) = 440Hz、1オクターブ = 2倍を12等分
            ScaleMode::Equal12 => 440.0 * 2f64.powf((key - 69.0) / 12.0),
            // キー63 = 311.127Hz、1トライターブ = 3倍を13等分
            ScaleMode::BohlenPierce13 => {
                BOHLEN_PIERCE_REFERENCE_HZ * 3f64.powf((key - BOHLEN_PIERCE_REFERENCE_KEY) / 13.0)
            }
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            ScaleMode::Equal12 => "12平均律",
            ScaleMode::BohlenPierce13 => "B-P 13音",
        }
    }
}

/// 1つのノート
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Note {
    /// 開始位置 (四分音符単位)
    pub start_tick: f32,
    /// 持続時間 (四分音符単位)
    pub duration: f32,
    /// 平均律の半音 0..=11 (C=0)
    pub semitone: i32,
    /// オクターブ -2..=8 (基準4)
    pub octave: i32,
    /// ベロシティ 0..=127
    pub velocity: u8,
    /// 所属するトラック (0 始まり)。トラックごとに音源を持つ。
    pub track: usize,
    /// トラック内の段 (0 始まり)。音高とは独立で、どの段に置くかだけを表す。
    pub lane: usize,
}

impl Note {
    pub fn end_tick(&self) -> f32 {
        self.start_tick + self.duration
    }

    /// MIDI ノート番号。0..=127 の範囲外なら None。
    ///
    /// (半音0, オクターブ4) が常に 60 (中央ハ) になるよう基準を取る。
    /// 12平均律なら (0,4)=60 / (9,4)=69、ボーレン・ピアースなら
    /// (0,4)=60 / (3,4)=63 (平均律の E♭4 と同じノート番号) となる。
    pub fn key(&self, scale: ScaleMode) -> Option<u8> {
        let key = scale.key_number(self.semitone, self.octave);
        u8::try_from(key).ok().filter(|k| *k <= 127)
    }

    /// この音律でのノートの周波数 (Hz)
    pub fn frequency(&self, scale: ScaleMode) -> f64 {
        scale.frequency(self.semitone, self.octave)
    }

    /// "(半音, オクターブ)" 形式の表示名。例: (0,4)
    pub fn name(&self) -> String {
        format!("({},{})", self.semitone, self.octave)
    }

}

/// 新しいトラックが最初に持つ段数。
/// 必要な分だけ [+] で足していく運用にしている。
pub const DEFAULT_LANES: usize = 1;

/// トラック1本ぶんの情報。音源 (プラグイン) はホスト側が別に持つ。
#[derive(Clone, Debug, PartialEq)]
pub struct TrackInfo {
    pub name: String,
    /// このトラックが持つ段数 (1 以上)
    pub lanes: usize,
    /// 消音中か
    pub muted: bool,
    /// ソロ指定中か (どれか1つでもソロなら、ソロ以外は鳴らさない)
    pub soloed: bool,
    /// スウィングを掛けるか。
    /// 伴奏は正確な拍のまま、ソロだけ跳ねさせる、という使い方をするので
    /// トラックごとに持つ。
    pub swing: bool,
}

impl TrackInfo {
    pub fn new(index: usize) -> Self {
        Self {
            name: format!("トラック {}", index + 1),
            lanes: DEFAULT_LANES,
            muted: false,
            soloed: false,
            swing: false,
        }
    }
}

/// シーケンス全体 (エディタが編集する対象)
#[derive(Debug)]
pub struct MidiEditor {
    pub notes: Vec<Note>,
    /// トラック (上から順に並ぶ)。必ず1本以上ある。
    pub tracks: Vec<TrackInfo>,
    /// BPM (四分音符の数/分)
    pub tempo: u32,
    /// 拍子の分子
    pub beats: u32,
    /// 拍子の分母 (1,2,4,8,16,32 のいずれか)
    pub beat_type: u32,
    /// 音階モード (ノート番号の算出方法)
    pub scale: ScaleMode,
    /// スウィングの強さ (200BPM 時の裏拍の比)。1.0 で直線。
    /// 掛けるかどうかはトラックごと ([`TrackInfo::swing`])。
    pub swing_peak_ratio: f32,
}

impl Default for MidiEditor {
    fn default() -> Self {
        Self {
            notes: Vec::new(),
            tracks: vec![TrackInfo::new(0)],
            tempo: 120,
            beats: 4,
            beat_type: 4,
            scale: ScaleMode::Equal12,
            swing_peak_ratio: crate::swing::DEFAULT_PEAK_RATIO,
        }
    }
}

impl MidiEditor {
    /// トラック数 (必ず1以上)
    pub fn track_count(&self) -> usize {
        self.tracks.len().max(1)
    }

    /// 指定トラックの段数 (範囲外なら既定値)
    pub fn lanes(&self, track: usize) -> usize {
        self.tracks
            .get(track)
            .map_or(DEFAULT_LANES, |info| info.lanes.max(1))
    }

    /// 全トラックの段を上から並べたときの行数
    pub fn total_rows(&self) -> usize {
        self.tracks.iter().map(|info| info.lanes.max(1)).sum()
    }

    /// ソロ指定のトラックがあるか
    pub fn has_solo(&self) -> bool {
        self.tracks.iter().any(|info| info.soloed)
    }

    /// そのトラックの音を出すか。
    /// ソロが1つでもあれば、ソロのトラックだけが鳴る (ミュートより優先)。
    pub fn is_audible(&self, track: usize) -> bool {
        let Some(info) = self.tracks.get(track) else {
            return false;
        };
        if self.has_solo() {
            info.soloed
        } else {
            !info.muted
        }
    }

    /// トラックを末尾に追加する
    pub fn add_track(&mut self) {
        self.tracks.push(TrackInfo::new(self.tracks.len()));
    }

    /// 末尾のトラックを削除する。
    /// ノートが残っているトラックと最後の1本は消さない (事故防止)。
    /// 削除したら true。
    pub fn remove_last_track(&mut self) -> bool {
        if self.tracks.len() <= 1 {
            return false;
        }
        let last = self.tracks.len() - 1;
        if self.notes.iter().any(|note| note.track == last) {
            return false;
        }
        self.tracks.pop().is_some()
    }

    /// 指定トラックの段を1つ増やす
    pub fn add_lane(&mut self, track: usize) -> bool {
        match self.tracks.get_mut(track) {
            Some(info) => {
                info.lanes += 1;
                true
            }
            None => false,
        }
    }

    /// 指定トラックの最後の段を削除する。
    /// その段にノートがあるときと、段が1つしかないときは削除しない。
    pub fn remove_last_lane(&mut self, track: usize) -> bool {
        let Some(info) = self.tracks.get_mut(track) else {
            return false;
        };
        if info.lanes <= 1 {
            return false;
        }
        let last = info.lanes - 1;
        if self
            .notes
            .iter()
            .any(|note| note.track == track && note.lane == last)
        {
            return false;
        }
        info.lanes -= 1;
        true
    }

    /// 今あるノートが全部収まるようにトラック数と段数を広げる。
    /// 読み込みや貼り付けの直後に呼ぶ (画面外にノートが隠れるのを防ぐ)。
    pub fn ensure_capacity_for_notes(&mut self) {
        for index in 0..self.notes.len() {
            let (track, lane) = (self.notes[index].track, self.notes[index].lane);
            while self.tracks.len() <= track {
                self.add_track();
            }
            let info = &mut self.tracks[track];
            info.lanes = info.lanes.max(lane + 1);
        }
        if self.tracks.is_empty() {
            self.tracks.push(TrackInfo::new(0));
        }
    }

    /// 1拍の長さ (四分音符単位)。例: 拍子分母4なら1.0、8なら0.5
    pub fn quarters_per_beat(&self) -> f32 {
        4.0 / self.beat_type as f32
    }

    /// 1小節の長さ (四分音符単位)
    pub fn quarters_per_bar(&self) -> f32 {
        self.beats as f32 * self.quarters_per_beat()
    }

    /// 全ノートの終端 (四分音符単位)
    pub fn length_quarters(&self) -> f32 {
        self.notes
            .iter()
            .map(|n| n.end_tick())
            .fold(0.0, f32::max)
    }

    /// 終端を小節境界に切り上げた長さ (最低1小節)。
    /// f32 の誤差で「ちょうど小節境界」がわずかに超過し、余分な1小節が
    /// 追加されるのを防ぐため、僅かな許容誤差を持たせている。
    pub fn length_quarters_bar_aligned(&self) -> f32 {
        const EPS: f32 = 1e-3;
        let bar = self.quarters_per_bar();
        let bars = ((self.length_quarters() - EPS).max(0.0) / bar).ceil().max(1.0);
        bars * bar
    }

    /// 1四分音符あたりのサンプル数
    pub fn samples_per_quarter(&self, sample_rate: f64) -> f64 {
        sample_rate * 60.0 / self.tempo.max(1) as f64
    }

    /// そのトラックにスウィングを掛けるか
    pub fn track_swings(&self, track: usize) -> bool {
        self.tracks.get(track).is_some_and(|info| info.swing)
    }

    /// 出力用に、スウィングを適用したノート列を返す。記譜 (`self.notes`) は変えない。
    ///
    /// 再生・WAV・CCS・MIDI エクスポートはすべてこれを通す。記譜位置は8分の
    /// 等分のまま保ち、跳ねは出力の直前に乗せる、という分担にしている。
    ///
    /// 開始と終端の**両方**に同じオフセットを掛けるので、前のノートの終端と
    /// 次のノートの開始には必ず同じ値が乗り、重なりも隙間も生まれない。
    pub fn performed_notes(&self) -> Vec<Note> {
        if !swing::applies_to(self.beat_type) {
            return self.notes.clone();
        }

        // 演奏位置が記譜上の終端を超えると、トランスポートの終端フラッシュ
        // (sample_time > end_sample で打ち切る) から漏れて鳴りっぱなしになる。
        // 小節線ちょうどで終わるノートは普通にあるので、ここで頭打ちにする。
        let limit = self.length_quarters_bar_aligned();

        self.notes
            .iter()
            .map(|note| {
                // 音価0以下のノートは各出力が弾く。ここで下限を掛けてしまうと
                // スウィングの有無で弾かれ方が変わるので、触らずに返す。
                if note.duration <= 0.0 || !self.track_swings(note.track) {
                    return *note;
                }
                let shift = |tick: f32| {
                    tick + swing::offset(tick, self.tempo, self.swing_peak_ratio)
                };
                let start = shift(note.start_tick);
                // 極端に短いノートは終端が開始を追い越すので下限を設ける
                let end = shift(note.end_tick())
                    .min(limit)
                    .max(start + swing::MIN_PERFORMED_DURATION);
                Note {
                    start_tick: start,
                    duration: end - start,
                    ..*note
                }
            })
            .collect()
    }

    /// ノート列をサンプル時刻付きイベント列に変換する。
    /// 同時刻ではノートオフがノートオンより先に来るようソートされる。
    pub fn to_events(&self, sample_rate: f64) -> Vec<SeqEvent> {
        self.collect_events(None, sample_rate)
    }

    /// 指定トラックのノートだけをイベント列にする (トラックごとの音源へ送る用)
    pub fn to_events_for_track(&self, track: usize, sample_rate: f64) -> Vec<SeqEvent> {
        self.collect_events(Some(track), sample_rate)
    }

    fn collect_events(&self, only_track: Option<usize>, sample_rate: f64) -> Vec<SeqEvent> {
        let spq = self.samples_per_quarter(sample_rate);
        // 鳴らすのは記譜位置ではなく演奏位置 (スウィングを乗せたもの)
        let performed = self.performed_notes();
        let mut events = Vec::with_capacity(performed.len() * 2);

        for note in &performed {
            if only_track.is_some_and(|track| note.track != track) {
                continue;
            }
            let Some(key) = note.key(self.scale) else {
                continue;
            };
            if note.duration <= 0.0 {
                continue;
            }
            let start = (note.start_tick.max(0.0) as f64 * spq) as u64;
            let end = (note.end_tick().max(0.0) as f64 * spq) as u64;
            events.push(SeqEvent {
                sample_time: start,
                key,
                velocity: note.velocity as f64 / 127.0,
                is_on: true,
            });
            events.push(SeqEvent {
                sample_time: end.max(start + 1),
                key,
                velocity: 0.0,
                is_on: false,
            });
        }

        // 同時刻ならオフを先に (false < true)
        events.sort_by_key(|e| (e.sample_time, e.is_on));
        events
    }
}

/// 再生エンジンに渡す、サンプル時刻の付いたノートイベント
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SeqEvent {
    /// シーケンス先頭からのサンプル位置
    pub sample_time: u64,
    /// MIDI ノート番号
    pub key: u8,
    /// 0.0..=1.0
    pub velocity: f64,
    /// true = NoteOn, false = NoteOff
    pub is_on: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn note(start: f32, dur: f32, semitone: i32, octave: i32) -> Note {
        Note {
            start_tick: start,
            duration: dur,
            semitone,
            octave,
            velocity: 100,
            track: 0,
            lane: 0,
        }
    }

    /// トラックと段の増減は、ノートを消してしまわないこと
    #[test]
    fn tracks_and_lanes_protect_existing_notes() {
        let mut editor = MidiEditor::default();
        assert_eq!(editor.track_count(), 1);
        assert_eq!(editor.total_rows(), DEFAULT_LANES);

        editor.add_track();
        assert_eq!(editor.track_count(), 2);
        assert_eq!(editor.total_rows(), DEFAULT_LANES * 2);

        // 空のトラックは消せる
        assert!(editor.remove_last_track());
        assert_eq!(editor.track_count(), 1);

        // 最後の1本は消せない
        assert!(!editor.remove_last_track());

        // ノートのある段は消せない
        editor.add_track();
        let last_lane = editor.lanes(1) - 1;
        editor.notes.push(Note {
            track: 1,
            lane: last_lane,
            ..note(0.0, 1.0, 0, 4)
        });
        assert!(!editor.remove_last_lane(1), "ノートのある段は残ること");
        assert!(!editor.remove_last_track(), "ノートのあるトラックは残ること");

        // 空の段は消せる
        editor.add_lane(1);
        assert!(editor.remove_last_lane(1));
        assert_eq!(editor.lanes(1), last_lane + 1);
    }

    /// ミュートとソロ: ソロがあればソロだけが鳴り、無ければミュート以外が鳴ること
    #[test]
    fn mute_and_solo_decide_audibility() {
        let mut editor = MidiEditor::default();
        editor.tracks = vec![TrackInfo::new(0), TrackInfo::new(1), TrackInfo::new(2)];

        // 既定では全部鳴る
        assert!((0..3).all(|track| editor.is_audible(track)));

        // ミュートしたトラックだけ鳴らない
        editor.tracks[1].muted = true;
        assert!(editor.is_audible(0));
        assert!(!editor.is_audible(1));
        assert!(editor.is_audible(2));

        // ソロがあればソロだけ (ミュートより優先)
        editor.tracks[2].soloed = true;
        assert!(!editor.is_audible(0));
        assert!(!editor.is_audible(1));
        assert!(editor.is_audible(2));

        // ミュートされたトラックをソロにしたら、それが鳴る
        editor.tracks[1].soloed = true;
        assert!(editor.is_audible(1), "ソロはミュートより優先されること");

        // ソロを解除すると元の判定に戻る
        editor.tracks[1].soloed = false;
        editor.tracks[2].soloed = false;
        assert!(editor.is_audible(0));
        assert!(!editor.is_audible(1));
    }

    /// 読み込んだノートが画面外に隠れないよう、トラックと段が広がること
    #[test]
    fn capacity_grows_for_imported_notes() {
        let mut editor = MidiEditor::default();
        editor.notes = vec![Note {
            track: 2,
            lane: 11,
            ..note(0.0, 1.0, 0, 4)
        }];

        editor.ensure_capacity_for_notes();
        assert_eq!(editor.track_count(), 3);
        assert_eq!(editor.lanes(2), 12);
        assert_eq!(editor.lanes(0), DEFAULT_LANES, "他のトラックは既定のまま");
    }

    /// 再生用イベントの生成が、設定した音階モードを実際に反映していること。
    /// (Note::key が正しくても to_events がモードを渡し忘れると再生だけズレるため)
    #[test]
    fn to_events_uses_scale_mode() {
        let mut editor = MidiEditor::default();
        // (0,4) と (9,4): 12平均律なら 60 と 69、B-P なら 60 と 69 (同オクターブ内は同じ)。
        // オクターブをまたぐ (0,5) で差が出る: 12平均律 72 / B-P 73。
        editor.notes = vec![note(0.0, 1.0, 0, 5)];

        editor.scale = ScaleMode::Equal12;
        let keys: Vec<u8> = editor.to_events(44100.0).iter().map(|e| e.key).collect();
        assert_eq!(keys, vec![72, 72], "12平均律で (0,5) は 72 になること");

        editor.scale = ScaleMode::BohlenPierce13;
        let keys: Vec<u8> = editor.to_events(44100.0).iter().map(|e| e.key).collect();
        assert_eq!(keys, vec![73, 73], "B-P で (0,5) は 73 になること");
    }

    /// スウィングを掛けた4/4のエディタ (トラック0 のみ ON)
    fn swung() -> MidiEditor {
        let mut editor = MidiEditor::default(); // 120BPM / 4/4
        editor.tracks[0].swing = true;
        editor
    }

    fn close(actual: f32, expected: f32) -> bool {
        (actual - expected).abs() < 1e-4
    }

    /// スウィングで前後のノートが繋がったままであること。
    ///
    /// これがこの実装の核心。開始と終端に同じオフセットを掛けているので、
    /// 前のノートの終端と次のノートの開始は一致し続ける。ずれると CeVIO の
    /// 単旋律パートで重なりや隙間になる。
    #[test]
    fn swing_keeps_notes_contiguous() {
        let mut editor = swung();
        // 拍頭→裏拍→次の拍頭→その裏拍 と繋がる8分音符4つ
        editor.notes = vec![
            note(0.0, 0.5, 0, 4),
            note(0.5, 0.5, 0, 4),
            note(1.0, 0.5, 0, 4),
            note(1.5, 0.5, 0, 4),
        ];

        let played = editor.performed_notes();
        for pair in played.windows(2) {
            assert!(
                close(pair[0].end_tick(), pair[1].start_tick),
                "{} で終わり {} で始まっている",
                pair[0].end_tick(),
                pair[1].start_tick
            );
        }
        // 拍頭は遅れ、裏拍は比の位置へ
        assert!(close(played[0].start_tick, 94.0 / 960.0), "表拍の遅れ");
        assert!(close(played[1].start_tick, 0.566_724), "裏拍の位置");
    }

    /// 表拍が遅れることで、跳ねた8分の音価が前後で変わること
    #[test]
    fn swing_makes_the_first_eighth_longer() {
        let mut editor = swung();
        editor.notes = vec![note(0.0, 0.5, 0, 4), note(0.5, 0.5, 0, 4)];

        let played = editor.performed_notes();
        assert!(
            played[0].duration < played[1].duration,
            "表拍が遅れるぶん、前の8分は短くなる: {} / {}",
            played[0].duration,
            played[1].duration
        );
    }

    /// 極端に短いノートでも音価が負にならないこと。
    /// (表拍の遅れが音価を追い越すため)
    #[test]
    fn swing_never_produces_negative_duration() {
        let mut editor = swung();
        editor.notes = vec![note(0.0, 1.0 / 16.0, 0, 4)]; // 64分音符

        let played = editor.performed_notes();
        assert!(played[0].duration > 0.0, "実際 {}", played[0].duration);
    }

    /// 小節線ちょうどで終わるノートが、記譜上の終端を超えないこと。
    ///
    /// 超えるとノートオフがトランスポートの終端フラッシュから漏れ、
    /// 音が鳴りっぱなしになる。
    #[test]
    fn swing_keeps_the_last_note_inside_the_bar() {
        let mut editor = swung();
        editor.notes = vec![note(3.0, 1.0, 0, 4)]; // 4/4 の4拍目、小節線で終わる

        let limit = editor.length_quarters_bar_aligned();
        assert_eq!(limit, 4.0);
        let played = editor.performed_notes();
        assert!(
            played[0].end_tick() <= limit,
            "終端 {} が {} を超えている",
            played[0].end_tick(),
            limit
        );
    }

    /// 三連符の刻みが均等のまま保たれること (2音目・3音目は動かない)。
    ///
    /// 音価は均等にならない。3音目の終端は次の拍頭にあたるので、
    /// 表拍の遅れぶんだけ伸びる。刻みが揃っていることが要件。
    #[test]
    fn swing_leaves_triplets_even() {
        let mut editor = swung();
        let third = 1.0 / 3.0;
        editor.notes = vec![
            note(third, third, 0, 4),
            note(third * 2.0, third, 0, 4),
        ];

        let played = editor.performed_notes();
        assert!(close(played[0].start_tick, third), "2音目は動かないこと");
        assert!(close(played[1].start_tick, third * 2.0), "3音目も動かないこと");
        assert!(
            close(played[1].start_tick - played[0].start_tick, third),
            "刻みが均等のままであること"
        );
        assert!(
            close(played[1].end_tick(), 1.0 + swing::downbeat_delay(editor.tempo)),
            "3音目は遅れた次の拍頭まで伸びること: {}",
            played[1].end_tick()
        );
    }

    /// 音価0のノートはスウィングを掛けても音価0のままであること。
    ///
    /// 最小音価の下限を掛けてしまうと、各出力の「音価0は書き出さない」判定を
    /// すり抜けて、スウィングの有無で結果が変わってしまう。
    #[test]
    fn swing_leaves_empty_notes_empty() {
        let mut editor = swung();
        editor.notes = vec![note(0.0, 0.0, 0, 4)];

        let played = editor.performed_notes();
        assert_eq!(played[0], editor.notes[0], "何も変えないこと");
        assert!(editor.to_events(48_000.0).is_empty(), "イベントも出ないこと");
    }

    /// 再生用のイベントにスウィングが乗ること
    #[test]
    fn events_carry_swing() {
        let mut editor = swung();
        editor.notes = vec![note(0.0, 0.5, 0, 4)];

        let rate = 48_000.0;
        let spq = editor.samples_per_quarter(rate);
        let events = editor.to_events(rate);

        let expected = (swing::downbeat_delay(editor.tempo) as f64 * spq) as u64;
        assert!(expected > 0, "拍頭が遅れる設定であること");
        assert_eq!(events[0].sample_time, expected, "ノートオンが遅れること");
    }

    /// スウィングを掛けても、イベントが記譜上の終端を超えないこと。
    ///
    /// 超えるとトランスポートの終端フラッシュ (`sample_time > end_sample` で
    /// 打ち切り) から漏れ、ノートオフが出ずに音が鳴りっぱなしになる。
    /// 「最後のノートが小節線ちょうどで終わる」のは最も普通の書き方なので、
    /// 塞いでおかないと高確率で踏む。
    #[test]
    fn swung_events_stay_within_the_sequence_end() {
        let mut editor = swung();
        editor.notes = vec![note(0.0, 1.0, 0, 4), note(3.0, 1.0, 4, 4)];

        let rate = 48_000.0;
        let spq = editor.samples_per_quarter(rate);
        let end_sample = (editor.length_quarters_bar_aligned() as f64 * spq) as u64;

        for event in editor.to_events(rate) {
            assert!(
                event.sample_time <= end_sample,
                "{} が終端 {} を超えている",
                event.sample_time,
                end_sample
            );
        }
    }

    /// 三連符の真ん中を削って隙間が空いた場合。
    ///
    /// 3音目 (2/3) は表拍でも裏拍でもないので動かず、拍頭だけが遅れる。
    /// 隙間の長さは変わらず、重なりも生じない。
    #[test]
    fn swing_handles_a_triplet_with_its_middle_removed() {
        let mut editor = swung();
        let third = 1.0 / 3.0;
        editor.notes = vec![note(0.0, third, 0, 4), note(third * 2.0, third, 0, 4)];

        let played = editor.performed_notes();
        let delay = swing::downbeat_delay(editor.tempo);

        assert!(close(played[0].start_tick, delay), "拍頭は遅れる");
        assert!(close(played[0].end_tick(), third), "終端は動かない");
        assert!(close(played[1].start_tick, third * 2.0), "3音目は動かない");

        assert!(played[0].duration > 0.0 && played[1].duration > 0.0);
        assert!(
            played[0].end_tick() <= played[1].start_tick,
            "重ならないこと"
        );
        assert!(
            close(played[1].start_tick - played[0].end_tick(), third),
            "隙間の長さが変わらないこと"
        );
    }

    /// 三連符の真ん中を消して1音目を伸ばした場合 (跳ねた8分を手書きした形)。
    ///
    /// 拍頭だけが遅れ、3音目は動かないので、前後は繋がったまま。
    #[test]
    fn swing_handles_a_triplet_written_as_a_swung_pair() {
        let mut editor = swung();
        let third = 1.0 / 3.0;
        editor.notes = vec![
            note(0.0, third * 2.0, 0, 4), // 真ん中まで伸ばした1音目
            note(third * 2.0, third, 0, 4),
        ];

        let played = editor.performed_notes();
        assert!(
            close(played[0].end_tick(), played[1].start_tick),
            "繋がったままであること"
        );
        assert!(played[0].duration > 0.0 && played[1].duration > 0.0);
    }

    /// 同じ「跳ねた8分」でも、書き方によって実際に鳴る比が変わること。
    ///
    /// - **8分の等分 + スウィング**: 拍頭が遅れ、裏拍も比のぶん遅れる
    /// - **三連符の真ん中抜き**: 3音目は動かず、拍頭だけが遅れる
    ///
    /// 拍頭の遅れは共通だが、裏拍にあたる音を動かすかどうかで結果が分かれる。
    /// 前者は「短→長」、後者は「長→短」になる。どちらが望ましいかは
    /// 音楽的な判断なので、ここでは現状を数値で固定するに留める。
    #[test]
    fn swing_result_depends_on_how_the_figure_is_written() {
        let third = 1.0 / 3.0;

        // 鳴っている間隔の比 (前の音の始まり→次の音の始まり : 次の音→その次の拍頭)
        let sounding_ratio = |notes: Vec<Note>| {
            let mut editor = swung();
            editor.notes = notes;
            let played = editor.performed_notes();
            let front = played[1].start_tick - played[0].start_tick;
            let back = played[1].end_tick() - played[1].start_tick;
            front / back
        };

        let even_eighths = sounding_ratio(vec![note(0.0, 0.5, 0, 4), note(0.5, 0.5, 0, 4)]);
        let written_triplet = sounding_ratio(vec![
            note(0.0, third * 2.0, 0, 4),
            note(third * 2.0, third, 0, 4),
        ]);

        assert!(
            close(even_eighths, 0.8826),
            "8分の等分 + スウィング: {even_eighths}"
        );
        assert!(
            close(written_triplet, 1.3188),
            "三連符の真ん中抜き: {written_triplet}"
        );
    }

    /// スウィングが OFF のトラックは1 tick も動かないこと。
    /// (伴奏とソロを同時に鳴らして検証するための要)
    #[test]
    fn swing_only_touches_enabled_tracks() {
        let mut editor = swung();
        editor.add_track(); // トラック1 は OFF のまま
        editor.notes = vec![
            note(0.0, 0.5, 0, 4),
            Note {
                track: 1,
                ..note(0.0, 0.5, 0, 4)
            },
        ];

        let played = editor.performed_notes();
        assert!(played[0].start_tick > 0.0, "ソロは遅れること");
        assert_eq!(played[1].start_tick, 0.0, "伴奏は動かないこと");
        assert_eq!(played[1].duration, 0.5);
    }

    /// N/4 以外の拍子では何も起きないこと
    #[test]
    fn swing_is_limited_to_quarter_note_beats() {
        let mut editor = swung();
        editor.beats = 6;
        editor.beat_type = 8;
        editor.notes = vec![note(0.0, 0.5, 0, 4), note(0.5, 0.5, 0, 4)];

        assert_eq!(editor.performed_notes(), editor.notes);
    }

    /// 強さ 1.0 では裏拍が動かないこと (表拍の遅れは残る)
    #[test]
    fn lowest_strength_keeps_offbeats_straight() {
        let mut editor = swung();
        editor.swing_peak_ratio = 1.0;
        editor.notes = vec![note(0.5, 0.5, 0, 4)];

        assert!(close(editor.performed_notes()[0].start_tick, 0.5));
    }

    /// 周波数が基準の音高と一致すること。
    /// (CCS の LogF0 はこの値をそのまま書き出すので、ここがずれると音痴になる)
    #[test]
    fn frequency_matches_the_reference_pitches() {
        let eq = ScaleMode::Equal12;
        assert!((eq.frequency(9, 4) - 440.0).abs() < 1e-9, "A4 = 440Hz");
        assert!((eq.frequency(0, 4) - 261.625_565).abs() < 1e-4, "C4");
        assert!((eq.frequency(9, 5) - 880.0).abs() < 1e-9, "1オクターブで倍");
        // B-P の基準キー63 = (半音3, オクターブ4)
        let bp = ScaleMode::BohlenPierce13;
        assert!((bp.frequency(3, 4) - 311.127).abs() < 1e-9, "基準は 311.127Hz");
    }

    /// ボーレン・ピアースの1オクターブ表記が、実際にはトライターブ (3:1) であること
    #[test]
    fn bohlen_pierce_spans_a_tritave() {
        let bp = ScaleMode::BohlenPierce13;
        let low = bp.frequency(0, 4);
        let high = bp.frequency(0, 5);
        assert!(
            (high / low - 3.0).abs() < 1e-9,
            "オクターブ違いは3倍になること (実測 {})",
            high / low
        );
        // 1ステップは 3^(1/13) ≒ 146.3 セント (半音より広い)
        let step = bp.frequency(1, 4) / bp.frequency(0, 4);
        assert!((step - 3f64.powf(1.0 / 13.0)).abs() < 1e-9);
    }

    #[test]
    fn key_conversion() {
        let eq = ScaleMode::Equal12;
        assert_eq!(note(0.0, 1.0, 0, 4).key(eq), Some(60)); // C4
        assert_eq!(note(0.0, 1.0, 9, 4).key(eq), Some(69)); // A4
        assert_eq!(note(0.0, 1.0, 0, -2).key(eq), None); // key -12 は範囲外
        assert_eq!(note(0.0, 1.0, 11, 8).key(eq), Some(119)); // B8
    }

    /// ボーレン・ピアースは13ステップで1オクターブ (トライターブ) 進む。
    /// (3,4) が平均律の E♭4 と同じノート番号 63 になることが要件。
    #[test]
    fn bohlen_pierce_key_conversion() {
        let bp = ScaleMode::BohlenPierce13;
        assert_eq!(note(0.0, 1.0, 0, 4).key(bp), Some(60)); // 基準は中央ハと共通
        assert_eq!(note(0.0, 1.0, 3, 4).key(bp), Some(63)); // E♭4 と同じノート番号
        assert_eq!(note(0.0, 1.0, 12, 4).key(bp), Some(72)); // 13音目
        assert_eq!(note(0.0, 1.0, 0, 5).key(bp), Some(73)); // 次のトライターブ
        assert_eq!(note(0.0, 1.0, 0, 3).key(bp), Some(47)); // 下のトライターブ
    }

    #[test]
    fn events_sorted_off_before_on() {
        let mut editor = MidiEditor::default(); // 120bpm → 四分音符 = 22050 samples @44.1k
        editor.notes = vec![note(0.0, 1.0, 0, 4), note(1.0, 1.0, 4, 4)];
        let events = editor.to_events(44100.0);

        assert_eq!(events.len(), 4);
        // 22050 サンプル地点: C4 オフが E4 オンより先
        assert_eq!(events[1].sample_time, 22050);
        assert!(!events[1].is_on);
        assert_eq!(events[2].sample_time, 22050);
        assert!(events[2].is_on);
    }

    #[test]
    fn bar_length_respects_time_signature() {
        let editor = MidiEditor {
            beats: 6,
            beat_type: 8,
            ..Default::default()
        };
        assert_eq!(editor.quarters_per_bar(), 3.0); // 6/8 = 四分音符3個分
    }
}
