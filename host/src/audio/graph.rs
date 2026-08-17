//! オーディオトラックと、その間の受け渡し。
//!
//! **再生 (`audio::mod`) と書き出し (`audio::offline`) の両方がここを通る。**
//! 片方だけ別の組み方をすると、鳴らした音と書き出した音が食い違う。
//!
//! # オーディオトラック
//!
//! 打ち込み側の「トラック」(`sequencer::TrackInfo`) とは**別のもの**。
//! 音を出す系統が [`AUDIO_TRACKS`] 本あり、それぞれが
//! [`TrackProcessor`](super::TrackProcessor) (音源とエフェクトの列) を持つ。
//!
//! - **0番はマスター** ([`MASTER`])。ここの出力がそのまま最終出力になる
//! - **0番は送り側にならない。** マスターから他へ送れないので、0 を含む輪は作れない
//! - 打ち込みトラックとの対応は [`AudioTrack::midi_track`]。**多対1** になりうる
//!   (同じ打ち込みを複数の音源で重ねる)
//!
//! 本数は固定。増減しないので**添字がそのまま識別子**になり、消したときに
//! 番号がずれる問題が起きない。
//!
//! # チャンネル
//!
//! **グラフの中は常に [`BUS_CHANNELS`] (2ch)。** デバイスのチャンネル数とは
//! 切り離してある。合わせてしまうと、モノラル出力の環境では**書き出しまで
//! モノラルになる**。デバイスへ落とすのは [`write_to_device`] の1回だけ。

use super::events::BlockEvents;
use super::transport::{BlockPlan, Transport};
use super::{ProcessError, TrackProcessor};
use cpal::FromSample;

/// オーディオトラックの本数。0番がマスター
pub const AUDIO_TRACKS: usize = 16;

/// マスターのトラック番号
pub const MASTER: usize = 0;

/// 打ち込みトラックに対応するオーディオトラックの番号。
///
/// **0 がマスターなので1つずらす。** 画面から作れるトラックがまだ
/// 「打ち込み1本につき音源1つ」なので、この対応で足りている
/// (任意に繋ぎ替えられるようにするのはフェーズ4)。
///
/// **16本に収まらなければ `None`。** 打ち込みトラック数に上限が無いので、
/// 音を出せる系統より多く作れてしまう。
pub fn audio_track_for(midi_track: usize) -> Option<usize> {
    let index = midi_track + 1;
    (index < AUDIO_TRACKS).then_some(index)
}

/// [`audio_track_for`] の逆。**マスターには打ち込みトラックが無い**ので `None`。
pub fn midi_track_for(audio_track: usize) -> Option<usize> {
    (audio_track > MASTER && audio_track < AUDIO_TRACKS).then(|| audio_track - 1)
}

/// グラフの中を流れるチャンネル数。**デバイスとは独立**
pub const BUS_CHANNELS: usize = 2;

/// オーディオトラック1本
#[derive(Default)]
pub struct AudioTrack {
    /// 載っている音源とエフェクトの列。空なら何も鳴らない
    pub processor: Option<Box<TrackProcessor>>,
    /// MIDI をどの打ち込みトラックから取るか。`None` は入力なし (バス用)
    pub midi_track: Option<usize>,
    /// 送り先。**マスターは常に空** (送り側にならない)。
    ///
    /// 空なら鳴らない。それを仕様としているので、繋がっていないトラックを
    /// 画面で分かるようにするのは UI 側の仕事 (フェーズ4)。
    pub sends: Vec<usize>,
}

impl AudioTrack {
    /// マスターへ送るだけのトラック (音源を載せたときの既定)
    pub fn to_master(midi_track: Option<usize>) -> Self {
        Self {
            processor: None,
            midi_track,
            sends: vec![MASTER],
        }
    }
}

