//! CeVIO のプロジェクトファイル (.ccs) への書き出し。
//!
//! **1トラック目だけ**を対象に、段ごとに1パート (Unit + Group) を作る。
//! CeVIO のソング Unit は単旋律しか歌えないので、和音を置くための「段」を
//! そのままパートの分割単位にしている。
//!
//! 音高の扱いは音律によって変わる。
//!
//! - **12平均律**: CeVIO のノート (PitchStep / PitchOctave) だけで正確に表せるので、
//!   ピッチ (LogF0) は書かない。CeVIO 側が自前のピッチカーブを付けるため自然に歌う。
//! - **ボーレン・ピアース**: 1ステップが 146.3 セントで12平均律の格子に乗らない。
//!   最寄りの12平均律音をノートに書き、実際の周波数を LogF0 で与えて補正する。
//!   ノートごとに平坦な値を置くので、歌い方は機械的になる。
//!
//! XML の構造と各属性の要否は、実際に CeVIO が保存した .ccs から割り出したもの。
//! 「無いと壊れる」ものにはその旨をコメントで残してある。

use crate::sequencer::{MidiEditor, Note, ScaleMode};
use quick_xml::events::{BytesDecl, BytesEnd, BytesStart, BytesText, Event};
use quick_xml::Writer;
use std::io::{Cursor, Write};
use uuid::Uuid;

/// 書き出し対象のトラック (1トラック目)
const TARGET_TRACK: usize = 0;

/// CeVIO の四分音符あたりのティック数
const TICKS_PER_QUARTER: f64 = 960.0;

/// パラメータ (LogF0) の1フレームの長さ (秒)
const FRAME_SECONDS: f64 = 0.005;

/// 歌詞。音程だけを扱うので固定にしている
const LYRIC: &str = "ラ";

/// Scenario の Code。他人の .ccs でも同じ値が入っている
const SCENARIO_CODE: &str = "7251BC4B6168E7B2992FA620BD3E1E77";

/// 書き出し結果
pub struct Exported {
    pub bytes: Vec<u8>,
    /// パート数 (= ノートのある段の数)
    pub parts: usize,
    /// 書き出したノート数
    pub notes: usize,
    /// 音域外などで除外したノート数
    pub skipped: usize,
}

/// 1パート (CeVIO の Unit ひとつ) 分のノート
struct Part {
    /// 元になった段 (表示名に使う)
    lane: usize,
    /// Unit と Group を結びつける UUID
    group_id: String,
    /// 開始順に並んだノート
    notes: Vec<Note>,
}

/// シーケンスを .ccs のバイト列にする。
///
/// 1トラック目にノートが無ければエラー。ミュート/ソロは見ない
/// (MIDI エクスポートと同じく、鳴らすためではなくデータを渡すための書き出しのため)。
pub fn export(editor: &MidiEditor) -> Result<Exported, String> {
    let (parts, skipped) = collect_parts(editor);
    if parts.is_empty() {
        return Err("1トラック目にノートがありません。\n\
                    CCS へ書き出せるのは1トラック目だけです。"
            .into());
    }

    let notes = parts.iter().map(|part| part.notes.len()).sum();
    let bytes = write_document(editor, &parts).map_err(|e| format!("XML を組み立てられません: {e}"))?;

    Ok(Exported {
        bytes,
        parts: parts.len(),
        notes,
        skipped,
    })
}

/// 1トラック目のノートを段ごとに仕分ける。戻り値は (パート列, 除外したノート数)。
/// ノートの無い段は Unit を作らない (空のパートが CeVIO に並ぶのを避ける)。
fn collect_parts(editor: &MidiEditor) -> (Vec<Part>, usize) {
    let mut parts = Vec::new();
    let mut skipped = 0;

    for lane in 0..editor.lanes(TARGET_TRACK) {
        let mut notes: Vec<Note> = Vec::new();
        for note in &editor.notes {
            if note.track != TARGET_TRACK || note.lane != lane {
                continue;
            }
            if note.duration <= 0.0 || !is_writable(editor.scale, note) {
                skipped += 1;
                continue;
            }
            notes.push(*note);
        }

        if notes.is_empty() {
            continue;
        }
        notes.sort_by(|a, b| a.start_tick.total_cmp(&b.start_tick));
        parts.push(Part {
            lane,
            group_id: Uuid::new_v4().to_string(),
            notes,
        });
    }

    (parts, skipped)
}

