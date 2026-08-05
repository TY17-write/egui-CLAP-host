//! シーケンス再生のトランスポート (オーディオスレッド上で動く)。
//! サンプル精度でノートイベントをブロック内オフセット付きで発行する。

use crate::sequencer::SeqEvent;
use clack_host::events::event_types::{MidiEvent, NoteChokeEvent, NoteOffEvent, NoteOnEvent};
use clack_host::events::{EventFlags, Match, Pckn};
use clack_host::prelude::*;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

/// GUI からトランスポートへ送る操作
pub enum TransportMsg {
    /// 指定トラックのシーケンスを差し替える。
    /// end_sample はループ/停止判定用の終端 (全トラック共通)。
    SetSequence {
        track: usize,
        events: Box<[SeqEvent]>,
        end_sample: u64,
    },
    Play,
    Stop,
    /// 指定サンプル位置へ移動
    Seek { sample: u64 },
    SetLoop { enabled: bool },
}

/// UI と共有する再生状態
#[derive(Clone)]
pub struct TransportShared {
    /// 現在の再生位置 (サンプル)
    pub pos: Arc<AtomicU64>,
    /// 再生中か
    pub playing: Arc<AtomicBool>,
}

impl TransportShared {
    pub fn new() -> Self {
        Self {
            pos: Arc::new(AtomicU64::new(0)),
            playing: Arc::new(AtomicBool::new(false)),
        }
    }
}

impl Default for TransportShared {
    fn default() -> Self {
        Self::new()
    }
}

/// トラック1本ぶんのシーケンス。
/// 再生位置はトランスポートが1つだけ持ち、ここでは自分のイベント列だけを見る。
struct TrackSequence {
    events: Box<[SeqEvent]>,
    /// 次に発行するイベントのインデックス
    next_event: usize,
}

impl TrackSequence {
    fn new() -> Self {
        Self {
            events: Box::new([]),
            next_event: 0,
        }
    }

    /// pos 以降で最初のイベントを指すようインデックスを更新する
    fn seek_to(&mut self, pos: u64) {
        self.next_event = self.events.partition_point(|e| e.sample_time < pos);
    }
}

/// 再生エンジン本体。
/// 再生位置・テンポは全トラックで共有し、イベント列だけトラックごとに持つ。
pub struct Transport {
    tracks: Vec<TrackSequence>,
    end_sample: u64,
    playing: bool,
    looping: bool,
    /// 現在位置 (サンプル)
    pos: u64,
    shared: TransportShared,
}

impl Transport {
    pub fn new(shared: TransportShared) -> Self {
        Self {
            tracks: vec![TrackSequence::new()],
            end_sample: 0,
            playing: false,
            looping: false,
            pos: 0,
            shared,
        }
    }

    /// トラック数を確保する (足りなければ空のシーケンスで埋める)
    fn ensure_track(&mut self, track: usize) {
        while self.tracks.len() <= track {
            self.tracks.push(TrackSequence::new());
        }
    }

    /// 全トラックのイベント位置を現在位置に合わせ直す
    fn recompute_next_event(&mut self) {
        let pos = self.pos;
        for track in &mut self.tracks {
            track.seek_to(pos);
        }
    }

    fn publish_state(&self) {
        self.shared.pos.store(self.pos, Ordering::Relaxed);
        self.shared.playing.store(self.playing, Ordering::Relaxed);
    }

    /// GUI からの操作を処理する。
    /// 鳴りっぱなし防止の NoteChoke が必要な場合は buffer に積む。
    pub fn handle_msg(
        &mut self,
        msg: TransportMsg,
        buffer: &mut EventBuffer,
        note_port: Option<(u16, bool)>,
    ) {
        match msg {
            TransportMsg::SetSequence {
                track,
                events,
                end_sample,
            } => {
                self.ensure_track(track);
                self.tracks[track].events = events;
                self.end_sample = end_sample;
                self.recompute_next_event();
                if self.playing {
                    // 差し替え前のノートオフが消えている可能性があるため全消音
                    push_choke(buffer, note_port, 0);
                }
            }
            TransportMsg::Play => {
                if self.pos >= self.end_sample {
                    self.pos = 0;
                }
                self.recompute_next_event();
                self.playing = true;
            }
            TransportMsg::Stop => {
                if self.playing {
                    push_choke(buffer, note_port, 0);
                }
                self.playing = false;
            }
            TransportMsg::Seek { sample } => {
                if self.playing {
                    push_choke(buffer, note_port, 0);
                }
                self.pos = sample;
                self.recompute_next_event();
            }
            TransportMsg::SetLoop { enabled } => {
                self.looping = enabled;
            }
        }
        self.publish_state();
    }