/// オーディオトラック一式と、その作業バッファ。
///
/// バッファは**最初に全本数ぶん確保する**。16本 × 2048フレーム × 2ch × 4B で
/// 256KiB しかないので、使っていないトラックのぶんを惜しむ理由がない。
pub struct Graph {
    tracks: Vec<AudioTrack>,
    /// トラックごとの出力 (2ch インターリーブ)。`frames` ごとに区切って使う
    buffers: Vec<f32>,
    /// いま確保してあるフレーム数
    capacity_frames: usize,
}

impl Default for Graph {
    fn default() -> Self {
        Self::new()
    }
}

impl Graph {
    pub fn new() -> Self {
        Self {
            tracks: (0..AUDIO_TRACKS).map(|_| AudioTrack::default()).collect(),
            buffers: Vec::new(),
            capacity_frames: 0,
        }
    }

    pub fn track(&self, index: usize) -> Option<&AudioTrack> {
        self.tracks.get(index)
    }

    pub fn track_mut(&mut self, index: usize) -> Option<&mut AudioTrack> {
        self.tracks.get_mut(index)
    }

    /// オーディオトラックに処理器を載せる。既に載っていたものを返す。
    ///
    /// 送り先は**マスターへ1本**にする (マスター自身は空)。
    /// 任意の繋ぎ方を許すのはフェーズ3。
    pub fn place(
        &mut self,
        index: usize,
        midi_track: Option<usize>,
        processor: Box<TrackProcessor>,
    ) -> Option<Box<TrackProcessor>> {
        let Some(slot) = self.tracks.get_mut(index) else {
            return Some(processor);
        };
        slot.midi_track = midi_track;
        slot.sends = if index == MASTER {
            Vec::new()
        } else {
            vec![MASTER]
        };
        slot.processor.replace(processor)
    }

    /// 載っている処理器を全部取り出す (トラック番号の昇順)。
    ///
    /// **書き出しのために借りたものを戻すときに使う。**
    pub fn take_processors(&mut self) -> Vec<(usize, Box<TrackProcessor>)> {
        self.tracks
            .iter_mut()
            .enumerate()
            .filter_map(|(index, track)| track.processor.take().map(|p| (index, p)))
            .collect()
    }

    /// 載っている処理器を (トラック番号, 処理器) で順に見る
    pub fn processors_mut(&mut self) -> impl Iterator<Item = (usize, &mut Box<TrackProcessor>)> {
        self.tracks
            .iter_mut()
            .enumerate()
            .filter_map(|(index, track)| track.processor.as_mut().map(|p| (index, p)))
    }

    /// そのオーディオトラックのイベント置き場 (呼び出し側が積む)
    pub fn events_mut(&mut self, index: usize) -> Option<&mut BlockEvents> {
        self.tracks
            .get_mut(index)?
            .processor
            .as_mut()
            .map(|p| p.events_mut())
    }

    /// トランスポートの計画を各トラックへ配る。
    ///
    /// **打ち込みトラックとオーディオトラックは番号が違う。** どの打ち込みを
    /// 取るかはオーディオトラックごとに決まっているので、その対応で配る。
    /// 1つの打ち込みを複数のオーディオトラックが見ていれば、どれにも届く。
    ///
    /// 再生・書き出し・検証バイナリのすべてがここを通る
    /// (配り方が食い違うと、経路によって鳴る音が変わる)。
    ///
    /// **イベントは消さない。** GUI から積んだぶんと混ぜて送るため、
    /// [`clear_events`](Self::clear_events) は呼び出し側がブロックの頭で呼ぶ。
    pub fn emit_from(&mut self, transport: &mut Transport, plan: &BlockPlan) {
        if plan.is_empty() {
            return;
        }
        for index in 0..AUDIO_TRACKS {
            let Some(midi_track) = self.track(index).and_then(|slot| slot.midi_track) else {
                continue;
            };
            let Some(events) = self.events_mut(index) else {
                continue;
            };
            transport.emit_track(midi_track, plan, events);
        }
    }

    /// 全トラックのイベントを空にする
    pub fn clear_events(&mut self) {
        for track in self.tracks.iter_mut() {
            if let Some(processor) = track.processor.as_mut() {
                processor.events_mut().clear();
            }
        }
    }