/// .ccs 全体を組み立てる
fn write_document(editor: &MidiEditor, parts: &[Part]) -> std::io::Result<Vec<u8>> {
    let mut w = Writer::new_with_indent(Cursor::new(Vec::new()), b' ', 2);

    w.write_event(Event::Decl(BytesDecl::new("1.0", Some("utf-8"), None)))?;

    start(&mut w, "Scenario", &[("Code", SCENARIO_CODE)])?;
    start(&mut w, "Sequence", &[("Id", "")])?;
    start(&mut w, "Scene", &[("Id", "")])?;

    start(&mut w, "Units", &[])?;
    for part in parts {
        write_unit(&mut w, editor, part)?;
    }
    end(&mut w, "Units")?;

    start(&mut w, "Groups", &[])?;
    for part in parts {
        write_group(&mut w, editor, part, parts.len())?;
    }
    end(&mut w, "Groups")?;

    // 用途は不明だが必須。値は CeVIO が保存した既定値に倣いつつ、拍子とテンポは合わせる
    let rhythm = format!("{}/{}", editor.beats, editor.beat_type);
    let tempo = editor.tempo.to_string();
    empty(
        &mut w,
        "SoundSetting",
        &[
            ("Rhythm", &rhythm),
            ("Tempo", &tempo),
            ("MasterVolume", "0"),
        ],
    )?;

    for tag in ["Scene", "Sequence", "Scenario"] {
        end(&mut w, tag)?;
    }

    Ok(w.into_inner().into_inner())
}

/// パート1つぶんの Unit (ノートとピッチの中身)
fn write_unit<W: Write>(w: &mut Writer<W>, editor: &MidiEditor, part: &Part) -> std::io::Result<()> {
    start(
        w,
        "Unit",
        &[
            ("Version", ""),
            ("Id", ""),
            ("Category", "SingerSong"), // 必須
            ("Group", &part.group_id),  // Groups 側と同じ UUID を指す
            ("StartTime", "00:00:00"),  // 無いと再生できなくなる
            ("Duration", ""),           // 無くてもよい
            ("CastId", ""),             // 空ならソングボイスが自動で割り当てられる
            ("Language", ""),           // 空なら既定の言語
        ],
    )?;

    // Song と CommonKeys が無いとノートが消える
    start(w, "Song", &[("Version", ""), ("CommonKeys", "true")])?;

    start(w, "Tempo", &[])?;
    empty(
        w,
        "Sound",
        &[("Clock", "0"), ("Tempo", &editor.tempo.to_string())],
    )?;
    end(w, "Tempo")?;

    // 途中の拍子変更には未対応 (先頭の1つだけ)
    start(w, "Beat", &[])?;
    empty(
        w,
        "Time",
        &[
            ("Clock", "0"),
            ("Beats", &editor.beats.to_string()),
            ("BeatType", &editor.beat_type.to_string()),
        ],
    )?;
    end(w, "Beat")?;

    write_score(w, editor, part)?;
    if needs_log_f0(editor.scale) {
        write_log_f0(w, editor, part)?;
    }

    end(w, "Song")?;
    end(w, "Unit")
}

/// ノートの配置
fn write_score<W: Write>(
    w: &mut Writer<W>,
    editor: &MidiEditor,
    part: &Part,
) -> std::io::Result<()> {
    let delay = blank_bar_quarters(editor);

    start(w, "Score", &[])?;
    for note in &part.notes {
        let (step, octave) = pitch_step_octave(written_key(editor.scale, note));
        let clock = ((note.start_tick as f64 + delay) * TICKS_PER_QUARTER).round() as u64;
        let duration = (note.duration as f64 * TICKS_PER_QUARTER).round().max(1.0) as u64;

        empty(
            w,
            "Note",
            &[
                ("Clock", &clock.to_string()),
                ("PitchStep", &step.to_string()),
                ("PitchOctave", &octave.to_string()),
                ("Duration", &duration.to_string()),
                ("Lyric", LYRIC),
            ],
        )?;
    }
    end(w, "Score")
}

