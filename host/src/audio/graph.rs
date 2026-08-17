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
}

/// 繋ぎ方と処理順。**メインスレッドで組み立てて丸ごと差し替える。**
///
/// オーディオスレッドは受け取ったものを**上から実行するだけ**で、探索もソートも
/// 確保もしない。輪になっていないことは組み立てた時点で保証されているので、
/// 実行中に確かめる必要もない。
///
/// # なぜビット列と固定長配列なのか
///
/// **`Copy` にするため。** リングバッファ越しに丸ごと送るので、`Vec` を持つと
/// オーディオスレッド側で解放が起きる。16本なので送り先は `u16` 1つに収まり、
/// 順序も `[u8; 16]` で足りる (合わせて 49 バイト)。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Routing {
    /// `sends[i]` のビット `j` が立っていれば **i から j へ送る**
    sends: [u16; AUDIO_TRACKS],
    /// 処理する順。`order[..len]` だけが有効
    order: [u8; AUDIO_TRACKS],
    len: u8,
}

impl Default for Routing {
    /// 全部がマスターへ直結した状態
    fn default() -> Self {
        let mut sends = [0u16; AUDIO_TRACKS];
        for (index, bits) in sends.iter_mut().enumerate() {
            if index != MASTER {
                *bits = 1 << MASTER;
            }
        }
        // 破れないはずの繋ぎ方なので、失敗したらこちらの組み立て間違い
        Self::build(&sends).expect("マスター直結が組めること")
    }
}

impl Routing {
    /// 送り先のビット列から処理順を決める。
    ///
    /// 返すのは**人が読める形の問題**。1つでもあれば繋ぎ方を採用しない。
    /// 見るのは次の5つ。
    ///
    /// - 送り先が範囲内か
    /// - 自分自身へ送っていないか
    /// - **マスターが送り側になっていないか** (0 を含む輪を作れてしまう)
    /// - **2本の間に両向きの接続が無いか** (どちらが送り側か決まらない)
    /// - **輪になっていないか**
    ///
    /// 最後の1つが本体。上の2つでは `1 → 2 → 3 → 1` が塞がらない。
    pub fn build(sends: &[u16; AUDIO_TRACKS]) -> Result<Self, Vec<String>> {
        let mut problems = Vec::new();

        for (index, bits) in sends.iter().enumerate() {
            if index == MASTER && *bits != 0 {
                problems.push("・マスターは送り先を持てません".into());
            }
            if bits & (1 << index) != 0 {
                problems.push(format!(
                    "・オーディオトラック {index}: 自分自身へは送れません"
                ));
            }
            for target in targets(*bits) {
                if sends[target] & (1 << index) != 0 && index < target {
                    problems.push(format!(
                        "・オーディオトラック {index} と {target} が互いに送り合っています"
                    ));
                }
            }
        }
        if !problems.is_empty() {
            return Err(problems);
        }

        match topological_order(sends) {
            Some((order, len)) => Ok(Self {
                sends: *sends,
                order,
                len,
            }),
            None => Err(vec![
                "・オーディオトラックの繋ぎ方が輪になっています".to_string()
            ]),
        }
    }

    /// 送り先の一覧 (トラック番号) からビット列を組む。
    /// **範囲外・自分自身・重複はここで弾く。**
    pub fn from_lists(lists: &[Vec<usize>]) -> Result<Self, Vec<String>> {
        let mut sends = [0u16; AUDIO_TRACKS];
        let mut problems = Vec::new();

        for (index, list) in lists.iter().enumerate().take(AUDIO_TRACKS) {
            for target in list {
                if *target >= AUDIO_TRACKS {
                    problems.push(format!(
                        "・オーディオトラック {index}: 送り先 {target} は存在しません"
                    ));
                    continue;
                }
                let bit = 1u16 << target;
                if sends[index] & bit != 0 {
                    problems.push(format!(
                        "・オーディオトラック {index}: 送り先 {target} が重複しています"
                    ));
                    continue;
                }
                sends[index] |= bit;
            }
        }
        if !problems.is_empty() {
            return Err(problems);
        }
        Self::build(&sends)
    }

