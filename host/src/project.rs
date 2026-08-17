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
/// 1: シーケンスと設定のみ / 2: 音源 (`plugins`) を追加 /
/// 3: 音源を**オーディオトラック** (`audio_tracks`) へ移した
const VERSION: u32 = 3;

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

/// 保存するオーディオトラック1本ぶん。
///
/// 打ち込み側の「トラック」とは別物 (`audio::graph` を参照)。
/// 番号は並び順そのもので、**常に [`AUDIO_TRACKS`] 本**ある。
#[derive(Clone, Debug, PartialEq, Default)]
pub struct AudioTrackSnapshot {
    pub name: String,
    /// 上から順に通す音源とエフェクト
    pub nodes: Vec<PluginSnapshot>,
    /// MIDI をどの打ち込みトラックから取るか。`None` は入力なし (バス用)
    pub midi_track: Option<usize>,
    /// 送り先のオーディオトラック番号。**マスター (0) では常に空**
    pub sends: Vec<usize>,
}

/// オーディオトラックの本数 (`audio::graph::AUDIO_TRACKS` と同じ)。
///
/// `project` は音を扱わないので、`audio` に依存させずここで持つ。
/// **食い違うと保存と再生でトラック数が合わなくなる**ので、テストで縛ってある。
pub const AUDIO_TRACKS: usize = 16;

/// マスターのトラック番号
pub const MASTER: usize = 0;

/// 読み込んだプロジェクト
pub struct Loaded {
    pub editor: MidiEditor,
    /// オーディオトラック。常に [`AUDIO_TRACKS`] 本ある
    pub audio_tracks: Vec<AudioTrackSnapshot>,
    /// 16本に収まらず読み込めなかったものの説明。空なら全部載った
    pub overflow: Vec<String>,
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
    /// **バージョン2 まで**の音源欄 (打ち込みトラック番号順)。
    /// 読むためだけに残してある。書き出すのは `audio_tracks` のほう。
    #[serde(default, skip_serializing)]
    plugins: Vec<Option<PluginEntry>>,
    /// オーディオトラック。巨大になりうるので**末尾に置く**
    /// (テンポ・拍子・ノートといった読みたい部分をファイル先頭に残すため)。
    /// バージョン2 以前のファイルには無いので `default` で空になる。
    #[serde(default)]
    audio_tracks: Vec<AudioTrackEntry>,
}

#[derive(Serialize, Deserialize)]
struct AudioTrackEntry {
    #[serde(default)]
    name: String,
    /// `None` は入力なし (バス用)
    #[serde(default)]
    midi_track: Option<usize>,
    /// 送り先。**片側だけ書く。** 両側に書くと「A は→B と言い、B は何も
    /// 言っていない」という矛盾したファイルが作れてしまう
    #[serde(default)]
    sends: Vec<SendEntry>,
    /// 上から順に通す音源とエフェクト
    #[serde(default)]
    nodes: Vec<PluginEntry>,
}

