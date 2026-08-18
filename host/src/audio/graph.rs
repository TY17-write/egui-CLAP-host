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
use super::{Node, NodeAddr, ProcessError, TrackProcessor};
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

/// 1本のオーディオトラックに刺せるノードの上限。
///
/// **オーディオスレッドで確保しないために要る。** チェーンの列をこの数だけ
/// 先に取っておけば、ノードを足すときに伸びない。
pub const MAX_NODES: usize = 16;

/// オーディオトラック1本
pub struct AudioTrack {
    /// 載っている音源とエフェクトの列。**空でも器はある** (ノードを足す先として)
    pub processor: Box<TrackProcessor>,
    /// MIDI をどの打ち込みトラックから取るか。`None` は未割り当て
    pub midi_track: Option<usize>,
}

impl AudioTrack {
    /// 空のトラック。**チェーンの容量を先に取っておく**
    fn new() -> Self {
        Self {
            processor: Box::new(TrackProcessor::empty(MAX_NODES)),
            midi_track: None,
        }
    }
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

/// 繋ぎ方に音量・パン・ミュート/ソロを足したもの。
///
/// [`Routing`] と同じく**メインスレッドで組み立てて丸ごと差し替える** `Copy` な値。
/// ミュートとソロは**解決済みのビット列**として持つので、オーディオスレッドは
/// ビットを見るだけでよい (ソロは経路を辿る必要があり、実行中にやる話ではない)。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Mixer {
    pub routing: Routing,
    /// チェーンの後に掛かる音量 (線形)。**すべての送りに効く**
    gain: [f32; AUDIO_TRACKS],
    /// パン `-1.0` (左) 〜 `1.0` (右)
    pan: [f32; AUDIO_TRACKS],
    /// 鳴らすトラック。ミュートとソロを解いた結果
    audible: u16,
}

impl Default for Mixer {
    fn default() -> Self {
        Self {
            routing: Routing::default(),
            gain: [1.0; AUDIO_TRACKS],
            pan: [0.0; AUDIO_TRACKS],
            audible: u16::MAX,
        }
    }
}

impl Mixer {
    /// 繋ぎ方と、トラックごとの音量・パン・ミュート・ソロから組み立てる。
    ///
    /// # ソロの意味
    ///
    /// **ソロにしたトラックがマスターへ至る経路を阻害しない。** そのトラックと、
    /// そこから送りを辿って届くトラック (バスやマスター) を鳴らす。
    /// こうしないと、リバーブ用のバスへ送っているトラックをソロにした瞬間に
    /// リバーブが切れて、**ソロにしただけで音が変わる**。
    ///
    /// ミュートはソロより強い。ミュートしたトラックは経路上でも鳴らない
    /// (バスをミュートすれば、そこを通る音は止まる)。
    pub fn build(
        routing: Routing,
        gain: &[f32; AUDIO_TRACKS],
        pan: &[f32; AUDIO_TRACKS],
        muted: u16,
        soloed: u16,
    ) -> Self {
        let audible = if soloed == 0 {
            // ソロが1つも無ければ、ミュート以外は全部鳴る
            !muted
        } else {
            // ソロにしたトラックから送りを前向きに辿って広げる
            let mut reached = soloed;
            loop {
                let mut added = false;
                for from in targets(reached) {
                    for to in routing.sends_of(from) {
                        if reached & (1 << to) == 0 {
                            reached |= 1 << to;
                            added = true;
                        }
                    }
                }
                if !added {
                    break;
                }
            }
            reached & !muted
        };

        // NaN や負の値を通すと、以降のブロックすべてに波及する
        let mut safe_gain = [1.0f32; AUDIO_TRACKS];
        let mut safe_pan = [0.0f32; AUDIO_TRACKS];
        for index in 0..AUDIO_TRACKS {
            safe_gain[index] = if gain[index].is_finite() {
                gain[index].max(0.0)
            } else {
                1.0
            };
            safe_pan[index] = if pan[index].is_finite() {
                pan[index].clamp(-1.0, 1.0)
            } else {
                0.0
            };
        }

        Self {
            routing,
            gain: safe_gain,
            pan: safe_pan,
            audible,
        }
    }

