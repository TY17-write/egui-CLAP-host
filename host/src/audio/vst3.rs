//! VST3 バックエンド。
//!
//! 中立の [`BlockEvents`] を VST3 のイベントへ移し、プラグインを1ブロック処理して、
//! 出力をインターリーブで足し込むところまでを受け持つ。
//!
//! ここより上 (トランスポート・ミキサ・オフラインレンダラ) は VST3 を知らない。
//!
//! CLAP との差が3つあり、いずれもこのファイルの中で吸収する。
//!
//! - **消音イベントが無い。** CLAP の `NoteChoke` に当たるものが VST3 には無いので、
//!   鳴らしたキーを自前で覚えておき、choke ではそれを1つずつ note-off する
//!   (詳しくは [`Vst3Processor::active`])
//! - **音源をメインスレッドと共有する。** [`SharedPlugin`] を参照
//! - **バッファがチャンネルごとの `Vec`。** CLAP のポート連続配置とは別物なので、
//!   インターリーブへの変換をここに持つ

use crate::audio::config::StreamAudioConfig;
use crate::audio::events::{BlockEvent, BlockEvents};
use crate::audio::ProcessError;
use crate::sequencer::CC_RELEASE;
use std::sync::{Arc, Mutex, MutexGuard};
use vst3_host::audio::AudioBuffers;
use vst3_host::midi::{MidiChannel, MidiEvent};
use vst3_host::plugin::Plugin;

/// 音源を取れずに送り損ねたイベントを溜めておく上限。
///
/// 1ブロックに数十件しか出ないので、これだけあれば数ブロック分の取りこぼしを
/// 吸収できる。溢れるのは異常事態なので、そのときは捨てる。
const MAX_DEFERRED: usize = 512;

/// メインスレッドとオーディオスレッドで1つの音源を共有する入れ物。
///
/// CLAP では音源本体がメインスレッドに残り、切り離した処理器だけを
/// オーディオスレッドへ渡せる。`vst3-host` の [`Plugin`] は処理器と
/// エディットコントローラを1つの型にまとめているため、この分け方ができない。
/// エディタの表示 (`open_editor`) も状態の保存・復元 (`save_state` / `load_state`) も
/// メインスレッドから `&mut Plugin` を要求するので、値でオーディオスレッドへ
/// 渡してしまうと二度と届かなくなる。
///
/// そこで `Arc<Mutex<..>>` で共有する。**オーディオスレッド側は必ず
/// [`try_lock`](Self::try_lock) を使い、取れなければそのブロックを飛ばす。**
/// 待たないので締め切りは落とさない。
///
/// 取り合いになるのはエディタの開閉と状態の保存・復元のときだけだが、
/// **エディタを開く間は長い**。`open_editor` は音源の中でエディタを組み立てるため、
/// Surge XT では1.7秒ほど握りっぱなしになり、その間そのトラックは無音になる
/// (計測と内訳は `docs/vst3_host_plan.md` の「フェーズ3 で分かったこと」)。
/// 音がずれないよう、飛ばしたブロックのイベントは持ち越して次に送る。
///
/// 破棄はメインスレッドで行われる。オーディオスレッドが持つ複製は
/// `retired` 経由でメインスレッドへ戻り、そこで落ちるため。
#[derive(Clone)]
pub struct SharedPlugin(Arc<Mutex<Plugin>>);

impl SharedPlugin {
    pub fn new(plugin: Plugin) -> Self {
        Self(Arc::new(Mutex::new(plugin)))
    }

