//! オーディオストリームとプラグインポート構成のネゴシエーション。
//! clack リポジトリの cpal サンプル (MIT OR Apache-2.0) をほぼそのまま利用。

use crate::host::MiniHost;
use clack_extensions::audio_ports::{
    AudioPortFlags, AudioPortInfoBuffer, AudioPortType, PluginAudioPorts,
};
use clack_host::prelude::{
    ClapId, PluginAudioConfiguration, PluginInstance, PluginMainThreadHandle,
};
use cpal::traits::DeviceTrait;
use cpal::{
    BufferSize, Device, SampleFormat, SampleRate, StreamConfig, SupportedBufferSize,
    SupportedStreamConfigRange,
};
use std::cmp::Ordering;
use std::error::Error;
use std::fmt::{Display, Formatter};

/// CPAL ストリームと CLAP プラグインバッファの両方を構成するための完全な設定
pub struct FullAudioConfig {
    pub plugin_input_port_config: PluginAudioPortsConfig,
    pub plugin_output_port_config: PluginAudioPortsConfig,
    /// CPAL 出力のチャンネル数 (1 か 2 のみ対応)
    pub output_channel_count: usize,
    pub min_buffer_size: u32,
    /// CPAL が一度に処理するバッファの「おそらく最大の」サイズ。
    /// 稀にこれを超えるサイズが来ることがある。
    pub max_likely_buffer_size: u32,
    pub sample_rate: u32,
    pub sample_format: SampleFormat,
}

/// ストリーム側だけの構成 (プラグインに依存しない部分)。
///
/// トラックごとに音源を持つと、1本のストリームを全プラグインで共有するため、
/// サンプルレートやバッファ長はプラグインより先に決めておく必要がある。
#[derive(Clone, Copy, Debug)]
pub struct StreamAudioConfig {
    /// CPAL 出力のチャンネル数 (1 か 2 のみ対応)
    pub output_channel_count: usize,
    pub min_buffer_size: u32,
    pub max_likely_buffer_size: u32,
    pub sample_rate: u32,
    pub sample_format: SampleFormat,
}

impl StreamAudioConfig {
    /// デバイスが対応する中で最良の構成を選ぶ (プラグインは見ない)
    pub fn find_best(device: &Device) -> Result<Self, Box<dyn Error>> {
        let configs = list_device_configs_ordered(device)?;
        let best = configs
            .first()
            .ok_or("使えるオーディオ出力構成が見つかりません")?;

        // 44.1kHz 以上で最小のサンプルレートを選ぶ (対応範囲に収める)
        let sample_rate = best
            .min_sample_rate()
            .max(SampleRate(44_100))
            .min(best.max_sample_rate());
        let config = best.with_sample_rate(sample_rate);
        let (min_buffer_size, max_likely_buffer_size) = buffer_size_range(&config);

        Ok(Self {
            output_channel_count: config.channels() as usize,
            min_buffer_size,
            max_likely_buffer_size,
            sample_rate: sample_rate.0,
            sample_format: config.sample_format(),
        })
    }

    pub fn as_cpal_stream_config(&self) -> StreamConfig {
        StreamConfig {
            channels: self.output_channel_count as u16,
            buffer_size: BufferSize::Fixed(self.max_likely_buffer_size),
            sample_rate: SampleRate(self.sample_rate),
        }
    }

    pub fn as_clack_plugin_config(&self) -> PluginAudioConfiguration {
        PluginAudioConfiguration {
            sample_rate: self.sample_rate as f64,
            min_frames_count: self.min_buffer_size,
            max_frames_count: self.max_likely_buffer_size,
        }
    }
}

impl Display for StreamAudioConfig {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} channels at {:.1}kHz, buffer {}-{}",
            self.output_channel_count,
            self.sample_rate as f64 / 1_000.0,
            self.min_buffer_size,
            self.max_likely_buffer_size,
        )
    }
}

impl FullAudioConfig {
    /// 決まっているストリーム構成に、プラグインのポート構成を組み合わせる。
    /// 出力チャンネル数が食い違う場合はバッファ側で変換する (モノ↔ステレオ)。
    pub fn for_plugin(stream: &StreamAudioConfig, instance: &mut PluginInstance<MiniHost>) -> Self {
        Self {
            plugin_input_port_config: get_config_from_ports(&mut instance.plugin_handle(), true),
            plugin_output_port_config: get_config_from_ports(&mut instance.plugin_handle(), false),
            output_channel_count: stream.output_channel_count,
            min_buffer_size: stream.min_buffer_size,
            max_likely_buffer_size: stream.max_likely_buffer_size,
            sample_rate: stream.sample_rate,
            sample_format: stream.sample_format,
        }
    }

