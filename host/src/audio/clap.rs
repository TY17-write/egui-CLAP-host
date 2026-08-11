//! CLAP バックエンド。
//!
//! 中立の [`BlockEvents`] を CLAP のイベント形式へ移し、プラグインを1ブロック
//! 処理して、出力をインターリーブで足し込むところまでを受け持つ。
//!
//! ここより上 (トランスポート・ミキサ・オフラインレンダラ) は CLAP を知らない。

use crate::audio::buffers::HostAudioBuffers;
use crate::audio::config::FullAudioConfig;
use crate::audio::events::{BlockEvent, BlockEvents};
use crate::audio::ProcessError;
use crate::host::MiniHost;
use crate::sequencer::CC_RELEASE;
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
    /// 使うノート入力ポート。ノート入力が無ければ None。
    note_port: Option<NoteInput>,
    /// 効かせている CC。ビット位置が CC 番号 (0〜127)。
    ///
    /// choke で解除値に戻すために覚えておく。これが無いと、ペダルを踏んだまま
    /// 停止したときに踏みっぱなしで残る。128ビットなので確保は起きない。
    active_ccs: u128,
}

impl ClapProcessor {
    pub fn new(
        audio_processor: StartedPluginAudioProcessor<MiniHost>,
        config: FullAudioConfig,
        note_port: Option<NoteInput>,
    ) -> Self {
        Self {
            audio_processor,
            buffers: HostAudioBuffers::from_config(config),
            native: EventBuffer::with_capacity(128),
            note_port,
            active_ccs: 0,
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
    ) -> Result<(), ProcessError> {
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
                BlockEvent::Cc {
                    offset,
                    number,
                    value,
                } => self.push_cc(offset, number, value),
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
        let Some(NoteInput {
            index: port,
            prefers_midi,
            ..
        }) = self.note_port
        else {
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
        let Some(NoteInput {
            index: port,
            prefers_midi,
            ..
        }) = self.note_port
        else {
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
    ///
    /// **効かせた CC も解除する。** 音符を止めてもペダルは踏まれたままなので、
    /// これが無いと停止・シークのあとも踏みっぱなしで残る。
    fn push_choke(&mut self, offset: u32) {
        let Some(NoteInput {
            index: port,
            prefers_midi,
            ..
        }) = self.note_port
        else {
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

        let mut remaining = self.active_ccs;
        while remaining != 0 {
            let number = remaining.trailing_zeros() as u8;
            remaining &= remaining - 1; // 最下位の立っているビットを落とす
            self.push_cc(offset, number, CC_RELEASE);
        }
    }

    /// CC を生 MIDI で送る。
    ///
    /// CLAP に CC のイベントは無いので、MIDI を受け取れるポートでしか送れない。
    /// 送れないときは黙って捨てる (段の見た目で分かるようにしてある)。
    fn push_cc(&mut self, offset: u32, number: u8, value: u8) {
        let Some(NoteInput {
            index: port,
            supports_midi: true,
            ..
        }) = self.note_port
        else {
            return;
        };
        let number = number.min(127);
        self.native.push(
            &MidiEvent::new(offset, port, [0xB0, number, value.min(127)])
                .with_flags(EventFlags::IS_LIVE),
        );

        // 解除値まで戻したものは「効いていない」ので覚えておく必要がない
        if value == CC_RELEASE {
            self.active_ccs &= !(1u128 << number);
        } else {
            self.active_ccs |= 1u128 << number;
        }
    }
}

/// 使うノート入力ポートと、そこで通じる表し方。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct NoteInput {
    pub index: u16,
    /// CLAP の音符イベントを解さないので、音符も生 MIDI で送る
    pub prefers_midi: bool,
    /// 生 MIDI を受け取れる。
    ///
    /// **CC はこれが真のときしか送れない。** CLAP には CC に当たる
    /// イベントが無く、生 MIDI で送るしかないため。CLAP のダイアレクト
    /// しか持たないポートでは CC 段が効かない。
    pub supports_midi: bool,
}

/// プラグインのメインノート入力ポートを探す
pub fn find_main_note_port(instance: &mut PluginInstance<MiniHost>) -> Option<NoteInput> {
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

        return Some(NoteInput {
            index: i as u16,
            prefers_midi: !port_info.supported_dialects.intersects(NoteDialects::CLAP),
            supports_midi: port_info.supported_dialects.intersects(NoteDialects::MIDI),
        });
    }

    None
}
