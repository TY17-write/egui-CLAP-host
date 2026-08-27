//! 標準 MIDI ファイル (SMF) の読み書き。
//!
//! エディタの内部表現は「四分音符 = 1.0」の実数なので、SMF のティックとは
//! ここで相互変換する。段 (lane) は MIDI に存在しない概念なので、読み込み時は
//! 重ならないように機械的に割り振る。

#[cfg(test)]
use crate::sequencer::TrackInfo;
use crate::sequencer::{MidiEditor, Note, ScaleMode, CC_RELEASE};
use midly::num::{u15, u24, u28, u4, u7};
use midly::{
    Format, Header, MetaMessage, MidiMessage, Smf, Timing, Track, TrackEvent, TrackEventKind,
};
use std::collections::{BTreeMap, HashMap};

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
    /// CC 段にする (トラック, 段, CC 番号)。`notes` の中でこの段に置かれたものは
    /// 音符ではなく CC ブロックになる。
    pub lane_ccs: Vec<(usize, usize, u8)>,
}

/// 120 以上はチャンネルモードメッセージ (オールノートオフなど) で、
/// 書いて並べる類の CC ではない。DAW が末尾に入れることが多いので読み飛ばす。
const FIRST_CHANNEL_MODE_CC: u8 = 120;

/// トラック名に入れる目印。読み込み時にここからトラックと段を復元する。
/// 例: `Track 0 Lane 2`
const TRACK_NAME_PREFIX: &str = "Track ";
const LANE_NAME_PREFIX: &str = "Lane ";