/// ピッチ (LogF0) の書き出し。12平均律では呼ばない。
///
/// ノートの区間だけに平坦な値を置く。ノート間には何も置かないので、
/// そこは CeVIO 側の扱いに任せる。
fn write_log_f0<W: Write>(
    w: &mut Writer<W>,
    editor: &MidiEditor,
    part: &Part,
) -> std::io::Result<()> {
    let delay = blank_bar_quarters(editor);
    let frames = frames_per_quarter(editor);

    // 全ノートの終端の最大値。開始順に並んでいても音価はまちまちなので max を取る
    let length = part
        .notes
        .iter()
        .map(|note| (note.end_tick() as f64 + delay) * frames)
        .fold(0.0, f64::max)
        .ceil() as u64;

    start(w, "Parameter", &[])?;
    start(w, "LogF0", &[("Length", &length.to_string())])?;

    for note in &part.notes {
        let index = ((note.start_tick as f64 + delay) * frames).round() as u64;
        let repeat = (note.duration as f64 * frames).round() as u64;
        if repeat == 0 {
            continue;
        }
        // CeVIO のピッチは周波数の自然対数
        let log_f0 = note.frequency(editor.scale).ln();
        text(
            w,
            "Data",
            &log_f0.to_string(),
            &[
                ("Index", &index.to_string()),
                ("Repeat", &repeat.to_string()),
            ],
        )?;
    }

    end(w, "LogF0")?;
    end(w, "Parameter")
}

/// パートに対応する Group。Unit の Group 属性と UUID を一致させる。
fn write_group<W: Write>(
    w: &mut Writer<W>,
    editor: &MidiEditor,
    part: &Part,
    total_parts: usize,
) -> std::io::Result<()> {
    let track_name = editor
        .tracks
        .get(TARGET_TRACK)
        .map_or("ソング", |info| info.name.as_str());
    // 段が1つならトラック名だけ。複数あるときだけ段番号を添える
    let name = if total_parts > 1 {
        format!("{} 段{}", track_name, part.lane + 1)
    } else {
        track_name.to_string()
    };

    empty(
        w,
        "Group",
        &[
            ("Version", ""),
            ("Id", &part.group_id),     // Unit の Group 属性と一致させる
            ("Category", "SingerSong"), // 無いとエラーになる
            ("Name", &name),
            ("Color", "#ffffffff"), // 無いと音符が再配置されピッチ情報が失われる
            ("Volume", "0"),
            ("Pan", "0"),
            ("IsSolo", "false"),
            ("IsMuted", "false"),
            ("CastId", ""),
            ("Language", "Japanese"),
        ],
    )
}

/// CeVIO では1小節目が空白扱いになるので、その分だけ後ろへずらす (四分音符単位)
fn blank_bar_quarters(editor: &MidiEditor) -> f64 {
    editor.quarters_per_bar() as f64
}

/// 四分音符あたりのパラメータフレーム数。
/// 1拍 = 60/BPM 秒、1フレーム = 5ms なので 12000/BPM になる。
fn frames_per_quarter(editor: &MidiEditor) -> f64 {
    (60.0 / editor.tempo.max(1) as f64) / FRAME_SECONDS
}

/// CeVIO のノートに書く MIDI ノート番号。
///
/// 12平均律はキー番号がそのまま使える。ボーレン・ピアースは格子に乗らないので
/// 最寄りの12平均律音にする (ズレは必ず ±50 セント以内で、LogF0 が補正する)。
fn written_key(scale: ScaleMode, note: &Note) -> i32 {
    match scale {
        ScaleMode::Equal12 => scale.key_number(note.semitone, note.octave),
        ScaleMode::BohlenPierce13 => nearest_equal_key(note.frequency(scale)),
    }
}

/// CeVIO のノートとして置けるか。
///
/// 元のキー番号と、実際に書き込む12平均律のノート番号の**両方**が MIDI の音域に
/// 収まる必要がある。ボーレン・ピアースは1ステップが 146.3 セントと広いため、
/// キー番号が 0..=127 でも書き込む音が振り切れることがある
/// (例: キー127 は 69.5kHz 相当で、12平均律換算だと 157 になる)。
fn is_writable(scale: ScaleMode, note: &Note) -> bool {
    note.key(scale).is_some() && (0..=127).contains(&written_key(scale, note))
}

/// ピッチ (LogF0) を明示する必要があるか。
/// 12平均律は CeVIO のノートだけで正確に表せるので要らない。
fn needs_log_f0(scale: ScaleMode) -> bool {
    !matches!(scale, ScaleMode::Equal12)
}

/// 周波数にいちばん近い12平均律の MIDI ノート番号
fn nearest_equal_key(frequency: f64) -> i32 {
    (69.0 + 12.0 * (frequency / 440.0).log2()).round() as i32
}