    /// 処理する順 (先に流すものから)
    pub fn order(&self) -> impl Iterator<Item = usize> + '_ {
        self.order[..self.len as usize].iter().map(|i| *i as usize)
    }

    /// `index` の送り先
    pub fn sends_of(&self, index: usize) -> impl Iterator<Item = usize> {
        targets(self.sends.get(index).copied().unwrap_or(0))
    }

    /// マスターへ辿り着けるか。**辿り着けないトラックは鳴らない。**
    ///
    /// 「繋がっていなければ鳴らない」を仕様にしたぶん、画面で分かるようにする
    /// 必要がある (フェーズ4 の赤枠)。
    pub fn reaches_master(&self, index: usize) -> bool {
        if index == MASTER {
            return true;
        }
        // マスターから辺を逆向きに辿って印を付ける
        let mut reached = 1u16 << MASTER;
        loop {
            let mut added = false;
            for (from, bits) in self.sends.iter().enumerate() {
                if reached & (1 << from) != 0 {
                    continue;
                }
                if bits & reached != 0 {
                    reached |= 1 << from;
                    added = true;
                }
            }
            if !added {
                break;
            }
        }
        reached & (1 << index) != 0
    }
}

/// ビット列の立っている位置を昇順で返す
fn targets(bits: u16) -> impl Iterator<Item = usize> {
    (0..AUDIO_TRACKS).filter(move |index| bits & (1 << index) != 0)
}

/// 送り元が送り先より先に来る順を求める。輪があれば `None`。
///
/// 深さ優先で1周するだけ (16本しかないので、確保も再帰の深さも問題にならない)。
fn topological_order(sends: &[u16; AUDIO_TRACKS]) -> Option<([u8; AUDIO_TRACKS], u8)> {
    /// 0: 未訪問 / 1: 訪問中 / 2: 済
    let mut mark = [0u8; AUDIO_TRACKS];
    let mut order = [0u8; AUDIO_TRACKS];
    let mut len = 0usize;

    // 再帰の代わりに自前の積みで辿る (戻りがけに順へ入れる)
    for start in 0..AUDIO_TRACKS {
        if mark[start] != 0 {
            continue;
        }
        let mut stack = vec![(start, targets(sends[start]).collect::<Vec<_>>(), 0usize)];
        mark[start] = 1;

        while let Some((index, children, cursor)) = stack.last_mut() {
            if *cursor < children.len() {
                let child = children[*cursor];
                *cursor += 1;
                match mark[child] {
                    // 辿っている最中の相手へ戻った = 輪
                    1 => return None,
                    0 => {
                        mark[child] = 1;
                        let next = targets(sends[child]).collect::<Vec<_>>();
                        stack.push((child, next, 0));
                    }
                    _ => {}
                }
            } else {
                mark[*index] = 2;
                // 送り先を全部済ませてから自分を積む → 逆順が処理順になる
                order[len] = *index as u8;
                len += 1;
                stack.pop();
            }
        }
    }

    // 戻りがけに入れたので「送り先が先」。**逆にすると送り元が先**になる
    order[..len].reverse();
    Some((order, len as u8))
}

