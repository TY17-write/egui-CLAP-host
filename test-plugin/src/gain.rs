//! テスト用の最小 CLAP エフェクト: 入力に係数を掛けて直流を足すだけ。
//!
//! ```text
//! out = in * gain + offset
//! ```
//!
//! **ルーティングを検証するための治具**であって、音として使うものではない。
//! 「音が通ったか」「何段通ったか」「どの順で通ったか」を**数値で確かめる**ために
//! いちばん単純な形にしてある。
//!
//! 実物のエフェクトで代用しない理由は、落ちたときに**こちらの問題か相手の問題か
//! 切り分けられない**ため。
//!
//! # なぜこの2つのパラメータなのか
//!
//! - **`gain` の既定を 0.5 にしてある。** 等倍にすると「効いた」と「素通しだった」
//!   が区別できず、**バイパスの検証にならない**
//! - **`offset` があるのは順序を見るため。** 掛け算だけだと前後を入れ替えても
//!   結果が同じで、チェーンの順が守られているか分からない。
//!   `(x*g)+o` と `(x+o)*g` は一致しないので、**2段の順序を出力から言い当てられる**
//! - **入力が無音でも `offset` のぶんは出る。** 音源を載せずにエフェクトだけの
//!   トラック (センド先) を試せる

use crate::AtomicF32;
use clack_extensions::{audio_ports::*, params::*, state::*};
use clack_plugin::events::event_types::ParamValueEvent;
use clack_plugin::events::spaces::CoreEventSpace;
use clack_plugin::prelude::*;
use clack_plugin::stream::{InputStream, OutputStream};
use std::ffi::CStr;
use std::fmt::Write as _;
use std::sync::atomic::Ordering;

/// 係数パラメータの ID
const PARAM_GAIN_ID: ClapId = ClapId::new(1);
/// 直流パラメータの ID
const PARAM_OFFSET_ID: ClapId = ClapId::new(2);

/// 係数の初期値。**等倍にしない** (素通しと区別できなくなるため)
const DEFAULT_GAIN: f32 = 0.5;
/// 係数の範囲。1.0 を挟むように取ってある
const MAX_GAIN: f32 = 2.0;

/// 直流の初期値。**足さないのが既定** (音を素直に通したいときのため)
const DEFAULT_OFFSET: f32 = 0.0;
/// 直流の範囲
const MAX_OFFSET: f32 = 1.0;

pub struct TestGainPlugin;

impl Plugin for TestGainPlugin {
    type AudioProcessor<'a> = TestGainAudioProcessor<'a>;
    type Shared<'a> = TestGainShared;
    type MainThread<'a> = TestGainMainThread<'a>;

    fn declare_extensions(builder: &mut PluginExtensions<Self>, _shared: Option<&TestGainShared>) {
        builder
            .register::<PluginAudioPorts>()
            .register::<PluginParams>()
            // 音源と同じく、プロジェクト保存の検証に使えるように持たせる
            .register::<PluginState>();
    }
}

impl DefaultPluginFactory for TestGainPlugin {
    fn get_descriptor() -> PluginDescriptor {
        use clack_plugin::plugin::features::*;

        PluginDescriptor::new("com.example.test-gain", "Test Gain Effect").with_features([
            AUDIO_EFFECT,
            MONO,
            UTILITY,
        ])
    }

    fn new_shared(_host: HostSharedHandle) -> Result<TestGainShared, PluginError> {
        Ok(TestGainShared {
            gain: AtomicF32::new(DEFAULT_GAIN),
            offset: AtomicF32::new(DEFAULT_OFFSET),
        })
    }

    fn new_main_thread<'a>(
        _host: HostMainThreadHandle<'a>,
        shared: &'a TestGainShared,
    ) -> Result<TestGainMainThread<'a>, PluginError> {
        Ok(TestGainMainThread { shared })
    }
}

/// スレッド間で共有されるデータ (パラメータ値)
pub struct TestGainShared {
    gain: AtomicF32,
    offset: AtomicF32,
}

