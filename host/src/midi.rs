//! 標準 MIDI ファイル (SMF) の読み書き。
//!
//! エディタの内部表現は「四分音符 = 1.0」の実数なので、SMF のティックとは
//! ここで相互変換する。段 (lane) は MIDI に存在しない概念なので、読み込み時は
//! 重ならないように機械的に割り振る。

use crate::sequencer::{MidiEditor, Note, ScaleMode};
use midly::num::{u4, u7, u15, u24, u28};
use midly::{Format, Header, MetaMessage, MidiMessage, Smf, Timing, Track, TrackEvent, TrackEventKind};
use std::collections::HashMap;

/// 書き出すときの分解能 (四分音符あたりのティック数)。
/// 連符 (3/5/6/7 等分) が割り切れるよう 960 にしている。
const TICKS_PER_QUARTER: u16 = 960;

/// 読み込んだ内容
pub struct Imported {
    pub notes: Vec<Note>,
    /// テンポ (BPM)。ファイルに無ければ None
    pub tempo: Option<u32>,
    /// 拍子 (分子, 分母)。ファイルに無ければ None
    pub time_signature: Option<(u32, u32)>,
}

/// トラック名に入れる段の目印。読み込み時にここから段を復元する。
const LANE_NAME_PREFIX: &str = "Lane ";

/// シーケンスを SMF のバイト列にする。
///
/// 段 (lane) は MIDI に無い概念なので、**1段につき1トラック**として書き出し、
/// トラック名に段番号を入れる (`Lane 3` など)。読み戻したときに段の配置が
/// そのまま復元でき、段数16の制限も受けない。ノートの無い段も空トラックとして
/// 残すので、段の抜けも保たれる。チャンネルにも `段 % 16` を入れてあるので、
/// チャンネルで分けるアプリでも段ごとに分かれて見える。
pub fn to_bytes(editor: &MidiEditor) -> Result<Vec<u8>, String> {
    let tpq = TICKS_PER_QUARTER as f32;
    let scale = editor.scale;

    let lane_count = editor
        .notes
        .iter()
        .map(|note| note.lane + 1)
        .max()
        .unwrap_or(0);

    // トラック名はイベントから借用されるので、書き出しが終わるまで保持しておく
    let lane_names: Vec<String> = (0..lane_count)
        .map(|lane| format!("{LANE_NAME_PREFIX}{lane}"))
        .collect();

    // 先頭はテンポ・拍子だけを持つコンダクタトラック (SMF の慣習)
    let us_per_quarter = 60_000_000u32 / editor.tempo.max(1);
    let conductor = vec![
        TrackEvent {
            delta: u28::from(0),
            kind: TrackEventKind::Meta(MetaMessage::Tempo(u24::from(us_per_quarter))),
        },
        TrackEvent {
            delta: u28::from(0),
            kind: TrackEventKind::Meta(MetaMessage::TimeSignature(
                editor.beats.clamp(1, 255) as u8,
                denominator_power(editor.beat_type),
                24,
                8,
            )),
        },
        TrackEvent {
            delta: u28::from(0),
            kind: TrackEventKind::Meta(MetaMessage::EndOfTrack),
        },
    ];

    let mut tracks: Vec<Track> = Vec::with_capacity(lane_count + 1);
    tracks.push(conductor);

    for (lane, name) in lane_names.iter().enumerate() {
        // (絶対ティック, ノートオフを先に並べるための順序, イベント)
        let mut events: Vec<(u32, u8, TrackEventKind)> = Vec::new();
        let channel = u4::from((lane % 16) as u8);

        for note in editor.notes.iter().filter(|note| note.lane == lane) {
            let Some(key) = note.key(scale) else {
                continue; // MIDI の範囲外 (0..=127) は書き出せない
            };
            if note.duration <= 0.0 {
                continue;
            }
            let start = (note.start_tick.max(0.0) * tpq).round() as u32;
            let end = ((note.end_tick().max(0.0) * tpq).round() as u32).max(start + 1);
            let key = u7::from(key.min(127));
            let velocity = u7::from(note.velocity.clamp(1, 127));

            events.push((
                start,
                1,
                TrackEventKind::Midi {
                    channel,
                    message: MidiMessage::NoteOn { key, vel: velocity },
                },
            ));
            events.push((
                end,
                0,
                TrackEventKind::Midi {
                    channel,
                    message: MidiMessage::NoteOff {
                        key,
                        vel: u7::from(0),
                    },
                },
            ));
        }

        // 同時刻ではノートオフを先に (順序フィールドが小さい方が先)
        events.sort_by_key(|(tick, order, _)| (*tick, *order));

        let mut track: Track = Vec::with_capacity(events.len() + 2);
        track.push(TrackEvent {
            delta: u28::from(0),
            kind: TrackEventKind::Meta(MetaMessage::TrackName(name.as_bytes())),
        });

        let mut previous = 0u32;
        for (tick, _, kind) in events {
            track.push(TrackEvent {
                delta: u28::from(tick - previous),
                kind,
            });
            previous = tick;
        }
        track.push(TrackEvent {
            delta: u28::from(0),
            kind: TrackEventKind::Meta(MetaMessage::EndOfTrack),
        });
        tracks.push(track);
    }

    let smf = Smf {
        header: Header::new(
            Format::Parallel,
            Timing::Metrical(u15::from(TICKS_PER_QUARTER)),
        ),
        tracks,
    };

    let mut bytes = Vec::new();
    smf.write_std(&mut bytes)
        .map_err(|e| format!("MIDI の書き出しに失敗しました: {e}"))?;
    Ok(bytes)
}