    /// **たまにしか起きない操作**のためのもの。取れるまで待つ。
    ///
    /// エディタの開閉、状態の保存・復元、停止のように、ユーザー操作の頻度でしか
    /// 起きないものに限って使うこと。**毎フレームここで待ってはいけない。**
    /// 理由は [`try_lock`](Self::try_lock) を参照。
    ///
    /// 毒された (ロック中にパニックした) 場合も中身は取り出す。VST3 の音源は
    /// COM オブジェクトで、ここで諦めると破棄する手立てが無くなる。
    pub fn lock(&self) -> MutexGuard<'_, Plugin> {
        self.0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// 待たずに取る。オーディオスレッドと、**メインスレッドの毎フレーム処理**の両方で使う。
    ///
    /// メインスレッド側が [`lock`](Self::lock) で待つと、オーディオスレッドが
    /// 手放した瞬間に必ず横取りする形になり、次のブロックの `try_lock` が落ちる。
    /// 毎フレーム待ちに行くと待ち手が絶えないので、これが常態化する。
    /// 取れなければ次のフレームに回せばよいものは、必ずこちらを使うこと。
    pub fn try_lock(&self) -> Option<MutexGuard<'_, Plugin>> {
        match self.0.try_lock() {
            Ok(guard) => Some(guard),
            Err(std::sync::TryLockError::Poisoned(poisoned)) => Some(poisoned.into_inner()),
            Err(std::sync::TryLockError::WouldBlock) => None,
        }
    }
}

/// VST3 の音源1つぶんの処理器
pub struct Vst3Processor {
    plugin: SharedPlugin,
    /// プラグインに渡すチャンネルごとのバッファ。長さが1ブロックのフレーム数になる。
    buffers: AudioBuffers,
    /// CPAL 側の出力チャンネル数 (1 か 2)
    output_channels: usize,
    /// 鳴らしているキー。ビット位置がキー番号 (0〜127)。
    ///
    /// VST3 には消音イベントが無いので、choke ではここに立っているキーを
    /// 1つずつ note-off する。128ビットなのでオーディオスレッドでも確保は起きない。
    active: u128,
    /// 効かせている CC。ビット位置が CC 番号 (0〜127)。
    ///
    /// choke で解除値に戻すために覚えておく。これが無いと、ペダルを踏んだまま
    /// 停止したときに踏みっぱなしで残る。
    active_ccs: u128,
    /// 音源を取れずに送れなかったイベント。次に取れたブロックの先頭で送り直す。
    ///
    /// 捨ててしまうと note-off が失われて音が鳴りっぱなしになる。
    /// 位置は失われるが、鳴り続けるよりはるかにましと判断した。
    deferred: Vec<BlockEvent>,
}

impl Vst3Processor {
    /// `plugin_channels` はプラグインの出力チャンネル数 (CPAL 側とは違ってよい)。
    ///
    /// **入力チャンネル数はホスト側が決めている。** `vst3-host` には
    /// `output_channel_count` はあるが入力側を問い合わせる術が無く、
    /// `Vst3HostBuilder::input_channels` で指定した数にバスが並ぶ。
    /// そのため `mod.rs` が渡した値をそのまま使う。
    pub fn new(
        plugin: SharedPlugin,
        plugin_channels: usize,
        input_channels: usize,
        stream_config: &StreamAudioConfig,
    ) -> Self {
        Self {
            plugin,
            buffers: AudioBuffers::new(
                input_channels,
                plugin_channels.max(1),
                stream_config.max_likely_buffer_size as usize,
                stream_config.sample_rate as f64,
            ),
            output_channels: stream_config.output_channel_count.max(1),
            active: 0,
            active_ccs: 0,
            deferred: Vec::with_capacity(MAX_DEFERRED),
        }
    }

    /// メインスレッドへ返せる形にする。
    ///
    /// CLAP と違って停止はここで行わない。`setProcessing(false)` はリアルタイム安全でなく、
    /// 呼ぶ相手もメインスレッドなので、受け取った側で止める。
    pub fn into_shared(self) -> SharedPlugin {
        self.plugin
    }

