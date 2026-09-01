//! 消音イベント (NoteChoke / CC123) の検証。オーディオデバイス不要。
//!
//! ロングトーンを鳴らしている最中に停止・シークを行い、
//! 「音が残らずに止まる」「シークした先でまた鳴る」をサンプル位置で検証する。
//!
//! 発音中の停止で音が止まるかどうかは、ホストが消音イベントを送るかと、
//! プラグインがそれを処理するかの両方で決まる。ここは後者を落とさないための検証。
//! (プラグインが NoteChoke を無視すると、ホストが正しくても鳴りっぱなしになる)
//!
//! 使い方: cargo run -p egui-clap-host --bin choke_smoke -- <path\to\plugin.clap>

use clack_host::prelude::*;
use egui_clap_host::audio::config::StreamAudioConfig;
use egui_clap_host::audio::transport::{self, Transport, TransportMsg, TransportShared};
use egui_clap_host::audio::{self, TrackProcessor};
use egui_clap_host::discovery;
use egui_clap_host::host::{MiniHost, MiniHostMainThread, MiniHostShared};
use egui_clap_host::sequencer::{MidiEditor, Note};
use std::error::Error;
use std::ffi::CString;
use std::path::Path;

const SAMPLE_RATE: f64 = 44_100.0;
const BLOCK_SIZE: usize = 512;
const CHANNELS: usize = 2;

/// 無音とみなす上限 / 鳴っているとみなす下限
const SILENT: f32 = 0.001;
const AUDIBLE: f32 = 0.01;

/// 何ブロック目で停止するか / シークするか (どちらもロングトーンの途中)
const STOP_AT: usize = 10;
const SEEK_AT: usize = 20;
const TOTAL_BLOCKS: usize = 30;

