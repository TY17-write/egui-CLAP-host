//! シーケンサーのデータモデル。
//! 時間の単位はすべて「四分音符 = 1.0」の実数 (tick と呼ぶ)。

/// 音階モード。1オクターブを何ステップに分けるかを決める。
///
/// ホストが変えるのは MIDI ノート番号だけで、実際の音高は
/// プラグイン側の音律設定 (Scala の .scl ファイルなど) が決める。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ScaleMode {
    /// 12平均律。半音 0..=11。
    Equal12,
    /// ボーレン・ピアース音階。トライターブ (3:1) を13等分。半音 0..=12。
    BohlenPierce13,
}

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
        let key = 60 + (self.octave - 4) * scale.steps_per_octave() + self.semitone;
        u8::try_from(key).ok().filter(|k| *k <= 127)
    }

    /// "(半音, オクターブ)" 形式の表示名。例: (0,4)
    pub fn name(&self) -> String {
        format!("({},{})", self.semitone, self.octave)
    }

}

/// 新しいトラックが最初に持つ段数
pub const DEFAULT_LANES: usize = 8;

/// トラック1本ぶんの情報。音源 (プラグイン) はホスト側が別に持つ。
#[derive(Clone, Debug, PartialEq)]
pub struct TrackInfo {
    pub name: String,
    /// このトラックが持つ段数 (1 以上)
    pub lanes: usize,
}

impl TrackInfo {
    pub fn new(index: usize) -> Self {
        Self {
            name: format!("トラック {}", index + 1),
            lanes: DEFAULT_LANES,
        }
    }
}

/// シーケンス全体 (エディタが編集する対象)
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

    /// ノート列をサンプル時刻付きイベント列に変換する。
    /// 同時刻ではノートオフがノートオンより先に来るようソートされる。
    pub fn to_events(&self, sample_rate: f64) -> Vec<SeqEvent> {
        let spq = self.samples_per_quarter(sample_rate);
        let mut events = Vec::with_capacity(self.notes.len() * 2);

        for note in &self.notes {
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