    /// 1ブロック分 (sample_count フレーム) のイベントを発行し、位置を進める。
    ///
    /// `outputs` はトラックごとの (イベントバッファ, ノートポート)。
    /// 各トラックのイベントは自分のバッファにだけ入るので、そのまま
    /// 対応するプラグインへ渡せる。
    pub fn process_block(
        &mut self,
        outputs: &mut [(&mut EventBuffer, Option<(u16, bool)>)],
        sample_count: u64,
    ) {
        if !self.playing {
            return;
        }

        let mut block_offset = 0u64; // ブロック内の書き込み基準位置
        let mut remaining = sample_count;

        // ループ時はブロックが終端をまたぐことがあるため、区間ごとに処理する
        while remaining > 0 {
            let span = if self.looping && self.end_sample > self.pos {
                remaining.min(self.end_sample - self.pos)
            } else {
                remaining
            };

            let span_end = self.pos + span;
            for (index, track) in self.tracks.iter_mut().enumerate() {
                let Some((buffer, note_port)) = outputs.get_mut(index) else {
                    continue; // 音源の無いトラックは鳴らさない
                };
                while let Some(event) = track.events.get(track.next_event) {
                    if event.sample_time >= span_end {
                        break;
                    }
                    let offset = (block_offset + (event.sample_time - self.pos)) as u32;
                    push_note_event(buffer, *note_port, offset, event);
                    track.next_event += 1;
                }
            }

            self.pos = span_end;
            block_offset += span;
            remaining -= span;

            if self.end_sample > 0 && self.pos >= self.end_sample {
                // 終端ちょうど (sample_time == end_sample) のイベントは
                // 「span_end 未満」の条件から漏れるため、停止/ループ前にここで発行する。
                // これを怠ると、小節境界で終わるノートのノートオフが
                // ブロック境界と終端が一致したときに欠落し、音が鳴りっぱなしになる。
                let flush_offset = block_offset.min(sample_count.saturating_sub(1)) as u32;
                for (index, track) in self.tracks.iter_mut().enumerate() {
                    let Some((buffer, note_port)) = outputs.get_mut(index) else {
                        continue;
                    };
                    while let Some(event) = track.events.get(track.next_event) {
                        if event.sample_time > self.end_sample {
                            break;
                        }
                        push_note_event(buffer, *note_port, flush_offset, event);
                        track.next_event += 1;
                    }
                }

                if self.looping {
                    // 先頭に巻き戻して同じブロックの残りを処理する
                    self.pos = 0;
                    for track in &mut self.tracks {
                        track.next_event = 0;
                    }
                } else {
                    self.playing = false;
                    self.pos = self.end_sample;
                    break;
                }
            }
        }

        self.publish_state();
    }
}