    /// そのトラックが鳴るか
    pub fn is_audible(&self, index: usize) -> bool {
        self.audible & (1 << index) != 0
    }

    /// チェーンの後に掛ける左右の係数。
    ///
    /// **パンは定パワーだが、中央を等倍に揃えてある。**
    ///
    /// 素の定パワー (`cos`/`sin`) は中央で `1/√2` になる。それをそのまま使うと
    /// **パンを触っていないトラックまで -3dB 下がる**ので、`√2` を掛けて
    /// 中央を `1.0` にする。振り切ると片側が `√2` (+3dB) まで上がるが、
    /// 「既定が素通し」であることのほうが大事。
    ///
    /// 鳴らないトラック (ミュート・ソロで外れたもの) は 0 を返す。
    pub fn channel_gains(&self, index: usize) -> (f32, f32) {
        if !self.is_audible(index) {
            return (0.0, 0.0);
        }
        let gain = self.gain.get(index).copied().unwrap_or(1.0);
        let pan = self.pan.get(index).copied().unwrap_or(0.0);
        // -1.0 → 0、0.0 → π/4、1.0 → π/2
        let angle = (pan + 1.0) * std::f32::consts::FRAC_PI_4;
        let normalize = std::f32::consts::SQRT_2;
        (
            gain * angle.cos() * normalize,
            gain * angle.sin() * normalize,
        )
    }
}

/// 送り元が送り先より先に来る順を求める。輪があれば `None`。
///
/// 深さ優先で1周するだけ (16本しかないので、確保も再帰の深さも問題にならない)。
fn topological_order(sends: &[u16; AUDIO_TRACKS]) -> Option<([u8; AUDIO_TRACKS], u8)> {
    // 0: 未訪問 / 1: 訪問中 / 2: 済
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
    /// 繋ぎ方・音量・ミュート/ソロ。**丸ごと差し替える**
    mixer: Mixer,
    /// トラックごとの出力 (2ch インターリーブ)。`frames` ごとに区切って使う
    buffers: Vec<f32>,
    /// いま確保してあるフレーム数
    capacity_frames: usize,
    /// トランスポートから取り出したイベントの受け皿 (打ち込み1本ぶん)。
    ///
    /// **同じ打ち込みを複数のトラックへ配るために要る**
    /// ([`emit_from`](Self::emit_from) を参照)。容量は確保済み。
    emit_scratch: BlockEvents,
}

impl Default for Graph {
    fn default() -> Self {
        Self::new()
    }
}

impl Graph {
    pub fn new() -> Self {
        Self {
            tracks: (0..AUDIO_TRACKS).map(|_| AudioTrack::new()).collect(),
            mixer: Mixer::default(),
            buffers: Vec::new(),
            capacity_frames: 0,
            emit_scratch: BlockEvents::with_capacity(128),
        }
    }

    /// 繋ぎ方と音量を差し替える。**組み立て済みのものしか受け取らない**ので、
    /// ここで検査することは何も無い。
    pub fn set_mixer(&mut self, mixer: Mixer) {
        self.mixer = mixer;
    }

    pub fn mixer(&self) -> &Mixer {
        &self.mixer
    }

    pub fn track(&self, index: usize) -> Option<&AudioTrack> {
        self.tracks.get(index)
    }

    pub fn track_mut(&mut self, index: usize) -> Option<&mut AudioTrack> {
        self.tracks.get_mut(index)
    }

    /// どの打ち込みトラックから MIDI を取るかを決める
    pub fn set_midi_track(&mut self, index: usize, midi_track: Option<usize>) {
        if let Some(slot) = self.tracks.get_mut(index) {
            slot.midi_track = midi_track;
        }
    }

    /// チェーンの `at` 段目を差し替える。段数と同じ位置なら末尾へ足す。
    /// 押し出されたノードを返す (**メインスレッドで始末する**)。
    pub fn set_node(&mut self, index: usize, at: usize, node: Node) -> Option<Node> {
        match self.tracks.get_mut(index) {
            Some(slot) => slot.processor.set_node(at, node),
            // 範囲外。落とさずに呼び出し側へ返す
            None => Some(node),
        }
    }

