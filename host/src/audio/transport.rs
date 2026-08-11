//! シーケンス再生のトランスポート (オーディオスレッド上で動く)。
//! サンプル精度でノートイベントをブロック内オフセット付きで発行する。

use crate::audio::events::{BlockEvent, BlockEvents};
use crate::sequencer::{SeqEvent, SeqEventKind};
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
    /// 戻り値が true なら、鳴りっぱなし防止のため全トラックを消音する必要がある
    /// (どのトラックへ消音イベントを積むかは呼び出し側が決める)。
    #[must_use]
    pub fn handle_msg(&mut self, msg: TransportMsg) -> bool {
        let mut needs_choke = false;
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
                // 差し替え前のノートオフが消えている可能性があるため全消音
                needs_choke = self.playing;
            }
            TransportMsg::Play => {
                if self.pos >= self.end_sample {
                    self.pos = 0;
                }
                self.recompute_next_event();
                self.playing = true;
            }
            TransportMsg::Stop => {
                needs_choke = self.playing;
                self.playing = false;
            }
            TransportMsg::Seek { sample } => {
                needs_choke = self.playing;
                self.pos = sample;
                self.recompute_next_event();
            }
            TransportMsg::SetLoop { enabled } => {
                self.looping = enabled;
            }
        }
        self.publish_state();
        needs_choke
    }

    /// 1ブロックを区切った区間。ループで終端をまたぐと複数になる。
    /// 位置の前進 (クロック) とイベント発行を分けるための中間データ。
    pub fn plan_block(&mut self, sample_count: u64) -> BlockPlan {
        let mut plan = BlockPlan::default();
        if !self.playing {
            return plan;
        }

        let mut block_offset = 0u64; // ブロック内の書き込み基準位置
        let mut remaining = sample_count;

        while remaining > 0 && plan.count < MAX_SPANS {
            let length = if self.looping && self.end_sample > self.pos {
                remaining.min(self.end_sample - self.pos)
            } else {
                remaining
            };

            let span_end = self.pos + length;
            let mut span = BlockSpan {
                start: self.pos,
                end: span_end,
                block_offset,
                // 終端ちょうど (sample_time == end_sample) のイベントは「end 未満」の
                // 条件から漏れるので、停止/ループの前にここでまとめて出す。
                // これを怠ると小節境界で終わるノートのノートオフが欠落し、
                // 音が鳴りっぱなしになる。
                flush_end: false,
                rewind: false,
                flush_offset: 0,
            };

            self.pos = span_end;
            block_offset += length;
            remaining -= length;

            if self.end_sample > 0 && self.pos >= self.end_sample {
                span.flush_end = true;
                span.flush_offset = block_offset.min(sample_count.saturating_sub(1)) as u32;
                if self.looping {
                    span.rewind = true;
                    self.pos = 0;
                } else {
                    self.playing = false;
                    self.pos = self.end_sample;
                    plan.push(span);
                    break;
                }
            }
            plan.push(span);
        }

        self.publish_state();
        plan
    }

    /// 1トラック分のイベントを、計画した区間に沿って発行する
    pub fn emit_track(&mut self, track: usize, plan: &BlockPlan, events: &mut BlockEvents) {
        let end_sample = self.end_sample;
        let Some(sequence) = self.tracks.get_mut(track) else {
            return;
        };

        for span in plan.spans() {
            while let Some(event) = sequence.events.get(sequence.next_event) {
                if event.sample_time >= span.end {
                    break;
                }
                let offset = (span.block_offset + (event.sample_time - span.start)) as u32;
                events.push(note_event(offset, event));
                sequence.next_event += 1;
            }

            if span.flush_end {
                while let Some(event) = sequence.events.get(sequence.next_event) {
                    if event.sample_time > end_sample {
                        break;
                    }
                    events.push(note_event(span.flush_offset, event));
                    sequence.next_event += 1;
                }
                if span.rewind {
                    sequence.next_event = 0;
                }
            }
        }
    }
}

/// 1ブロックを区切った1区間
#[derive(Clone, Copy, Default)]
pub struct BlockSpan {
    start: u64,
    end: u64,
    block_offset: u64,
    /// この区間の終わりで終端イベントを出すか
    flush_end: bool,
    /// 出したあとに先頭へ巻き戻すか (ループ時)
    rewind: bool,
    flush_offset: u32,
}

/// ループでブロック内に収まる区間の上限 (極端に短いループの保険)
const MAX_SPANS: usize = 8;

/// 1ブロック分の区間の並び。オーディオスレッドで確保しないよう固定長で持つ。
#[derive(Clone, Copy)]
pub struct BlockPlan {
    spans: [BlockSpan; MAX_SPANS],
    count: usize,
}

