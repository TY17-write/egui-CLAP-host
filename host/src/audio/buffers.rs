//! CLAP のオーディオバッファの管理と、インターリーブ形式への変換。
//! clack リポジトリの cpal サンプル (MIT OR Apache-2.0) をほぼそのまま利用。
//!
//! ここは CLAP バックエンド専用 (`audio::clap` からのみ使う)。
//! チャンネル変換 (`mux` / `mix_mono` / `mono_to_multi`) は素のスライスを扱うので
//! 共有できそうに見えるが、CLAP はポートごとに連続した配置 (`[l,l,l,r,r,r]`)、
//! VST3 はチャンネルごとの `Vec` と元の形が違う。共通化は VST3 側の要求が
//! はっきりしてから判断する。

use crate::audio::config::FullAudioConfig;
use clack_host::prelude::{
    AudioPortBuffer, AudioPortBufferType, AudioPorts, InputAudioBuffers, InputChannel,
    OutputAudioBuffers,
};
use cpal::{FromSample, Sample};

/// ホストが扱うすべてのオーディオバッファ
pub struct HostAudioBuffers {
    config: FullAudioConfig,

    input_ports: AudioPorts,
    output_ports: AudioPorts,

    /// 各入力ポートのチャンネルバッファ (1ポート分の全チャンネルを連続配置)
    input_port_channels: Box<[Vec<f32>]>,
    /// 各出力ポートのチャンネルバッファ (1ポート分の全チャンネルを連続配置)
    output_port_channels: Box<[Vec<f32>]>,

    /// CPAL のインターリーブ形式に変換済みの出力データ
    muxed: Vec<f32>,

    /// チャンネルバッファの実サイズ。CPAL の要求次第で初期設定より大きくなりうる。
    actual_frame_count: usize,
}

impl HostAudioBuffers {
    pub fn from_config(config: FullAudioConfig) -> Self {
        if config.output_channel_count > 2 {
            panic!(
                "{}チャンネル構成は未対応です (モノラルかステレオのみ)",
                config.output_channel_count
            )
        }

        let total_input_channel_count = config.plugin_input_port_config.total_channel_count();
        let total_output_channel_count = config.plugin_output_port_config.total_channel_count();
        let frame_count = config.max_likely_buffer_size as usize;

        Self {
            input_ports: AudioPorts::with_capacity(
                total_input_channel_count,
                config.plugin_input_port_config.ports.len(),
            ),
            output_ports: AudioPorts::with_capacity(
                total_output_channel_count,
                config.plugin_output_port_config.ports.len(),
            ),
            input_port_channels: config
                .plugin_input_port_config
                .ports
                .iter()
                .map(|p| vec![0.0; frame_count * p.port_layout.channel_count() as usize])
                .collect(),
            output_port_channels: config
                .plugin_output_port_config
                .ports
                .iter()
                .map(|p| vec![0.0; frame_count * p.port_layout.channel_count() as usize])
                .collect(),
            muxed: vec![0.0; frame_count * config.output_channel_count],
            config,
            actual_frame_count: frame_count,
        }
    }

    /// CPAL バッファのサイズに合わせて内部バッファを(必要なら)拡張する
    pub fn ensure_buffer_size_matches(&mut self, cpal_buffer_size: usize) {
        let current_frame_count = self.cpal_buf_len_to_frame_count(cpal_buffer_size);

        if current_frame_count > self.actual_frame_count {
            self.actual_frame_count = current_frame_count;

            for (buf, port) in self
                .input_port_channels
                .iter_mut()
                .zip(&self.config.plugin_input_port_config.ports)
            {
                buf.resize(
                    current_frame_count * port.port_layout.channel_count() as usize,
                    0.0,
                );
            }

            for (buf, port) in self
                .output_port_channels
                .iter_mut()
                .zip(&self.config.plugin_output_port_config.ports)
            {
                buf.resize(
                    current_frame_count * port.port_layout.channel_count() as usize,
                    0.0,
                );
            }

            self.muxed
                .resize(current_frame_count * self.config.output_channel_count, 0.0);
        }
    }

    /// CPAL バッファ長をフレーム数に変換する
    pub fn cpal_buf_len_to_frame_count(&self, buf_len: usize) -> usize {
        buf_len / self.config.output_channel_count
    }

