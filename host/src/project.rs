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
use crate::waltz;
use base64::Engine;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// 今このビルドが書き出す形式のバージョン。
///
/// 1: シーケンスと設定のみ / 2: 音源 (`plugins`) を追加
const VERSION: u32 = 2;

/// 音源の種別。
///
/// 既定を `Clap` にしてあるので、種別を持たないバージョン1 のファイルも読める。
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, Serialize, Deserialize)]
pub enum PluginKind {
    #[default]
    Clap,
    Vst3,
}

/// 保存する音源1つぶん。
///
/// `main.rs` が組み立てて渡す。`project.rs` は `state` の中身を解釈しない
/// (プラグイン形式の知識をここへ持ち込まないため)。
#[derive(Clone, Debug, PartialEq)]
pub struct PluginSnapshot {
    pub kind: PluginKind,
    pub path: PathBuf,
    /// CLAP のプラグイン ID、または VST3 のクラス UID。
    /// 1つのファイルに複数入りうるので、パスだけでは足りない。
    pub id: String,
    /// 音源が書き出した状態。中身は不透明なバイト列。
    pub state: Vec<u8>,
}

/// 読み込んだプロジェクト
pub struct Loaded {
    pub editor: MidiEditor,
    /// トラック番号順の音源。載っていないトラックは `None`。
    pub plugins: Vec<Option<PluginSnapshot>>,
}

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

fn default_waltz_ratio() -> f32 {
    waltz::DEFAULT_RATIO
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
    #[serde(default = "default_waltz_ratio")]
    waltz_ratio: f32,
    #[serde(default)]
    tracks: Vec<TrackEntry>,
    #[serde(default)]
    notes: Vec<NoteEntry>,
    /// トラック番号順の音源。巨大になりうるので**末尾に置く**
    /// (テンポ・拍子・ノートといった読みたい部分をファイル先頭に残すため)。
    /// バージョン1 のファイルには無いので `default` で空になる。
    #[serde(default)]
    plugins: Vec<Option<PluginEntry>>,
}

