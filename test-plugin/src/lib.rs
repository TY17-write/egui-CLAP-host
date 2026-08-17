//! テスト用の最小 CLAP プラグイン: 16ボイスのサイン波シンセ。
//! ボリュームパラメータを1つだけ持つ。
//! clack の polysynth サンプルを簡略化したもの。

use clack_extensions::{audio_ports::*, note_ports::*, params::*, state::*};
use clack_plugin::events::event_types::ParamValueEvent;
use clack_plugin::events::spaces::CoreEventSpace;
use clack_plugin::events::Match;
use clack_plugin::prelude::*;
use clack_plugin::stream::{InputStream, OutputStream};
use std::ffi::CStr;
use std::fmt::Write as _;
use std::sync::atomic::{AtomicU32, Ordering};

/// ボリュームパラメータの ID
const PARAM_VOLUME_ID: ClapId = ClapId::new(1);
/// ボリュームの初期値
const DEFAULT_VOLUME: f32 = 0.2;
/// 最大同時発音数
const VOICE_COUNT: usize = 16;

pub struct TestSinePlugin;

impl Plugin for TestSinePlugin {
    type AudioProcessor<'a> = TestSineAudioProcessor<'a>;
    type Shared<'a> = TestSineShared;
    type MainThread<'a> = TestSineMainThread<'a>;

    fn declare_extensions(builder: &mut PluginExtensions<Self>, _shared: Option<&TestSineShared>) {
        builder
            .register::<PluginAudioPorts>()
            .register::<PluginNotePorts>()
            .register::<PluginParams>()
            // ホスト側のプロジェクト保存を検証できるように state も持つ
            .register::<PluginState>();
    }
}

impl DefaultPluginFactory for TestSinePlugin {
    fn get_descriptor() -> PluginDescriptor {
        use clack_plugin::plugin::features::*;

        PluginDescriptor::new("com.example.test-sine", "Test Sine Synth").with_features([
            SYNTHESIZER,
            MONO,
            INSTRUMENT,
        ])
    }

    fn new_shared(_host: HostSharedHandle) -> Result<TestSineShared, PluginError> {
        Ok(TestSineShared {
            volume: AtomicF32::new(DEFAULT_VOLUME),
        })
    }

    fn new_main_thread<'a>(
        _host: HostMainThreadHandle<'a>,
        shared: &'a TestSineShared,
    ) -> Result<TestSineMainThread<'a>, PluginError> {
        Ok(TestSineMainThread { shared })
    }
}

/// スレッド間で共有されるデータ(パラメータ値)
pub struct TestSineShared {
    volume: AtomicF32,
}

impl TestSineShared {
    fn handle_param_event(&self, event: &ParamValueEvent) {
        if event.param_id() == Some(PARAM_VOLUME_ID) {
            self.volume
                .store((event.value() as f32).clamp(0.0, 1.0), Ordering::SeqCst);
        }
    }
}

impl PluginShared<'_> for TestSineShared {}

pub struct TestSineMainThread<'a> {
    shared: &'a TestSineShared,
}

impl<'a> PluginMainThread<'a, TestSineShared> for TestSineMainThread<'a> {}

/// 1ボイス分のサイン波オシレータ
#[derive(Copy, Clone)]
struct Voice {
    phase: f32,
    increment: f32,
    key: u8,
    channel: u8,
    note_id: Option<u32>,
    /// ノートオンで受け取ったベロシティ 0.0..=1.0
    velocity: f32,
}

impl Voice {
    fn new() -> Self {
        Self {
            phase: 0.0,
            increment: 0.0,
            key: 0,
            channel: 0,
            note_id: None,
            velocity: 1.0,
        }
    }

    fn start(
        &mut self,
        sample_rate: f32,
        channel: u8,
        key: u8,
        note_id: Option<u32>,
        velocity: f32,
    ) {
        let freq = 440.0 * f32::powf(2.0, (key as f32 - 69.0) / 12.0);
        self.phase = 0.0;
        self.increment = freq / sample_rate;
        self.key = key;
        self.channel = channel;
        self.note_id = note_id;
        self.velocity = velocity.clamp(0.0, 1.0);
    }

    fn matches(&self, channel: Match<u16>, key: Match<u16>, note_id: Match<u32>) -> bool {
        if !channel.matches(self.channel) || !key.matches(self.key) {
            return false;
        }
        note_id.matches(match self.note_id {
            None => Match::All,
            Some(id) => Match::Specific(id),
        })
    }

    /// バッファにサイン波を加算する。振幅は Volume パラメータ × ベロシティ。
    fn add_samples(&mut self, output: &mut [f32], volume: f32) {
        let amplitude = volume * self.velocity;
        for sample in output {
            *sample += (self.phase * std::f32::consts::TAU).sin() * amplitude;
            self.phase = (self.phase + self.increment).fract();
        }
    }
}