/// オーディオトラック一式と、その作業バッファ。
///
/// バッファは**最初に全本数ぶん確保する**。16本 × 2048フレーム × 2ch × 4B で
/// 256KiB しかないので、使っていないトラックのぶんを惜しむ理由がない。
pub struct Graph {
    tracks: Vec<AudioTrack>,
    /// 繋ぎ方と処理順。**丸ごと差し替える**
    routing: Routing,
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
            routing: Routing::default(),
            buffers: Vec::new(),
            capacity_frames: 0,
        }
    }

    /// 繋ぎ方を差し替える。**組み立て済みのものしか受け取らない**ので、
    /// ここで検査することは何も無い。
    pub fn set_routing(&mut self, routing: Routing) {
        self.routing = routing;
    }

    pub fn routing(&self) -> &Routing {
        &self.routing
    }

    pub fn track(&self, index: usize) -> Option<&AudioTrack> {
        self.tracks.get(index)
    }

    pub fn track_mut(&mut self, index: usize) -> Option<&mut AudioTrack> {
        self.tracks.get_mut(index)
    }

    /// オーディオトラックに処理器を載せる。既に載っていたものを返す。
    ///
    /// **繋ぎ方には触らない。** 繋ぎ方は [`set_routing`](Self::set_routing) で
    /// 丸ごと差し替えるもので、音源の載せ降ろしとは独立している。
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
    /// **[`Routing`] が決めた順に上から実行するだけ。** 送り元が必ず先に来るので、
    /// あるトラックを処理する時点で、そこへ入ってくる音は全部揃っている。
    /// ここで探索もソートも確保もしない。
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

        let routing = self.routing;
        for index in routing.order() {
            self.run_track(index, steady, len, on_error);

            // 送り先へ足す。**加算コピー**なので、複数から送られれば混ざる
            for target in routing.sends_of(index) {
                self.add_into(index, target, len);
            }
        }
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

    /// 送り先の一覧から繋ぎ方を組む (テスト用の短縮)
    fn routing(edges: &[(usize, usize)]) -> Result<Routing, Vec<String>> {
        let mut lists = vec![Vec::new(); AUDIO_TRACKS];
        for (from, to) in edges {
            lists[*from].push(*to);
        }
        Routing::from_lists(&lists)
    }

    #[test]
    fn master_is_track_zero_and_never_sends() {
        assert_eq!(MASTER, 0);
        let default = Routing::default();
        assert_eq!(default.sends_of(MASTER).count(), 0);
    }

    /// 既定は全部がマスターへ直結していること
    #[test]
    fn default_routing_sends_everything_to_master() {
        let default = Routing::default();
        for index in 1..AUDIO_TRACKS {
            assert_eq!(default.sends_of(index).collect::<Vec<_>>(), vec![MASTER]);
        }
        // マスターは最後に処理される (全部が足し込まれてから)
        assert_eq!(default.order().last(), Some(MASTER));
    }

    /// **送り元が送り先より必ず先に来ること。** ここが崩れると、
    /// まだ音が揃っていないトラックを処理してしまう。
    #[test]
    fn order_puts_sources_before_targets() {
        // 3 → 1 → 2 → 0
        let routing = routing(&[(3, 1), (1, 2), (2, MASTER)]).unwrap();
        let order: Vec<_> = routing.order().collect();
        let at = |track: usize| order.iter().position(|step| *step == track).unwrap();

        assert!(at(3) < at(1));
        assert!(at(1) < at(2));
        assert!(at(2) < at(MASTER));
        assert_eq!(order.len(), AUDIO_TRACKS, "全トラックが1回ずつ入ること");
    }

    /// 1本が複数へ送る形でも順序が守られること (センド)
    #[test]
    fn order_handles_one_source_feeding_several_targets() {
        // 5 は 0 と 7 の両方へ送り、7 も 0 へ送る
        let routing = routing(&[(5, MASTER), (5, 7), (7, MASTER)]).unwrap();
        let order: Vec<_> = routing.order().collect();
        let at = |track: usize| order.iter().position(|step| *step == track).unwrap();

        assert!(at(5) < at(7));
        assert!(at(7) < at(MASTER));
    }

    /// **3本以上の輪を拒否すること。** 対で1本・マスターは受け手、の2つでは
    /// 塞がらない形なので、ここが最後の砦になる。
    #[test]
    fn cycles_are_refused() {
        let error = routing(&[(1, 2), (2, 3), (3, 1)]).unwrap_err();
        assert!(
            error.iter().any(|line| line.contains("輪になっています")),
            "実際: {error:?}"
        );
    }

    /// 互いに送り合うのは1本の接続として矛盾するので拒否すること
    #[test]
    fn mutual_sends_are_refused() {
        let error = routing(&[(1, 2), (2, 1)]).unwrap_err();
        assert!(
            error.iter().any(|line| line.contains("互いに送り合って")),
            "実際: {error:?}"
        );
    }

    /// マスターが送り側になるのを拒否すること (0 を含む輪を作れてしまう)
    #[test]
    fn master_may_not_send() {
        let error = routing(&[(MASTER, 1)]).unwrap_err();
        assert!(
            error.iter().any(|line| line.contains("マスターは送り先")),
            "実際: {error:?}"
        );
    }

    /// 範囲外・自分自身・重複を拒否すること
    #[test]
    fn invalid_targets_are_refused() {
        assert!(routing(&[(1, 99)])
            .unwrap_err()
            .iter()
            .any(|line| line.contains("存在しません")));
        assert!(routing(&[(1, 1)])
            .unwrap_err()
            .iter()
            .any(|line| line.contains("自分自身")));
        assert!(routing(&[(1, 0), (1, 0)])
            .unwrap_err()
            .iter()
            .any(|line| line.contains("重複")));
    }

    /// **マスターへ辿り着けないトラックが分かること。**
    /// 「繋がっていなければ鳴らない」を仕様にしたぶん、画面で示す必要がある。
    #[test]
    fn tracks_that_cannot_reach_master_are_detectable() {
        // 1 → 0 は届く。5 → 7 は行き止まり
        let routing = routing(&[(1, MASTER), (5, 7)]).unwrap();

        assert!(routing.reaches_master(MASTER));
        assert!(routing.reaches_master(1));
        assert!(!routing.reaches_master(5), "行き止まりの先は届かない");
        assert!(!routing.reaches_master(7));
        assert!(!routing.reaches_master(9), "送り先が空なら届かない");
    }

    /// 2段先からでも辿り着けること
    #[test]
    fn reaching_master_follows_the_whole_path() {
        let routing = routing(&[(4, 2), (2, MASTER)]).unwrap();
        assert!(routing.reaches_master(4));
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
