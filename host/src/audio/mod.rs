//! プラグインのアクティベートと CPAL ストリームへの接続。
//! GUI からのノート/パラメータイベントはリングバッファ経由でオーディオスレッドに渡す。

use crate::host::MiniHost;
use clack_extensions::note_ports::{NoteDialects, NotePortInfoBuffer, PluginNotePorts};
use clack_host::events::event_types::{MidiEvent, NoteOffEvent, NoteOnEvent, ParamValueEvent};
use clack_host::events::{EventFlags, Match};
use clack_host::prelude::*;
use clack_host::utils::Cookie;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{
    BuildStreamError, Device, FromSample, OutputCallbackInfo, SampleFormat, Stream, StreamConfig,
};
use rtrb::Consumer;
use std::error::Error;

pub mod buffers;
pub mod config;
pub mod transport;

use buffers::*;
use config::*;
use transport::{Transport, TransportMsg, TransportShared};

/// GUI スレッドからオーディオスレッドへ送るメッセージ
pub enum GuiMsg {
    NoteOn { key: u16, velocity: f64 },
    NoteOff { key: u16 },
    ParamValue { id: ClapId, value: f64 },
    Transport(TransportMsg),
}

/// プラグインをアクティベートし、CPAL 出力ストリームに接続して再生を開始する。
/// 戻り値はストリームと確定したサンプルレート。
pub fn activate_to_stream(
    instance: &mut PluginInstance<MiniHost>,
    gui_events: Consumer<GuiMsg>,
    transport_shared: TransportShared,
) -> Result<(Stream, u32), Box<dyn Error>> {
    let cpal_host = cpal::default_host();

    let output_device = cpal_host
        .default_output_device()
        .ok_or("オーディオ出力デバイスが見つかりません")?;

    let audio_config = FullAudioConfig::find_best_from(&output_device, instance)?;
    println!("オーディオ構成: {audio_config}");

    let note_port = find_main_note_port(instance);
    if note_port.is_none() {
        println!("このプラグインにはノート入力ポートがありません。");
    }

    let plugin_audio_processor = instance
        .activate(|_, _| (), audio_config.as_clack_plugin_config())?
        .start_processing()?;

    let sample_format = audio_config.sample_format;
    let sample_rate = audio_config.sample_rate;
    let cpal_config = audio_config.as_cpal_stream_config();
    let audio_processor = StreamAudioProcessor::new(
        plugin_audio_processor,
        gui_events,
        note_port,
        transport_shared,
        audio_config,
    );

    let stream = build_output_stream_for_sample_format(
        &output_device,
        audio_processor,
        &cpal_config,
        sample_format,
    )?;
    stream.play()?;

    Ok((stream, sample_rate))
}

/// サンプル形式に応じた CPAL 出力ストリームを構築する
fn build_output_stream_for_sample_format(
    device: &Device,
    processor: StreamAudioProcessor,
    config: &StreamConfig,
    sample_format: SampleFormat,
) -> Result<Stream, BuildStreamError> {
    let err = |e| eprintln!("{e}");

    match sample_format {
        SampleFormat::I8 => {
            device.build_output_stream(config, make_stream_runner::<i8>(processor), err, None)
        }
        SampleFormat::I16 => {
            device.build_output_stream(config, make_stream_runner::<i16>(processor), err, None)
        }
        SampleFormat::I32 => {
            device.build_output_stream(config, make_stream_runner::<i32>(processor), err, None)
        }
        SampleFormat::I64 => {
            device.build_output_stream(config, make_stream_runner::<i64>(processor), err, None)
        }
        SampleFormat::U8 => {
            device.build_output_stream(config, make_stream_runner::<u8>(processor), err, None)
        }
        SampleFormat::U16 => {
            device.build_output_stream(config, make_stream_runner::<u16>(processor), err, None)
        }
        SampleFormat::U32 => {
            device.build_output_stream(config, make_stream_runner::<u32>(processor), err, None)
        }
        SampleFormat::U64 => {
            device.build_output_stream(config, make_stream_runner::<u64>(processor), err, None)
        }
        SampleFormat::F32 => {
            device.build_output_stream(config, make_stream_runner::<f32>(processor), err, None)
        }
        SampleFormat::F64 => {
            device.build_output_stream(config, make_stream_runner::<f64>(processor), err, None)
        }
        f => unimplemented!("未知のサンプル形式: {f:?}"),
    }
}

fn make_stream_runner<S: FromSample<f32>>(
    mut audio_processor: StreamAudioProcessor,
) -> impl FnMut(&mut [S], &OutputCallbackInfo) {
    move |data, _info| audio_processor.process(data)
}