fn main() -> Result<(), Box<dyn Error>> {
    let path = std::env::args()
        .nth(1)
        .ok_or("使い方: choke_smoke <path\\to\\plugin.clap>")?;

    let (entry, plugins) = discovery::load_clap_file(Path::new(&path))?;
    let target = &plugins[0];
    println!("プラグイン: {} ({})", target.name, target.id);

    let host_info = HostInfo::new(
        "Choke Smoke",
        "egui-clap-host",
        "https://example.com",
        "0.1.0",
    )?;
    let plugin_id = CString::new(target.id.as_str())?;
    let (sender, _receiver) = crossbeam_channel::unbounded();

    let mut instance = PluginInstance::<MiniHost>::new(
        |_| MiniHostShared::new(sender.clone()),
        |shared| MiniHostMainThread::new(shared),
        &entry,
        &plugin_id,
        &host_info,
    )?;

    // バッファやイベントの組み立ては本体と同じ経路を通す
    let stream_config = StreamAudioConfig {
        output_channel_count: CHANNELS,
        min_buffer_size: 1,
        max_likely_buffer_size: BLOCK_SIZE as u32,
        sample_rate: SAMPLE_RATE as u32,
        sample_format: cpal::SampleFormat::F32,
    };
    let mut processor: Box<TrackProcessor> = audio::activate_track(&mut instance, &stream_config)?;

    // ---- シーケンス: C4 のロングトーン1本 (4拍) ----
    // 途中で止めるためのものなので、検証の間ずっと鳴り続ける長さにする。
    let mut editor = MidiEditor::default(); // 120bpm → 四分音符 = 22050 サンプル
    editor.notes = vec![Note {
        start_tick: 0.0,
        duration: 4.0,
        semitone: 0,
        octave: 4,
        velocity: 127,
        velocity_to: 127,
        track: 0,
        lane: 0,
    }];

    let spq = editor.samples_per_quarter(SAMPLE_RATE);
    let events = editor.to_events(SAMPLE_RATE).into_boxed_slice();
    let end_sample = (editor.length_quarters_bar_aligned() as f64 * spq) as u64;

    let mut transport = Transport::new(TransportShared::new(), SAMPLE_RATE);

    let _ = transport.handle_msg(TransportMsg::SetSequence {
        track: 0,
        events,
        end_sample,
    });
    let _ = transport.handle_msg(TransportMsg::Play);

    // ---- オフライン処理 ----
    let mut mix = vec![0.0f32; BLOCK_SIZE * CHANNELS];

    // 各区間のピーク [再生中, 停止後, シーク後]
    let mut peaks = [0.0f32; 3];
    let mut pos = 0u64;

    for block in 0..TOTAL_BLOCKS {
        processor.events_mut().clear();

        // 本体と同じ手順: トランスポートを操作し、要求されたら消音イベントを積む
        let msg = match block {
            STOP_AT => Some(TransportMsg::Stop),
            // 止めたまま頭へ戻して再生し直す (シークでも消音が要る)
            SEEK_AT => Some(TransportMsg::Seek { sample: 0 }),
            _ => None,
        };
        if let Some(msg) = msg {
            if transport.handle_msg(msg) {
                transport::push_choke(processor.events_mut(), 0);
            }
        }
        if block == SEEK_AT {
            let _ = transport.handle_msg(TransportMsg::Play);
        }

        let plan = transport.plan_block(BLOCK_SIZE as u64);
        transport.emit_track(0, &plan, processor.events_mut());

        mix.fill(0.0);
        processor.process(&transport.describe(&plan, pos), &mut mix)?;

        let peak = mix.iter().fold(0.0f32, |acc, s| acc.max(s.abs()));
        // 操作したブロック自体は境界なので、どの区間にも数えない
        match block {
            b if b < STOP_AT => peaks[0] = peaks[0].max(peak),
            b if b > STOP_AT && b < SEEK_AT => peaks[1] = peaks[1].max(peak),
            b if b > SEEK_AT => peaks[2] = peaks[2].max(peak),
            _ => {}
        }

        pos += BLOCK_SIZE as u64;
    }

    // 使い終わった処理器はメインスレッドで停止・解放する
    let Some(audio::RetiredProcessor::Clap(stopped)) = processor.into_single_retired() else {
        return Err("CLAP 1段を載せたのに別のものが返ってきた".into());
    };
    instance.deactivate(stopped);

    println!(
        "ピーク値: 再生中={:.4} 停止後={:.4} シーク後={:.4}",
        peaks[0], peaks[1], peaks[2]
    );

    let mut failures = Vec::new();
    if peaks[0] < AUDIBLE {
        failures.push("再生中に音が出ていない (検証になっていない)".to_string());
    }
    if peaks[1] > SILENT {
        failures.push(format!(
            "停止しても鳴り続けている (ピーク {:.4})。\
             プラグインが NoteChoke を無視していないか確認",
            peaks[1]
        ));
    }
    if peaks[2] < AUDIBLE {
        failures.push("シークして再生し直しても音が出ない".to_string());
    }

    failures.extend(bypass_pass(&entry, &target.id)?);

    if failures.is_empty() {
        println!("✅ 消音イベントのテスト成功");
        Ok(())
    } else {
        Err(format!("❌ 失敗: {}", failures.join(", ")).into())
    }
}