impl TestGainShared {
    fn handle_param_event(&self, event: &ParamValueEvent) {
        let value = event.value() as f32;
        match event.param_id() {
            Some(PARAM_GAIN_ID) => self
                .gain
                .store(value.clamp(0.0, MAX_GAIN), Ordering::SeqCst),
            Some(PARAM_OFFSET_ID) => self
                .offset
                .store(value.clamp(-MAX_OFFSET, MAX_OFFSET), Ordering::SeqCst),
            _ => {}
        }
    }

    /// このブロックで使う係数と直流
    fn coefficients(&self) -> (f32, f32) {
        (
            self.gain.load(Ordering::SeqCst),
            self.offset.load(Ordering::SeqCst),
        )
    }
}

impl PluginShared<'_> for TestGainShared {}

pub struct TestGainMainThread<'a> {
    shared: &'a TestGainShared,
}

impl<'a> PluginMainThread<'a, TestGainShared> for TestGainMainThread<'a> {}

/// オーディオスレッドで動くプロセッサ。持ち回る状態は無い
pub struct TestGainAudioProcessor<'a> {
    shared: &'a TestGainShared,
}

impl<'a> PluginAudioProcessor<'a, TestGainShared, TestGainMainThread<'a>>
    for TestGainAudioProcessor<'a>
{
    fn activate(
        _host: HostAudioProcessorHandle<'a>,
        _main_thread: &TestGainMainThread,
        shared: &'a TestGainShared,
        _audio_config: PluginAudioConfiguration,
    ) -> Result<Self, PluginError> {
        Ok(Self { shared })
    }

    fn process(
        &mut self,
        _process: Process,
        mut audio: Audio,
        events: Events,
    ) -> Result<ProcessStatus, PluginError> {
        let mut port_pair = audio
            .port_pair(0)
            .ok_or(PluginError::Message("No input/output port pair found"))?;

        let mut channels = port_pair
            .channels()?
            .into_f32()
            .ok_or(PluginError::Message("Expected f32 input/output"))?;

        // サンプル精度でイベントを処理しつつ加工する
        for event_batch in events.input.batch() {
            for event in event_batch.events() {
                if let Some(CoreEventSpace::ParamValue(event)) = event.as_core_event() {
                    self.shared.handle_param_event(event);
                }
            }

            let (gain, offset) = self.shared.coefficients();
            let range = event_batch.sample_bounds();

            for pair in channels.iter_mut() {
                match pair {
                    // ホストが入出力に別のバッファを渡してきた場合
                    ChannelPair::InputOutput(input, output) => {
                        let output = &mut output[range];
                        let input = &input[range];
                        for (out, inp) in output.iter_mut().zip(input) {
                            *out = *inp * gain + offset;
                        }
                    }
                    // 同じバッファを入出力に使う場合
                    ChannelPair::InPlace(buffer) => {
                        for sample in &mut buffer[range] {
                            *sample = *sample * gain + offset;
                        }
                    }
                    // 入力が繋がっていない。直流だけ出す
                    // (センド先として、音源なしで鳴らせるかを見るため)
                    ChannelPair::OutputOnly(output) => {
                        output[range].fill(offset);
                    }
                    // 出力が無いので書く先がない
                    ChannelPair::InputOnly(_) => {}
                }
            }
        }

        // **無音でも寝かせない。** `offset` を足す以上、入力が無音でも
        // 出力は無音とは限らない
        Ok(ProcessStatus::Continue)
    }
}

/// 状態の先頭に置く目印。別形式のデータを読まされたときに弾くため。
const STATE_MAGIC: &[u8; 4] = b"TGN1";