    /// 1ブロックぶんのバッファを確保する。**メインスレッドで呼ぶこと**
    /// (オーディオスレッドで伸ばすと確保が起きる)。
    pub fn reserve(&mut self, frames: usize) {
        if frames <= self.capacity_frames {
            return;
        }
        self.capacity_frames = frames;
        self.buffers
            .resize(AUDIO_TRACKS * frames * BUS_CHANNELS, 0.0);
    }

    /// 1ブロック処理する。結果は [`master`](Self::master) に入る。
    ///
    /// 処理の順は **1〜15 を順に、最後にマスター**。今は送り先がマスターだけ
    /// なのでこれで足りる。**任意の繋ぎ方を許すフェーズ3 では、ここが
    /// トポロジカルソートの結果に変わる。**
    ///
    /// 失敗したトラックは `on_error` に渡し、そのトラックは**無音のまま混ざる**
    /// (途中まで処理した音を出すと、加工されていない原音が漏れる)。
    pub fn process(
        &mut self,
        steady: u64,
        frames: usize,
        on_error: &mut dyn FnMut(usize, ProcessError),
    ) {
        debug_assert!(frames <= self.capacity_frames, "先に reserve すること");
        let frames = frames.min(self.capacity_frames);
        let len = frames * BUS_CHANNELS;

        // 全トラックのバッファを 0 にしてから始める。
        // 音源は読まずに上書きし、エフェクトは無音を加工することになる
        for index in 0..AUDIO_TRACKS {
            self.buffer_mut(index, len).fill(0.0);
        }

        for index in (MASTER + 1)..AUDIO_TRACKS {
            self.run_track(index, steady, len, on_error);

            // 送り先へ足す。**加算コピー**なので、複数から送られれば混ざる
            for send in 0..self.tracks[index].sends.len() {
                let target = self.tracks[index].sends[send];
                if target == index || target >= AUDIO_TRACKS {
                    continue; // 自分への送りは無視 (グラフとして意味を成さない)
                }
                self.add_into(index, target, len);
            }
        }

        // マスターは最後。ここまでに全部が足し込まれている
        self.run_track(MASTER, steady, len, on_error);
    }

    /// 最終出力 (マスターの中身)。`frames` フレーム × [`BUS_CHANNELS`]
    pub fn master(&self, frames: usize) -> &[f32] {
        let len = (frames * BUS_CHANNELS).min(self.capacity_frames * BUS_CHANNELS);
        &self.buffers[MASTER * self.capacity_frames * BUS_CHANNELS..][..len]
    }

    /// 1トラックぶんのチェーンを、そのトラックのバッファへ通す
    fn run_track(
        &mut self,
        index: usize,
        steady: u64,
        len: usize,
        on_error: &mut dyn FnMut(usize, ProcessError),
    ) {
        let stride = self.capacity_frames * BUS_CHANNELS;
        let Some(processor) = self.tracks[index].processor.as_mut() else {
            return;
        };
        let buffer = &mut self.buffers[index * stride..][..len];
        if let Err(e) = processor.process(steady, buffer) {
            on_error(index, e);
        }
    }

    /// `from` のバッファを `to` のバッファへ足し込む
    fn add_into(&mut self, from: usize, to: usize, len: usize) {
        let stride = self.capacity_frames * BUS_CHANNELS;
        // 別のトラックなので範囲は重ならない。split_at_mut で2つに分ける
        let (low, high) = if from < to { (from, to) } else { (to, from) };
        let (first, second) = self.buffers.split_at_mut(high * stride);
        let (source, destination) = if from < to {
            (&first[low * stride..][..len], &mut second[..len])
        } else {
            (&second[..len], &mut first[low * stride..][..len])
        };
        for (out, sample) in destination.iter_mut().zip(source.iter()) {
            *out += *sample;
        }
    }

    fn buffer_mut(&mut self, index: usize, len: usize) -> &mut [f32] {
        let stride = self.capacity_frames * BUS_CHANNELS;
        &mut self.buffers[index * stride..][..len]
    }
}