/// 全ノート消音イベントを積む
fn push_choke(buffer: &mut EventBuffer, note_port: Option<(u16, bool)>, time: u32) {
    let Some((port, prefers_midi)) = note_port else {
        return;
    };
    if prefers_midi {
        // CC 123 (All Notes Off)
        buffer.push(&MidiEvent::new(time, port, [0xB0, 123, 0]).with_flags(EventFlags::IS_LIVE));
    } else {
        buffer.push(&NoteChokeEvent::new(time, Pckn::match_all()).with_flags(EventFlags::IS_LIVE));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PORT: Option<(u16, bool)> = Some((0u16, false));

    fn make_transport(events: Vec<SeqEvent>, end_sample: u64) -> (Transport, TransportShared) {
        let shared = TransportShared::new();
        let mut transport = Transport::new(shared.clone());
        let mut buffer = EventBuffer::with_capacity(16);
        transport.handle_msg(
            TransportMsg::SetSequence {
                track: 0,
                events: events.into_boxed_slice(),
                end_sample,
            },
            &mut buffer,
            PORT,
        );
        (transport, shared)
    }

    /// トラック1本ぶんのブロック処理 (テスト用の短縮形)
    fn process_block(transport: &mut Transport, buffer: &mut EventBuffer, sample_count: u64) {
        let mut outputs = [(buffer, PORT)];
        transport.process_block(&mut outputs, sample_count);
    }

    fn on(sample_time: u64) -> SeqEvent {
        SeqEvent {
            sample_time,
            key: 60,
            velocity: 0.8,
            is_on: true,
        }
    }

    fn off(sample_time: u64) -> SeqEvent {
        SeqEvent {
            sample_time,
            key: 60,
            velocity: 0.0,
            is_on: false,
        }
    }

    /// 終端ちょうどのノートオフが、ブロック境界 = 終端のときにも発行されること
    #[test]
    fn final_note_off_at_end_boundary_is_emitted() {
        let (mut transport, shared) = make_transport(vec![on(0), off(200)], 200);
        let mut buffer = EventBuffer::with_capacity(16);
        transport.handle_msg(TransportMsg::Play, &mut buffer, PORT);

        buffer.clear();
        process_block(&mut transport, &mut buffer, 100); // [0,100)
        assert_eq!(buffer.as_input().len(), 1); // ノートオンのみ

        buffer.clear();
        process_block(&mut transport, &mut buffer, 100); // [100,200) + 終端フラッシュ
        assert_eq!(buffer.as_input().len(), 1); // ノートオフが欠落しないこと

        assert!(!shared.playing.load(Ordering::Relaxed)); // 自動停止していること
    }

    /// エディタで設定したベロシティが NoteOn イベントまで届くこと
    #[test]
    fn note_on_carries_velocity() {
        use crate::sequencer::{MidiEditor, Note};
        use clack_host::events::spaces::CoreEventSpace;

        let mut editor = MidiEditor::default();
        editor.notes = vec![Note {
            start_tick: 0.0,
            duration: 1.0,
            semitone: 0,
            octave: 4,
            velocity: 40, // 40/127 ≒ 0.315
            track: 0,
            lane: 0,
        }];

        let events = editor.to_events(44100.0);
        let (mut transport, _shared) = make_transport(events, 44100);
        let mut buffer = EventBuffer::with_capacity(16);
        transport.handle_msg(TransportMsg::Play, &mut buffer, PORT);

        buffer.clear();
        process_block(&mut transport, &mut buffer, 64);

        let velocities: Vec<f64> = buffer
            .as_input()
            .into_iter()
            .filter_map(|e| match e.as_core_event() {
                Some(CoreEventSpace::NoteOn(on)) => Some(on.velocity()),
                _ => None,
            })
            .collect();

        assert_eq!(velocities.len(), 1);
        assert!(
            (velocities[0] - 40.0 / 127.0).abs() < 1e-6,
            "期待値 {} に対し実際は {}",
            40.0 / 127.0,
            velocities[0]
        );
    }

    /// ループ巻き戻し時にも終端のノートオフが欠落しないこと
    #[test]
    fn loop_wrap_emits_final_note_off() {
        let (mut transport, _shared) = make_transport(vec![on(0), off(200)], 200);
        let mut buffer = EventBuffer::with_capacity(16);
        transport.handle_msg(TransportMsg::SetLoop { enabled: true }, &mut buffer, PORT);
        transport.handle_msg(TransportMsg::Play, &mut buffer, PORT);

        buffer.clear();
        process_block(&mut transport, &mut buffer, 100); // [0,100)
        buffer.clear();
        // [100,200) + 終端フラッシュ + 巻き戻して [0,100) → オフ1 + オン1
        process_block(&mut transport, &mut buffer, 200);
        assert_eq!(buffer.as_input().len(), 2);
    }
}

/// シーケンスイベント1つをノートオン/オフとして積む
fn push_note_event(
    buffer: &mut EventBuffer,
    note_port: Option<(u16, bool)>,
    time: u32,
    event: &SeqEvent,
) {
    let Some((port, prefers_midi)) = note_port else {
        return;
    };

    if event.is_on {
        if prefers_midi {
            let vel = (event.velocity * 127.0).round().clamp(1.0, 127.0) as u8;
            buffer.push(
                &MidiEvent::new(time, port, [0x90, event.key, vel]).with_flags(EventFlags::IS_LIVE),
            );
        } else {
            buffer.push(
                &NoteOnEvent::new(
                    time,
                    Pckn::new(port, 0u16, event.key as u16, Match::All),
                    event.velocity,
                )
                .with_flags(EventFlags::IS_LIVE),
            );
        }
    } else if prefers_midi {
        buffer.push(
            &MidiEvent::new(time, port, [0x80, event.key, 0]).with_flags(EventFlags::IS_LIVE),
        );
    } else {
        buffer.push(
            &NoteOffEvent::new(
                time,
                Pckn::new(port, 0u16, event.key as u16, Match::All),
                0.0,
            )
            .with_flags(EventFlags::IS_LIVE),
        );
    }
}