/// SMF のバイト列を読み込む。段は重ならないように割り振る。
pub fn from_bytes(bytes: &[u8], scale: ScaleMode) -> Result<Imported, String> {
    let smf = Smf::parse(bytes).map_err(|e| format!("MIDI を読めませんでした: {e}"))?;

    let ticks_per_quarter = match smf.header.timing {
        Timing::Metrical(tpq) => tpq.as_int() as f32,
        Timing::Timecode(..) => {
            return Err("SMPTE タイムコードの MIDI には未対応です".to_string())
        }
    };
    if ticks_per_quarter <= 0.0 {
        return Err("分解能が不正な MIDI です".to_string());
    }

    let mut tempo = None;
    let mut time_signature = None;
    // トラックごとの (トラック名から分かる段, ノート列)
    let mut parsed_tracks: Vec<(Option<usize>, Vec<(u32, u32, u8, u8)>)> = Vec::new();

    for track in &smf.tracks {
        // 鳴っている音: (チャンネル, キー) -> (開始ティック, ベロシティ)
        let mut sounding: HashMap<(u8, u8), (u32, u8)> = HashMap::new();
        // (開始ティック, 終了ティック, キー, ベロシティ)
        let mut raw_notes: Vec<(u32, u32, u8, u8)> = Vec::new();
        let mut lane_from_name = None;

        let mut at = 0u32;
        for event in track {
            at = at.saturating_add(event.delta.as_int());
            match event.kind {
                TrackEventKind::Midi { channel, message } => {
                    let channel = channel.as_int();
                    match message {
                        // ベロシティ0のノートオンはノートオフ扱い (よくある省略形)
                        MidiMessage::NoteOn { key, vel } if vel.as_int() > 0 => {
                            sounding.insert((channel, key.as_int()), (at, vel.as_int()));
                        }
                        MidiMessage::NoteOn { key, .. } | MidiMessage::NoteOff { key, .. } => {
                            if let Some((start, velocity)) =
                                sounding.remove(&(channel, key.as_int()))
                            {
                                raw_notes.push((start, at.max(start + 1), key.as_int(), velocity));
                            }
                        }
                        _ => {}
                    }
                }
                TrackEventKind::Meta(MetaMessage::Tempo(us_per_quarter)) => {
                    // 途中のテンポ変化には未対応なので最初の1つだけ使う
                    if tempo.is_none() {
                        let us = us_per_quarter.as_int().max(1);
                        tempo = Some((60_000_000 / us).clamp(20, 300));
                    }
                }
                TrackEventKind::Meta(MetaMessage::TimeSignature(numerator, denominator, ..)) => {
                    if time_signature.is_none() {
                        let denominator = 1u32 << denominator.min(5);
                        time_signature = Some((numerator.max(1) as u32, denominator));
                    }
                }
                TrackEventKind::Meta(MetaMessage::TrackName(name)) => {
                    lane_from_name = lane_from_name.or_else(|| parse_lane_name(name));
                }
                _ => {}
            }
        }

        raw_notes.sort_by_key(|(start, end, key, _)| (*start, *end, *key));
        if !raw_notes.is_empty() || lane_from_name.is_some() {
            parsed_tracks.push((lane_from_name, raw_notes));
        }
    }

    let steps = scale.steps_per_octave().max(1);
    // 段ごとの「最後に音が終わる位置」。段が未指定のトラックの割り振りに使う
    let mut lane_ends: HashMap<usize, f32> = HashMap::new();
    let mut notes = Vec::new();

    for (lane_from_name, raw_notes) in parsed_tracks {
        // 自分で書き出したファイルはトラック名から段が分かるので、そのまま使う
        // (重なりも含めて見た目が完全に復元される)。
        // 段の情報が無いファイルは、トラックごとに空いている段へ順に置く。
        let base = lane_from_name.unwrap_or_else(|| {
            lane_ends.keys().map(|lane| lane + 1).max().unwrap_or(0)
        });

        for (start, end, key, velocity) in raw_notes {
            let start_tick = start as f32 / ticks_per_quarter;
            let duration = (end - start) as f32 / ticks_per_quarter;

            // (半音, オクターブ) へ。(0,4) が 60 になる基準は Note::key と揃える
            let from_middle = key as i32 - 60;
            let semitone = from_middle.rem_euclid(steps);
            let octave = 4 + from_middle.div_euclid(steps);

            const EPS: f32 = 1e-4;
            let mut lane = base;
            if lane_from_name.is_none() {
                // 和音などで重なるときは下の段へずらす
                while lane_ends
                    .get(&lane)
                    .is_some_and(|end| *end > start_tick + EPS)
                {
                    lane += 1;
                }
            }
            lane_ends.insert(lane, start_tick + duration);

            notes.push(Note {
                start_tick,
                duration,
                semitone,
                octave,
                velocity: velocity.clamp(1, 127),
                lane,
            });
        }

        // ノートが無くても段は使用済みにして、空の段が詰まらないようにする
        lane_ends.entry(base).or_insert(0.0);
    }

    notes.sort_by(|a, b| {
        a.start_tick
            .total_cmp(&b.start_tick)
            .then(a.lane.cmp(&b.lane))
    });

    Ok(Imported {
        notes,
        tempo,
        time_signature,
    })
}