/// プラグインのメインノート入力ポートを探す。
/// 戻り値は (ポートインデックス, MIDI ダイアレクトを優先するか)。
fn find_main_note_port(instance: &mut PluginInstance<MiniHost>) -> Option<(u16, bool)> {
    let mut handle = instance.plugin_handle();
    let plugin_note_ports = handle.get_extension::<PluginNotePorts>()?;

    let mut buffer = NotePortInfoBuffer::new();

    let ports_count = plugin_note_ports
        .count(&mut handle, true)
        .min(u16::MAX as u32);

    for i in 0..ports_count {
        let Some(port_info) = plugin_note_ports.get(&mut handle, i, true, &mut buffer) else {
            continue;
        };

        if !port_info
            .supported_dialects
            .intersects(NoteDialects::CLAP | NoteDialects::MIDI)
        {
            continue;
        }

        let prefers_midi = !port_info.supported_dialects.intersects(NoteDialects::CLAP);
        return Some((i as u16, prefers_midi));
    }

    None
}

/// オーディオスレッド上で動くデータ一式
struct StreamAudioProcessor {
    audio_processor: StartedPluginAudioProcessor<MiniHost>,
    buffers: HostAudioBuffers,
    gui_events: Consumer<GuiMsg>,
    clap_events_buffer: EventBuffer,
    /// (ノートポートのインデックス, MIDI ダイアレクト優先か)。ノート入力がなければ None。
    note_port: Option<(u16, bool)>,
    transport: Transport,
    steady_counter: u64,
}

impl StreamAudioProcessor {
    fn new(
        plugin_instance: StartedPluginAudioProcessor<MiniHost>,
        gui_events: Consumer<GuiMsg>,
        note_port: Option<(u16, bool)>,
        transport_shared: TransportShared,
        audio_config: FullAudioConfig,
    ) -> Self {
        Self {
            audio_processor: plugin_instance,
            buffers: HostAudioBuffers::from_config(audio_config),
            gui_events,
            clap_events_buffer: EventBuffer::with_capacity(128),
            note_port,
            transport: Transport::new(transport_shared),
            steady_counter: 0,
        }
    }

    /// GUI からのメッセージをすべて取り出し、CLAP イベントバッファに変換する。
    /// その後、再生中ならシーケンスのイベントを sample_count 分発行する。
    fn collect_gui_events(&mut self, sample_count: u64) {
        self.clap_events_buffer.clear();

        while let Ok(msg) = self.gui_events.pop() {
            match msg {
                GuiMsg::NoteOn { key, velocity } => {
                    let Some((port, prefers_midi)) = self.note_port else {
                        continue;
                    };
                    if prefers_midi {
                        self.clap_events_buffer.push(
                            &MidiEvent::new(0, port, [0x90, key as u8, 100])
                                .with_flags(EventFlags::IS_LIVE),
                        );
                    } else {
                        self.clap_events_buffer.push(
                            &NoteOnEvent::new(
                                0,
                                Pckn::new(port, 0u16, key, Match::All),
                                velocity,
                            )
                            .with_flags(EventFlags::IS_LIVE),
                        );
                    }
                }
                GuiMsg::NoteOff { key } => {
                    let Some((port, prefers_midi)) = self.note_port else {
                        continue;
                    };
                    if prefers_midi {
                        self.clap_events_buffer.push(
                            &MidiEvent::new(0, port, [0x80, key as u8, 0])
                                .with_flags(EventFlags::IS_LIVE),
                        );
                    } else {
                        self.clap_events_buffer.push(
                            &NoteOffEvent::new(0, Pckn::new(port, 0u16, key, Match::All), 0.0)
                                .with_flags(EventFlags::IS_LIVE),
                        );
                    }
                }
                GuiMsg::ParamValue { id, value } => {
                    self.clap_events_buffer.push(&ParamValueEvent::new(
                        0,
                        id,
                        Pckn::match_all(),
                        value,
                        Cookie::empty(),
                    ));
                }
                GuiMsg::Transport(msg) => {
                    self.transport
                        .handle_msg(msg, &mut self.clap_events_buffer, self.note_port);
                }
            }
        }

        // 再生中のシーケンスイベントを発行 (ブロック内オフセット付き)。
        // 今はトラック1本ぶんだけ (複数トラックは以降の段階で対応する)。
        let mut outputs = [(&mut self.clap_events_buffer, self.note_port)];
        self.transport.process_block(&mut outputs, sample_count);
    }

    /// CPAL の出力バッファ1回分を処理する
    fn process<S: FromSample<f32>>(&mut self, data: &mut [S]) {
        self.buffers.ensure_buffer_size_matches(data.len());
        let sample_count = self.buffers.cpal_buf_len_to_frame_count(data.len());

        self.collect_gui_events(sample_count as u64);
        let events = self.clap_events_buffer.as_input();

        let (ins, mut outs) = self.buffers.prepare_plugin_buffers(data.len());

        match self.audio_processor.process(
            &ins,
            &mut outs,
            &events,
            &mut OutputEvents::void(),
            Some(self.steady_counter),
            None,
        ) {
            Ok(_) => self.buffers.write_to_cpal_buffer(data),
            Err(e) => eprintln!("{e}"),
        }

        self.steady_counter += sample_count as u64;
    }
}