/// シーケンスを SMF のバイト列にする。
///
/// トラックと段 (lane) の入れ子は MIDI に無い概念なので、**(トラック, 段) の
/// 組ごとに1つの SMF トラック**として書き出し、トラック名にその番号を入れる
/// (`Track 0 Lane 2` など)。読み戻したときに配置がそのまま復元でき、
/// 段数16の制限も受けない。チャンネルにも `段 % 16` を入れてあるので、
/// チャンネルで分けるアプリでも段ごとに分かれて見える。
pub fn to_bytes(editor: &MidiEditor) -> Result<Vec<u8>, String> {
    let tpq = TICKS_PER_QUARTER as f32;
    let scale = editor.scale;

    // 空の段も書き出して、段の抜けや空トラックの構成をそのまま保つ
    let mut cells: Vec<(usize, usize)> = Vec::new();
    for (track, info) in editor.tracks.iter().enumerate() {
        for lane in 0..info.lanes.max(1) {
            cells.push((track, lane));
        }
    }
    for note in &editor.notes {
        if !cells.contains(&(note.track, note.lane)) {
            cells.push((note.track, note.lane));
        }
    }

    // トラック名はイベントから借用されるので、書き出しが終わるまで保持しておく
    let names: Vec<String> = cells
        .iter()
        .map(|(track, lane)| format!("{TRACK_NAME_PREFIX}{track} {LANE_NAME_PREFIX}{lane}"))
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

    let mut tracks: Vec<Track> = Vec::with_capacity(cells.len() + 1);
    tracks.push(conductor);

    // スウィングを乗せた位置で書き出す (記譜のまま残すのはプロジェクト保存の役目)。
    // 段の割り当ては変わらないので、上の cells は記譜のノートから作ってよい。
    let performed = editor.performed_notes();

    for (&(track_index, lane), name) in cells.iter().zip(names.iter()) {
        // (絶対ティック, ノートオフを先に並べるための順序, イベント)
        let mut events: Vec<(u32, u8, TrackEventKind)> = Vec::new();
        let channel = u4::from((lane % 16) as u8);

        // CC 段は音符ではなくコントロールチェンジとして書き出す
        let lane_cc = editor.lane_cc(track_index, lane);

        for note in performed
            .iter()
            .filter(|note| note.track == track_index && note.lane == lane)
        {
            if note.duration <= 0.0 {
                continue;
            }
            let start = (note.start_tick.max(0.0) * tpq).round() as u32;
            let end = ((note.end_tick().max(0.0) * tpq).round() as u32).max(start + 1);

            // 書いた区間だけ効かせる。頭で値、尻で解除値 (再生時と同じ)。
            if let Some(number) = lane_cc {
                let controller = u7::from(number.min(127));
                events.push((
                    start,
                    1,
                    TrackEventKind::Midi {
                        channel,
                        message: MidiMessage::Controller {
                            controller,
                            value: u7::from(note.velocity.min(127)),
                        },
                    },
                ));
                events.push((
                    end,
                    0,
                    TrackEventKind::Midi {
                        channel,
                        message: MidiMessage::Controller {
                            controller,
                            value: u7::from(CC_RELEASE),
                        },
                    },
                ));
                continue;
            }

            let Some(key) = note.key(scale) else {
                continue; // MIDI の範囲外 (0..=127) は書き出せない
            };
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
        // 続くブロックの手前で解除を挟まない (踏み直しになるため。再生側と同じ扱い)
        suppress_redundant_cc_releases(&mut events);

        let mut track: Track = Vec::with_capacity(events.len() + 3);
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

/// 同じ位置で「解除 → 次のブロックの値」と並ぶ解除を取り除く。
///
/// ブロックが隙間なく続くとき、解除の 0 をそのまま書くと**一瞬離して踏み直す**
/// 形になる。再生側 (`sequencer`) と同じ扱いに揃えてある。
///
/// 並び替え済みであること (同時刻では解除が先に来る) が前提。
fn suppress_redundant_cc_releases(events: &mut Vec<(u32, u8, TrackEventKind)>) {
    let controller_of = |kind: &TrackEventKind| match kind {
        TrackEventKind::Midi {
            message: MidiMessage::Controller { controller, value },
            ..
        } => Some((controller.as_int(), value.as_int())),
        _ => None,
    };

    let mut remove = vec![false; events.len()];
    for index in 0..events.len() {
        let Some((number, value)) = controller_of(&events[index].2) else {
            continue;
        };
        if value != CC_RELEASE {
            continue;
        }
        for later in events[index + 1..].iter() {
            if later.0 != events[index].0 {
                break;
            }
            if controller_of(&later.2).is_some_and(|(n, _)| n == number) {
                remove[index] = true;
                break;
            }
        }
    }
    let mut keep = remove.iter().map(|r| !r);
    events.retain(|_| keep.next().unwrap_or(true));
}

/// 段番号が決まるまで持ち越す CC 1トラックぶん。
///
/// **(アプリのトラック, 名前から分かる段, CC 番号ごとのイベント)**。
/// CC 段は音符段より下に並べるので、そのトラックの音符段が確定するまで
/// 段番号を振れない。
type PendingCc = (usize, Option<usize>, BTreeMap<u8, Vec<(u32, u8)>>);

/// SMF のバイト列を読み込む。段は重ならないように割り振る。
pub fn from_bytes(bytes: &[u8], scale: ScaleMode) -> Result<Imported, String> {
    let smf = Smf::parse(bytes).map_err(|e| format!("MIDI を読めませんでした: {e}"))?;

    let ticks_per_quarter = match smf.header.timing {
        Timing::Metrical(tpq) => tpq.as_int() as f32,
        Timing::Timecode(..) => return Err("SMPTE タイムコードの MIDI には未対応です".to_string()),
    };
    if ticks_per_quarter <= 0.0 {
        return Err("分解能が不正な MIDI です".to_string());
    }

    let mut tempo = None;
    let mut time_signature = None;
    /// SMF トラック1つぶんの読み取り結果
    struct ParsedTrack {
        /// トラック名から分かる (トラック, 段)
        from_name: Option<(usize, usize)>,
        /// (開始ティック, 終了ティック, キー, ベロシティ)
        notes: Vec<(u32, u32, u8, u8)>,
        /// CC 番号ごとの (ティック, 値)。ブロックへの復元はあとでまとめて行う
        ccs: BTreeMap<u8, Vec<(u32, u8)>>,
    }
    let mut parsed_tracks: Vec<ParsedTrack> = Vec::new();

    for track in &smf.tracks {
        // 鳴っている音: (チャンネル, キー) -> (開始ティック, ベロシティ)
        let mut sounding: HashMap<(u8, u8), (u32, u8)> = HashMap::new();
        // (開始ティック, 終了ティック, キー, ベロシティ)
        let mut raw_notes: Vec<(u32, u32, u8, u8)> = Vec::new();
        // 番号順に並べたいので BTreeMap (段の並びがファイルごとに揺れないように)
        let mut raw_ccs: BTreeMap<u8, Vec<(u32, u8)>> = BTreeMap::new();
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
                        // チャンネルは見ない (自分で書き出したファイルは
                        // 1つの SMF トラック = 1つの段 なので混ざらない)
                        MidiMessage::Controller { controller, value }
                            if controller.as_int() < FIRST_CHANNEL_MODE_CC =>
                        {
                            raw_ccs
                                .entry(controller.as_int())
                                .or_default()
                                .push((at, value.as_int()));
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
                    lane_from_name = lane_from_name.or_else(|| parse_track_name(name));
                }
                _ => {}
            }
        }

        raw_notes.sort_by_key(|(start, end, key, _)| (*start, *end, *key));
        // 変化のないコントローラは段を作らない (下記)
        raw_ccs.retain(|_, events| changes_at_least_once(events));
        if !raw_notes.is_empty() || !raw_ccs.is_empty() || lane_from_name.is_some() {
            parsed_tracks.push(ParsedTrack {
                from_name: lane_from_name,
                notes: raw_notes,
                ccs: raw_ccs,
            });
        }
    }

    let steps = scale.steps_per_octave().max(1);
    // (トラック, 段) ごとの「最後に音が終わる位置」。名前の無いファイルの割り振りに使う
    let mut lane_ends: HashMap<(usize, usize), f32> = HashMap::new();
    // 名前の無い SMF トラックは、アプリのトラックへ順番に割り当てる
    let mut next_unnamed_track = 0;
    let mut notes = Vec::new();

    // CC の復元は、そのトラックの音符段が決まってからでないと段番号を振れない
    // (CC 段は音符段より下に並べる)。ここでは「どの SMF トラックが
    // アプリのどのトラックへ行ったか」を控えておく。
    let mut pending_ccs: Vec<PendingCc> = Vec::new();
    // ファイル全体の終端。閉じていない CC ブロックをここで切る
    let end_tick = parsed_tracks
        .iter()
        .flat_map(|parsed| {
            parsed
                .notes
                .iter()
                .map(|(_, end, _, _)| *end)
                .chain(parsed.ccs.values().flatten().map(|(at, _)| *at))
        })
        .max()
        .unwrap_or(0);

    for ParsedTrack {
        from_name,
        notes: raw_notes,
        ccs,
    } in parsed_tracks
    {
        // 自分で書き出したファイルは名前から (トラック, 段) が分かるので、そのまま使う
        // (重なりも含めて配置が完全に復元される)。
        // 名前の無いファイルは、SMF トラックごとにアプリのトラックを1本使い、
        // 段は和音が重ならないように割り振る。
        let (base_track, base_lane) = from_name.unwrap_or_else(|| {
            let track = next_unnamed_track;
            next_unnamed_track += 1;
            (track, 0)
        });
        let had_notes = !raw_notes.is_empty();

        for (start, end, key, velocity) in raw_notes {
            let start_tick = start as f32 / ticks_per_quarter;
            let duration = (end - start) as f32 / ticks_per_quarter;

            // (半音, オクターブ) へ。(0,4) が 60 になる基準は Note::key と揃える
            let from_middle = key as i32 - 60;
            let semitone = from_middle.rem_euclid(steps);
            let octave = 4 + from_middle.div_euclid(steps);

            const EPS: f32 = 1e-4;
            let mut lane = base_lane;
            if from_name.is_none() {
                // 和音などで重なるときは下の段へずらす
                while lane_ends
                    .get(&(base_track, lane))
                    .is_some_and(|end| *end > start_tick + EPS)
                {
                    lane += 1;
                }
            }
            lane_ends.insert((base_track, lane), start_tick + duration);

            notes.push(Note {
                start_tick,
                duration,
                semitone,
                octave,
                velocity: velocity.clamp(1, 127),
                // 音符段では読まれない
                velocity_to: velocity.clamp(1, 127),
                track: base_track,
                lane,
            });
        }

        // ノートが無くても使用済みにして、空の段が詰まらないようにする
        lane_ends.entry((base_track, base_lane)).or_insert(0.0);
        next_unnamed_track = next_unnamed_track.max(base_track + 1);

        if !ccs.is_empty() {
            // 自分で書き出したファイルは 1つの SMF トラック = 1つの段 なので、
            // 名前があって音符が無ければ**その段がそのまま CC 段**。
            // これで書き出す前の配置がそのまま戻る。
            let named_lane = from_name.map(|(_, lane)| lane).filter(|_| !had_notes);
            pending_ccs.push((base_track, named_lane, ccs));
        }
    }

    // ---- CC をブロックへ戻す ----
    // 書き出しの規則 (頭で値・尻で 0) をそのまま逆にたどる。
    // 段は音符段の下に積む (アプリ側の「CC 段は最下段」に合わせる)。
    let mut lane_ccs = Vec::new();
    for (track, named_lane, ccs) in pending_ccs {
        // この時点で使われている段の数 = 次に使える段番号
        let mut next_lane = lane_ends
            .keys()
            .filter(|(t, _)| *t == track)
            .map(|(_, lane)| lane + 1)
            .max()
            .unwrap_or(0);
        // 名前から分かる段は、最初の1つにだけ使う
        let mut named_lane = named_lane;

        for (number, events) in ccs {
            let blocks = cc_blocks(&events, end_tick);
            if blocks.is_empty() {
                continue;
            }
            let lane = named_lane.take().unwrap_or_else(|| {
                let lane = next_lane;
                next_lane += 1;
                lane
            });
            lane_ccs.push((track, lane, number));

            for (start, end, value) in blocks {
                notes.push(Note {
                    start_tick: start as f32 / ticks_per_quarter,
                    duration: (end - start) as f32 / ticks_per_quarter,
                    // CC 段では音高は使わないので、既定の位置に置いておく
                    semitone: 0,
                    octave: 4,
                    velocity: value.clamp(1, 127),
                    // CC 段では読まれない
                    velocity_to: value.clamp(1, 127),
                    track,
                    lane,
                });
            }
        }
    }

    notes.sort_by(|a, b| {
        a.start_tick
            .total_cmp(&b.start_tick)
            .then(a.track.cmp(&b.track))
            .then(a.lane.cmp(&b.lane))
    });

    Ok(Imported {
        notes,
        tempo,
        time_signature,
        lane_ccs,
    })
}

/// そのコントローラが一度でも値を変えているか。
///
/// **変えていないものは段を作らない。** DAW の書き出しには「先頭で音量を1回だけ
/// 送る」ような初期設定が入っていることが多く、それを段にすると**書いてもいない
/// 帯が並ぶ**。値が2種類以上あるものだけを、書いて並べた CC とみなす。
fn changes_at_least_once(events: &[(u32, u8)]) -> bool {
    let Some((_, first)) = events.first() else {
        return false;
    };
    events.iter().any(|(_, value)| value != first)
}

/// CC のイベント列をブロックへ戻す。返すのは (開始, 終了, 値)。
///
/// 書き出しの規則 (ブロックの頭で値・尻で 0) をそのまま逆にたどる。
/// **値が 0 でないところで始まり、次のイベントで終わる**。次が別の値なら、
/// そこから続けて新しいブロックが始まる (隣り合うブロックは 0 を挟まないため)。
///
/// 閉じないまま終わったものは `end_tick` で切る。自分で書き出したファイルには
/// 必ず末尾の 0 があるので、これが要るのは他アプリのファイルだけ。
fn cc_blocks(events: &[(u32, u8)], end_tick: u32) -> Vec<(u32, u32, u8)> {
    let mut blocks = Vec::new();
    let mut open: Option<(u32, u8)> = None;

    for (at, value) in events {
        if let Some((start, held)) = open.take() {
            if *at > start {
                blocks.push((start, *at, held));
            }
        }
        if *value != 0 {
            open = Some((*at, *value));
        }
    }
    if let Some((start, held)) = open {
        blocks.push((start, end_tick.max(start + 1), held));
    }
    blocks
}

/// トラック名から (トラック, 段) を読む (`Track 1 Lane 3` -> (1, 3))。
/// 段だけの古い形式 (`Lane 3`) はトラック0として読む。
/// それ以外の名前なら None。
fn parse_track_name(name: &[u8]) -> Option<(usize, usize)> {
    let name = std::str::from_utf8(name).ok()?.trim();

    if let Some(rest) = name.strip_prefix(TRACK_NAME_PREFIX) {
        let (track, lane) = rest.split_once(LANE_NAME_PREFIX)?;
        return Some((track.trim().parse().ok()?, lane.trim().parse().ok()?));
    }
    let lane = name.strip_prefix(LANE_NAME_PREFIX)?.trim().parse().ok()?;
    Some((0, lane))
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
            velocity_to: velocity,
            track: 0,
            lane: 0,
        }
    }

    fn on_lane(mut note: Note, lane: usize) -> Note {
        note.lane = lane;
        note
    }

    /// MIDI エクスポートにもスウィングが乗ること。
    ///
    /// 記譜のまま残す役目はプロジェクト保存 (.ron) が担うので、
    /// こちらは「鳴るとおりに書き出す」側に倒している。
    /// そのぶん書き出した MIDI を読み戻すと跳ねが二重に掛かる。
    #[test]
    fn export_applies_swing() {
        let mut editor = MidiEditor::default(); // 120BPM / 4/4
        editor.notes = vec![note(0.0, 0.5, 0, 4, 100), note(0.5, 0.5, 0, 4, 100)];

        // OFF なら記譜位置がそのまま出る
        let straight = from_bytes(&to_bytes(&editor).unwrap(), ScaleMode::Equal12).unwrap();
        assert!(straight.notes[0].start_tick.abs() < 1e-3);
        assert!((straight.notes[1].start_tick - 0.5).abs() < 1e-3);

        editor.tracks[0].swing = true;
        let swung = from_bytes(&to_bytes(&editor).unwrap(), ScaleMode::Equal12).unwrap();
        assert!(swung.notes[0].start_tick > 0.0, "拍頭が遅れること");
        assert!(swung.notes[1].start_tick > 0.5, "裏拍が跳ねること");
    }

    /// CC 段は音符ではなく、頭で値・尻で解除値のコントロールチェンジになること。
    ///
    /// **「書かれていない部分は CC 無し」の書き出し側。** ここが抜けると、
    /// 書き出した MIDI でペダルが踏みっぱなしになる。
    #[test]
    fn cc_lane_exports_value_and_release() {
        let mut editor = MidiEditor::default();
        editor.tracks[0].set_lane_cc(0, Some(64));
        editor.notes = vec![note(0.0, 1.0, 0, 4, 100)];

        let bytes = to_bytes(&editor).unwrap();
        let smf = Smf::parse(&bytes).expect("解析できること");
        let mut seen: Vec<(u32, u8, u8)> = Vec::new();
        for track in &smf.tracks {
            let mut tick = 0u32;
            for event in track {
                tick += event.delta.as_int();
                if let TrackEventKind::Midi {
                    message: MidiMessage::Controller { controller, value },
                    ..
                } = event.kind
                {
                    seen.push((tick, controller.as_int(), value.as_int()));
                }
            }
        }

        assert_eq!(
            seen,
            vec![(0, 64, 100), (TICKS_PER_QUARTER as u32, 64, CC_RELEASE),],
            "頭で値、尻で解除値が並ぶこと"
        );

        // 読み戻すと CC 段として復元されること。
        // **音符段として戻ると音源が鳴ってしまう**ので、段の種別まで確かめる。
        let back = from_bytes(&bytes, ScaleMode::Equal12).unwrap();
        assert_eq!(back.lane_ccs, vec![(0, 0, 64)], "CC64 の段が1本戻ること");
        assert_eq!(back.notes.len(), 1, "ブロックが1つ戻ること");
        assert_eq!(back.notes[0].lane, 0);
        assert_eq!(back.notes[0].velocity, 100, "値が保たれること");
        assert!(
            (back.notes[0].duration - 1.0).abs() < 1e-4,
            "長さが保たれること"
        );
    }

    /// 音符段と CC 段が混ざっていても、配置がそのまま戻ること。
    ///
    /// **段番号がずれると音符が CC 段に入る** (逆も同じ)。往復で崩れないことを、
    /// 段の種別まで含めて確かめる。
    #[test]
    fn round_trip_keeps_notes_and_cc_lanes_together() {
        let mut editor = MidiEditor::default();
        editor.tracks[0].lanes = 2;
        editor.add_cc_lane(0, 64); // 段2 が CC
        editor.notes = vec![
            on_track(note(0.0, 1.0, 0, 4, 100), 0, 0),
            on_track(note(1.0, 1.0, 4, 4, 90), 0, 1),
            on_track(note(0.0, 2.0, 0, 4, 80), 0, 2), // CC ブロック
        ];

        let back = from_bytes(&to_bytes(&editor).unwrap(), ScaleMode::Equal12).unwrap();
        assert_eq!(back.lane_ccs, vec![(0, 2, 64)], "CC 段の位置が戻ること");

        let mut lanes: Vec<usize> = back.notes.iter().map(|note| note.lane).collect();
        lanes.sort_unstable();
        assert_eq!(lanes, vec![0, 1, 2], "音符2つと CC ブロック1つが元の段へ");

        let cc = back.notes.iter().find(|note| note.lane == 2).unwrap();
        assert_eq!(cc.velocity, 80, "CC 値が保たれること");
    }

    /// 隣り合うブロック (境目に 0 が無い) も、2つに分かれて戻ること
    #[test]
    fn adjacent_cc_blocks_round_trip() {
        let mut editor = MidiEditor::default();
        editor.tracks[0].set_lane_cc(0, Some(64));
        editor.notes = vec![note(0.0, 1.0, 0, 4, 100), note(1.0, 1.0, 0, 4, 40)];

        let back = from_bytes(&to_bytes(&editor).unwrap(), ScaleMode::Equal12).unwrap();
        assert_eq!(back.lane_ccs, vec![(0, 0, 64)]);
        let mut blocks: Vec<(f32, u8)> = back
            .notes
            .iter()
            .map(|note| (note.start_tick, note.velocity))
            .collect();
        blocks.sort_by(|a, b| a.0.total_cmp(&b.0));
        assert_eq!(blocks, vec![(0.0, 100), (1.0, 40)]);
    }

    /// 変化のないコントローラは段にしないこと。
    ///
    /// **DAW の書き出しには「先頭で音量を1回だけ送る」ような初期設定が入っている
    /// ことが多く**、それを段にすると書いてもいない帯が並ぶ。
    #[test]
    fn unchanging_controllers_do_not_become_lanes() {
        let smf = Smf {
            header: Header::new(
                Format::Parallel,
                Timing::Metrical(u15::from(TICKS_PER_QUARTER)),
            ),
            tracks: vec![vec![
                // 音量を先頭で1回だけ (初期設定)
                controller_event(0, 7, 100),
                // 同じ値を何度送っても「変化なし」
                controller_event(480, 10, 64),
                controller_event(480, 10, 64),
                // オールノートオフ (チャンネルモード。書いた CC ではない)
                controller_event(960, 123, 0),
                TrackEvent {
                    delta: u28::from(0),
                    kind: TrackEventKind::Meta(MetaMessage::EndOfTrack),
                },
            ]],
        };
        let mut bytes = Vec::new();
        smf.write_std(&mut bytes).unwrap();

        let back = from_bytes(&bytes, ScaleMode::Equal12).unwrap();
        assert!(back.lane_ccs.is_empty(), "段が1本も作られないこと");
        assert!(back.notes.is_empty());
    }

    fn controller_event(delta: u32, controller: u8, value: u8) -> TrackEvent<'static> {
        TrackEvent {
            delta: u28::from(delta),
            kind: TrackEventKind::Midi {
                channel: u4::from(0),
                message: MidiMessage::Controller {
                    controller: u7::from(controller),
                    value: u7::from(value),
                },
            },
        }
    }

    fn on_track(mut note: Note, track: usize, lane: usize) -> Note {
        note.track = track;
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
            // このエディタはスウィング OFF なので記譜位置がそのまま出る
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

    /// トラックと段の入れ子が往復で保たれること
    #[test]
    fn round_trip_keeps_tracks_and_lanes() {
        let mut editor = MidiEditor::default();
        editor.tracks = vec![TrackInfo::new(0), TrackInfo::new(1), TrackInfo::new(2)];
        editor.notes = vec![
            on_track(note(0.0, 1.0, 0, 4, 100), 0, 0),
            on_track(note(1.0, 1.0, 4, 4, 100), 0, 3),
            on_track(note(0.0, 2.0, 7, 3, 80), 2, 1),
        ];

        let bytes = to_bytes(&editor).expect("書き出せること");
        let imported = from_bytes(&bytes, ScaleMode::Equal12).expect("読み戻せること");

        let mut placed: Vec<(usize, usize)> = imported
            .notes
            .iter()
            .map(|note| (note.track, note.lane))
            .collect();
        placed.sort_unstable();
        assert_eq!(placed, vec![(0, 0), (0, 3), (2, 1)]);
    }

    /// 段の情報が無いファイル (他アプリの MIDI) は、トラックごとに分かれること
    #[test]
    fn import_without_names_uses_one_track_per_smf_track() {
        // トラック名を落とした2トラックの MIDI を作る
        let mut editor = MidiEditor::default();
        editor.tracks = vec![TrackInfo::new(0), TrackInfo::new(1)];
        editor.notes = vec![
            on_track(note(0.0, 1.0, 0, 4, 100), 0, 0),
            on_track(note(0.0, 1.0, 7, 4, 100), 1, 0),
        ];
        let bytes = to_bytes(&editor).expect("書き出せること");
        let mut smf = Smf::parse(&bytes).expect("読めること");
        for track in &mut smf.tracks {
            track.retain(|event| {
                !matches!(event.kind, TrackEventKind::Meta(MetaMessage::TrackName(_)))
            });
        }
        // ノートの無いトラックは落として、他アプリが書いたファイルに近づける
        smf.tracks.retain(|track| {
            track.iter().any(|event| {
                matches!(
                    event.kind,
                    TrackEventKind::Midi {
                        message: MidiMessage::NoteOn { .. },
                        ..
                    }
                )
            })
        });
        let mut foreign = Vec::new();
        smf.write_std(&mut foreign).expect("書き出せること");

        let imported = from_bytes(&foreign, ScaleMode::Equal12).expect("読み戻せること");
        let tracks: Vec<usize> = imported.notes.iter().map(|note| note.track).collect();
        assert_eq!(tracks, vec![0, 1], "SMF トラックごとに別トラックへ入ること");
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
        let imported = from_bytes(&bytes, ScaleMode::BohlenPierce13).expect("読み戻せること");
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