/// トラック名から段番号を読む (`Lane 3` -> 3)。それ以外の名前なら None。
fn parse_lane_name(name: &[u8]) -> Option<usize> {
    let name = std::str::from_utf8(name).ok()?;
    name.trim()
        .strip_prefix(LANE_NAME_PREFIX)?
        .trim()
        .parse()
        .ok()
}

/// 拍子の分母を SMF の指数表現にする (4 -> 2、8 -> 3)
fn denominator_power(beat_type: u32) -> u8 {
    match beat_type {
        1 => 0,
        2 => 1,
        4 => 2,
        8 => 3,
        16 => 4,
        _ => 5,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn note(start: f32, duration: f32, semitone: i32, octave: i32, velocity: u8) -> Note {
        Note {
            start_tick: start,
            duration,
            semitone,
            octave,
            velocity,
            lane: 0,
        }
    }

    fn on_lane(mut note: Note, lane: usize) -> Note {
        note.lane = lane;
        note
    }

    /// 書き出して読み戻すと、位置・音価・音高・ベロシティが保たれること
    #[test]
    fn round_trip_keeps_notes() {
        let mut editor = MidiEditor::default();
        editor.tempo = 96;
        editor.beats = 3;
        editor.beat_type = 8;
        editor.notes = vec![
            note(0.0, 1.0, 0, 4, 100),
            note(0.5, 0.25, 7, 5, 40),
            note(2.0, 1.5, 11, 3, 127),
        ];

        let bytes = to_bytes(&editor).expect("書き出せること");
        let imported = from_bytes(&bytes, ScaleMode::Equal12).expect("読み戻せること");

        assert_eq!(imported.tempo, Some(96));
        assert_eq!(imported.time_signature, Some((3, 8)));
        assert_eq!(imported.notes.len(), 3);

        for (restored, original) in imported.notes.iter().zip(editor.notes.iter()) {
            assert!((restored.start_tick - original.start_tick).abs() < 1e-3);
            assert!((restored.duration - original.duration).abs() < 1e-3);
            assert_eq!(restored.semitone, original.semitone);
            assert_eq!(restored.octave, original.octave);
            assert_eq!(restored.velocity, original.velocity);
        }
    }

    /// 段の配置が往復で完全に保たれること (段の抜けや、同じ段での重なりも含む)
    #[test]
    fn round_trip_keeps_lanes() {
        let mut editor = MidiEditor::default();
        editor.notes = vec![
            on_lane(note(0.0, 1.0, 0, 4, 100), 0),
            // 段1〜4 は空 (抜けたまま復元されること)
            on_lane(note(0.0, 1.0, 4, 4, 100), 5),
            on_lane(note(2.0, 1.0, 7, 4, 100), 5),
            // 同じ段で重なっているノートも、そのままの段で戻ること
            on_lane(note(0.0, 2.0, 0, 5, 100), 7),
            on_lane(note(1.0, 2.0, 4, 5, 100), 7),
        ];

        let bytes = to_bytes(&editor).expect("書き出せること");
        let imported = from_bytes(&bytes, ScaleMode::Equal12).expect("読み戻せること");

        let mut lanes: Vec<usize> = imported.notes.iter().map(|n| n.lane).collect();
        lanes.sort_unstable();
        assert_eq!(lanes, vec![0, 5, 5, 7, 7]);
        assert_eq!(imported.notes.len(), 5);
    }

    /// 段の情報が無いファイル (他アプリの MIDI) は、重ならないよう段に割り振ること
    #[test]
    fn import_without_lane_names_spreads_chords() {
        // トラック名を持たない単一トラックの MIDI を組み立てる
        let mut editor = MidiEditor::default();
        editor.notes = vec![
            note(0.0, 1.0, 0, 4, 100), // 和音の下
            note(0.0, 1.0, 4, 4, 100), // 和音の上
            note(1.0, 1.0, 7, 4, 100), // 空いたので段0 に戻る
        ];
        let bytes = to_bytes(&editor).expect("書き出せること");
        let mut smf = Smf::parse(&bytes).expect("読めること");
        for track in &mut smf.tracks {
            track.retain(|event| {
                !matches!(event.kind, TrackEventKind::Meta(MetaMessage::TrackName(_)))
            });
        }
        let mut foreign = Vec::new();
        smf.write_std(&mut foreign).expect("書き出せること");

        let imported = from_bytes(&foreign, ScaleMode::Equal12).expect("読み戻せること");
        let lanes: Vec<usize> = imported.notes.iter().map(|n| n.lane).collect();
        assert_eq!(lanes, vec![0, 1, 0], "重なる音だけ下の段へずれること");
    }

    /// ボーレン・ピアースでは13ステップで1オクターブ進むこと
    #[test]
    fn import_uses_the_scale_mode() {
        let mut editor = MidiEditor::default();
        editor.scale = ScaleMode::BohlenPierce13;
        editor.notes = vec![note(0.0, 1.0, 0, 5, 100)]; // key = 73

        let bytes = to_bytes(&editor).expect("書き出せること");
        let imported =
            from_bytes(&bytes, ScaleMode::BohlenPierce13).expect("読み戻せること");
        assert_eq!(imported.notes[0].semitone, 0);
        assert_eq!(imported.notes[0].octave, 5);

        // 同じファイルを12平均律として読むと (1,5) になる (73 = 60 + 13)
        let imported = from_bytes(&bytes, ScaleMode::Equal12).expect("読み戻せること");
        assert_eq!(imported.notes[0].semitone, 1);
        assert_eq!(imported.notes[0].octave, 5);
    }

    /// MIDI の範囲外のノートは書き出さないこと
    #[test]
    fn out_of_range_notes_are_skipped() {
        let mut editor = MidiEditor::default();
        editor.notes = vec![note(0.0, 1.0, 0, -2, 100), note(0.0, 1.0, 0, 4, 100)];

        let bytes = to_bytes(&editor).expect("書き出せること");
        let imported = from_bytes(&bytes, ScaleMode::Equal12).expect("読み戻せること");
        assert_eq!(imported.notes.len(), 1, "範囲外の1つは落ちること");
        assert_eq!(imported.notes[0].octave, 4);
    }
}