    /// チェーンの `at` 段目を外す
    pub fn remove_node(&mut self, index: usize, at: usize) -> Option<Node> {
        self.tracks.get_mut(index)?.processor.remove_node(at)
    }

    /// チェーンの `from` 段目を `to` 段目へ動かす (並べ替え)
    pub fn move_node(&mut self, index: usize, from: usize, to: usize) {
        if let Some(slot) = self.tracks.get_mut(index) {
            slot.processor.move_node(from, to);
        }
    }

    /// チェーンの `at` 段目を素通しにするか
    pub fn set_bypassed(&mut self, index: usize, at: usize, bypassed: bool) {
        if let Some(slot) = self.tracks.get_mut(index) {
            slot.processor.set_bypassed(at, bypassed);
        }
    }

    /// トラックへチェーンを丸ごと載せる。
    ///
    /// **メインスレッド専用** (確保が起きうる)。書き出しのために借りたものを
    /// 組み直すときと、検証バイナリが使う。再生中の載せ替えは
    /// [`set_node`](Self::set_node) のほう。
    pub fn place_chain(&mut self, index: usize, midi_track: Option<usize>, nodes: Vec<Node>) {
        let Some(slot) = self.tracks.get_mut(index) else {
            return;
        };
        slot.midi_track = midi_track;
        let _ = slot.processor.take_nodes();
        for node in nodes {
            slot.processor.push_node(node);
        }
    }

    /// 載っているノードを全部取り出す (トラック番号・段の昇順)。
    ///
    /// **書き出しのために借りたものを戻すときに使う。**
    pub fn take_nodes(&mut self) -> Vec<(NodeAddr, Node)> {
        let mut taken = Vec::new();
        for (index, track) in self.tracks.iter_mut().enumerate() {
            for (at, node) in track.processor.take_nodes().into_iter().enumerate() {
                taken.push((NodeAddr { track: index, at }, node));
            }
        }
        taken
    }

    /// 全トラックの処理器を (トラック番号, 処理器) で順に見る
    pub fn processors_mut(&mut self) -> impl Iterator<Item = (usize, &mut Box<TrackProcessor>)> {
        self.tracks
            .iter_mut()
            .enumerate()
            .map(|(index, track)| (index, &mut track.processor))
    }

    /// そのオーディオトラックのイベント置き場 (呼び出し側が積む)
    pub fn events_mut(&mut self, index: usize) -> Option<&mut BlockEvents> {
        Some(self.tracks.get_mut(index)?.processor.events_mut())
    }

    /// トランスポートの計画を各トラックへ配る。
    ///
    /// **打ち込みトラックとオーディオトラックは番号が違う。** どの打ち込みを
    /// 取るかはオーディオトラックごとに決まっているので、その対応で配る。
    ///
    /// **1つの打ち込みは、それを見ている全トラックへ届く** (同じ打ち込みを
    /// 複数の音源で重ねられる)。ただし [`Transport::emit_track`] は取り出しながら
    /// カーソルを進めるので、**同じ打ち込みに2度聞くと2度目は空になる**。
    /// そこで打ち込み1本につき1回だけ取り出し、受け皿から配る。
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
        // 受け皿とトラックを同時に触るので、フィールドを個別に借りる
        let Self {
            tracks,
            emit_scratch,
            ..
        } = self;