/// オーディオスレッドで動くプロセッサ
pub struct TestSineAudioProcessor<'a> {
    voices: [Voice; VOICE_COUNT],
    active_voices: usize,
    sample_rate: f32,
    shared: &'a TestSineShared,
}

impl<'a> PluginAudioProcessor<'a, TestSineShared, TestSineMainThread<'a>>
    for TestSineAudioProcessor<'a>
{
    fn activate(
        _host: HostAudioProcessorHandle<'a>,
        _main_thread: &TestSineMainThread,
        shared: &'a TestSineShared,
        audio_config: PluginAudioConfiguration,
    ) -> Result<Self, PluginError> {
        Ok(Self {
            voices: [Voice::new(); VOICE_COUNT],
            active_voices: 0,
            sample_rate: audio_config.sample_rate as f32,
            shared,
        })
    }

    fn process(
        &mut self,
        _process: Process,
        mut audio: Audio,
        events: Events,
    ) -> Result<ProcessStatus, PluginError> {
        let mut output_port = audio
            .output_port(0)
            .ok_or(PluginError::Message("No output port found"))?;

        let mut output_channels = output_port
            .channels()?
            .into_f32()
            .ok_or(PluginError::Message("Expected f32 output"))?;

        let output_buffer = output_channels
            .channel_mut(0)
            .ok_or(PluginError::Message("Expected at least one channel"))?;

        output_buffer.fill(0.0);

        // サンプル精度でイベントを処理しつつ波形を生成
        for event_batch in events.input.batch() {
            for event in event_batch.events() {
                self.handle_event(event);
            }

            let output_buffer = &mut output_buffer[event_batch.sample_bounds()];
            let volume = self.shared.volume.load(Ordering::SeqCst);
            for voice in &mut self.voices[..self.active_voices] {
                voice.add_samples(output_buffer, volume);
            }
        }

        // ホストがステレオ以上を要求してきた場合は全チャンネルに複製
        if output_channels.channel_count() > 1 {
            let (first, others) = output_channels.split_at_mut(1);
            let first = first.channel(0).unwrap();
            for other in others {
                other.copy_from_slice(first);
            }
        }

        if self.active_voices > 0 {
            Ok(ProcessStatus::Continue)
        } else {
            Ok(ProcessStatus::Sleep)
        }
    }

    fn stop_processing(&mut self) {
        self.active_voices = 0;
    }
}

impl TestSineAudioProcessor<'_> {
    fn handle_event(&mut self, event: &UnknownEvent) {
        match event.as_core_event() {
            Some(CoreEventSpace::NoteOn(event)) => {
                if let (Match::Specific(channel), Match::Specific(key)) =
                    (event.channel(), event.key())
                {
                    self.start_voice(
                        channel as u8,
                        key as u8,
                        event.note_id().into_specific(),
                        event.velocity() as f32,
                    );
                }
            }
            Some(CoreEventSpace::NoteOff(event)) => {
                self.stop_voices(event.channel(), event.key(), event.note_id());
            }
            // ホストが停止・シーク・シーケンス差し替えのときに送ってくる消音イベント。
            // 無視すると、鳴っている最中に停止したときボイスが残って鳴りっぱなしになる。
            // 本来はリリースを持たせず即座に切るものだが、このプラグインはエンベロープを
            // 持たないので NoteOff と同じ扱いでよい。
            Some(CoreEventSpace::NoteChoke(event)) => {
                self.stop_voices(event.channel(), event.key(), event.note_id());
            }
            Some(CoreEventSpace::ParamValue(event)) => {
                self.shared.handle_param_event(event);
            }
            _ => {}
        }
    }

    fn start_voice(&mut self, channel: u8, key: u8, note_id: Option<u32>, velocity: f32) {
        let Some(voice) = self.voices.get_mut(self.active_voices) else {
            return; // ボイスを使い切った
        };
        voice.start(self.sample_rate, channel, key, note_id, velocity);
        self.active_voices += 1;
    }

    fn stop_voices(&mut self, channel: Match<u16>, key: Match<u16>, note_id: Match<u32>) {
        while let Some(index) = self.voices[..self.active_voices]
            .iter()
            .position(|v| v.matches(channel, key, note_id))
        {
            self.voices.swap(index, self.active_voices - 1);
            self.active_voices -= 1;
        }
    }
}

/// 状態の先頭に置く目印。別形式のデータを読まされたときに弾くため。
const STATE_MAGIC: &[u8; 4] = b"TSS1";

