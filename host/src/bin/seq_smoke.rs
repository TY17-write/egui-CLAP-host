//! シーケンサー再生エンジンのオフライン検証。
//! C4(0〜1拍) → 休符(1〜2拍) → E4(2〜3拍) のシーケンスを再生し、
//! 「音が鳴るべき区間で音が出て、休符区間は無音」をサンプル位置で検証する。
//!
//! バッファやイベントの組み立ては本体と同じ経路 (`activate_track` →
//! `TrackProcessor`) を通す。手組みの並行実装を持つと、抽象化を変えたときに
//! ここだけ古い形のまま通ってしまう。
//!
//! 使い方: cargo run -p egui-clap-host --bin seq_smoke -- <path\to\plugin.clap>

use clack_host::prelude::*;
use egui_clap_host::audio::config::StreamAudioConfig;
use egui_clap_host::audio::transport::{Transport, TransportMsg, TransportShared};
use egui_clap_host::audio::{self, TrackProcessor};
use egui_clap_host::discovery;
use egui_clap_host::host::{MiniHost, MiniHostMainThread, MiniHostShared};
use egui_clap_host::sequencer::{MidiEditor, Note};
use std::error::Error;
use std::ffi::CString;
use std::path::Path;
use std::sync::atomic::Ordering;

const SAMPLE_RATE: f64 = 44_100.0;
const BLOCK_SIZE: usize = 512;
const CHANNELS: usize = 2;

fn main() -> Result<(), Box<dyn Error>> {
    let path = std::env::args()
        .nth(1)
        .ok_or("使い方: seq_smoke <path\\to\\plugin.clap>")?;

    let (entry, plugins) = discovery::load_clap_file(Path::new(&path))?;
    let target = &plugins[0];
    println!("プラグイン: {} ({})", target.name, target.id);

    let host_info = HostInfo::new(
        "Seq Smoke",
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

    // デバイスを開かずに構成を作る (本体が使うものと同じ形)
    let stream_config = StreamAudioConfig {
        output_channel_count: CHANNELS,
        min_buffer_size: 1,
        max_likely_buffer_size: BLOCK_SIZE as u32,
        sample_rate: SAMPLE_RATE as u32,
        sample_format: cpal::SampleFormat::F32,
    };
    let mut processor: Box<TrackProcessor> = audio::activate_track(&mut instance, &stream_config)?;

    // ---- シーケンス: C4 [0,1拍) / 休符 [1,2拍) / E4 [2,3拍) @ 120bpm ----
    // 2音目はベロシティを半分にして、音量差が出ることも確認する。
    const VEL_FULL: u8 = 127;
    const VEL_HALF: u8 = 64;
    let mut editor = MidiEditor::default(); // 120bpm → 四分音符 = 22050 サンプル
    editor.notes = vec![
        Note {
            start_tick: 0.0,
            duration: 1.0,
            semitone: 0,
            octave: 4,
            velocity: VEL_FULL,
            velocity_to: VEL_FULL,
            track: 0,
            lane: 0,
        },
        Note {
            start_tick: 2.0,
            duration: 1.0,
            semitone: 4,
            octave: 4,
            velocity: VEL_HALF,
            velocity_to: VEL_HALF,
            track: 0,
            lane: 1,
        },
    ];

    let spq = editor.samples_per_quarter(SAMPLE_RATE); // 22050
    let events = editor.to_events(SAMPLE_RATE).into_boxed_slice();
    let end_sample = (editor.length_quarters_bar_aligned() as f64 * spq) as u64; // 4拍 = 88200

    let shared = TransportShared::new();
    let mut transport = Transport::new(shared.clone());
    let _ = transport.handle_msg(TransportMsg::SetSequence {
        track: 0,
        events,
        end_sample,
    });
    let _ = transport.handle_msg(TransportMsg::Play);

    // ---- オフライン処理 ----
    let mut mix = vec![0.0f32; BLOCK_SIZE * CHANNELS];

    // 各区間のピーク値 [C4区間, 休符区間, E4区間, 終端後]
    let mut region_peaks = [0.0f32; 4];
    let boundaries = [0u64, 22050, 44100, 66150, 88200];
    let margin = (BLOCK_SIZE * 2) as u64; // 境界ブロックの曖昧さを除外

    let total_blocks = (end_sample as usize).div_ceil(BLOCK_SIZE) + 4;
    let mut pos = 0u64;

    for _ in 0..total_blocks {
        let plan = transport.plan_block(BLOCK_SIZE as u64);
        processor.events_mut().clear();
        transport.emit_track(0, &plan, processor.events_mut());

        mix.fill(0.0);
        processor.process(pos, &mut mix)?;

        // このブロックがどの区間に完全に収まるかを判定してピークを記録
        let block_start = pos;
        let block_end = pos + BLOCK_SIZE as u64;
        for region in 0..4 {
            let lo = boundaries[region] + margin;
            let hi = boundaries[region + 1].saturating_sub(margin);
            if block_start >= lo && block_end <= hi {
                let peak = mix.iter().fold(0.0f32, |acc, s| acc.max(s.abs()));
                region_peaks[region] = region_peaks[region].max(peak);
            }
        }

        pos += BLOCK_SIZE as u64;
    }

    // 使い終わった処理器はメインスレッドで停止・解放する
    let Some(audio::RetiredProcessor::Clap(stopped)) = processor.into_single_retired() else {
        return Err("CLAP 1段を載せたのに別のものが返ってきた".into());
    };
    instance.deactivate(stopped);

    println!(
        "ピーク値: C4区間={:.4} 休符区間={:.4} E4区間={:.4} 終端後={:.4}",
        region_peaks[0], region_peaks[1], region_peaks[2], region_peaks[3]
    );
    println!(
        "最終位置: {} / 再生中: {}",
        shared.pos.load(Ordering::Relaxed),
        shared.playing.load(Ordering::Relaxed)
    );

    // ベロシティ比 (127 → 64) が音量比として現れているか
    let velocity_ratio = if region_peaks[0] > 0.0 {
        region_peaks[2] / region_peaks[0]
    } else {
        0.0
    };
    let expected_ratio = VEL_HALF as f32 / VEL_FULL as f32;
    println!(
        "ベロシティ比: 実測 {:.3} / 期待 {:.3}",
        velocity_ratio, expected_ratio
    );

    let mut failures = Vec::new();
    if region_peaks[0] < 0.01 {
        failures.push("C4 区間が無音".to_string());
    }
    if region_peaks[1] > 0.001 {
        failures.push("休符区間で音が出ている".to_string());
    }
    if region_peaks[2] < 0.01 {
        failures.push("E4 区間が無音".to_string());
    }
    if region_peaks[3] > 0.001 {
        failures.push("終端後に音が出ている".to_string());
    }
    if shared.playing.load(Ordering::Relaxed) {
        failures.push("終端で自動停止していない".to_string());
    }
    if (velocity_ratio - expected_ratio).abs() > 0.05 {
        failures.push(format!(
            "ベロシティが音量に反映されていない (実測比 {velocity_ratio:.3}, 期待 {expected_ratio:.3})"
        ));
    }

    if failures.is_empty() {
        println!("✅ シーケンス再生テスト成功");
        Ok(())
    } else {
        Err(format!("❌ 失敗: {}", failures.join(", ")).into())
    }
}
