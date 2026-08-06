//! プロジェクトファイル (.ron) の読み書き。
//!
//! MIDI は書き出しでスウィングを乗せてしまうため、読み戻すと跳ねが二重に掛かる。
//! 編集を保存する役目はこちらが担い、**記譜位置と設定をそのまま**残す。
//!
//! 形式はファイル側の構造体 (`Project` など) を別に持ち、内部のデータモデルとは
//! 直接繋げていない。モデルを変えてもファイル形式が巻き添えにならないようにするため。
//!
//! # 壊れたファイルの扱い
//!
//! serde は構文と型しか見ないので、意味の検証を別に持つ。方針は
//! **エディタに触る前に全部検証し、1つでも駄目なら何も変更しない**。
//! 問題はまとめて列挙して返す (直すたびに読み直すのを避けるため)。

use crate::sequencer::{MidiEditor, Note, ScaleMode, TrackInfo};
use crate::swing;
use serde::{Deserialize, Serialize};

/// 今このビルドが書き出す形式のバージョン
const VERSION: u32 = 1;

/// テンポの許容範囲
const TEMPO_RANGE: std::ops::RangeInclusive<u32> = 1..=999;

/// 拍子の分母として使える値 (ツールバーの選択肢と揃える)
const BEAT_TYPES: [u32; 6] = [1, 2, 4, 8, 16, 32];

/// バージョンだけを先に読むための入れ物。
///
/// 全体を先に解析すると、将来の形式に対して「構文エラー」という
/// 見当違いのメッセージが出てしまう。
#[derive(Deserialize)]
struct VersionProbe {
    version: u32,
}

fn default_scale() -> ScaleMode {
    ScaleMode::Equal12
}

fn default_peak_ratio() -> f32 {
    swing::DEFAULT_PEAK_RATIO
}

/// ファイルに書き出すシーケンス一式
#[derive(Serialize, Deserialize)]
struct Project {
    version: u32,
    #[serde(default)]
    tempo: u32,
    #[serde(default)]
    beats: u32,
    #[serde(default)]
    beat_type: u32,
    // Option にすると `Some(...)` が書き出されて手で読み書きしづらい。
    // 省略されたときの値は既定値関数で埋める。
    #[serde(default = "default_scale")]
    scale: ScaleMode,
    #[serde(default = "default_peak_ratio")]
    swing_peak_ratio: f32,
    #[serde(default)]
    tracks: Vec<TrackEntry>,
    #[serde(default)]
    notes: Vec<NoteEntry>,
}

#[derive(Serialize, Deserialize)]
struct TrackEntry {
    #[serde(default)]
    name: String,
    #[serde(default)]
    lanes: usize,
    #[serde(default)]
    muted: bool,
    #[serde(default)]
    soloed: bool,
    #[serde(default)]
    swing: bool,
}

#[derive(Serialize, Deserialize)]
struct NoteEntry {
    #[serde(default)]
    start: f32,
    #[serde(default)]
    duration: f32,
    #[serde(default)]
    semitone: i32,
    #[serde(default)]
    octave: i32,
    #[serde(default)]
    velocity: u8,
    #[serde(default)]
    track: usize,
    #[serde(default)]
    lane: usize,
}

/// シーケンスを .ron のテキストにする。
///
/// スウィングは適用しない (記譜位置をそのまま残す)。
pub fn to_string(editor: &MidiEditor) -> Result<String, String> {
    let project = Project {
        version: VERSION,
        tempo: editor.tempo,
        beats: editor.beats,
        beat_type: editor.beat_type,
        scale: editor.scale,
        swing_peak_ratio: editor.swing_peak_ratio,
        tracks: editor
            .tracks
            .iter()
            .map(|info| TrackEntry {
                name: info.name.clone(),
                lanes: info.lanes,
                muted: info.muted,
                soloed: info.soloed,
                swing: info.swing,
            })
            .collect(),
        notes: editor
            .notes
            .iter()
            .map(|note| NoteEntry {
                start: note.start_tick,
                duration: note.duration,
                semitone: note.semitone,
                octave: note.octave,
                velocity: note.velocity,
                track: note.track,
                lane: note.lane,
            })
            .collect(),
    };

    // ノート1件を1行に収める (数百件になると1件9行では読めない)。
    // 深さ2 (配列の要素) より内側は改行しない。
    let config = ron::ser::PrettyConfig::new()
        .indentor("    ".to_string())
        .struct_names(false)
        .depth_limit(2);
    ron::ser::to_string_pretty(&project, config).map_err(|e| format!("組み立てられません: {e}"))
}