        for index in 0..AUDIO_TRACKS {
            let Some(midi_track) = tracks[index].midi_track else {
                continue;
            };
            // 同じ打ち込みを見ているトラックが先にあれば、そこで配り済み
            let first = (0..index).all(|earlier| tracks[earlier].midi_track != Some(midi_track));
            if !first {
                continue;
            }

            emit_scratch.clear();
            transport.emit_track(midi_track, plan, emit_scratch);
            if emit_scratch.is_empty() {
                continue;
            }

            for target in tracks.iter_mut().skip(index) {
                if target.midi_track != Some(midi_track) {
                    continue;
                }
                let events = target.processor.events_mut();
                for event in emit_scratch.iter() {
                    events.push(*event);
                }
            }
        }
    }

    /// 全トラックのイベントを空にする
    pub fn clear_events(&mut self) {
        for track in self.tracks.iter_mut() {
            track.processor.events_mut().clear();
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

        let mixer = self.mixer;
        for index in mixer.routing.order() {
            self.run_track(index, steady, len, on_error);

            // チェーンの後に音量とパンを掛ける。**送りに入る前**なので、
            // すべての送り先に同じように効く。
            // 鳴らさないトラック (ミュート・ソロで外れたもの) はここで 0 になる。
            //
            // 等倍のときに飛ばす細工はしない。浮動小数の一致で分岐すると
            // 「掛かるはずが飛ばされた」が起きうるわりに、省ける手間は
            // 1フレーム2回の掛け算しかない
            let (left, right) = mixer.channel_gains(index);
            let buffer = self.buffer_mut(index, len);
            for frame in buffer.chunks_exact_mut(BUS_CHANNELS) {
                frame[0] *= left;
                frame[1] *= right;
            }

            // 送り先へ足す。**加算コピー**なので、複数から送られれば混ざる
            for target in mixer.routing.sends_of(index) {
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
        let processor = &mut self.tracks[index].processor;
        if processor.node_count() == 0 {
            return; // 何も刺さっていない。入ってきた音をそのまま流す
        }
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

    /// 既定 (等倍・中央・ミュートなし) で組む
    fn mixer(edges: &[(usize, usize)], muted: u16, soloed: u16) -> Mixer {
        Mixer::build(
            routing(edges).unwrap(),
            &[1.0; AUDIO_TRACKS],
            &[0.0; AUDIO_TRACKS],
            muted,
            soloed,
        )
    }

    /// **既定のパンは素通しであること。**
    ///
    /// 素の定パワーは中央で `1/√2` になる。それをそのまま使うと、パンを
    /// 触っていないトラックまで -3dB 下がる (実際に mixed_smoke が半分になった)。
    #[test]
    fn centre_pan_is_unity() {
        let mixer = mixer(&[(1, MASTER)], 0, 0);
        let (left, right) = mixer.channel_gains(1);
        assert!((left - 1.0).abs() < 1e-6, "左が等倍でない: {left}");
        assert!((right - 1.0).abs() < 1e-6, "右が等倍でない: {right}");
    }

    /// 振り切ると片側が消え、定パワーが保たれること
    #[test]
    fn hard_pan_silences_one_side_and_keeps_power() {
        let mut pan = [0.0f32; AUDIO_TRACKS];
        pan[1] = -1.0;
        let mixer = Mixer::build(
            routing(&[(1, MASTER)]).unwrap(),
            &[1.0; AUDIO_TRACKS],
            &pan,
            0,
            0,
        );
        let (left, right) = mixer.channel_gains(1);
        assert!(right.abs() < 1e-6, "右が消えること: {right}");
        // 左右の二乗和は中央と同じ (1^2 + 1^2 = 2)
        assert!((left * left + right * right - 2.0).abs() < 1e-5);
    }

    /// ミュートしたトラックは 0 になること
    #[test]
    fn muted_tracks_are_silenced() {
        let mixer = mixer(&[(1, MASTER), (2, MASTER)], 1 << 1, 0);
        assert_eq!(mixer.channel_gains(1), (0.0, 0.0));
        assert!(mixer.is_audible(2));
    }

    /// **ソロは経路を辿って残すこと。**
    ///
    /// バスへ送っているトラックをソロにしたとき、バスも鳴らないと
    /// ソロにしただけでリバーブが切れて音が変わってしまう。
    #[test]
    fn solo_keeps_the_path_to_master() {
        // 1 → 3 (バス) → 0、2 → 0。1 をソロにする
        let mixer = mixer(&[(1, 3), (3, MASTER), (2, MASTER)], 0, 1 << 1);

        assert!(mixer.is_audible(1), "ソロにした本人");
        assert!(mixer.is_audible(3), "経路上のバスは残る");
        assert!(mixer.is_audible(MASTER), "マスターは残る");
        assert!(!mixer.is_audible(2), "経路外は黙る");
    }

    /// ミュートはソロより強いこと (バスを止めれば通る音も止まる)
    #[test]
    fn mute_wins_over_solo() {
        let mixer = mixer(&[(1, 3), (3, MASTER)], 1 << 3, 1 << 1);
        assert!(mixer.is_audible(1));
        assert!(!mixer.is_audible(3), "ミュートしたバスは経路上でも黙る");
    }

    /// ソロが1つも無ければ、ミュート以外は全部鳴ること
    #[test]
    fn without_solo_everything_but_muted_sounds() {
        let mixer = mixer(&[(1, MASTER), (2, MASTER)], 0, 0);
        assert!(mixer.is_audible(1));
        assert!(mixer.is_audible(2));
    }

    /// **MIDI の割り当てどおりに配ること。**
    ///
    /// 実際にここで音が出なくなった: 割り当てがオーディオスレッドへ届いておらず、
    /// 画面には「トラック1」と出ているのに `midi_track` は `None` のままだった。
    /// 割り当てが無いトラックへイベントが行かないことを、ここで縛っておく。
    #[test]
    fn events_go_to_the_assigned_midi_track_only() {
        use crate::audio::transport::{Transport, TransportMsg, TransportShared};
        use crate::sequencer::{SeqEvent, SeqEventKind};

        let mut transport = Transport::new(TransportShared::new());
        let _ = transport.handle_msg(TransportMsg::SetSequence {
            track: 0,
            events: vec![SeqEvent {
                sample_time: 0,
                kind: SeqEventKind::NoteOn {
                    key: 60,
                    velocity: 1.0,
                },
            }]
            .into_boxed_slice(),
            end_sample: 4096,
        });
        let _ = transport.handle_msg(TransportMsg::Play);

        let mut graph = Graph::new();
        graph.set_midi_track(1, Some(0)); // 打ち込み0 を鳴らす
        graph.set_midi_track(2, None); // 未割り当て

        let plan = transport.plan_block(512);
        graph.clear_events();
        graph.emit_from(&mut transport, &plan);

        assert!(
            !graph.events_mut(1).unwrap().is_empty(),
            "割り当てたトラックには届くこと"
        );
        assert!(
            graph.events_mut(2).unwrap().is_empty(),
            "未割り当てのトラックには届かないこと"
        );
    }

    /// 1つの打ち込みを複数のオーディオトラックが見ていれば、どれにも届くこと
    #[test]
    fn one_midi_track_can_feed_several_audio_tracks() {
        use crate::audio::transport::{Transport, TransportMsg, TransportShared};
        use crate::sequencer::{SeqEvent, SeqEventKind};

        let mut transport = Transport::new(TransportShared::new());
        let _ = transport.handle_msg(TransportMsg::SetSequence {
            track: 3,
            events: vec![SeqEvent {
                sample_time: 0,
                kind: SeqEventKind::NoteOn {
                    key: 64,
                    velocity: 1.0,
                },
            }]
            .into_boxed_slice(),
            end_sample: 4096,
        });
        let _ = transport.handle_msg(TransportMsg::Play);

        let mut graph = Graph::new();
        graph.set_midi_track(1, Some(3));
        graph.set_midi_track(5, Some(3));

        let plan = transport.plan_block(512);
        graph.clear_events();
        graph.emit_from(&mut transport, &plan);

        assert!(!graph.events_mut(1).unwrap().is_empty());
        assert!(!graph.events_mut(5).unwrap().is_empty());
    }

    /// NaN や負の音量を通さないこと (以降のブロックすべてに波及する)
    #[test]
    fn broken_gain_falls_back() {
        let mut gain = [1.0f32; AUDIO_TRACKS];
        gain[1] = f32::NAN;
        gain[2] = -1.0;
        let mixer = Mixer::build(
            routing(&[(1, MASTER), (2, MASTER)]).unwrap(),
            &gain,
            &[0.0; AUDIO_TRACKS],
            0,
            0,
        );
        assert!(mixer.channel_gains(1).0.is_finite());
        assert!(mixer.channel_gains(2).0 >= 0.0);
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
