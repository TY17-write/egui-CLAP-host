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
    NoteOn {
        track: usize,
        key: u16,
        velocity: f64,
    },
    NoteOff {
        track: usize,
        key: u16,
    },
    ParamValue {
        track: usize,
        id: ClapId,
        value: f64,
    },
    Transport(TransportMsg),
    /// トラックに音源を載せる (差し替え時は古い方を retired へ返す)
    SetTrack {
        track: usize,
        processor: Box<TrackProcessor>,
    },
    /// トラックの音源を外す
    ClearTrack {
        track: usize,
    },
}

/// オーディオスレッドが持つトラック1本ぶんの処理器一式
pub struct TrackProcessor {
    audio_processor: StartedPluginAudioProcessor<MiniHost>,
    buffers: HostAudioBuffers,
    events: EventBuffer,
    /// (ノートポートのインデックス, MIDI ダイアレクト優先か)。ノート入力がなければ None。
    note_port: Option<(u16, bool)>,
}

impl TrackProcessor {
    pub fn new(
        audio_processor: StartedPluginAudioProcessor<MiniHost>,
        config: FullAudioConfig,
        note_port: Option<(u16, bool)>,
    ) -> Self {
        Self {
            audio_processor,
            buffers: HostAudioBuffers::from_config(config),
            events: EventBuffer::with_capacity(128),
            note_port,
        }
    }

    /// 停止させてプラグインインスタンスへ返せる形にする
    pub fn into_stopped(self) -> StoppedPluginAudioProcessor<MiniHost> {
        self.audio_processor.stop_processing()
    }
}

/// 出力ストリームを1本だけ作って再生を開始する。
///
/// トラックの音源は後から `GuiMsg::SetTrack` で載せる。ストリームを作り直さずに
/// 差し替えられるので、トランスポート (再生位置) は1つのまま保たれる。
/// 外した音源は `retired` 経由でメインスレッドへ返す (オーディオスレッドで
/// 解放しないため)。
pub fn start_engine(
    gui_events: Consumer<GuiMsg>,
    retired: rtrb::Producer<Box<TrackProcessor>>,
    transport_shared: TransportShared,
) -> Result<(Stream, StreamAudioConfig), Box<dyn Error>> {
    let cpal_host = cpal::default_host();

    let output_device = cpal_host
        .default_output_device()
        .ok_or("オーディオ出力デバイスが見つかりません")?;

    let stream_config = StreamAudioConfig::find_best(&output_device)?;
    println!("オーディオ構成: {stream_config}");

    let cpal_config = stream_config.as_cpal_stream_config();
    let sample_format = stream_config.sample_format;
    let audio_processor = StreamAudioProcessor::new(
        gui_events,
        retired,
        transport_shared,
        stream_config.output_channel_count,
    );

    let stream = build_output_stream_for_sample_format(
        &output_device,
        audio_processor,
        &cpal_config,
        sample_format,
    )?;
    stream.play()?;

    Ok((stream, stream_config))
}

/// プラグインを指定のストリーム構成でアクティベートし、トラック処理器にする
pub fn activate_track(
    instance: &mut PluginInstance<MiniHost>,
    stream_config: &StreamAudioConfig,
) -> Result<Box<TrackProcessor>, Box<dyn Error>> {
    let audio_config = FullAudioConfig::for_plugin(stream_config, instance);

    let note_port = find_main_note_port(instance);
    if note_port.is_none() {
        println!("このプラグインにはノート入力ポートがありません。");
    }

    let processor = instance
        .activate(|_, _| (), stream_config.as_clack_plugin_config())?
        .start_processing()?;

    Ok(Box::new(TrackProcessor::new(
        processor,
        audio_config,
        note_port,
    )))
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
    /// トラックごとの音源。None は音源未ロードのトラック。
    tracks: Vec<Option<Box<TrackProcessor>>>,
    /// 外した音源をメインスレッドへ返す口 (ここで解放しないため)
    retired: rtrb::Producer<Box<TrackProcessor>>,
    gui_events: Consumer<GuiMsg>,
    /// 全トラックの出力を足し込むバッファ (インターリーブ済み)
    mix: Vec<f32>,
    output_channel_count: usize,
    transport: Transport,
    steady_counter: u64,
}

impl StreamAudioProcessor {
    fn new(
        gui_events: Consumer<GuiMsg>,
        retired: rtrb::Producer<Box<TrackProcessor>>,
        transport_shared: TransportShared,
        output_channel_count: usize,
    ) -> Self {
        Self {
            tracks: Vec::new(),
            retired,
            gui_events,
            mix: Vec::new(),
            output_channel_count,
            transport: Transport::new(transport_shared),
            steady_counter: 0,
        }
    }

    /// トラック数を確保する (足りなければ空きで埋める)
    fn ensure_track(&mut self, track: usize) {
        while self.tracks.len() <= track {
            self.tracks.push(None);
        }
    }