/// MIDI ノート番号を CeVIO の (PitchStep, PitchOctave) に分ける。
/// 負のノート番号でも破綻しないよう剰余は rem_euclid を使う。
fn pitch_step_octave(key: i32) -> (i32, i32) {
    (key.rem_euclid(12), key.div_euclid(12) - 1)
}

// ---- quick-xml の薄いラッパ ----

fn start<W: Write>(w: &mut Writer<W>, name: &str, attrs: &[(&str, &str)]) -> std::io::Result<()> {
    let mut tag = BytesStart::new(name);
    for (key, value) in attrs {
        tag.push_attribute((*key, *value));
    }
    w.write_event(Event::Start(tag))
}

fn empty<W: Write>(w: &mut Writer<W>, name: &str, attrs: &[(&str, &str)]) -> std::io::Result<()> {
    let mut tag = BytesStart::new(name);
    for (key, value) in attrs {
        tag.push_attribute((*key, *value));
    }
    w.write_event(Event::Empty(tag))
}

fn text<W: Write>(
    w: &mut Writer<W>,
    name: &str,
    body: &str,
    attrs: &[(&str, &str)],
) -> std::io::Result<()> {
    start(w, name, attrs)?;
    w.write_event(Event::Text(BytesText::new(body)))?;
    end(w, name)
}