#[derive(Serialize, Deserialize)]
struct PluginEntry {
    #[serde(default)]
    kind: PluginKind,
    #[serde(default)]
    path: String,
    #[serde(default)]
    id: String,
    /// 状態を base64 にしたもの。`Vec<u8>` のまま serde に渡すと
    /// RON では数値の配列になってしまう。
    #[serde(default)]
    state: String,
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
    #[serde(default)]
    waltz: bool,
    /// 段ごとの CC 番号 (`None` は音符段)。CC 段が無ければ空。
    /// これより前のバージョンのファイルには無いので `default` で空になる。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    lane_ccs: Vec<Option<u8>>,
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

/// シーケンスと音源を .ron のテキストにする。
///
/// スウィングは適用しない (記譜位置をそのまま残す)。
/// `plugins` はトラック番号順で、載っていないトラックは `None`。
pub fn to_string(
    editor: &MidiEditor,
    plugins: &[Option<PluginSnapshot>],
) -> Result<String, String> {
    let base64 = base64::engine::general_purpose::STANDARD;
    let project = Project {
        plugins: plugins
            .iter()
            .map(|slot| {
                slot.as_ref().map(|plugin| PluginEntry {
                    kind: plugin.kind,
                    path: plugin.path.to_string_lossy().into_owned(),
                    id: plugin.id.clone(),
                    state: base64.encode(&plugin.state),
                })
            })
            .collect(),
        version: VERSION,
        tempo: editor.tempo,
        beats: editor.beats,
        beat_type: editor.beat_type,
        scale: editor.scale,
        swing_peak_ratio: editor.swing_peak_ratio,
        waltz_ratio: editor.waltz_ratio,
        tracks: editor
            .tracks
            .iter()
            .map(|info| TrackEntry {
                name: info.name.clone(),
                lanes: info.lanes,
                muted: info.muted,
                soloed: info.soloed,
                swing: info.swing,
                waltz: info.waltz,
                lane_ccs: info.lane_ccs.clone(),
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
pub fn from_str(text: &str) -> Result<Loaded, String> {
    // 1. バージョンだけ先に見る
    let probe: VersionProbe =
        ron::from_str(text).map_err(|e| format!("プロジェクトファイルとして読めません:\n{e}"))?;
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

/// 検証済みの音源欄をデータへ移す。
///
/// base64 が壊れているものは**その音源だけ捨てる**（トラックは音源なしになる）。
/// ノートやトラック構成は無事なので、プロジェクト全体を拒否するのは損が大きい。
fn build_plugins(entries: Vec<Option<PluginEntry>>, tracks: usize) -> Vec<Option<PluginSnapshot>> {
    let base64 = base64::engine::general_purpose::STANDARD;
    let mut plugins: Vec<Option<PluginSnapshot>> = entries
        .into_iter()
        .map(|slot| {
            let entry = slot?;
            if entry.path.is_empty() {
                return None;
            }
            Some(PluginSnapshot {
                kind: entry.kind,
                path: PathBuf::from(entry.path),
                id: entry.id,
                state: base64.decode(&entry.state).unwrap_or_default(),
            })
        })
        .collect();
    // トラック数と長さを揃える (音源欄が短い/長いファイルへの備え)
    plugins.resize(tracks, None);
    plugins
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
            problems.push(format!("{at}: トラック {} は存在しません", note.track + 1));
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
fn build(project: Project) -> Loaded {
    let tracks = project.tracks.len().max(1);
    let plugins = build_plugins(project.plugins, tracks);
    let editor = MidiEditor {
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
                waltz: entry.waltz,
                // 範囲外の CC 番号は音符段として読む (壊れたファイルで
                // 段が丸ごと使えなくなるより、音符段に落ちるほうがまし)
                lane_ccs: entry
                    .lane_ccs
                    .into_iter()
                    .map(|cc| cc.filter(|number| *number <= 127))
                    .collect(),
            })
            .collect(),
        tempo: project.tempo,
        beats: project.beats,
        beat_type: project.beat_type,
        scale: project.scale,
        swing_peak_ratio: project
            .swing_peak_ratio
            .clamp(swing::MIN_PEAK_RATIO, swing::MAX_PEAK_RATIO),
        // **0 以下や NaN を通すと拍長が壊れる**ので、丸めは waltz 側に任せる
        waltz_ratio: waltz::clamp_ratio(project.waltz_ratio),
    };
    Loaded { editor, plugins }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 音源なしで書き出す (音源を扱わないテスト用)
    fn save(editor: &MidiEditor) -> String {
        to_string(editor, &[]).unwrap()
    }

    /// 読み込んでエディタだけ取り出す
    fn load(text: &str) -> Result<MidiEditor, String> {
        from_str(text).map(|loaded| loaded.editor)
    }

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
        let restored = load(&save(&original)).unwrap();

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
    /// CC 段の割り当てが保存・復元されること。
    ///
    /// 落ちると、開き直したときに CC 段が音符段に戻り、**ペダルのつもりの
    /// ブロックが音として鳴ってしまう**。
    #[test]
    fn round_trip_keeps_cc_lanes() {
        let mut editor = sample();
        editor.tracks[0].lanes = 3;
        editor.tracks[0].set_lane_cc(1, Some(64));

        let restored = load(&save(&editor)).unwrap();
        assert_eq!(restored.tracks[0].lane_cc(0), None, "音符段のまま");
        assert_eq!(restored.tracks[0].lane_cc(1), Some(64));
        assert_eq!(restored.tracks[0].lane_cc(2), None);
        assert_eq!(restored.tracks[1].lane_cc(0), None, "他のトラックは無関係");
    }

    #[test]
    fn round_trip_keeps_swing_settings() {
        let restored = load(&save(&sample())).unwrap();
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

        let restored = load(&save(&editor)).unwrap();
        assert_eq!(restored.notes[0].start_tick, 0.0, "拍頭のまま");
        assert_eq!(restored.notes[0].duration, 0.5);
    }

    fn snapshot(path: &str, id: &str, state: &[u8]) -> PluginSnapshot {
        PluginSnapshot {
            kind: PluginKind::Clap,
            path: PathBuf::from(path),
            id: id.into(),
            state: state.to_vec(),
        }
    }

    /// 音源のパス・ID・状態が往復すること
    #[test]
    fn round_trip_keeps_plugins() {
        let editor = sample(); // トラック2本
        let plugins = vec![
            Some(snapshot(
                "C:\\音源\\Surge XT.clap",
                "org.surge-synth-team.surge-xt",
                &[0, 1, 2, 255],
            )),
            None,
        ];

        let restored = from_str(&to_string(&editor, &plugins).unwrap()).unwrap();
        assert_eq!(restored.plugins, plugins);
    }

    /// 状態は base64 で載ること (数値の配列にしない)
    #[test]
    fn state_is_stored_as_base64() {
        let plugins = vec![Some(snapshot("a.clap", "x", b"Hello"))];
        let text = to_string(&MidiEditor::default(), &plugins).unwrap();

        assert!(text.contains("SGVsbG8="), "base64 で載ること: {text}");
        assert!(!text.contains("state: ["), "数値の配列にしないこと");
    }

    /// 音源欄はファイル末尾に置くこと (読みたい部分を先頭に残すため)
    #[test]
    fn plugins_are_written_last() {
        let plugins = vec![Some(snapshot("a.clap", "x", &[0; 64]))];
        let text = to_string(&sample(), &plugins).unwrap();

        let at = |needle: &str| text.find(needle).expect(needle);
        assert!(at("tempo:") < at("plugins:"));
        assert!(at("notes:") < at("plugins:"));
    }

    /// バージョン1 のファイル (音源欄なし) がそのまま読めること
    #[test]
    fn version_one_files_still_load() {
        let text = "(version: 1, tempo: 120, beats: 4, beat_type: 4, \
                    tracks: [ (name: \"A\", lanes: 1) ], \
                    notes: [ (start: 0.0, duration: 1.0) ])";

        let loaded = from_str(text).expect("読めること");
        assert_eq!(loaded.editor.tempo, 120);
        assert_eq!(loaded.plugins, vec![None], "音源なしとして扱うこと");
    }

    /// 音源欄の長さがトラック数と合わなくても、トラック数に揃うこと
    #[test]
    fn plugin_list_is_resized_to_the_tracks() {
        let editor = sample(); // トラック2本
                               // 音源を3つ書いた壊れたファイル相当
        let plugins = vec![
            Some(snapshot("a.clap", "x", b"1")),
            Some(snapshot("b.clap", "y", b"2")),
            Some(snapshot("c.clap", "z", b"3")),
        ];

        let restored = from_str(&to_string(&editor, &plugins).unwrap()).unwrap();
        assert_eq!(restored.plugins.len(), 2, "トラック数に揃うこと");
    }

    /// base64 が壊れていても、その音源だけを捨ててプロジェクトは読めること
    #[test]
    fn broken_state_only_drops_that_plugin() {
        let text = "(version: 2, tempo: 120, beats: 4, beat_type: 4, \
                    tracks: [ (name: \"A\", lanes: 1) ], \
                    notes: [ (start: 0.0, duration: 1.0) ], \
                    plugins: [ Some((kind: Clap, path: \"a.clap\", id: \"x\", state: \"!!!壊れた!!!\")) ])";

        let loaded = from_str(text).expect("プロジェクト自体は読めること");
        assert_eq!(loaded.editor.notes.len(), 1, "ノートは無事であること");
        let plugin = loaded.plugins[0].as_ref().expect("音源は残ること");
        assert!(plugin.state.is_empty(), "状態だけ諦めること");
        assert_eq!(plugin.id, "x");
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

        let editor = load(text).expect("読めること");
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
        let error = load(text).unwrap_err();
        assert!(error.contains("バージョン 99"), "実際: {error}");
    }

    /// 壊れた構文は位置つきで伝えること
    #[test]
    fn broken_syntax_is_reported() {
        let error = load("これは RON ではありません").unwrap_err();
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

        assert!(load(&doc("120", "4", "4")).is_ok(), "正常なものは通ること");
        assert!(load(&doc("0", "4", "4")).unwrap_err().contains("テンポ"));
        assert!(load(&doc("1000", "4", "4")).unwrap_err().contains("テンポ"));
        assert!(load(&doc("120", "0", "4")).unwrap_err().contains("分子"));
        assert!(load(&doc("120", "4", "3")).unwrap_err().contains("分母"));
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
            load(&doc("start: 0.0, duration: 1.0")).is_ok(),
            "正常なものは通ること"
        );
        for (note, expected) in [
            ("start: 0.0, duration: 0.0", "音価"),
            ("start: -1.0, duration: 1.0", "位置"),
            ("start: 0.0, duration: 1.0, semitone: 99", "半音"),
            ("start: 0.0, duration: 1.0, track: 5", "トラック"),
            ("start: 0.0, duration: 1.0, lane: 7", "段"),
        ] {
            let error = load(&doc(note)).unwrap_err();
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
            let error = load(&text).unwrap_err();
            assert!(error.contains("数値ではありません"), "{value}: {error}");
        }
    }

    /// 問題が複数あればまとめて挙げること (直すたびに読み直さずに済む)
    #[test]
    fn every_problem_is_listed_at_once() {
        let text = "(version: 1, tempo: 0, beats: 0, beat_type: 3, tracks: [], notes: [])";
        let error = load(text).unwrap_err();

        assert!(error.contains("テンポ"));
        assert!(error.contains("分子"));
        assert!(error.contains("分母"));
        assert!(error.contains("トラックが1つもありません"));
    }
}
