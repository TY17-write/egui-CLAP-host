//! CLAP バックエンド。
//!
//! 中立の [`BlockEvents`] を CLAP のイベント形式へ移し、プラグインを1ブロック
//! 処理して、出力をインターリーブで足し込むところまでを受け持つ。
//!
//! ここより上 (トランスポート・ミキサ・オフラインレンダラ) は CLAP を知らない。

use crate::audio::buffers::HostAudioBuffers;
use crate::audio::config::FullAudioConfig;
use crate::audio::events::{BlockEvent, BlockEvents};
use crate::audio::transport::BlockTransport;
use crate::audio::ProcessError;
use crate::host::MiniHost;
use crate::sequencer::CC_RELEASE;
use clack_extensions::note_ports::{NoteDialects, NotePortInfoBuffer, PluginNotePorts};
use clack_host::events::event_types::{
    MidiEvent, NoteChokeEvent, NoteOffEvent, NoteOnEvent, ParamValueEvent, TransportEvent,
    TransportFlags,
};
use clack_host::events::{EventFlags, EventHeader, Match, Pckn};
use clack_host::prelude::*;
use clack_host::utils::{BeatTime, SecondsTime};

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

    /// 音を返すか (返さないものはチェーンで素通しになる)
    pub fn produces_audio(&self) -> bool {
        self.buffers.config().produces_audio()
    }

    /// 1ブロック処理して `buf` を**置き換える**。
    ///
    /// `buf` は入力と出力を兼ねる。入ってきた音を読み、処理した音で上書きする。
    /// 長さがそのままブロック長 (フレーム数 × チャンネル数) になる。
    ///
    /// `node` はチェーンの何段目か。**自分に宛てたパラメータだけを拾う**ために使う。
    ///
    /// `transport` は再生ヘッドの盤面。CLAP の transport イベントに移して毎ブロック渡す
    /// (渡さないとプラグインは「拍のないホスト」とみなし、小節表示が自走する)。
    pub fn process(
        &mut self,
        events: &BlockEvents,
        node: usize,
        transport: &BlockTransport,
        buf: &mut [f32],
    ) -> Result<(), ProcessError> {
        self.translate(events, node);

        self.buffers.ensure_buffer_size_matches(buf.len());
        let input_events = self.native.as_input();
        let (ins, mut outs) = self.buffers.prepare_plugin_buffers(buf.len(), buf);

        let transport_event = transport_event(transport);
        self.audio_processor.process(
            &ins,
            &mut outs,
            &input_events,
            &mut OutputEvents::void(),
            Some(transport.steady),
            Some(&transport_event),
        )?;
        self.buffers.write_into(buf);
        Ok(())
    }

    /// 中立のイベント列を CLAP の形へ移す。
    ///
    /// ノート入力ポートが無いプラグインにはノートを送らない。
    /// パラメータは**自分に宛てたものだけ**を送る (`node` で振り分ける)。
    fn translate(&mut self, events: &BlockEvents, node: usize) {
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
                // 自分に宛てたものだけ。同じエフェクトを2段刺したとき、
                // 片方だけを動かせなくなるため
                BlockEvent::Param {
                    offset,
                    node: target,
                    id,
                    value,
                } if target == node => {
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
                // 他のノード宛て
                BlockEvent::Param { .. } => {}
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

/// 中立の盤面を CLAP の transport イベントへ移す。
///
/// CLAP の拍 (`BeatTime`) は**四分音符**単位。換算は [`BlockTransport`] に
/// 1本化してあり、ここは値を詰め替えるだけ。ループ始端は常に 0 (ホストの仕様)。
fn transport_event(bt: &BlockTransport) -> TransportEvent {
    let mut flags = TransportFlags::HAS_TEMPO
        | TransportFlags::HAS_BEATS_TIMELINE
        | TransportFlags::HAS_SECONDS_TIMELINE
        | TransportFlags::HAS_TIME_SIGNATURE;
    if bt.playing {
        flags |= TransportFlags::IS_PLAYING;
    }
    if bt.looping {
        flags |= TransportFlags::IS_LOOP_ACTIVE;
    }

    TransportEvent {
        header: EventHeader::new_core(0, EventFlags::empty()),
        flags,
        song_pos_beats: BeatTime::from_float(bt.pos_quarters()),
        song_pos_seconds: SecondsTime::from_float(bt.pos_seconds()),
        tempo: bt.tempo,
        // ブロック内でテンポは変えない
        tempo_inc: 0.0,
        loop_start_beats: BeatTime::from_int(0),
        loop_end_beats: BeatTime::from_float(bt.end_quarters()),
        loop_start_seconds: SecondsTime::from_int(0),
        loop_end_seconds: SecondsTime::from_float(bt.end_seconds()),
        bar_start: BeatTime::from_float(bt.bar_start_quarters()),
        bar_number: bt.bar_number(),
        time_signature_numerator: bt.beats.max(1),
        time_signature_denominator: bt.beat_type.max(1),
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

#[cfg(test)]
mod tests {
    use super::*;

    /// 中立の盤面が CLAP の transport イベントへ正しく移ること。
    /// (拍は四分音符単位。ここがずれると同期系プラグインが全部ずれる)
    #[test]
    fn transport_event_mirrors_the_block_transport() {
        let bt = BlockTransport {
            steady: 0,
            playing: true,
            looping: true,
            // 48kHz・90BPM で 2 秒 = 3 四分音符
            pos_samples: 96_000,
            end_samples: 96_000 * 2,
            sample_rate: 48_000.0,
            tempo: 90.0,
            beats: 3,
            beat_type: 4,
        };
        let event = transport_event(&bt);

        assert!(event.flags.contains(TransportFlags::IS_PLAYING));
        assert!(event.flags.contains(TransportFlags::IS_LOOP_ACTIVE));
        assert!(event.flags.contains(TransportFlags::HAS_TEMPO));
        assert!(event.flags.contains(TransportFlags::HAS_TIME_SIGNATURE));

        assert!((event.song_pos_beats.to_float() - 3.0).abs() < 1e-6);
        assert!((event.song_pos_seconds.to_float() - 2.0).abs() < 1e-6);
        assert_eq!(event.tempo, 90.0);
        assert!((event.loop_end_beats.to_float() - 6.0).abs() < 1e-6);
        // 3/4 の 3拍目終わり = 2小節目 (0始まりで1) の頭
        assert_eq!(event.bar_number, 1);
        assert!((event.bar_start.to_float() - 3.0).abs() < 1e-6);
        assert_eq!(event.time_signature_numerator, 3);
        assert_eq!(event.time_signature_denominator, 4);
    }

    /// 停止中は IS_PLAYING が立たないこと (これが立ちっぱなしだと
    /// 停止しても小節が進み続けるプラグインがある)
    #[test]
    fn a_stopped_transport_clears_the_playing_flag() {
        let bt = BlockTransport {
            playing: false,
            looping: false,
            ..BlockTransport::free_run(0, 48_000.0)
        };
        let event = transport_event(&bt);
        assert!(!event.flags.contains(TransportFlags::IS_PLAYING));
        assert!(!event.flags.contains(TransportFlags::IS_LOOP_ACTIVE));
    }
}