fn end<W: Write>(w: &mut Writer<W>, name: &str) -> std::io::Result<()> {
    w.write_event(Event::End(BytesEnd::new(name)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sequencer::TrackInfo;

    fn note(start_tick: f32, duration: f32, semitone: i32, octave: i32, lane: usize) -> Note {
        Note {
            start_tick,
            duration,
            semitone,
            octave,
            velocity: 100,
            track: 0,
            lane,
        }
    }

    fn xml(editor: &MidiEditor) -> String {
        String::from_utf8(export(editor).unwrap().bytes).unwrap()
    }

    /// n 番目の `<Note>` タグから属性を取り出す。
    /// Clock などは Sound / Time にも付くので、タグを絞ってから読む。
    fn note_attr(xml: &str, name: &str, nth: usize) -> String {
        let (at, _) = xml.match_indices("<Note ").nth(nth).expect("Note タグ");
        let rest = &xml[at..];
        attr(&rest[..rest.find("/>").expect("閉じタグ")], name, 0)
    }

    /// 属性 `name="..."` の値を n 番目の出現から取り出す
    fn attr(xml: &str, name: &str, nth: usize) -> String {
        let needle = format!("{name}=\"");
        xml.match_indices(&needle)
            .nth(nth)
            .map(|(at, _)| {
                let rest = &xml[at + needle.len()..];
                rest[..rest.find('"').unwrap()].to_string()
            })
            .unwrap_or_else(|| panic!("{name} の {nth} 番目が見つかりません"))
    }

    /// 12平均律ではピッチを書かない (CeVIO 側の自然なピッチカーブに任せる)
    #[test]
    fn equal_temperament_omits_log_f0() {
        let mut editor = MidiEditor::default();
        editor.notes = vec![note(0.0, 1.0, 9, 4, 0)];

        let out = xml(&editor);
        assert!(!out.contains("LogF0"), "LogF0 を書かないこと");
        assert!(!out.contains("Parameter"), "Parameter ごと書かないこと");
        // A4 はそのまま PitchStep 9 / PitchOctave 4
        assert_eq!(note_attr(&out, "PitchStep", 0), "9");
        assert_eq!(note_attr(&out, "PitchOctave", 0), "4");
    }

    /// ボーレン・ピアースは最寄りの12平均律音 + LogF0 で表すこと
    #[test]
    fn bohlen_pierce_writes_log_f0_at_the_reference_pitch() {
        let mut editor = MidiEditor::default();
        editor.scale = ScaleMode::BohlenPierce13;
        editor.notes = vec![note(0.0, 1.0, 3, 4, 0)]; // 基準キー63 = 311.127Hz

        let out = xml(&editor);
        // 311.127Hz は 12平均律の E♭4 = キー63 → step 3 / octave 4
        assert_eq!(note_attr(&out, "PitchStep", 0), "3");
        assert_eq!(note_attr(&out, "PitchOctave", 0), "4");

        let body = out
            .split("<Data")
            .nth(1)
            .and_then(|s| s.split('>').nth(1))
            .and_then(|s| s.split('<').next())
            .expect("Data の中身");
        let log_f0: f64 = body.parse().expect("数値であること");
        assert!(
            (log_f0 - 311.127_f64.ln()).abs() < 1e-9,
            "ln(311.127) を書くこと (実際 {log_f0})"
        );
    }

    /// ボーレン・ピアースの音がズレたぶんだけ LogF0 で補正されること。
    /// (半音4, オクターブ4) は12平均律の格子から外れる。
    #[test]
    fn bohlen_pierce_off_grid_note_keeps_its_true_frequency() {
        let scale = ScaleMode::BohlenPierce13;
        let target = scale.frequency(4, 4); // 311.127 * 3^(1/13)
        let written = nearest_equal_key(target);
        let written_freq = 440.0 * 2f64.powf((written as f64 - 69.0) / 12.0);

        // ノートに書ける音は最大でも半音の半分しかずれない
        let cents = 1200.0 * (target / written_freq).log2();
        assert!(cents.abs() <= 50.0, "ズレは ±50 セント以内 (実際 {cents})");
        // ズレていること自体が、LogF0 が要る理由
        assert!(cents.abs() > 1.0, "格子から外れていること");
    }

    /// CeVIO は1小節目が空白なので、拍子に応じてノートが後ろへずれること
    #[test]
    fn notes_start_after_a_blank_bar() {
        let mut editor = MidiEditor::default(); // 4/4
        editor.notes = vec![note(0.0, 1.0, 0, 4, 0)];
        // 4拍 * 960 = 3840
        assert_eq!(note_attr(&xml(&editor), "Clock", 0), "3840");

        // 3/8 は1小節 = 四分音符1.5個 → 1440
        editor.beats = 3;
        editor.beat_type = 8;
        assert_eq!(note_attr(&xml(&editor), "Clock", 0), "1440");
    }

    /// LogF0 のフレーム位置がテンポに追従すること。
    /// (元の実装は 60BPM 決め打ちで、他のテンポだと位置が倍/半分にずれていた)
    #[test]
    fn log_f0_frames_follow_the_tempo() {
        let mut editor = MidiEditor::default();
        editor.scale = ScaleMode::BohlenPierce13;
        editor.notes = vec![note(0.0, 1.0, 3, 4, 0)];

        // 120BPM: 四分音符 = 0.5秒 = 100フレーム。空白1小節(4拍)ぶん後ろ
        editor.tempo = 120;
        let out = xml(&editor);
        assert_eq!(attr(&out, "Index", 0), "400");
        assert_eq!(attr(&out, "Repeat", 0), "100");
        assert_eq!(attr(&out, "Length", 0), "500", "終端 = 5拍ぶん");

        // 60BPM なら倍
        editor.tempo = 60;
        let out = xml(&editor);
        assert_eq!(attr(&out, "Index", 0), "800");
        assert_eq!(attr(&out, "Repeat", 0), "200");
    }

    /// Length は「最後に置いたノート」ではなく、いちばん遅い終端で決まること
    #[test]
    fn log_f0_length_uses_the_latest_end() {
        let mut editor = MidiEditor::default();
        editor.scale = ScaleMode::BohlenPierce13;
        editor.tempo = 120;
        // 長いノートのあとに短いノートを置く (並び順に釣られないことの確認)
        editor.notes = vec![note(0.0, 4.0, 3, 4, 0), note(1.0, 0.5, 4, 4, 0)];

        // 終端は 4拍(空白) + 4拍 = 8拍 = 800フレーム
        assert_eq!(attr(&xml(&editor), "Length", 0), "800");
    }

    /// 段ごとに Unit と Group が分かれ、UUID で正しく結ばれること
    #[test]
    fn lanes_become_separate_parts() {
        let mut editor = MidiEditor::default();
        editor.tracks[0].lanes = 2;
        editor.notes = vec![note(0.0, 1.0, 0, 4, 0), note(0.0, 1.0, 4, 4, 1)];

        let out = xml(&editor);
        assert_eq!(out.matches("<Unit ").count(), 2, "段ごとに Unit");
        assert_eq!(out.matches("<Group ").count(), 2, "段ごとに Group");

        // Unit の Group 属性と Group の Id が対応していること
        let unit_groups = [attr(&out, "Group", 0), attr(&out, "Group", 1)];
        assert_ne!(unit_groups[0], unit_groups[1], "UUID は段ごとに別");
        for id in &unit_groups {
            assert!(out.contains(&format!("Id=\"{id}\"")), "Group 側にも {id} が要る");
        }

        assert!(out.contains("段1") && out.contains("段2"), "段番号が名前に入る");
    }

    /// ノートの無い段は Unit を作らないこと (空パートが並ぶのを避ける)
    #[test]
    fn empty_lanes_are_skipped() {
        let mut editor = MidiEditor::default();
        editor.tracks[0].lanes = 3;
        editor.notes = vec![note(0.0, 1.0, 0, 4, 2)]; // 3段目だけ

        let out = xml(&editor);
        assert_eq!(out.matches("<Unit ").count(), 1);
        // 1パートしかないのでトラック名だけになる
        assert!(!out.contains("段"), "段番号は付けないこと");
    }

    /// 2トラック目以降は書き出さないこと
    #[test]
    fn only_the_first_track_is_exported() {
        let mut editor = MidiEditor::default();
        editor.tracks.push(TrackInfo::new(1));
        editor.notes = vec![
            note(0.0, 1.0, 0, 4, 0),
            Note {
                track: 1,
                ..note(0.0, 1.0, 4, 4, 0)
            },
        ];

        let out = xml(&editor);
        assert_eq!(out.matches("<Note ").count(), 1, "1トラック目のみ");
        assert_eq!(export(&editor).unwrap().notes, 1);
    }

    /// 1トラック目が空ならエラーにすること (中身のない .ccs を作らない)
    #[test]
    fn empty_first_track_is_an_error() {
        let mut editor = MidiEditor::default();
        editor.tracks.push(TrackInfo::new(1));
        editor.notes = vec![Note {
            track: 1,
            ..note(0.0, 1.0, 0, 4, 0)
        }];
        assert!(export(&editor).is_err());
    }

    /// 音域外・音価0のノートは除外し、その数を報告すること
    #[test]
    fn unplayable_notes_are_reported() {
        let mut editor = MidiEditor::default();
        editor.notes = vec![
            note(0.0, 1.0, 0, 4, 0),
            note(1.0, 0.0, 0, 4, 0),  // 音価0
            note(2.0, 1.0, 0, -2, 0), // キー番号 -12 で音域外
        ];

        let exported = export(&editor).unwrap();
        assert_eq!(exported.notes, 1);
        assert_eq!(exported.skipped, 2);
    }

    /// B-P はステップが広いので、キー番号が MIDI 範囲内でも書き込む音が
    /// 振り切れることがある。そのノートは出さないこと。
    #[test]
    fn bohlen_pierce_notes_beyond_the_midi_range_are_skipped() {
        let scale = ScaleMode::BohlenPierce13;
        // (半音2, オクターブ9) はキー127 (範囲内) だが 69.5kHz 相当
        let too_high = note(0.0, 1.0, 2, 9, 0);
        assert!(too_high.key(scale).is_some(), "キー番号自体は範囲内");
        assert!(written_key(scale, &too_high) > 127, "書き込む音は振り切れる");

        let mut editor = MidiEditor::default();
        editor.scale = scale;
        editor.notes = vec![note(0.0, 1.0, 3, 4, 0), too_high];

        let exported = export(&editor).unwrap();
        assert_eq!(exported.notes, 1);
        assert_eq!(exported.skipped, 1);
    }

    /// 低音側でオクターブが負に回っても分解が壊れないこと
    #[test]
    fn pitch_split_handles_low_keys() {
        assert_eq!(pitch_step_octave(60), (0, 4)); // C4
        assert_eq!(pitch_step_octave(69), (9, 4)); // A4
        assert_eq!(pitch_step_octave(0), (0, -1)); // 最低音
        assert_eq!(pitch_step_octave(-1), (11, -2)); // 範囲外でも破綻しない
    }

    /// トラック名に XML の特殊文字が入ってもエスケープされること
    #[test]
    fn track_name_is_escaped() {
        let mut editor = MidiEditor::default();
        editor.tracks[0].name = "A & B \"C\"".into();
        editor.notes = vec![note(0.0, 1.0, 0, 4, 0)];

        let out = xml(&editor);
        assert!(out.contains("&amp;"), "& がエスケープされること");
        assert!(!out.contains("Name=\"A & B"), "生の & を出さないこと");
    }
}