    /// 1ブロック処理して `buf` を**置き換える**。
    ///
    /// `buf` は入力と出力を兼ねる。入ってきた音を読み、処理した音で上書きする。
    /// 長さがそのままブロック長 (フレーム数 × チャンネル数) になる。
    ///
    /// `node` はチェーンの何段目か。**自分に宛てたパラメータだけを拾う**ために使う。
    pub fn process(
        &mut self,
        events: &BlockEvents,
        node: usize,
        buf: &mut [f32],
    ) -> Result<(), ProcessError> {
        let Some(mut plugin) = self.plugin.try_lock() else {
            // メインスレッドが音源を使っている。このブロックは無音のまま返し、
            // イベントは取っておいて次に送る。
            self.defer(events, node);
            return Err(ProcessError::Busy);
        };

        let frames = buf.len() / self.output_channels;
        if frames == 0 {
            return Ok(());
        }
        resize_buffers(&mut self.buffers, frames);

        // 溜めておいたぶんを先に送る (順序は保つ)
        for index in 0..self.deferred.len() {
            let event = self.deferred[index];
            send(
                &mut plugin,
                &mut self.active,
                &mut self.active_ccs,
                event,
                frames,
            );
        }
        self.deferred.clear();
        for event in events {
            if !addressed_to(event, node) {
                continue;
            }
            send(
                &mut plugin,
                &mut self.active,
                &mut self.active_ccs,
                *event,
                frames,
            );
        }

        // 出力だけを 0 にする。**入力は今から書くので消してはいけない**
        for channel in self.buffers.outputs.iter_mut() {
            channel.fill(0.0);
        }
        feed_input(&mut self.buffers, buf, self.output_channels, frames);
        plugin.process_audio(&mut self.buffers)?;
        drop(plugin);

        self.write_into(buf, frames);
        Ok(())
    }

    /// 送れなかったイベントを取っておく。上限を超えたぶんは捨てる。
    ///
    /// **他のノード宛てのパラメータは取っておかない。** 持ち越すと、次に
    /// 取れたブロックで自分に効いてしまう。
    fn defer(&mut self, events: &BlockEvents, node: usize) {
        for event in events {
            if self.deferred.len() >= MAX_DEFERRED {
                return;
            }
            if !addressed_to(event, node) {
                continue;
            }
            // 次のブロックで送るので、ブロック内の位置は先頭に潰す
            self.deferred.push(at_block_start(*event));
        }
    }

    /// プラグインの出力を CPAL のインターリーブ形式へ移して**書き込む** (加算ではない)。
    /// チャンネル数が食い違うときは複製・平均で合わせる (`buffers.rs` と同じ方針)。
    ///
    /// **上書きなのは、このバッファが次のノードの入力になるため** (`buffers.rs` の
    /// `write_into` と同じ理由)。トラック同士を混ぜるのは呼び出し側の仕事。
    fn write_into(&self, buf: &mut [f32], frames: usize) {
        let outputs = &self.buffers.outputs;
        let channels = self.output_channels;

        match outputs.len() {
            // 出力バスが無い。入ってきた音を残さず無音にする
            // (上書きの約束を守るため。加算だった頃は何もしないのが正しかった)
            0 => buf[..frames * channels].fill(0.0),
            // モノラル出力は全チャンネルへ複製する
            1 => {
                let source = &outputs[0];
                for (frame, samples) in buf.chunks_exact_mut(channels).take(frames).enumerate() {
                    let value = source[frame];
                    for sample in samples {
                        *sample = value;
                    }
                }
            }
            // モノラル出力へはダウンミックスする
            count if channels == 1 => {
                let scale = 1.0 / count as f32;
                for (frame, sample) in buf.iter_mut().take(frames).enumerate() {
                    let total: f32 = outputs.iter().map(|channel| channel[frame]).sum();
                    *sample = total * scale;
                }
            }
            // 足りないチャンネルは最後のもので埋める (2ch 出力に 1ch 音源は上で処理済み)
            count => {
                for (frame, samples) in buf.chunks_exact_mut(channels).take(frames).enumerate() {
                    for (channel, sample) in samples.iter_mut().enumerate() {
                        *sample = outputs[channel.min(count - 1)][frame];
                    }
                }
            }
        }
    }
}