impl PluginStateImpl for TestSineMainThread<'_> {
    /// 目印 + ボリューム (f32 リトルエンディアン) の8バイト
    fn save(&self, output: &mut OutputStream) -> Result<(), PluginError> {
        use std::io::Write;
        let volume = self.shared.volume.load(Ordering::SeqCst);
        output.write_all(STATE_MAGIC)?;
        output.write_all(&volume.to_le_bytes())?;
        Ok(())
    }

    fn load(&self, input: &mut InputStream) -> Result<(), PluginError> {
        use std::io::Read;
        let mut buffer = [0u8; 8];
        input.read_exact(&mut buffer)?;
        if &buffer[..4] != STATE_MAGIC {
            return Err(PluginError::Message("状態の形式が違います"));
        }
        let volume = f32::from_le_bytes([buffer[4], buffer[5], buffer[6], buffer[7]]);
        if !volume.is_finite() {
            return Err(PluginError::Message("ボリュームが数値ではありません"));
        }
        self.shared
            .volume
            .store(volume.clamp(0.0, 1.0), Ordering::SeqCst);
        Ok(())
    }
}

impl PluginAudioPortsImpl for TestSineMainThread<'_> {
    fn count(&self, is_input: bool) -> u32 {
        if is_input {
            0
        } else {
            1
        }
    }

    fn get(&self, index: u32, is_input: bool, writer: &mut AudioPortInfoWriter) {
        if !is_input && index == 0 {
            writer.set(&AudioPortInfo {
                id: ClapId::new(1),
                name: b"main",
                channel_count: 1,
                flags: AudioPortFlags::IS_MAIN,
                port_type: Some(AudioPortType::MONO),
                in_place_pair: None,
            });
        }
    }
}

impl PluginNotePortsImpl for TestSineMainThread<'_> {
    fn count(&self, is_input: bool) -> u32 {
        if is_input {
            1
        } else {
            0
        }
    }

    fn get(&self, index: u32, is_input: bool, writer: &mut NotePortInfoWriter) {
        if is_input && index == 0 {
            writer.set(&NotePortInfo {
                id: ClapId::new(1),
                name: b"main",
                preferred_dialect: Some(NoteDialect::Clap),
                supported_dialects: NoteDialects::CLAP,
            });
        }
    }
}

impl PluginMainThreadParams for TestSineMainThread<'_> {
    fn count(&self) -> u32 {
        1
    }

    fn get_info(&self, param_index: u32, info: &mut ParamInfoWriter) {
        if param_index == 0 {
            info.set(&ParamInfo {
                id: PARAM_VOLUME_ID,
                flags: ParamInfoFlags::IS_AUTOMATABLE,
                cookie: Default::default(),
                name: b"Volume",
                module: b"",
                min_value: 0.0,
                max_value: 1.0,
                default_value: DEFAULT_VOLUME as f64,
            });
        }
    }

    fn get_value(&self, param_id: ClapId) -> Option<f64> {
        if param_id == PARAM_VOLUME_ID {
            Some(self.shared.volume.load(Ordering::SeqCst) as f64)
        } else {
            None
        }
    }

    fn value_to_text(
        &self,
        param_id: ClapId,
        value: f64,
        writer: &mut ParamDisplayWriter,
    ) -> std::fmt::Result {
        if param_id == PARAM_VOLUME_ID {
            write!(writer, "{:.0} %", value * 100.0)
        } else {
            Err(std::fmt::Error)
        }
    }

    fn text_to_value(&self, param_id: ClapId, text: &CStr) -> Option<f64> {
        if param_id != PARAM_VOLUME_ID {
            return None;
        }
        let text = text.to_str().ok()?;
        let text = text.strip_suffix('%').unwrap_or(text).trim();
        Some(text.parse::<f64>().ok()? / 100.0)
    }

    fn flush(
        &self,
        input_parameter_changes: &InputEvents,
        _output_parameter_changes: &mut OutputEvents,
    ) {
        for event in input_parameter_changes {
            if let Some(CoreEventSpace::ParamValue(event)) = event.as_core_event() {
                self.shared.handle_param_event(event);
            }
        }
    }
}

impl PluginAudioProcessorParams for TestSineAudioProcessor<'_> {
    fn flush(
        &mut self,
        input_parameter_changes: &InputEvents,
        _output_parameter_changes: &mut OutputEvents,
    ) {
        for event in input_parameter_changes {
            self.handle_event(event);
        }
    }
}

/// f32 をアトミックに読み書きするためのヘルパー
struct AtomicF32(AtomicU32);

impl AtomicF32 {
    fn new(value: f32) -> Self {
        Self(AtomicU32::new(value.to_bits()))
    }

    fn store(&self, value: f32, order: Ordering) {
        self.0.store(value.to_bits(), order)
    }

    fn load(&self, order: Ordering) -> f32 {
        f32::from_bits(self.0.load(order))
    }
}

clack_export_entry!(SinglePluginEntry<TestSinePlugin>);