    /// プラグインに渡す入出力バッファを準備する。
    ///
    /// `input` は**このノードへ入ってきた音** (ストリームのインターリーブ形式)。
    /// メイン入力ポートへ配る。**入力ポートを持たないプラグイン (音源) では
    /// 何も起きない**ので、チェーンの先頭に音源を置けば入ってきた音は捨てられる。
    ///
    /// メイン以外の入力ポート (サイドチェーンなど) は無音のまま。繋ぐ手段を
    /// まだ持っていないため。
    pub fn prepare_plugin_buffers(
        &mut self,
        cpal_buf_len: usize,
        input: &[f32],
    ) -> (InputAudioBuffers<'_>, OutputAudioBuffers<'_>) {
        let sample_count = self.cpal_buf_len_to_frame_count(cpal_buf_len);
        assert!(sample_count <= self.actual_frame_count);

        self.output_port_channels
            .iter_mut()
            .for_each(|b| b.fill(0.0));
        self.input_port_channels
            .iter_mut()
            .for_each(|b| b.fill(0.0));
        self.feed_main_input(input, sample_count);

        // **音を入れたポートを「定数」と申告してはいけない。** `is_constant` は
        // 「このチャンネルは全サンプルが同じ値」という申告で、真に受けた
        // プラグインは先頭1サンプルだけ読んで済ませてよい。メイン入力には
        // 今まさに波形を書いたので偽、それ以外は無音のままなので真。
        let main_input = self.config.plugin_input_port_config.main_port_index as usize;
        let frame_count = self.actual_frame_count;

        (
            self.input_ports.with_input_buffers(
                self.input_port_channels
                    .iter_mut()
                    .enumerate()
                    .map(|(index, port_buf)| AudioPortBuffer {
                        latency: 0,
                        channels: AudioPortBufferType::f32_input_only(
                            port_buf.chunks_exact_mut(frame_count).map(move |buffer| {
                                InputChannel {
                                    buffer: &mut buffer[..sample_count],
                                    is_constant: index != main_input,
                                }
                            }),
                        ),
                    }),
            ),
            self.output_ports
                .with_output_buffers(self.output_port_channels.iter_mut().map(|port_buf| {
                    AudioPortBuffer {
                        latency: 0,
                        channels: AudioPortBufferType::f32_output_only(
                            port_buf
                                .chunks_exact_mut(self.actual_frame_count)
                                .map(|buf| &mut buf[..sample_count]),
                        ),
                    }
                })),
        )
    }

    /// 入ってきた音をメイン入力ポートのチャンネルバッファへ配る。
    ///
    /// **入力ポートが無ければ何もしない。** 音源はこちらに来る。
    fn feed_main_input(&mut self, input: &[f32], sample_count: usize) {
        let config = &self.config.plugin_input_port_config;
        let Some(port) = config.ports.get(config.main_port_index as usize) else {
            return;
        };
        let plugin_channels = port.port_layout.channel_count() as usize;
        let stream_channels = self.config.output_channel_count;
        if plugin_channels == 0 || stream_channels == 0 {
            return;
        }

        let buffer = &mut self.input_port_channels[config.main_port_index as usize];
        demux(
            &input[..sample_count * stream_channels],
            buffer,
            stream_channels,
            plugin_channels,
            self.actual_frame_count,
            sample_count,
        );
    }

    /// プラグイン出力を CPAL 用のインターリーブ形式に整える (内部バッファへ)
    fn muxed_output(&mut self, len: usize) -> &[f32] {
        let main_output = &self.output_port_channels
            [self.config.plugin_output_port_config.main_port_index as usize];
        let muxed = &mut self.muxed[..len];

        let plugin_output_channel_count = self
            .config
            .plugin_output_port_config
            .main_port()
            .port_layout
            .channel_count();

        match (
            plugin_output_channel_count,
            self.config.output_channel_count,
        ) {
            (1, 1) => muxed.copy_from_slice(&main_output[..len]),
            (n, 1) => mix_mono(main_output, muxed, n as usize),
            (1, 2) => mono_to_multi(main_output, muxed, 2),
            (_, 2) => mux(main_output, muxed, 2),
            (_, _) => unreachable!(),
        }
        muxed
    }

    /// プラグイン出力を CPAL バッファに書き出す (ダウンミックス/インターリーブ込み)
    pub fn write_to_cpal_buffer<S: FromSample<f32>>(&mut self, destination: &mut [S]) {
        let muxed = self.muxed_output(destination.len());
        for (out, muxed) in destination.iter_mut().zip(muxed) {
            *out = muxed.to_sample();
        }
    }

    /// プラグイン出力をノードのバッファへ**書き込む** (加算ではない)。
    ///
    /// チェーンでは、このバッファがそのまま次のノードの入力になる。
    /// **足し込みではなく上書き**にしているのは、エフェクトが「入ってきた音を
    /// 加工して返す」ものだから。加算にすると原音が必ず混ざってしまう。
    ///
    /// トラック同士を混ぜるのは呼び出し側 (`audio::mod`) の仕事。
    pub fn write_into(&mut self, destination: &mut [f32]) {
        let muxed = self.muxed_output(destination.len());
        destination.copy_from_slice(muxed);
    }
}