impl Default for BlockPlan {
    fn default() -> Self {
        Self {
            spans: [BlockSpan::default(); MAX_SPANS],
            count: 0,
        }
    }
}

impl BlockPlan {
    fn push(&mut self, span: BlockSpan) {
        if self.count < MAX_SPANS {
            self.spans[self.count] = span;
            self.count += 1;
        }
    }

    fn spans(&self) -> &[BlockSpan] {
        &self.spans[..self.count]
    }

    /// 発行すべきものが何もないか
    pub fn is_empty(&self) -> bool {
        self.count == 0
    }
}

/// 全ノート消音イベントを積む
pub fn push_choke(events: &mut BlockEvents, offset: u32) {
    events.push(BlockEvent::Choke { offset });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_transport(events: Vec<SeqEvent>, end_sample: u64) -> (Transport, TransportShared) {
        let shared = TransportShared::new();
        let mut transport = Transport::new(shared.clone());
        let _ = transport.handle_msg(TransportMsg::SetSequence {
            track: 0,
            events: events.into_boxed_slice(),
            end_sample,
        });
        (transport, shared)
    }

    /// トラック1本ぶんのブロック処理 (テスト用の短縮形)
    fn process_block(transport: &mut Transport, events: &mut BlockEvents, sample_count: u64) {
        let plan = transport.plan_block(sample_count);
        transport.emit_track(0, &plan, events);
    }

    fn on(sample_time: u64) -> SeqEvent {
        SeqEvent {
            sample_time,
            kind: SeqEventKind::NoteOn {
                key: 60,
                velocity: 0.8,
            },
        }
    }

    fn off(sample_time: u64) -> SeqEvent {
        SeqEvent {
            sample_time,
            kind: SeqEventKind::NoteOff { key: 60 },
        }
    }

    /// 終端ちょうどのノートオフが、ブロック境界 = 終端のときにも発行されること
    #[test]
    fn final_note_off_at_end_boundary_is_emitted() {
        let (mut transport, shared) = make_transport(vec![on(0), off(200)], 200);
        let mut events = BlockEvents::with_capacity(16);
        let _ = transport.handle_msg(TransportMsg::Play);

        events.clear();
        process_block(&mut transport, &mut events, 100); // [0,100)
        assert_eq!(events.len(), 1); // ノートオンのみ

        events.clear();
        process_block(&mut transport, &mut events, 100); // [100,200) + 終端フラッシュ
        assert_eq!(events.len(), 1); // ノートオフが欠落しないこと

        assert!(!shared.playing.load(Ordering::Relaxed)); // 自動停止していること
    }

    /// エディタで設定したベロシティが発行イベントまで届くこと
    #[test]
    fn note_on_carries_velocity() {
        use crate::sequencer::{MidiEditor, Note};

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

        let sequence = editor.to_events(44100.0);
        let (mut transport, _shared) = make_transport(sequence, 44100);
        let mut events = BlockEvents::with_capacity(16);
        let _ = transport.handle_msg(TransportMsg::Play);

        events.clear();
        process_block(&mut transport, &mut events, 64);

        let velocities: Vec<f64> = events
            .iter()
            .filter_map(|event| match event {
                BlockEvent::NoteOn { velocity, .. } => Some(*velocity),
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
        let mut events = BlockEvents::with_capacity(16);
        let _ = transport.handle_msg(TransportMsg::SetLoop { enabled: true });
        let _ = transport.handle_msg(TransportMsg::Play);

        events.clear();
        process_block(&mut transport, &mut events, 100); // [0,100)
        events.clear();
        // [100,200) + 終端フラッシュ + 巻き戻して [0,100) → オフ1 + オン1
        process_block(&mut transport, &mut events, 200);
        assert_eq!(events.len(), 2);
    }

    /// 消音イベントが中立の形で積まれること (バックエンドが自分の形へ移す)
    #[test]
    fn choke_is_emitted_as_a_neutral_event() {
        let mut events = BlockEvents::with_capacity(4);
        push_choke(&mut events, 32);

        assert_eq!(events.len(), 1);
        assert_eq!(
            events.iter().next(),
            Some(&BlockEvent::Choke { offset: 32 })
        );
    }
}

/// シーケンスイベント1つをブロックイベントにする
fn note_event(offset: u32, event: &SeqEvent) -> BlockEvent {
    match event.kind {
        SeqEventKind::NoteOn { key, velocity } => BlockEvent::NoteOn {
            offset,
            key,
            velocity,
        },
        SeqEventKind::NoteOff { key } => BlockEvent::NoteOff { offset, key },
        SeqEventKind::Cc { number, value } => BlockEvent::Cc {
            offset,
            number,
            value,
        },
    }
}