impl PluginStateImpl for TestGainMainThread<'_> {
    /// 目印 + 係数 + 直流 (どちらも f32 リトルエンディアン) の12バイト
    fn save(&self, output: &mut OutputStream) -> Result<(), PluginError> {
        use std::io::Write;
        let (gain, offset) = self.shared.coefficients();
        output.write_all(STATE_MAGIC)?;
        output.write_all(&gain.to_le_bytes())?;
        output.write_all(&offset.to_le_bytes())?;
        Ok(())
    }

    fn load(&self, input: &mut InputStream) -> Result<(), PluginError> {
        use std::io::Read;
        let mut buffer = [0u8; 12];
        input.read_exact(&mut buffer)?;
        if &buffer[..4] != STATE_MAGIC {
            return Err(PluginError::Message("状態の形式が違います"));
        }
        let gain = f32::from_le_bytes([buffer[4], buffer[5], buffer[6], buffer[7]]);
        let offset = f32::from_le_bytes([buffer[8], buffer[9], buffer[10], buffer[11]]);
        if !gain.is_finite() || !offset.is_finite() {
            return Err(PluginError::Message("係数が数値ではありません"));
        }
        self.shared
            .gain
            .store(gain.clamp(0.0, MAX_GAIN), Ordering::SeqCst);
        self.shared
            .offset
            .store(offset.clamp(-MAX_OFFSET, MAX_OFFSET), Ordering::SeqCst);
        Ok(())
    }
}

impl PluginAudioPortsImpl for TestGainMainThread<'_> {
    /// **入力・出力を1つずつ持つ。** 音源 (入力0) との違いはここで、
    /// ホストがエフェクトとして扱えるかどうかもここで決まる。
    fn count(&self, _is_input: bool) -> u32 {
        1
    }

    fn get(&self, index: u32, _is_input: bool, writer: &mut AudioPortInfoWriter) {
        if index == 0 {
            writer.set(&AudioPortInfo {
                id: ClapId::new(1),
                name: b"main",
                // 音源と揃えてモノラル。チェーンに繋いだときの計算が単純になる
                channel_count: 1,
                flags: AudioPortFlags::IS_MAIN,
                port_type: Some(AudioPortType::MONO),
                in_place_pair: None,
            });
        }
    }
}

impl PluginMainThreadParams for TestGainMainThread<'_> {
    fn count(&self) -> u32 {
        2
    }

    fn get_info(&self, param_index: u32, info: &mut ParamInfoWriter) {
        let (id, name, min, max, default) = match param_index {
            0 => (
                PARAM_GAIN_ID,
                b"Gain".as_slice(),
                0.0,
                MAX_GAIN as f64,
                DEFAULT_GAIN as f64,
            ),
            1 => (
                PARAM_OFFSET_ID,
                b"Offset".as_slice(),
                -MAX_OFFSET as f64,
                MAX_OFFSET as f64,
                DEFAULT_OFFSET as f64,
            ),
            _ => return,
        };

        info.set(&ParamInfo {
            id,
            flags: ParamInfoFlags::IS_AUTOMATABLE,
            cookie: Default::default(),
            name,
            module: b"",
            min_value: min,
            max_value: max,
            default_value: default,
        });
    }

    fn get_value(&self, param_id: ClapId) -> Option<f64> {
        let (gain, offset) = self.shared.coefficients();
        match param_id {
            PARAM_GAIN_ID => Some(gain as f64),
            PARAM_OFFSET_ID => Some(offset as f64),
            _ => None,
        }
    }

    fn value_to_text(
        &self,
        param_id: ClapId,
        value: f64,
        writer: &mut ParamDisplayWriter,
    ) -> std::fmt::Result {
        match param_id {
            PARAM_GAIN_ID => write!(writer, "x{value:.2}"),
            PARAM_OFFSET_ID => write!(writer, "{value:+.3}"),
            _ => Err(std::fmt::Error),
        }
    }

    fn text_to_value(&self, param_id: ClapId, text: &CStr) -> Option<f64> {
        let text = text.to_str().ok()?.trim();
        match param_id {
            PARAM_GAIN_ID => text.strip_prefix('x').unwrap_or(text).trim().parse().ok(),
            PARAM_OFFSET_ID => text.parse().ok(),
            _ => None,
        }
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

impl PluginAudioProcessorParams for TestGainAudioProcessor<'_> {
    fn flush(
        &mut self,
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