/// 入ってきた音をプラグインの入力バッファへ配る。
///
/// **入力バスが無ければ何もしない。** 音源はこちらに来る。
/// チャンネル数が食い違うときの扱いは `write_into` と揃える。
///
/// 自由関数にしてあるのは、呼び出し時点で `plugin` のロックを握っており
/// `&mut self` を取れないため (`resize_buffers` と同じ事情)。
fn feed_input(buffers: &mut AudioBuffers, buf: &[f32], channels: usize, frames: usize) {
    let inputs = &mut buffers.inputs;
    match inputs.len() {
        0 => {}
        // モノラル入力へは平均で落とす
        1 => {
            let scale = 1.0 / channels as f32;
            for (frame, sample) in inputs[0].iter_mut().take(frames).enumerate() {
                let base = frame * channels;
                *sample = buf[base..base + channels].iter().sum::<f32>() * scale;
            }
        }
        _ => {
            for (index, input) in inputs.iter_mut().enumerate() {
                if index >= channels {
                    // **バスに無いチャンネルは無音。** ステレオまでしか扱わない
                    // (最後の1本で埋めると、余った入力に R が入ってしまう)
                    let end = frames.min(input.len());
                    input[..end].fill(0.0);
                    continue;
                }
                for (frame, sample) in input.iter_mut().take(frames).enumerate() {
                    *sample = buf[frame * channels + index];
                }
            }
        }
    }
}

/// このノードが受け取るべきイベントか。
///
/// 音符・CC・消音はトラックの全ノードへ配る (受け取れないノードは自分で捨てる)。
/// **パラメータだけは宛先を見る。**
fn addressed_to(event: &BlockEvent, node: usize) -> bool {
    match event {
        BlockEvent::Param { node: target, .. } => *target == node,
        _ => true,
    }
}

/// 中立のイベント1件を VST3 の形で送り、鳴っているキーの記録を更新する。
///
/// 送信に失敗しても続ける。1件の取りこぼしでブロック全体を落とすより、
/// 残りを届けたほうが被害が小さい。
fn send(
    plugin: &mut Plugin,
    active: &mut u128,
    active_ccs: &mut u128,
    event: BlockEvent,
    frames: usize,
) {
    // ブロックの外を指す位置は端に寄せる (VST3 側の扱いが実装依存のため)
    let clamp = |offset: u32| offset.min(frames as u32 - 1) as i32;

    match event {
        BlockEvent::NoteOn {
            offset,
            key,
            velocity,
        } => {
            let key = key.min(127);
            let velocity = (velocity * 127.0).round().clamp(1.0, 127.0) as u8;
            if plugin
                .send_midi_event_at(
                    MidiEvent::NoteOn {
                        channel: MidiChannel::Ch1,
                        note: key,
                        velocity,
                    },
                    clamp(offset),
                )
                .is_ok()
            {
                *active |= 1u128 << key;
            }
        }
        BlockEvent::NoteOff { offset, key } => {
            let key = key.min(127);
            let _ = plugin.send_midi_event_at(note_off(key), clamp(offset));
            *active &= !(1u128 << key);
        }
        BlockEvent::Choke { offset } => {
            let offset = clamp(offset);
            let mut remaining = *active;
            while remaining != 0 {
                let key = remaining.trailing_zeros() as u8;
                remaining &= remaining - 1; // 最下位の立っているビットを落とす
                let _ = plugin.send_midi_event_at(note_off(key), offset);
            }
            *active = 0;

            // 効かせた CC も戻す。音符を止めてもペダルは踏まれたままなので、
            // これが無いと停止・シークのあとも踏みっぱなしで残る。
            let mut remaining = *active_ccs;
            while remaining != 0 {
                let number = remaining.trailing_zeros() as u8;
                remaining &= remaining - 1;
                let _ = plugin.send_midi_event_at(control_change(number, CC_RELEASE), offset);
            }
            *active_ccs = 0;
        }
        BlockEvent::Cc {
            offset,
            number,
            value,
        } => {
            let number = number.min(127);
            let value = value.min(127);
            if plugin
                .send_midi_event_at(control_change(number, value), clamp(offset))
                .is_ok()
            {
                // 解除値まで戻したものは「効いていない」ので覚えなくてよい
                if value == CC_RELEASE {
                    *active_ccs &= !(1u128 << number);
                } else {
                    *active_ccs |= 1u128 << number;
                }
            }
        }
        // 宛先の振り分けは `addressed_to` で済ませてある
        BlockEvent::Param {
            offset, id, value, ..
        } => {
            // パラメータ UI は現在無効なので、ここを通るのは鍵盤 UI 経由のときだけ。
            // VST3 の値域は 0.0〜1.0 に正規化されている。
            let _ = plugin.set_parameter_at(id, value.clamp(0.0, 1.0), clamp(offset));
        }
    }
}