/// .ron のテキストを読む。構文・意味の両方を検証してから返す。
pub fn from_str(text: &str) -> Result<MidiEditor, String> {
    // 1. バージョンだけ先に見る
    let probe: VersionProbe = ron::from_str(text)
        .map_err(|e| format!("プロジェクトファイルとして読めません:\n{e}"))?;
    if probe.version > VERSION {
        return Err(format!(
            "このファイルはバージョン {} で保存されています。\n\
             このビルドが読めるのはバージョン {VERSION} までです。",
            probe.version
        ));
    }

    // 2. 全体を解析
    let project: Project =
        ron::from_str(text).map_err(|e| format!("プロジェクトファイルとして読めません:\n{e}"))?;

    // 3. 意味の検証 (問題はまとめて返す)
    let problems = validate(&project);
    if !problems.is_empty() {
        return Err(format!(
            "ファイルの内容に問題があります。\n\n{}",
            problems.join("\n")
        ));
    }

    Ok(build(project))
}

/// 意味の検証。見つかった問題を人が読める形で並べて返す。
fn validate(project: &Project) -> Vec<String> {
    let mut problems = Vec::new();

    if !TEMPO_RANGE.contains(&project.tempo) {
        problems.push(format!(
            "・テンポ {} は範囲外です ({}〜{})",
            project.tempo,
            TEMPO_RANGE.start(),
            TEMPO_RANGE.end()
        ));
    }
    if project.beats < 1 {
        problems.push("・拍子の分子は 1 以上である必要があります".into());
    }
    if !BEAT_TYPES.contains(&project.beat_type) {
        problems.push(format!(
            "・拍子の分母 {} は使えません ({} のいずれか)",
            project.beat_type,
            BEAT_TYPES.map(|b| b.to_string()).join(" / ")
        ));
    }
    if project.tracks.is_empty() {
        problems.push("・トラックが1つもありません".into());
    }
    for (index, track) in project.tracks.iter().enumerate() {
        if track.lanes < 1 {
            problems.push(format!("・トラック {} の段数が 0 です", index + 1));
        }
    }

    let scale = project.scale;
    let max_semitone = scale.max_semitone();
    for (index, note) in project.notes.iter().enumerate() {
        let at = format!("・ノート {}", index + 1);
        // NaN / Inf は並べ替え・長さ計算・描画のすべてに波及するので必ず弾く
        if !note.start.is_finite() || !note.duration.is_finite() {
            problems.push(format!("{at}: 位置か音価が数値ではありません"));
            continue;
        }
        if note.start < 0.0 {
            problems.push(format!("{at}: 位置 {} が負です", note.start));
        }
        if note.duration <= 0.0 {
            problems.push(format!("{at}: 音価 {} が 0 以下です", note.duration));
        }
        if !(0..=max_semitone).contains(&note.semitone) {
            problems.push(format!(
                "{at}: 半音 {} は {} では 0〜{max_semitone} の範囲です",
                note.semitone,
                scale.label()
            ));
        }
        if note.track >= project.tracks.len() {
            problems.push(format!(
                "{at}: トラック {} は存在しません",
                note.track + 1
            ));
        } else if note.lane >= project.tracks[note.track].lanes.max(1) {
            problems.push(format!(
                "{at}: トラック {} に段 {} はありません",
                note.track + 1,
                note.lane + 1
            ));
        }
    }

    problems
}