    /// このプラグインが音を返すか。
    ///
    /// **返さないものはチェーンで素通しにする。** モニタリング系
    /// (アナライザ・チューナー・メーター) は出力を持たないので、書かせた
    /// バッファをそのまま採ると、そこから後ろが無音になる。
    pub fn produces_audio(&self) -> bool {
        self.plugin_output_port_config.main_channel_count() > 0
    }

    /// CPAL デバイスとプラグインの両方に合う最良の構成を探す
    pub fn find_best_from(
        device: &Device,
        instance: &mut PluginInstance<MiniHost>,
    ) -> Result<Self, Box<dyn Error>> {
        let best_cpal_configs = list_device_configs_ordered(device)?;

        let input_ports = get_config_from_ports(&mut instance.plugin_handle(), true);
        let output_ports = get_config_from_ports(&mut instance.plugin_handle(), false);

        Ok(find_matching_output_config(
            &best_cpal_configs,
            output_ports,
            input_ports,
        ))
    }

    pub fn as_cpal_stream_config(&self) -> StreamConfig {
        StreamConfig {
            channels: self.output_channel_count as u16,
            buffer_size: BufferSize::Fixed(self.max_likely_buffer_size),
            sample_rate: SampleRate(self.sample_rate),
        }
    }

    pub fn as_clack_plugin_config(&self) -> PluginAudioConfiguration {
        PluginAudioConfiguration {
            sample_rate: self.sample_rate as f64,
            min_frames_count: self.min_buffer_size,
            max_frames_count: self.max_likely_buffer_size,
        }
    }
}

impl Display for FullAudioConfig {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} channels at {:.1}kHz, buffer {}-{}, main port \"{}\" ({})",
            self.output_channel_count,
            self.sample_rate as f64 / 1_000.0,
            self.min_buffer_size,
            self.max_likely_buffer_size,
            self.plugin_output_port_config.main_port().name,
            self.plugin_output_port_config.main_port().port_layout
        )
    }
}

/// プラグインの入力または出力ポート一式の構成
#[derive(Clone, Debug)]
pub struct PluginAudioPortsConfig {
    pub ports: Vec<PluginAudioPortInfo>,
    pub main_port_index: u32,
    /// **出力ポートを1つも持たないと申告されたか。**
    ///
    /// モニタリング系のプラグイン (アナライザ・チューナー・メーター) がこれに当たる。
    /// バッファの器が無いと組み立てが通らないので `ports` には既定のステレオを
    /// 入れてあり、**「音を返さない」という事実だけをここに残す**。
    ///
    /// **ポート拡張そのものを持たないプラグインは含まない。** 申告しないだけで
    /// 音を書くものがあり、それを素通しにすると鳴らなくなる。
    pub no_audio_output: bool,
}

impl PluginAudioPortsConfig {
    fn empty() -> Self {
        PluginAudioPortsConfig {
            main_port_index: 0,
            ports: vec![],
            no_audio_output: false,
        }
    }

    /// ポート拡張未実装のプラグイン向けのデフォルト構成
    fn default() -> Self {
        PluginAudioPortsConfig {
            main_port_index: 0,
            ports: vec![PluginAudioPortInfo {
                _id: None,
                port_layout: AudioPortLayout::Stereo,
                name: "Default".into(),
            }],
            no_audio_output: false,
        }
    }

    /// 音を返さないプラグイン向けの構成 (器は既定のステレオ)
    fn silent() -> Self {
        PluginAudioPortsConfig {
            no_audio_output: true,
            ..Self::default()
        }
    }

    pub fn main_port(&self) -> &PluginAudioPortInfo {
        &self.ports[self.main_port_index as usize]
    }

    /// メインポートのチャンネル数。**音を返さないと申告されていれば 0**。
    ///
    /// 画面の「2→0ch」表示と、素通しにするかの判定がここを見る。
    pub fn main_channel_count(&self) -> u16 {
        if self.no_audio_output {
            return 0;
        }
        self.ports
            .get(self.main_port_index as usize)
            .map_or(0, |port| port.port_layout.channel_count())
    }

    pub fn total_channel_count(&self) -> usize {
        self.ports
            .iter()
            .map(|p| p.port_layout.channel_count() as usize)
            .sum()
    }
}

#[derive(Clone, Debug)]
pub struct PluginAudioPortInfo {
    pub _id: Option<ClapId>,
    pub port_layout: AudioPortLayout,
    pub name: String,
}

/// ポートのチャンネルレイアウト
#[derive(Eq, PartialEq, Copy, Clone, Debug)]
pub enum AudioPortLayout {
    Mono,
    Stereo,
    Unsupported { channel_count: u16 },
}