/// バイパス中に鳴っている最中の停止を挟んで、**戻したときに鳴り出さないこと**。
///
/// バイパス中のノードに `process` を渡さないと、消音イベントもノートオフも
/// 届かない。プラグインの中ではノートが鳴ったままなので、バイパスを戻した
/// 瞬間に音が出てくる。
fn bypass_pass(entry: &PluginEntry, plugin_id: &str) -> Result<Vec<String>, Box<dyn Error>> {
    /// 鳴っている最中にバイパスへ入れる / 停止する / バイパスを戻す
    const BYPASS_AT: usize = 5;
    const STOP_AT: usize = 10;
    const UNBYPASS_AT: usize = 15;
    const TOTAL: usize = 25;

    let host_info = HostInfo::new(
        "Choke Smoke",
        "egui-clap-host",
        "https://example.com",
        "0.1.0",
    )?;
    let id = CString::new(plugin_id)?;
    let (sender, _receiver) = crossbeam_channel::unbounded();
    let mut instance = PluginInstance::<MiniHost>::new(
        |_| MiniHostShared::new(sender.clone()),
        |shared| MiniHostMainThread::new(shared),
        entry,
        &id,
        &host_info,
    )?;

    let stream_config = StreamAudioConfig {
        output_channel_count: CHANNELS,
        min_buffer_size: 1,
        max_likely_buffer_size: BLOCK_SIZE as u32,
        sample_rate: SAMPLE_RATE as u32,
        sample_format: cpal::SampleFormat::F32,
    };
    let mut processor: Box<TrackProcessor> = audio::activate_track(&mut instance, &stream_config)?;

    let mut editor = MidiEditor::default();
    editor.notes = vec![Note {
        start_tick: 0.0,
        duration: 4.0,
        semitone: 0,
        octave: 4,
        velocity: 127,
        velocity_to: 127,
        track: 0,
        lane: 0,
    }];
    let spq = editor.samples_per_quarter(SAMPLE_RATE);
    let end_sample = (editor.length_quarters_bar_aligned() as f64 * spq) as u64;

    let mut transport = Transport::new(TransportShared::new(), SAMPLE_RATE);
    let _ = transport.handle_msg(TransportMsg::SetSequence {
        track: 0,
        events: editor.to_events(SAMPLE_RATE).into_boxed_slice(),
        end_sample,
    });
    let _ = transport.handle_msg(TransportMsg::Play);

    let mut mix = vec![0.0f32; BLOCK_SIZE * CHANNELS];
    // [バイパス前, バイパス中, 戻したあと]
    let mut peaks = [0.0f32; 3];
    let mut pos = 0u64;

    for block in 0..TOTAL {
        processor.events_mut().clear();

        match block {
            BYPASS_AT => processor.set_bypassed(0, true),
            UNBYPASS_AT => processor.set_bypassed(0, false),
            STOP_AT => {
                // 本体と同じ扱い: 停止で状態が変われば消音を積む
                let stopped = transport.handle_msg(TransportMsg::Stop);
                if stopped {
                    transport::push_choke(processor.events_mut(), 0);
                }
            }
            _ => {}
        }

        let plan = transport.plan_block(BLOCK_SIZE as u64);
        transport.emit_track(0, &plan, processor.events_mut());

        mix.fill(0.0);
        processor.process(&transport.describe(&plan, pos), &mut mix)?;

        let peak = mix.iter().fold(0.0f32, |acc, s| acc.max(s.abs()));
        match block {
            b if b < BYPASS_AT => peaks[0] = peaks[0].max(peak),
            b if b > BYPASS_AT && b < UNBYPASS_AT => peaks[1] = peaks[1].max(peak),
            b if b > UNBYPASS_AT => peaks[2] = peaks[2].max(peak),
            _ => {}
        }
        pos += BLOCK_SIZE as u64;
    }

    let Some(audio::RetiredProcessor::Clap(stopped)) = processor.into_single_retired() else {
        return Err("CLAP 1段を載せたのに別のものが返ってきた".into());
    };
    instance.deactivate(stopped);

    println!(
        "バイパス: 前={:.4} 中={:.4} 戻したあと={:.4}",
        peaks[0], peaks[1], peaks[2]
    );

    let mut failures = Vec::new();
    if peaks[0] < AUDIBLE {
        failures.push("バイパス前に音が出ていない (検証になっていない)".to_string());
    }
    if peaks[1] > SILENT {
        failures.push(format!(
            "バイパス中に音が漏れている (ピーク {:.4})",
            peaks[1]
        ));
    }
    if peaks[2] > SILENT {
        failures.push(format!(
            "停止済みなのにバイパスを戻すと鳴り出す (ピーク {:.4})。\
             バイパス中のノードに消音イベントが届いていない",
            peaks[2]
        ));
    }
    Ok(failures)
}