/// 検証済みのファイル内容をデータモデルに移す
fn build(project: Project) -> MidiEditor {
    MidiEditor {
        notes: project
            .notes
            .into_iter()
            .map(|entry| Note {
                start_tick: entry.start,
                duration: entry.duration,
                semitone: entry.semitone,
                octave: entry.octave,
                velocity: entry.velocity,
                track: entry.track,
                lane: entry.lane,
            })
            .collect(),
        tracks: project
            .tracks
            .into_iter()
            .enumerate()
            .map(|(index, entry)| TrackInfo {
                name: if entry.name.is_empty() {
                    TrackInfo::new(index).name
                } else {
                    entry.name
                },
                lanes: entry.lanes.max(1),
                muted: entry.muted,
                soloed: entry.soloed,
                swing: entry.swing,
            })
            .collect(),
        tempo: project.tempo,
        beats: project.beats,
        beat_type: project.beat_type,
        scale: project.scale,
        swing_peak_ratio: project
            .swing_peak_ratio
            .clamp(swing::MIN_PEAK_RATIO, swing::MAX_PEAK_RATIO),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> MidiEditor {
        let mut editor = MidiEditor::default();
        editor.tempo = 96;
        editor.beats = 3;
        editor.beat_type = 4;
        editor.scale = ScaleMode::BohlenPierce13;
        editor.swing_peak_ratio = 1.75;
        editor.add_track();
        editor.tracks[0].name = "リズム".into();
        editor.tracks[0].lanes = 2;
        editor.tracks[0].muted = true;
        editor.tracks[1].swing = true;
        editor.tracks[1].soloed = true;
        editor.notes = vec![
            Note {
                start_tick: 0.0,
                duration: 0.5,
                semitone: 3,
                octave: 4,
                velocity: 100,
                track: 0,
                lane: 1,
            },
            Note {
                start_tick: 1.5,
                duration: 2.0,
                semitone: 12,
                octave: 5,
                velocity: 64,
                track: 1,
                lane: 0,
            },
        ];
        editor
    }

    /// 保存して読み戻すと、シーケンスと設定がそのまま復元されること
    #[test]
    fn round_trip_keeps_everything() {
        let original = sample();
        let restored = from_str(&to_string(&original).unwrap()).unwrap();

        assert_eq!(restored.tempo, original.tempo);
        assert_eq!(restored.beats, original.beats);
        assert_eq!(restored.beat_type, original.beat_type);
        assert_eq!(restored.scale, original.scale);
        assert_eq!(restored.swing_peak_ratio, original.swing_peak_ratio);
        assert_eq!(restored.tracks, original.tracks);
        assert_eq!(restored.notes, original.notes);
    }

    /// スウィングの設定が保存されること (MIDI では持てなかったもの)
    #[test]
    fn round_trip_keeps_swing_settings() {
        let restored = from_str(&to_string(&sample()).unwrap()).unwrap();
        assert!(!restored.tracks[0].swing, "伴奏は OFF のまま");
        assert!(restored.tracks[1].swing, "ソロは ON のまま");
        assert_eq!(restored.swing_peak_ratio, 1.75);
    }

    /// 記譜位置のまま保存されること (スウィングを焼き込まない)
    #[test]
    fn saving_does_not_bake_in_swing() {
        let mut editor = MidiEditor::default();
        editor.tracks[0].swing = true;
        editor.notes = vec![Note {
            start_tick: 0.0,
            duration: 0.5,
            semitone: 0,
            octave: 4,
            velocity: 100,
            track: 0,
            lane: 0,
        }];

        let restored = from_str(&to_string(&editor).unwrap()).unwrap();
        assert_eq!(restored.notes[0].start_tick, 0.0, "拍頭のまま");
        assert_eq!(restored.notes[0].duration, 0.5);
    }

    /// 知らないフィールドは無視し、欠けたフィールドは既定値で埋めること。
    /// (新旧どちらのビルドでも開けるようにするため)
    #[test]
    fn unknown_fields_are_ignored_and_missing_ones_default() {
        let text = r#"(
            version: 1,
            tempo: 140,
            beats: 4,
            beat_type: 4,
            future_setting: "まだ無い項目",
            tracks: [ (name: "A", lanes: 1) ],
            notes: [ (start: 0.0, duration: 1.0, velocity: 90) ],
        )"#;

        let editor = from_str(text).expect("読めること");
        assert_eq!(editor.tempo, 140);
        assert_eq!(editor.tracks[0].name, "A");
        assert_eq!(editor.scale, ScaleMode::Equal12, "省略時は既定の音階");
        assert_eq!(
            editor.swing_peak_ratio,
            swing::DEFAULT_PEAK_RATIO,
            "省略時は既定の強さ"
        );
        assert!(!editor.tracks[0].muted, "省略時は OFF");
        assert_eq!(editor.notes[0].octave, 0);
    }

    /// 未来のバージョンは、構文エラーではなくバージョンの問題として断ること
    #[test]
    fn newer_versions_are_refused_clearly() {
        let text = "(version: 99, tempo: 120, beats: 4, beat_type: 4, tracks: [], notes: [])";
        let error = from_str(text).unwrap_err();
        assert!(error.contains("バージョン 99"), "実際: {error}");
    }

    /// 壊れた構文は位置つきで伝えること
    #[test]
    fn broken_syntax_is_reported() {
        let error = from_str("これは RON ではありません").unwrap_err();
        assert!(error.contains("読めません"), "実際: {error}");
    }

    /// 見出しの値が範囲外なら弾くこと
    #[test]
    fn invalid_header_values_are_rejected() {
        let doc = |tempo: &str, beats: &str, beat_type: &str| {
            format!(
                "(version: 1, tempo: {tempo}, beats: {beats}, beat_type: {beat_type}, \
                 tracks: [ (name: \"A\", lanes: 1) ], notes: [])"
            )
        };

        assert!(from_str(&doc("120", "4", "4")).is_ok(), "正常なものは通ること");
        assert!(from_str(&doc("0", "4", "4")).unwrap_err().contains("テンポ"));
        assert!(from_str(&doc("1000", "4", "4")).unwrap_err().contains("テンポ"));
        assert!(from_str(&doc("120", "0", "4")).unwrap_err().contains("分子"));
        assert!(from_str(&doc("120", "4", "3")).unwrap_err().contains("分母"));
    }

    /// ノートの値が範囲外なら弾くこと
    #[test]
    fn invalid_note_values_are_rejected() {
        let doc = |note: &str| {
            format!(
                "(version: 1, tempo: 120, beats: 4, beat_type: 4, \
                 tracks: [ (name: \"A\", lanes: 2) ], notes: [ ({note}) ])"
            )
        };

        assert!(
            from_str(&doc("start: 0.0, duration: 1.0")).is_ok(),
            "正常なものは通ること"
        );
        for (note, expected) in [
            ("start: 0.0, duration: 0.0", "音価"),
            ("start: -1.0, duration: 1.0", "位置"),
            ("start: 0.0, duration: 1.0, semitone: 99", "半音"),
            ("start: 0.0, duration: 1.0, track: 5", "トラック"),
            ("start: 0.0, duration: 1.0, lane: 7", "段"),
        ] {
            let error = from_str(&doc(note)).unwrap_err();
            assert!(
                error.contains(expected),
                "{note} で「{expected}」を指摘すること。実際: {error}"
            );
        }
    }

    /// NaN を弾くこと。
    /// 通してしまうと並べ替え・長さ計算・描画のすべてに波及する。
    #[test]
    fn non_finite_positions_are_rejected() {
        for value in ["NaN", "inf", "-inf"] {
            let text = format!(
                "(version: 1, tempo: 120, beats: 4, beat_type: 4, \
                 tracks: [ (name: \"A\", lanes: 1) ], \
                 notes: [ (start: {value}, duration: 1.0) ])"
            );
            let error = from_str(&text).unwrap_err();
            assert!(error.contains("数値ではありません"), "{value}: {error}");
        }
    }

    /// 問題が複数あればまとめて挙げること (直すたびに読み直さずに済む)
    #[test]
    fn every_problem_is_listed_at_once() {
        let text = "(version: 1, tempo: 0, beats: 0, beat_type: 3, tracks: [], notes: [])";
        let error = from_str(text).unwrap_err();

        assert!(error.contains("テンポ"));
        assert!(error.contains("分子"));
        assert!(error.contains("分母"));
        assert!(error.contains("トラックが1つもありません"));
    }
}