/// 送り1本。
///
/// 1項目でも構造体にしてあるのは、**あとで音量を足すため**
/// (`usize` の配列から構造体の配列へは移れない)。
#[derive(Serialize, Deserialize)]
struct SendEntry {
    #[serde(default)]
    to: usize,
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

/// シーケンスとオーディオトラックを .ron のテキストにする。
///
/// スウィングは適用しない (記譜位置をそのまま残す)。
/// `audio_tracks` は番号順で、[`AUDIO_TRACKS`] 本に満たなければ空きで埋める。
pub fn to_string(
    editor: &MidiEditor,
    audio_tracks: &[AudioTrackSnapshot],
) -> Result<String, String> {
    let base64 = base64::engine::general_purpose::STANDARD;
    let node_entry = |plugin: &PluginSnapshot| PluginEntry {
        kind: plugin.kind,
        path: plugin.path.to_string_lossy().into_owned(),
        id: plugin.id.clone(),
        state: base64.encode(&plugin.state),
    };

    let project = Project {
        audio_tracks: (0..AUDIO_TRACKS)
            .map(|index| {
                let track = audio_tracks.get(index).cloned().unwrap_or_default();
                AudioTrackEntry {
                    name: track.name,
                    midi_track: track.midi_track,
                    // マスターは送り側にならないので、書いてあっても落とす
                    sends: if index == MASTER {
                        Vec::new()
                    } else {
                        track.sends.iter().map(|to| SendEntry { to: *to }).collect()
                    },
                    nodes: track.nodes.iter().map(node_entry).collect(),
                }
            })
            .collect(),
        // 書き出さない (バージョン2 のファイルを読むためだけに残してある)
        plugins: Vec::new(),
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

/// 音源欄1つをデータへ移す。
///
/// base64 が壊れているものは**状態だけ捨てる**。ノートやトラック構成は
/// 無事なので、プロジェクト全体を拒否するのは損が大きい。
/// パスが空のものは「載っていない」とみなして `None`。
fn build_node(entry: PluginEntry) -> Option<PluginSnapshot> {
    if entry.path.is_empty() {
        return None;
    }
    let base64 = base64::engine::general_purpose::STANDARD;
    Some(PluginSnapshot {
        kind: entry.kind,
        path: PathBuf::from(entry.path),
        id: entry.id,
        state: base64.decode(&entry.state).unwrap_or_default(),
    })
}

/// バージョン2 以前の音源欄をオーディオトラックへ移す。
///
/// 打ち込みトラック `i` に載っていた音源を、**オーディオトラック `i + 1`** の
/// 1段目にする (0 はマスターなので1つずらす)。送り先はマスター。
///
/// **16本に収まらないぶんは載せられない。** 打ち込みトラック数に上限が無い
/// ため、音源を15個より多く載せたファイルがありうる。黙って捨てず、
/// 呼び出し側が名指しで知らせられるように説明を返す。
fn migrate_plugins(entries: Vec<Option<PluginEntry>>) -> (Vec<AudioTrackSnapshot>, Vec<String>) {
    let mut tracks = empty_audio_tracks();
    let mut overflow = Vec::new();

    for (midi_track, slot) in entries.into_iter().enumerate() {
        let Some(node) = slot.and_then(build_node) else {
            continue;
        };
        let audio_track = midi_track + 1;
        if audio_track >= AUDIO_TRACKS {
            overflow.push(format!(
                "・トラック {}: {} はオーディオトラックが足りず載せられません \
                 (音を出せるのは {} 本まで)",
                midi_track + 1,
                node.path.display(),
                AUDIO_TRACKS - 1
            ));
            continue;
        }
        tracks[audio_track] = AudioTrackSnapshot {
            name: String::new(),
            nodes: vec![node],
            midi_track: Some(midi_track),
            sends: vec![MASTER],
        };
    }

    (tracks, overflow)
}

/// 空のオーディオトラック [`AUDIO_TRACKS`] 本
fn empty_audio_tracks() -> Vec<AudioTrackSnapshot> {
    (0..AUDIO_TRACKS)
        .map(|_| AudioTrackSnapshot::default())
        .collect()
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

    problems.extend(validate_audio_tracks(&project.audio_tracks));
    problems
}

/// オーディオトラックの検証。
///
/// **編集操作で拒否するだけでは足りない。** 手で書き換えたファイルや、
/// 将来の版で作られたファイルが入口になりうるので、読み込みでも見る。
///
/// 繋ぎ方の規則そのものは [`Routing`](crate::audio::graph::Routing) が持っている。
/// **ここで別に書くと、画面から繋ぐときとファイルを読むときで規則がずれる。**
fn validate_audio_tracks(tracks: &[AudioTrackEntry]) -> Vec<String> {
    if tracks.is_empty() {
        return Vec::new(); // バージョン2 以前。移行側で扱う
    }
    if tracks.len() > AUDIO_TRACKS {
        return vec![format!(
            "・オーディオトラックが {} 本あります (使えるのは {AUDIO_TRACKS} 本まで)",
            tracks.len()
        )];
    }

    let lists: Vec<Vec<usize>> = tracks
        .iter()
        .map(|track| track.sends.iter().map(|send| send.to).collect())
        .collect();

    crate::audio::graph::Routing::from_lists(&lists)
        .err()
        .unwrap_or_default()
}

/// 検証済みのファイル内容をデータモデルに移す
fn build(project: Project) -> Loaded {
    // バージョン3 以降は `audio_tracks`、それ以前は `plugins` から移す。
    // 両方あるファイル (手で書いたもの) では新しいほうを採る
    let (audio_tracks, overflow) = if project.audio_tracks.is_empty() {
        migrate_plugins(project.plugins)
    } else {
        let mut tracks = empty_audio_tracks();
        for (index, entry) in project.audio_tracks.into_iter().enumerate() {
            if index >= AUDIO_TRACKS {
                break; // 検証で弾いてあるので、ここへは来ない
            }
            tracks[index] = AudioTrackSnapshot {
                name: entry.name,
                nodes: entry.nodes.into_iter().filter_map(build_node).collect(),
                midi_track: entry.midi_track,
                // マスターは送り側にならない
                sends: if index == MASTER {
                    Vec::new()
                } else {
                    entry.sends.into_iter().map(|send| send.to).collect()
                },
            };
        }
        (tracks, Vec::new())
    };

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
    Loaded {
        editor,
        audio_tracks,
        overflow,
    }
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
        editor.waltz_ratio = 0.8;
        editor.add_track();
        editor.tracks[0].name = "リズム".into();
        editor.tracks[0].lanes = 2;
        editor.tracks[0].muted = true;
        editor.tracks[1].swing = true;
        editor.tracks[1].waltz = true;
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

    #[test]
    fn round_trip_keeps_waltz_settings() {
        let restored = load(&save(&sample())).unwrap();
        assert!(!restored.tracks[0].waltz, "伴奏は OFF のまま");
        assert!(restored.tracks[1].waltz, "ソロは ON のまま");
        assert_eq!(restored.waltz_ratio, 0.8);
    }

    /// 不均等な拍を知らない頃の `.ron` が、既定値で開けること。
    /// **古いファイルを開けなくしないことが version を 1 のままにした条件。**
    #[test]
    fn older_files_without_waltz_fields_open_with_defaults() {
        let text = r#"(
            version: 1,
            tempo: 120,
            beats: 3,
            beat_type: 4,
            swing_peak_ratio: 1.5,
            tracks: [(name: "トラック 1", lanes: 1, muted: false, soloed: false, swing: false)],
            notes: [],
        )"#;

        let editor = load(text).expect("開けること");
        assert_eq!(editor.waltz_ratio, waltz::DEFAULT_RATIO);
        assert!(!editor.tracks[0].waltz);
    }

    /// **0 以下や NaN を通すと拍長が壊れる**ので、読み込みで丸めること
    #[test]
    fn out_of_range_waltz_ratio_is_clamped() {
        for (written, expected) in [
            ("0.0", waltz::MIN_RATIO),
            ("-3.0", waltz::MIN_RATIO),
            ("0.1", waltz::MIN_RATIO),
            ("99.0", waltz::MAX_RATIO),
        ] {
            let text = format!(
                r#"(version: 1, tempo: 120, beats: 3, beat_type: 4, waltz_ratio: {written},
                    tracks: [(name: "t", lanes: 1)], notes: [])"#
            );
            let editor = load(&text).expect("開けること");
            assert_eq!(editor.waltz_ratio, expected, "{written} を丸めること");
        }
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

    /// 打ち込みトラック `midi` の音源を、オーディオトラック `midi + 1` に載せる
    fn audio_track(midi: usize, path: &str, id: &str, state: &[u8]) -> AudioTrackSnapshot {
        AudioTrackSnapshot {
            name: String::new(),
            nodes: vec![snapshot(path, id, state)],
            midi_track: Some(midi),
            sends: vec![MASTER],
        }
    }

    /// 空のオーディオトラック16本に、指定のものを差し込む
    fn audio_tracks(entries: &[(usize, AudioTrackSnapshot)]) -> Vec<AudioTrackSnapshot> {
        let mut tracks = empty_audio_tracks();
        for (index, track) in entries {
            tracks[*index] = track.clone();
        }
        tracks
    }

    /// 音源のパス・ID・状態と、繋ぎ方が往復すること
    #[test]
    fn round_trip_keeps_audio_tracks() {
        let editor = sample(); // トラック2本
        let tracks = audio_tracks(&[(
            1,
            audio_track(
                0,
                "C:\\音源\\Surge XT.clap",
                "org.surge-synth-team.surge-xt",
                &[0, 1, 2, 255],
            ),
        )]);

        let restored = from_str(&to_string(&editor, &tracks).unwrap()).unwrap();
        assert_eq!(restored.audio_tracks, tracks);
        assert!(restored.overflow.is_empty());
    }

    /// 常に16本書き出すこと (番号がそのまま識別子なので、詰めてはいけない)
    #[test]
    fn always_writes_every_audio_track() {
        let restored =
            from_str(&to_string(&MidiEditor::default(), &empty_audio_tracks()).unwrap()).unwrap();
        assert_eq!(restored.audio_tracks.len(), AUDIO_TRACKS);
    }

    /// チェーン (複数段) が順序ごと往復すること
    #[test]
    fn round_trip_keeps_the_chain_order() {
        let mut track = audio_track(0, "synth.clap", "s", b"1");
        track.nodes.push(snapshot("gain.clap", "g", b"2"));
        track.nodes.push(snapshot("verb.clap", "v", b"3"));
        let tracks = audio_tracks(&[(1, track)]);

        let restored = from_str(&to_string(&MidiEditor::default(), &tracks).unwrap()).unwrap();
        let ids: Vec<_> = restored.audio_tracks[1]
            .nodes
            .iter()
            .map(|node| node.id.as_str())
            .collect();
        assert_eq!(ids, vec!["s", "g", "v"], "上から順のまま");
    }

    /// マスターは送り先を持たないこと (書き出しでも落とす)
    #[test]
    fn master_never_gets_sends() {
        let mut tracks = empty_audio_tracks();
        tracks[MASTER].sends = vec![3]; // 手で壊した状態
        let text = to_string(&MidiEditor::default(), &tracks).unwrap();

        let restored = from_str(&text).expect("書き出しで落ちるので読めること");
        assert!(restored.audio_tracks[MASTER].sends.is_empty());
    }

    /// 状態は base64 で載ること (数値の配列にしない)
    #[test]
    fn state_is_stored_as_base64() {
        let tracks = audio_tracks(&[(1, audio_track(0, "a.clap", "x", b"Hello"))]);
        let text = to_string(&MidiEditor::default(), &tracks).unwrap();

        assert!(text.contains("SGVsbG8="), "base64 で載ること: {text}");
        assert!(!text.contains("state: ["), "数値の配列にしないこと");
    }

    /// 音源欄はファイル末尾に置くこと (読みたい部分を先頭に残すため)
    #[test]
    fn audio_tracks_are_written_last() {
        let tracks = audio_tracks(&[(1, audio_track(0, "a.clap", "x", &[0; 64]))]);
        let text = to_string(&sample(), &tracks).unwrap();

        let at = |needle: &str| text.find(needle).expect(needle);
        assert!(at("tempo:") < at("audio_tracks:"));
        assert!(at("notes:") < at("audio_tracks:"));
    }

    /// バージョン1 のファイル (音源欄なし) がそのまま読めること
    #[test]
    fn version_one_files_still_load() {
        let text = "(version: 1, tempo: 120, beats: 4, beat_type: 4, \
                    tracks: [ (name: \"A\", lanes: 1) ], \
                    notes: [ (start: 0.0, duration: 1.0) ])";

        let loaded = from_str(text).expect("読めること");
        assert_eq!(loaded.editor.tempo, 120);
        assert_eq!(loaded.audio_tracks.len(), AUDIO_TRACKS);
        assert!(
            loaded
                .audio_tracks
                .iter()
                .all(|track| track.nodes.is_empty()),
            "音源なしとして扱うこと"
        );
    }

    /// **バージョン2 の音源が、1つずらしてオーディオトラックへ移ること。**
    /// 落ちると、開き直したときに音源が別のトラックへ載る。
    #[test]
    fn version_two_plugins_move_to_audio_tracks() {
        let text = "(version: 2, tempo: 120, beats: 4, beat_type: 4, \
                    tracks: [ (name: \"A\", lanes: 1), (name: \"B\", lanes: 1) ], \
                    notes: [], \
                    plugins: [ Some((kind: Clap, path: \"a.clap\", id: \"x\", state: \"\")), \
                               Some((kind: Vst3, path: \"b.vst3\", id: \"y\", state: \"\")) ])";

        let loaded = from_str(text).expect("読めること");
        assert!(loaded.audio_tracks[MASTER].nodes.is_empty(), "0 はマスター");

        let first = &loaded.audio_tracks[1];
        assert_eq!(first.nodes[0].id, "x");
        assert_eq!(first.midi_track, Some(0), "打ち込みトラック0 を受け取る");
        assert_eq!(first.sends, vec![MASTER], "マスターへ送る");

        let second = &loaded.audio_tracks[2];
        assert_eq!(second.nodes[0].id, "y");
        assert_eq!(second.midi_track, Some(1));
        assert!(loaded.overflow.is_empty());
    }

    /// **16本に収まらないバージョン2 は、溢れたぶんを名指しで知らせること。**
    /// 黙って捨てると、開いた人は音源が消えたことに気付けない。
    #[test]
    fn version_two_overflow_is_reported() {
        let mut plugins = String::new();
        for index in 0..AUDIO_TRACKS {
            plugins.push_str(&format!(
                "Some((kind: Clap, path: \"p{index}.clap\", id: \"p{index}\", state: \"\")),"
            ));
        }
        let tracks = (0..AUDIO_TRACKS)
            .map(|index| format!("(name: \"t{index}\", lanes: 1)"))
            .collect::<Vec<_>>()
            .join(",");
        let text = format!(
            "(version: 2, tempo: 120, beats: 4, beat_type: 4, \
             tracks: [{tracks}], notes: [], plugins: [{plugins}])"
        );

        let loaded = from_str(&text).expect("プロジェクト自体は読めること");
        // 打ち込み 0..14 が オーディオ 1..15 に載り、15 番目が溢れる
        assert_eq!(loaded.audio_tracks[15].nodes[0].id, "p14");
        assert_eq!(loaded.overflow.len(), 1, "溢れたのは1つ");
        assert!(
            loaded.overflow[0].contains("トラック 16"),
            "何番が載らなかったかを言うこと: {}",
            loaded.overflow[0]
        );
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
        let plugin = &loaded.audio_tracks[1].nodes[0];
        assert!(plugin.state.is_empty(), "状態だけ諦めること");
        assert_eq!(plugin.id, "x");
    }

    /// 送り先が範囲外・自分自身・重複なら弾くこと
    #[test]
    fn invalid_sends_are_rejected() {
        let doc = |track_index: usize, sends: &str| {
            let tracks = (0..AUDIO_TRACKS)
                .map(|index| {
                    if index == track_index {
                        format!("(sends: [{sends}])")
                    } else {
                        "()".into()
                    }
                })
                .collect::<Vec<_>>()
                .join(",");
            format!(
                "(version: 3, tempo: 120, beats: 4, beat_type: 4, \
                 tracks: [(name: \"A\", lanes: 1)], notes: [], audio_tracks: [{tracks}])"
            )
        };

        assert!(load(&doc(1, "(to: 0)")).is_ok(), "正常なものは通ること");
        assert!(load(&doc(1, "(to: 99)"))
            .unwrap_err()
            .contains("存在しません"));
        assert!(load(&doc(1, "(to: 1)")).unwrap_err().contains("自分自身"));
        assert!(load(&doc(1, "(to: 0),(to: 0)"))
            .unwrap_err()
            .contains("重複"));
        assert!(load(&doc(MASTER, "(to: 1)"))
            .unwrap_err()
            .contains("マスターは送り先を持てません"));
    }

    /// **輪になっているファイルを拒否すること。**
    ///
    /// 対で1本・マスターは受け手、の2つでは3本以上の輪が塞がらない。
    /// 通してしまうとオーディオスレッドが無限に辿る。
    #[test]
    fn cycles_in_the_file_are_rejected() {
        let tracks = (0..AUDIO_TRACKS)
            .map(|index| match index {
                1 => "(sends: [(to: 2)])".to_string(),
                2 => "(sends: [(to: 3)])".to_string(),
                3 => "(sends: [(to: 1)])".to_string(),
                _ => "()".to_string(),
            })
            .collect::<Vec<_>>()
            .join(",");
        let text = format!(
            "(version: 3, tempo: 120, beats: 4, beat_type: 4, \
             tracks: [(name: \"A\", lanes: 1)], notes: [], audio_tracks: [{tracks}])"
        );

        let error = load(&text).unwrap_err();
        assert!(error.contains("輪になっています"), "実際: {error}");
    }

    /// 互いに送り合うのは1本の接続として矛盾するので弾くこと
    #[test]
    fn mutual_sends_are_rejected() {
        let tracks = (0..AUDIO_TRACKS)
            .map(|index| match index {
                1 => "(sends: [(to: 2)])".to_string(),
                2 => "(sends: [(to: 1)])".to_string(),
                _ => "()".to_string(),
            })
            .collect::<Vec<_>>()
            .join(",");
        let text = format!(
            "(version: 3, tempo: 120, beats: 4, beat_type: 4, \
             tracks: [(name: \"A\", lanes: 1)], notes: [], audio_tracks: [{tracks}])"
        );

        let error = load(&text).unwrap_err();
        assert!(error.contains("互いに送り合って"), "実際: {error}");
    }

    /// **ファイルに書いた繋ぎ方が、そのまま実行順になること。**
    ///
    /// 落ちると「保存した繋ぎ方で鳴らない」ことになる。往復するだけでなく、
    /// エンジンが受け取れる形まで通して確かめる。
    #[test]
    fn routing_from_the_file_becomes_the_execution_order() {
        let mut tracks = empty_audio_tracks();
        // 3 → 1 → 0 の直列
        tracks[3].sends = vec![1];
        tracks[1].sends = vec![MASTER];

        let restored = from_str(&to_string(&MidiEditor::default(), &tracks).unwrap()).unwrap();
        assert_eq!(restored.audio_tracks[3].sends, vec![1]);
        assert_eq!(restored.audio_tracks[1].sends, vec![MASTER]);

        let lists: Vec<Vec<usize>> = restored
            .audio_tracks
            .iter()
            .map(|track| track.sends.clone())
            .collect();
        let routing = crate::audio::graph::Routing::from_lists(&lists).expect("組めること");

        let order: Vec<_> = routing.order().collect();
        let at = |track: usize| order.iter().position(|step| *step == track).unwrap();
        assert!(at(3) < at(1), "送り元が先に来ること");
        assert!(at(1) < at(MASTER));
    }

    /// 本数が `audio::graph` と食い違わないこと。
    /// **ずれると保存と再生でトラック数が合わなくなる。**
    #[test]
    fn audio_track_count_matches_the_engine() {
        assert_eq!(AUDIO_TRACKS, crate::audio::graph::AUDIO_TRACKS);
        assert_eq!(MASTER, crate::audio::graph::MASTER);
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