/// CPAL のインターリーブ形式 [l,r,l,r] を、チャンネルごとに連続した配置
/// [l,l,r,r] へ移す (`mux` の逆)。
///
/// `stride` は1チャンネル分の確保長。書き込むのは各チャンネルの先頭 `frames`
/// 分だけで、その先は呼び出し側が 0 にしてある。
///
/// チャンネル数が食い違うときの扱いは `muxed_output` と揃える
/// (モノラルは複製、多チャンネルからモノラルへは平均、足りないぶんは最後を流用)。
fn demux(
    interleaved: &[f32],
    channels_buffer: &mut [f32],
    source_channels: usize,
    destination_channels: usize,
    stride: usize,
    frames: usize,
) {
    assert!(source_channels > 0 && destination_channels > 0);
    assert!(interleaved.len() >= frames * source_channels);
    assert!(channels_buffer.len() >= (destination_channels - 1) * stride + frames);

    for destination in 0..destination_channels {
        let out = &mut channels_buffer[destination * stride..][..frames];

        if source_channels == 1 {
            // モノラルは全チャンネルへ複製する
            out.copy_from_slice(&interleaved[..frames]);
        } else if destination_channels == 1 {
            // 複数チャンネルはモノラルへ平均で落とす
            let scale = 1.0 / source_channels as f32;
            for (frame, sample) in out.iter_mut().enumerate() {
                let base = frame * source_channels;
                let total: f32 = interleaved[base..base + source_channels].iter().sum();
                *sample = total * scale;
            }
        } else {
            // 足りないチャンネルは最後のもので埋める
            let source = destination.min(source_channels - 1);
            for (frame, sample) in out.iter_mut().enumerate() {
                *sample = interleaved[frame * source_channels + source];
            }
        }
    }
}

/// 連続したチャンネルバッファ [l,l,l,r,r,r] を CPAL のインターリーブ形式 [l,r,l,r,l,r] に変換する
fn mux(channels_buffer: &[f32], output: &mut [f32], channel_count: usize) {
    assert!(channels_buffer.len() >= output.len());

    let single_channel_len = channels_buffer.len() / channel_count;
    for (muxed_index, output_sample) in output.iter_mut().enumerate() {
        let channel_number = muxed_index % channel_count;
        let channel_buffer_index = muxed_index / channel_count;
        let position = (channel_number * single_channel_len) + channel_buffer_index;

        *output_sample = channels_buffer[position]
    }
}

/// マルチチャンネルをモノラルにダウンミックスする
fn mix_mono(channels_buffer: &[f32], mono_output: &mut [f32], channel_count: usize) {
    assert!(channel_count > 0);
    assert!(channels_buffer.len() >= mono_output.len() * channel_count);

    let single_channel_len = channels_buffer.len() / channel_count;
    for (index, output_sample) in mono_output.iter_mut().enumerate() {
        let mut total = 0.0;
        for channel_number in 0..channel_count {
            let position = (channel_number * single_channel_len) + index;
            total += channels_buffer[position]
        }
        *output_sample = total / (channel_count as f32);
    }
}

/// モノラル入力を複数チャンネルに複製する
fn mono_to_multi(mono_input: &[f32], multi_output: &mut [f32], channel_count: usize) {
    assert!(channel_count > 0);

    for (output_samples, input_sample) in
        multi_output.chunks_exact_mut(channel_count).zip(mono_input)
    {
        output_samples.fill(*input_sample)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 2フレーム分を、確保長に余裕のあるバッファへ移す
    fn run(interleaved: &[f32], source: usize, destination: usize, stride: usize) -> Vec<f32> {
        let frames = interleaved.len() / source;
        let mut buffer = vec![0.0; stride * destination];
        demux(
            interleaved,
            &mut buffer,
            source,
            destination,
            stride,
            frames,
        );
        buffer
    }

    /// ステレオはチャンネルごとに固めて並ぶこと ([l,r,l,r] → [l,l,r,r])
    #[test]
    fn stereo_splits_into_contiguous_channels() {
        let out = run(&[1.0, -1.0, 2.0, -2.0, 3.0, -3.0], 2, 2, 3);
        assert_eq!(out, vec![1.0, 2.0, 3.0, -1.0, -2.0, -3.0]);
    }

    /// モノラルの音は全チャンネルへ複製されること。
    /// **落ちると、モノラル出力のデバイスでエフェクトに音が入らなくなる。**
    #[test]
    fn mono_source_is_copied_to_every_channel() {
        let out = run(&[1.0, 2.0], 1, 2, 2);
        assert_eq!(out, vec![1.0, 2.0, 1.0, 2.0]);
    }

    /// モノラル入力のプラグインへは平均で落とすこと
    #[test]
    fn stereo_is_averaged_down_to_mono() {
        let out = run(&[1.0, 3.0, -2.0, 0.0], 2, 1, 2);
        assert_eq!(out, vec![2.0, -1.0]);
    }

    /// 足りないチャンネルは最後のもので埋めること (`muxed_output` と揃える)
    #[test]
    fn missing_channels_reuse_the_last_source() {
        let out = run(&[1.0, -1.0], 2, 3, 1);
        assert_eq!(out, vec![1.0, -1.0, -1.0]);
    }

    /// 確保長より短いブロックでは、書いた先が残らないこと
    /// (前のブロックの尾が漏れると、エフェクトが古い音を拾う)
    #[test]
    fn only_the_current_frames_are_written() {
        // 確保長4に対して2フレームだけ書く
        let out = run(&[1.0, -1.0, 2.0, -2.0], 2, 2, 4);
        assert_eq!(out, vec![1.0, 2.0, 0.0, 0.0, -1.0, -2.0, 0.0, 0.0]);
    }
}