fn note_off(key: u8) -> MidiEvent {
    MidiEvent::NoteOff {
        channel: MidiChannel::Ch1,
        note: key,
        velocity: 0,
    }
}

/// VST3 では CC はイベントではなく、`IMidiMapping` で対応付けられた
/// パラメータの変更として届く。その変換は `vst3-host` が受け持つ。
fn control_change(number: u8, value: u8) -> MidiEvent {
    MidiEvent::ControlChange {
        channel: MidiChannel::Ch1,
        controller: number,
        value,
    }
}

/// ブロック内の位置を先頭へ潰す (持ち越したイベント用)
fn at_block_start(event: BlockEvent) -> BlockEvent {
    match event {
        BlockEvent::NoteOn { key, velocity, .. } => BlockEvent::NoteOn {
            offset: 0,
            key,
            velocity,
        },
        BlockEvent::NoteOff { key, .. } => BlockEvent::NoteOff { offset: 0, key },
        BlockEvent::Choke { .. } => BlockEvent::Choke { offset: 0 },
        BlockEvent::Param {
            node, id, value, ..
        } => BlockEvent::Param {
            offset: 0,
            node,
            id,
            value,
        },
        BlockEvent::Cc { number, value, .. } => BlockEvent::Cc {
            offset: 0,
            number,
            value,
        },
    }
}

/// バッファの長さを今回のブロックに合わせる。
///
/// `AudioBuffers` は各チャンネルの `Vec` の長さがそのままフレーム数になる仕様。
/// 生成時に上限ぶん確保してあるので、縮める・戻すだけなら確保は起きない
/// (上限を超えるブロックが来たときだけ伸びる)。
fn resize_buffers(buffers: &mut AudioBuffers, frames: usize) {
    let current = buffers.outputs.first().map_or(0, Vec::len);
    if current == frames {
        return;
    }
    for channel in buffers.outputs.iter_mut().chain(buffers.inputs.iter_mut()) {
        channel.resize(frames, 0.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 持ち越したイベントはブロックの先頭へ寄せること。
    /// (次のブロックで送るので、元の位置は意味を持たない)
    #[test]
    fn deferred_events_move_to_the_block_start() {
        let moved = at_block_start(BlockEvent::NoteOn {
            offset: 300,
            key: 60,
            velocity: 0.5,
        });
        assert_eq!(
            moved,
            BlockEvent::NoteOn {
                offset: 0,
                key: 60,
                velocity: 0.5
            }
        );

        let moved = at_block_start(BlockEvent::Choke { offset: 128 });
        assert_eq!(moved, BlockEvent::Choke { offset: 0 });
    }

    /// バッファの長さがフレーム数に追随し、縮めても容量が残ること。
    /// (容量が減ると次のブロックでオーディオスレッドが確保することになる)
    #[test]
    fn buffers_follow_the_frame_count_without_shrinking_capacity() {
        let mut buffers = AudioBuffers::new(0, 2, 512, 48_000.0);

        resize_buffers(&mut buffers, 128);
        assert_eq!(buffers.outputs[0].len(), 128);
        assert!(buffers.outputs[0].capacity() >= 512, "容量は減らないこと");

        resize_buffers(&mut buffers, 512);
        assert_eq!(buffers.outputs[1].len(), 512);
    }

    /// choke は鳴っているキーだけを消し、記録を空にすること
    #[test]
    fn choke_clears_every_sounding_key() {
        // 実際の送信はプラグインが要るので、ビット演算の部分だけを確かめる
        let mut active: u128 = 0;
        active |= 1u128 << 60;
        active |= 1u128 << 64;
        active |= 1u128 << 127;

        let mut keys = Vec::new();
        let mut remaining = active;
        while remaining != 0 {
            keys.push(remaining.trailing_zeros() as u8);
            remaining &= remaining - 1;
        }

        assert_eq!(
            keys,
            vec![60, 64, 127],
            "立っているキーだけを昇順で拾うこと"
        );
    }
}
