//! CLAP バックエンド。
//!
//! 中立の [`BlockEvents`] を CLAP のイベント形式へ移し、プラグインを1ブロック
//! 処理して、出力をインターリーブで足し込むところまでを受け持つ。
//!
//! ここより上 (トランスポート・ミキサ・オフラインレンダラ) は CLAP を知らない。

use crate::audio::buffers::HostAudioBuffers;
use crate::audio::config::FullAudioConfig;
use crate::audio::events::{BlockEvent, BlockEvents};
use crate::host::MiniHost;
use clack_extensions::note_ports::{NoteDialects, NotePortInfoBuffer, PluginNotePorts};
use clack_host::events::event_types::{
    MidiEvent, NoteChokeEvent, NoteOffEvent, NoteOnEvent, ParamValueEvent,
};
use clack_host::events::{EventFlags, Match, Pckn};
use clack_host::prelude::*;

/// CLAP の音源1つぶんの処理器
pub struct ClapProcessor {
    audio_processor: StartedPluginAudioProcessor<MiniHost>,
    buffers: HostAudioBuffers,
    /// 中立のイベントを移す先。毎ブロック clear して使い回す。
    native: EventBuffer,
    /// (ノートポートのインデックス, MIDI ダイアレクト優先か)。ノート入力がなければ None。
    note_port: Option<(u16, bool)>,
}

impl ClapProcessor {
    pub fn new(
        audio_processor: StartedPluginAudioProcessor<MiniHost>,
        config: FullAudioConfig,
        note_port: Option<(u16, bool)>,
    ) -> Self {
        Self {
            audio_processor,
            buffers: HostAudioBuffers::from_config(config),
            native: EventBuffer::with_capacity(128),
            note_port,
        }
    }

    /// 停止させてプラグインインスタンスへ返せる形にする
    pub fn into_stopped(self) -> StoppedPluginAudioProcessor<MiniHost> {
        self.audio_processor.stop_processing()
    }

    /// 1ブロック処理して `mix` に足し込む。
    /// `mix` の長さがそのままブロック長 (フレーム数 × チャンネル数) になる。
    pub fn process(
        &mut self,
        events: &BlockEvents,
        steady: u64,
        mix: &mut [f32],
    ) -> Result<(), PluginInstanceError> {
        self.translate(events);

        self.buffers.ensure_buffer_size_matches(mix.len());
        let input_events = self.native.as_input();
        let (ins, mut outs) = self.buffers.prepare_plugin_buffers(mix.len());

        self.audio_processor.process(
            &ins,
            &mut outs,
            &input_events,
            &mut OutputEvents::void(),
            Some(steady),
            None,
        )?;
        self.buffers.mix_into(mix);
        Ok(())
    }

    /// 中立のイベント列を CLAP の形へ移す。
    ///
    /// ノート入力ポートが無いプラグインにはノートを送らない (パラメータは送る)。
    fn translate(&mut self, events: &BlockEvents) {
        self.native.clear();
        for event in events {
            match *event {
                BlockEvent::NoteOn {
                    offset,
                    key,
                    velocity,
                } => self.push_note_on(offset, key, velocity),
                BlockEvent::NoteOff { offset, key } => self.push_note_off(offset, key),
                BlockEvent::Choke { offset } => self.push_choke(offset),
                BlockEvent::Param { offset, id, value } => {
                    // 中立側は素の u32。CLAP の ID に載らない値は捨てる
                    if let Some(id) = ClapId::from_raw(id) {
                        self.native.push(&ParamValueEvent::new(
                            offset,
                            id,
                            Pckn::match_all(),
                            value,
                        ));
                    }
                }
            }
        }
    }

    fn push_note_on(&mut self, offset: u32, key: u8, velocity: f64) {
        let Some((port, prefers_midi)) = self.note_port else {
            return;
        };
        if prefers_midi {
            let velocity = (velocity * 127.0).round().clamp(1.0, 127.0) as u8;
            self.native.push(
                &MidiEvent::new(offset, port, [0x90, key, velocity])
                    .with_flags(EventFlags::IS_LIVE),
            );
        } else {
            self.native.push(
                &NoteOnEvent::new(
                    offset,
                    Pckn::new(port, 0u16, key as u16, Match::All),
                    velocity,
                )
                .with_flags(EventFlags::IS_LIVE),
            );
        }
    }

    fn push_note_off(&mut self, offset: u32, key: u8) {
        let Some((port, prefers_midi)) = self.note_port else {
            return;
        };
        if prefers_midi {
            self.native.push(
                &MidiEvent::new(offset, port, [0x80, key, 0]).with_flags(EventFlags::IS_LIVE),
            );
        } else {
            self.native.push(
                &NoteOffEvent::new(offset, Pckn::new(port, 0u16, key as u16, Match::All), 0.0)
                    .with_flags(EventFlags::IS_LIVE),
            );
        }
    }

    /// CLAP には NoteChoke があるので1つで済む。
    /// MIDI ダイアレクトのプラグインには CC 123 (All Notes Off) を送る。
    fn push_choke(&mut self, offset: u32) {
        let Some((port, prefers_midi)) = self.note_port else {
            return;
        };
        if prefers_midi {
            self.native.push(
                &MidiEvent::new(offset, port, [0xB0, 123, 0]).with_flags(EventFlags::IS_LIVE),
            );
        } else {
            self.native.push(
                &NoteChokeEvent::new(offset, Pckn::match_all()).with_flags(EventFlags::IS_LIVE),
            );
        }
    }
}

/// プラグインのメインノート入力ポートを探す。
/// 戻り値は (ポートインデックス, MIDI ダイアレクトを優先するか)。
pub fn find_main_note_port(instance: &mut PluginInstance<MiniHost>) -> Option<(u16, bool)> {
    let handle = instance.plugin_handle();
    let plugin_note_ports = handle.get_extension::<PluginNotePorts>()?;

    let mut buffer = NotePortInfoBuffer::new();

    let ports_count = plugin_note_ports.count(&handle, true).min(u16::MAX as u32);

    for i in 0..ports_count {
        let Some(port_info) = plugin_note_ports.get(&handle, i, true, &mut buffer) else {
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