/// グラフの出力 (2ch) をデバイスのチャンネル数へ移す。
///
/// **ここがデバイスに合わせる唯一の場所。** グラフの中はデバイスに依らず
/// 2ch で通しているので、モノラル出力の環境でも書き出しはステレオのまま。
///
/// サンプル形式で generic にしてあるのは、CPAL が `i16` などを要求してくることが
/// あるため (書き出し側は `f32` で呼ぶ)。
pub fn write_to_device<S: FromSample<f32>>(bus: &[f32], device: &mut [S], device_channels: usize) {
    let channels = device_channels.max(1);
    for (frame, out) in device.chunks_exact_mut(channels).enumerate() {
        let base = frame * BUS_CHANNELS;
        if base + BUS_CHANNELS > bus.len() {
            break;
        }
        for (channel, sample) in out.iter_mut().enumerate() {
            let value = if channels == 1 {
                // 左右を平均して落とす
                bus[base..base + BUS_CHANNELS].iter().sum::<f32>() / BUS_CHANNELS as f32
            } else {
                // 足りないチャンネルは最後のもので埋める (`buffers.rs` と同じ方針)
                bus[base + channel.min(BUS_CHANNELS - 1)]
            };
            *sample = FromSample::from_sample_(value);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn master_is_track_zero_and_never_sends() {
        let graph = Graph::new();
        assert_eq!(MASTER, 0);
        assert!(
            graph.track(MASTER).unwrap().sends.is_empty(),
            "マスターは送り側にならないこと"
        );
    }

    /// 音源を載せたトラックは既定でマスターへ送ること
    #[test]
    fn new_tracks_send_to_master() {
        let track = AudioTrack::to_master(Some(3));
        assert_eq!(track.sends, vec![MASTER]);
        assert_eq!(track.midi_track, Some(3));
    }

    /// 送りは加算で混ざること (上書きしない)
    #[test]
    fn sends_accumulate_into_the_target() {
        let mut graph = Graph::new();
        graph.reserve(2);
        let len = 2 * BUS_CHANNELS;

        graph.buffer_mut(1, len).fill(0.25);
        graph.buffer_mut(2, len).fill(0.5);
        graph.buffer_mut(MASTER, len).fill(0.0);
        graph.add_into(1, MASTER, len);
        graph.add_into(2, MASTER, len);

        assert_eq!(graph.master(2), &[0.75; 4], "2本ぶんが足されること");
    }

    /// 送り先が自分より小さい番号でも大きい番号でも足せること
    /// (`split_at_mut` の分け方を間違えると片方向だけ壊れる)
    #[test]
    fn sends_work_in_both_index_directions() {
        let mut graph = Graph::new();
        graph.reserve(1);
        let len = BUS_CHANNELS;

        graph.buffer_mut(3, len).fill(1.0);
        graph.buffer_mut(9, len).fill(0.0);
        graph.add_into(3, 9, len); // 小 → 大
        assert_eq!(graph.buffer_mut(9, len), &[1.0, 1.0]);

        graph.buffer_mut(2, len).fill(0.0);
        graph.add_into(9, 2, len); // 大 → 小
        assert_eq!(graph.buffer_mut(2, len), &[1.0, 1.0]);
    }

    /// ステレオのバスをモノラルのデバイスへ落とすと平均になること
    #[test]
    fn mono_device_gets_the_average() {
        let bus = [1.0, 0.0, 0.5, 0.5];
        let mut device = [0.0; 2];
        write_to_device(&bus, &mut device, 1);
        assert_eq!(device, [0.5, 0.5]);
    }

    /// ステレオのデバイスへはそのまま通ること
    #[test]
    fn stereo_device_passes_through() {
        let bus = [1.0, -1.0, 0.25, 0.5];
        let mut device = [0.0; 4];
        write_to_device(&bus, &mut device, 2);
        assert_eq!(device, bus);
    }
}