    /// 外した音源をメインスレッドへ返す。返せなければやむなくここで解放する
    /// (リングバッファが詰まるのは異常時だけ)。
    fn retire(&mut self, processor: Option<Box<TrackProcessor>>) {
        if let Some(processor) = processor {
            let _ = self.retired.push(processor);
        }
    }

    /// GUI からのメッセージをすべて取り出し、トラックごとの CLAP イベントに変換する。
    /// その後、再生中ならシーケンスのイベントを sample_count 分発行する。
    fn collect_gui_events(&mut self, sample_count: u64) {
        for track in self.tracks.iter_mut().flatten() {
            track.events.clear();
        }

        while let Ok(msg) = self.gui_events.pop() {
            match msg {
                GuiMsg::NoteOn {
                    track,
                    key,
                    velocity,
                } => {
                    let Some(Some(target)) = self.tracks.get_mut(track) else {
                        continue;
                    };
                    let Some((port, prefers_midi)) = target.note_port else {
                        continue;
                    };
                    if prefers_midi {
                        target.events.push(
                            &MidiEvent::new(0, port, [0x90, key as u8, 100])
                                .with_flags(EventFlags::IS_LIVE),
                        );
                    } else {
                        target.events.push(
                            &NoteOnEvent::new(0, Pckn::new(port, 0u16, key, Match::All), velocity)
                                .with_flags(EventFlags::IS_LIVE),
                        );
                    }
                }
                GuiMsg::NoteOff { track, key } => {
                    let Some(Some(target)) = self.tracks.get_mut(track) else {
                        continue;
                    };
                    let Some((port, prefers_midi)) = target.note_port else {
                        continue;
                    };
                    if prefers_midi {
                        target.events.push(
                            &MidiEvent::new(0, port, [0x80, key as u8, 0])
                                .with_flags(EventFlags::IS_LIVE),
                        );
                    } else {
                        target.events.push(
                            &NoteOffEvent::new(0, Pckn::new(port, 0u16, key, Match::All), 0.0)
                                .with_flags(EventFlags::IS_LIVE),
                        );
                    }
                }
                GuiMsg::ParamValue { track, id, value } => {
                    let Some(Some(target)) = self.tracks.get_mut(track) else {
                        continue;
                    };
                    target.events.push(&ParamValueEvent::new(
                        0,
                        id,
                        Pckn::match_all(),
                        value,
                        Cookie::empty(),
                    ));
                }
                GuiMsg::Transport(msg) => {
                    // 状態の更新は1回。消音が要るときは全トラックへ配る
                    if self.transport.handle_msg(msg) {
                        for track in self.tracks.iter_mut().flatten() {
                            transport::push_choke(&mut track.events, track.note_port, 0);
                        }
                    }
                }
                GuiMsg::SetTrack { track, processor } => {
                    self.ensure_track(track);
                    let previous = self.tracks[track].replace(processor);
                    self.retire(previous);
                }
                GuiMsg::ClearTrack { track } => {
                    if let Some(slot) = self.tracks.get_mut(track) {
                        let previous = slot.take();
                        self.retire(previous);
                    }
                }
            }
        }

        // 再生位置を1回だけ進めてから、トラックごとにイベントを発行する。
        // (オーディオスレッドで確保しないよう、区間の計画は固定長で持ち回る)
        let plan = self.transport.plan_block(sample_count);
        if !plan.is_empty() {
            for (index, slot) in self.tracks.iter_mut().enumerate() {
                if let Some(track) = slot {
                    self.transport
                        .emit_track(index, &plan, &mut track.events, track.note_port);
                }
            }
        }
    }

    /// CPAL の出力バッファ1回分を処理する。
    /// 各トラックを自分のバッファへ処理してから、まとめて足し合わせる。
    fn process<S: FromSample<f32>>(&mut self, data: &mut [S]) {
        let sample_count = data.len() / self.output_channel_count.max(1);

        self.collect_gui_events(sample_count as u64);

        // ミックス用バッファを用意して 0 クリア
        if self.mix.len() < data.len() {
            self.mix.resize(data.len(), 0.0);
        }
        let mix = &mut self.mix[..data.len()];
        mix.fill(0.0);

        let steady = self.steady_counter;
        for track in self.tracks.iter_mut().flatten() {
            track.buffers.ensure_buffer_size_matches(data.len());
            let events = track.events.as_input();
            let (ins, mut outs) = track.buffers.prepare_plugin_buffers(data.len());

            match track.audio_processor.process(
                &ins,
                &mut outs,
                &events,
                &mut OutputEvents::void(),
                Some(steady),
                None,
            ) {
                Ok(_) => track.buffers.mix_into(mix),
                Err(e) => eprintln!("{e}"),
            }
        }

        for (out, mixed) in data.iter_mut().zip(self.mix.iter()) {
            *out = FromSample::from_sample_(*mixed);
        }

        self.steady_counter += sample_count as u64;
    }
}