impl AudioPortLayout {
    pub fn channel_count(&self) -> u16 {
        match self {
            AudioPortLayout::Mono => 1,
            AudioPortLayout::Stereo => 2,
            AudioPortLayout::Unsupported { channel_count } => *channel_count,
        }
    }
}

impl Display for AudioPortLayout {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            AudioPortLayout::Mono => f.write_str("mono"),
            AudioPortLayout::Stereo => f.write_str("stereo"),
            AudioPortLayout::Unsupported { channel_count } => write!(f, "{channel_count}-channels"),
        }
    }
}

/// 構成から使えるバッファ長の範囲を求める。
/// CPAL は最小バッファサイズ 0 を報告することがあるので補正する。
fn buffer_size_range(config: &cpal::SupportedStreamConfig) -> (u32, u32) {
    match config.buffer_size() {
        SupportedBufferSize::Range { min, max } => ((*min).max(1), 1024.clamp(*min, *max)),
        SupportedBufferSize::Unknown => (1, 1024),
    }
}

/// デバイスがサポートする出力構成を良い順に並べて返す
fn list_device_configs_ordered(
    device: &Device,
) -> Result<Vec<SupportedStreamConfigRange>, Box<dyn Error>> {
    let mut output_configs: Vec<_> = device
        .supported_output_configs()?
        .filter(is_device_config_supported)
        .collect();

    output_configs.sort_by(|a, b| compare_devices_configs(a, b).reverse());

    Ok(output_configs)
}

fn is_device_config_supported(config: &SupportedStreamConfigRange) -> bool {
    // **3ch 以上も受け入れる。** 扱うのはステレオまでだが、先頭2本へ出せば
    // 鳴らせる (`graph::write_to_device`)。以前はここで弾いていたので、
    // マルチチャンネルしか持たないデバイスでは起動できなかった
    if config.channels() < 1 {
        return false;
    }
    if config.max_sample_rate().0 < 44_100 {
        return false;
    }
    if sample_type_preference(config.sample_format()) == u8::MAX {
        return false;
    }
    true
}

fn compare_devices_configs(
    first: &SupportedStreamConfigRange,
    second: &SupportedStreamConfigRange,
) -> Ordering {
    // **ステレオ優先。** 次に3ch 以上 (先頭2本へ出せる)、最後にモノラル。
    //
    // 単純にチャンネル数の多い順にすると、2ch も選べる環境で 7.1 を開いてしまう。
    // 扱うのはステレオまでなので、開くだけ無駄になる。
    match stereo_preference(second.channels()).cmp(&stereo_preference(first.channels())) {
        o @ (Ordering::Less | Ordering::Greater) => return o,
        Ordering::Equal => {}
    }

    // 同じ順位なら、チャンネル数の少ないほうを選ぶ (余計な口を開けない)
    match first.channels().cmp(&second.channels()) {
        o @ (Ordering::Less | Ordering::Greater) => return o.reverse(),
        Ordering::Equal => {}
    }

    // サンプル形式のスコアが低いものを優先
    match sample_type_preference(first.sample_format())
        .cmp(&sample_type_preference(second.sample_format()))
    {
        o @ (Ordering::Less | Ordering::Greater) => return o.reverse(),
        Ordering::Equal => {}
    }

    // 44.1kHz 以上の中で最小のサンプルレートを優先
    match first.min_sample_rate().cmp(&second.min_sample_rate()) {
        o @ (Ordering::Less | Ordering::Greater) => return o.reverse(),
        Ordering::Equal => {}
    }
    first.cmp_default_heuristics(second)
}

/// チャンネル数の望ましさ (**大きいほど良い**)。
///
/// ステレオがいちばん素直。3ch 以上は先頭2本へ出せば鳴らせるので次点。
/// モノラルは鳴らせるが**ステレオ感が消える**ので最後。
fn stereo_preference(channels: u16) -> u8 {
    match channels {
        2 => 2,
        n if n > 2 => 1,
        _ => 0,
    }
}

/// サンプル形式の優先度 (低いほど良い)
fn sample_type_preference(sample_type: SampleFormat) -> u8 {
    match sample_type {
        SampleFormat::F32 => 0,
        SampleFormat::F64 => 1,
        SampleFormat::I64 => 2,
        SampleFormat::U64 => 3,
        SampleFormat::I32 => 4,
        SampleFormat::U32 => 5,
        SampleFormat::I16 => 6,
        SampleFormat::U16 => 7,
        SampleFormat::I8 => 8,
        SampleFormat::U8 => 9,
        _ => u8::MAX,
    }
}

