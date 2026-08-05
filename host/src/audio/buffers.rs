//! オーディオバッファの管理と CPAL バッファへの書き出し。
//! clack リポジトリの cpal サンプル (MIT OR Apache-2.0) をほぼそのまま利用。

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

    /// プラグインに渡す入出力バッファを準備する
    pub fn prepare_plugin_buffers(
        &mut self,
        cpal_buf_len: usize,
    ) -> (InputAudioBuffers<'_>, OutputAudioBuffers<'_>) {
        let sample_count = self.cpal_buf_len_to_frame_count(cpal_buf_len);
        assert!(sample_count <= self.actual_frame_count);

        self.output_port_channels
            .iter_mut()
            .for_each(|b| b.fill(0.0));
        self.input_port_channels
            .iter_mut()
            .for_each(|b| b.fill(0.0));

        (
            self.input_ports
                .with_input_buffers(self.input_port_channels.iter_mut().map(|port_buf| {
                    AudioPortBuffer {
                        latency: 0,
                        channels: AudioPortBufferType::f32_input_only(
                            port_buf
                                .chunks_exact_mut(self.actual_frame_count)
                                .map(|buffer| InputChannel {
                                    buffer: &mut buffer[..sample_count],
                                    is_constant: true,
                                }),
                        ),
                    }
                })),
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

    /// プラグイン出力をミックス用バッファに**加算**する。
    /// トラックごとの出力を1つの出力にまとめるために使う。
    pub fn mix_into(&mut self, accumulator: &mut [f32]) {
        let muxed = self.muxed_output(accumulator.len());
        for (out, muxed) in accumulator.iter_mut().zip(muxed) {
            *out += *muxed;
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
