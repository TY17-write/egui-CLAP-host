//! オフラインのスモークテスト。
//! オーディオデバイスなしで .clap をロード → アクティベート → ノートオン →
//! process() を回して、実際に波形が出力されることを確認する。
//!
//! 使い方: cargo run -p clap-host-test --bin smoke -- <path\to\plugin.clap>

use clack_host::events::event_types::NoteOnEvent;
use clack_host::events::{EventFlags, Match};
use clack_host::prelude::*;
use clap_host_test::discovery;
use clap_host_test::host::{MiniHost, MiniHostMainThread, MiniHostShared};
use std::error::Error;
use std::ffi::CString;
use std::path::Path;

const SAMPLE_RATE: f64 = 44_100.0;
const BLOCK_SIZE: usize = 512;
const CHANNELS: usize = 2;

fn main() -> Result<(), Box<dyn Error>> {
    let path = std::env::args()
        .nth(1)
        .ok_or("使い方: smoke <path\\to\\plugin.clap>")?;

    println!("ロード中: {path}");
    let (entry, plugins) = discovery::load_clap_file(Path::new(&path))?;

    for plugin in &plugins {
        println!("  発見: {} ({})", plugin.name, plugin.id);
    }
    let target = &plugins[0];

    let host_info = HostInfo::new("Smoke Test", "clap-host-test", "https://example.com", "0.1.0")?;
    let plugin_id = CString::new(target.id.as_str())?;
    let (sender, _receiver) = crossbeam_channel::unbounded();

    let mut instance = PluginInstance::<MiniHost>::new(
        |_| MiniHostShared::new(sender.clone()),
        |shared| MiniHostMainThread::new(shared),
        &entry,
        &plugin_id,
        &host_info,
    )?;
    println!("インスタンス化: OK");

    // パラメータ列挙の確認
    let params = clap_host_test::params::read_params(&mut instance);
    for p in &params {
        println!(
            "  パラメータ: {} (id={:?}, {}..{}, 現在値={})",
            p.name, p.id, p.min, p.max, p.value
        );
    }

    let config = PluginAudioConfiguration {
        sample_rate: SAMPLE_RATE,
        min_frames_count: 1,
        max_frames_count: BLOCK_SIZE as u32,
    };

    let mut processor = instance.activate(|_, _| (), config)?.start_processing()?;
    println!("アクティベート + 処理開始: OK");

    // ステレオ入出力バッファを用意 (テストプラグインはモノラル出力だが、
    // ポート構成は簡略化してステレオ1ポートで渡す)
    let mut input_buffers = [vec![0.0f32; BLOCK_SIZE], vec![0.0f32; BLOCK_SIZE]];
    let mut output_buffers = [vec![0.0f32; BLOCK_SIZE], vec![0.0f32; BLOCK_SIZE]];
    let mut input_ports = AudioPorts::with_capacity(CHANNELS, 1);
    let mut output_ports = AudioPorts::with_capacity(CHANNELS, 1);

    // ノートオンイベント (C4 = key 60)
    let mut event_buffer = EventBuffer::with_capacity(4);
    event_buffer.push(
        &NoteOnEvent::new(0, Pckn::new(0u16, 0u16, 60u16, Match::All), 0.8)
            .with_flags(EventFlags::IS_LIVE),
    );

    let mut peak: f32 = 0.0;
    let mut steady_counter = 0u64;

    for block in 0..20 {
        // 最初のブロックだけノートオンを送る
        let events = if block == 0 {
            event_buffer.as_input()
        } else {
            InputEvents::empty()
        };

        let inputs = input_ports.with_input_buffers([AudioPortBuffer {
            latency: 0,
            channels: AudioPortBufferType::f32_input_only(input_buffers.iter_mut().map(|b| {
                InputChannel {
                    buffer: b.as_mut_slice(),
                    is_constant: true,
                }
            })),
        }]);
        let mut outputs = output_ports.with_output_buffers([AudioPortBuffer {
            latency: 0,
            channels: AudioPortBufferType::f32_output_only(
                output_buffers.iter_mut().map(|b| b.as_mut_slice()),
            ),
        }]);

        processor.process(
            &inputs,
            &mut outputs,
            &events,
            &mut OutputEvents::void(),
            Some(steady_counter),
            None,
        )?;
        steady_counter += BLOCK_SIZE as u64;

        for buffer in &output_buffers {
            for sample in buffer {
                peak = peak.max(sample.abs());
            }
        }
    }

    println!("20ブロック処理完了。出力ピーク値: {peak:.4}");

    if peak > 0.001 {
        println!("✅ スモークテスト成功: プラグインは音声を生成しています");
        Ok(())
    } else {
        Err("❌ 出力が無音です。ノートイベントが処理されていない可能性があります".into())
    }
}