/// Audio Ports 拡張からプラグインのポート構成を取得する
pub fn get_config_from_ports(
    plugin: &mut PluginMainThreadHandle,
    is_input: bool,
) -> PluginAudioPortsConfig {
    let Some(ports) = plugin.get_extension::<PluginAudioPorts>() else {
        return PluginAudioPortsConfig::default();
    };

    let mut buffer = AudioPortInfoBuffer::new();
    let mut main_port_index = None;
    let mut discovered_ports = vec![];

    for i in 0..ports.count(plugin, is_input) {
        let Some(info) = ports.get(plugin, i, is_input, &mut buffer) else {
            continue;
        };
        let port_type = info
            .port_type
            .or_else(|| AudioPortType::from_channel_count(info.channel_count));

        let port_layout = match port_type {
            Some(l) if l == AudioPortType::MONO => AudioPortLayout::Mono,
            Some(l) if l == AudioPortType::STEREO => AudioPortLayout::Stereo,
            _ => AudioPortLayout::Unsupported {
                channel_count: info.channel_count as u16,
            },
        };

        if info.flags.contains(AudioPortFlags::IS_MAIN) && main_port_index.replace(i).is_some() {
            eprintln!("警告: プラグインが複数のメインポートを定義しています");
        }

        discovered_ports.push(PluginAudioPortInfo {
            _id: Some(info.id),
            port_layout,
            name: String::from_utf8_lossy(info.name).into_owned(),
        })
    }

    if discovered_ports.is_empty() {
        if is_input {
            return PluginAudioPortsConfig::empty();
        }
        // 出力ポートが無いと**申告している**。モニタリング系がこれに当たるので、
        // 音を返さないものとして扱う (チェーンでは素通しになる)
        return PluginAudioPortsConfig::silent();
    }

    let main_port_index = if let Some(main_port_index) = main_port_index {
        main_port_index
    } else {
        // メインポートが未定義なら、最初のステレオ→モノラルの順で代用する
        if let Some(first_stereo_port) = discovered_ports
            .iter()
            .enumerate()
            .find(|(_, p)| p.port_layout == AudioPortLayout::Stereo)
        {
            first_stereo_port.0 as u32
        } else if let Some(first_mono_port) = discovered_ports
            .iter()
            .enumerate()
            .find(|(_, p)| p.port_layout == AudioPortLayout::Mono)
        {
            first_mono_port.0 as u32
        } else {
            0
        }
    };

    PluginAudioPortsConfig {
        main_port_index,
        ports: discovered_ports,
        // ポートを申告しているので、音を返すかはチャンネル数で決まる
        no_audio_output: false,
    }
}

/// プラグインのポート構成に一番合う CPAL 構成を選ぶ
fn find_matching_output_config(
    ordered_stream_configs: &[SupportedStreamConfigRange],
    plugin_output_port_config: PluginAudioPortsConfig,
    plugin_input_port_config: PluginAudioPortsConfig,
) -> FullAudioConfig {
    let plugin_channel_count = plugin_output_port_config
        .main_port()
        .port_layout
        .channel_count();

    let matching_config = ordered_stream_configs
        .iter()
        .find(|c| c.channels() == plugin_channel_count);

    let best_stream_config = matching_config
        .or_else(|| ordered_stream_configs.first())
        .expect("出力デバイスがサポートする構成がありません");

    let (min_buffer_size, max_buffer_size) = match best_stream_config.buffer_size() {
        SupportedBufferSize::Range { min, max } => (*min, 1024.clamp(*min, *max)),
        SupportedBufferSize::Unknown => (1, 1024),
    };

    FullAudioConfig {
        output_channel_count: best_stream_config.channels() as usize,
        // CPAL は最小バッファサイズ 0 を報告することがあるので補正
        min_buffer_size: min_buffer_size.max(1),
        max_likely_buffer_size: max_buffer_size,
        sample_rate: 44_100.clamp(
            best_stream_config.min_sample_rate().0,
            best_stream_config.max_sample_rate().0,
        ),
        plugin_output_port_config,
        plugin_input_port_config,
        sample_format: best_stream_config.sample_format(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **ステレオがいちばん望ましいこと。**
    ///
    /// 単純にチャンネル数の多い順にすると、2ch も選べる環境で 7.1 を開いてしまう。
    /// 扱うのはステレオまでなので、開くだけ無駄になる。
    #[test]
    fn stereo_is_preferred_over_multichannel_and_mono() {
        assert!(stereo_preference(2) > stereo_preference(6));
        assert!(stereo_preference(6) > stereo_preference(1));
    }

    /// 3ch 以上どうしは同じ順位 (あとはチャンネル数の少ないほうを選ぶ)
    #[test]
    fn multichannel_configs_rank_the_same() {
        assert_eq!(stereo_preference(3), stereo_preference(8));
    }
}
