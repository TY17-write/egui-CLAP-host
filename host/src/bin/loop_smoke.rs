//! ループ再生の検証。オーディオデバイス不要。
//!
//! **ループを展開した並びと突き合わせる。** 4拍のループを3周したものが、
//! 同じノートを3つ並べた12拍のシーケンスを素直に再生したものと**サンプル単位で
//! 一致する**こと。ここが合っていれば、折り返しはホストから見て継ぎ目が無い。
//!
//! 一致しないときは、折り返しでイベントが落ちている・位置がずれている・
//! 音が重なっている、のいずれか。差の出た位置を出すので、そこを見ればよい。
//!
//! **段差 (隣り合うサンプルの差) も出す。** 一致していても段差が大きければ、
//! それは音源の側でノートを切り直しているぶん (エンベロープを持たない
//! 検証用サイン波では必ず出る)。
//!
//! 使い方: cargo run -p egui-clap-host --bin loop_smoke -- <path\to\plugin.clap>

use clack_host::prelude::*;
use egui_clap_host::audio::activate_node;
use egui_clap_host::audio::config::StreamAudioConfig;
use egui_clap_host::audio::graph::{self, Graph};
use egui_clap_host::audio::transport::{Transport, TransportMsg, TransportShared};
use egui_clap_host::discovery;
use egui_clap_host::host::{MiniHost, MiniHostMainThread, MiniHostShared};
use egui_clap_host::sequencer::{MidiEditor, Note};
use std::error::Error;
use std::ffi::CString;
use std::path::Path;

const SAMPLE_RATE: u32 = 44_100;
const BLOCK_SIZE: usize = 512;
const CHANNELS: usize = 2;
/// 何周ぶん回すか
const LOOPS: usize = 3;
/// 1周の長さ (四分音符)
const LOOP_QUARTERS: f32 = 4.0;

/// 一致とみなす差 (f32 の丸めぶん)
const TOLERANCE: f32 = 1e-5;

fn main() -> Result<(), Box<dyn Error>> {
    let plugin_path = std::env::args()
        .nth(1)
        .ok_or("使い方: loop_smoke <path\\to\\plugin.clap>")?;

    let mut failed = false;

    // ノートの置き方を変えて2通り見る。
    // **ループ全体を占める形**は折り返しでノートオフとノートオンが同じ位置に来る。
    // **真ん中に置く形**は折り返しの前後が無音になる。
    for (label, start, duration) in [
        ("ループ全体を占める", 0.0f32, LOOP_QUARTERS),
        ("真ん中だけ (前後は無音)", 1.0, 2.0),
    ] {
        println!("-- {label} --");

        let Pass {
            samples: looped,
            passes,
        } = render(&plugin_path, &[(start, duration)], LOOP_QUARTERS, true)?;
        // 頭から通し直した回数 (再生開始で1 + 折り返しごとに1)。
        // **Integrated ラウドネスの測り直しがこれで走る**ので、
        // 折り返しが数えられていないと積算が周をまたいでしまう
        println!(
            "  頭から通した回数: {passes} (再生開始1 + 折り返し{})",
            passes - 1
        );
        if passes < LOOPS as u64 {
            failed = true;
            println!("  ❌ 折り返しが数えられていない ({passes} 回。{LOOPS} 回以上のはず)");
        }
        // 同じノートを LOOPS 個並べた、ループしないシーケンス
        let laid_out: Vec<(f32, f32)> = (0..LOOPS)
            .map(|turn| (start + LOOP_QUARTERS * turn as f32, duration))
            .collect();
        let straight =
            render(&plugin_path, &laid_out, LOOP_QUARTERS * LOOPS as f32, false)?.samples;

        let frames = (looped.len() / CHANNELS).min(straight.len() / CHANNELS);
        let mut worst = 0.0f32;
        let mut worst_at = 0usize;
        for frame in 0..frames {
            let diff = (looped[frame * CHANNELS] - straight[frame * CHANNELS]).abs();
            if diff > worst {
                worst = diff;
                worst_at = frame;
            }
        }

        let loop_frames = frames / LOOPS;
        if worst > TOLERANCE {
            failed = true;
            println!(
                "  ❌ 展開した並びと違う: 最大の差 {worst:.6} @ {worst_at} フレーム \
                 ({} 周目の {} フレーム目)",
                worst_at / loop_frames + 1,
                worst_at % loop_frames
            );
        } else {
            println!("  ✅ 展開した並びと一致 (最大の差 {worst:.8})");
        }

        // 折り返しの段差。音源がエンベロープを持たなければ 0 にはならない
        for turn in 1..LOOPS {
            let boundary = loop_frames * turn;
            let step = max_step(&looped, boundary.saturating_sub(32), boundary + 32);
            let same = max_step(&straight, boundary.saturating_sub(32), boundary + 32);
            println!("     {turn} 周目の折り返しの段差: {step:.4} (展開した並びでも {same:.4})");
        }
        println!();
    }

    if failed {
        return Err("ループ再生の検証に失敗".into());
    }
    println!("✅ ループの折り返しは、ノートを並べて鳴らしたのと同じ結果になる");
    Ok(())
}

/// 鳴らした結果
struct Pass {
    samples: Vec<f32>,
    /// 頭から通した回数 (再生開始で1 + 折り返しごとに1)
    passes: u64,
}

/// ノートを置いて `total_quarters` 拍ぶんのシーケンスを鳴らす。
/// `looping` が true なら1周ぶんを [`LOOPS`] 周回す。
fn render(
    plugin_path: &str,
    notes: &[(f32, f32)],
    total_quarters: f32,
    looping: bool,
) -> Result<Pass, Box<dyn Error>> {
    let (entry, plugins) = discovery::load_clap_file(Path::new(plugin_path))?;
    let target = &plugins[0];

    let stream_config = StreamAudioConfig {
        output_channel_count: CHANNELS,
        min_buffer_size: 1,
        max_likely_buffer_size: BLOCK_SIZE as u32,
        sample_rate: SAMPLE_RATE,
        sample_format: cpal::SampleFormat::F32,
    };

    let mut instance = instantiate(&entry, &target.id)?;
    let node = activate_node(&mut instance, &stream_config)?;
    let mut graph = Graph::new();
    graph.reserve(BLOCK_SIZE);
    let audio_track = graph::audio_track_for(0).expect("1本なら収まる");
    graph.place_chain(audio_track, graph::MidiSources::one(0), vec![node]);

    let mut editor = MidiEditor::default(); // 120bpm → 四分音符 22050 サンプル
    editor.notes = notes
        .iter()
        .map(|(start, duration)| Note {
            start_tick: *start,
            duration: *duration,
            semitone: 0,
            octave: 4,
            velocity: 127,
            velocity_to: 127,
            track: 0,
            lane: 0,
        })
        .collect();

    let rate = SAMPLE_RATE as f64;
    let spq = editor.samples_per_quarter(rate);
    // **終端は指定どおりに固定する。** ノートの長さから求めると、
    // ループ側と展開側で1周の長さが揃わなくなる
    let end_sample = (total_quarters as f64 * spq) as u64;

    let shared = TransportShared::new();
    let mut transport = Transport::new(shared.clone(), rate);
    let _ = transport.handle_msg(TransportMsg::SetSequence {
        track: 0,
        events: editor.to_events_for_track(0, rate).into_boxed_slice(),
        end_sample,
    });
    let _ = transport.handle_msg(TransportMsg::SetLoop { enabled: looping });
    let _ = transport.handle_msg(TransportMsg::Play);

    // ループ側は1周 × LOOPS、展開側はそのままの長さ
    let total = if looping {
        end_sample as usize * LOOPS
    } else {
        end_sample as usize
    };

    let mut samples = Vec::with_capacity((total + BLOCK_SIZE) * CHANNELS);
    let mut steady = 0u64;
    while samples.len() / CHANNELS < total {
        let plan = transport.plan_block(BLOCK_SIZE as u64);
        graph.clear_events();
        graph.emit_from(&mut transport, &plan);
        graph.process(
            &transport.describe(&plan, steady),
            BLOCK_SIZE,
            &mut |track, e| {
                eprintln!("オーディオトラック {track}: {e}");
            },
        );
        samples.extend_from_slice(graph.master(BLOCK_SIZE));
        steady += BLOCK_SIZE as u64;
    }
    samples.truncate(total * CHANNELS);

    // 使い終わった処理器はメインスレッドで停止・解放する
    for (_, node) in graph.take_nodes() {
        let egui_clap_host::audio::RetiredProcessor::Clap(stopped) = node.into_retired() else {
            return Err("CLAP を載せたのに別形式が返ってきた".into());
        };
        instance.deactivate(stopped);
    }

    Ok(Pass {
        samples,
        passes: shared.pass.load(std::sync::atomic::Ordering::Relaxed),
    })
}

/// 隣り合うサンプルの差の最大 (段差 = クリック音の元)
fn max_step(samples: &[f32], from: usize, to: usize) -> f32 {
    let frames = samples.len() / CHANNELS;
    let from = from.max(1).min(frames);
    let to = to.min(frames);
    let mut worst = 0.0f32;
    for frame in from..to {
        let current = samples[frame * CHANNELS];
        let previous = samples[(frame - 1) * CHANNELS];
        worst = worst.max((current - previous).abs());
    }
    worst
}

fn instantiate(
    entry: &PluginEntry,
    plugin_id: &str,
) -> Result<PluginInstance<MiniHost>, Box<dyn Error>> {
    let host_info = HostInfo::new(
        "loop_smoke",
        "egui-clap-host",
        "https://example.com",
        "0.1.0",
    )?;
    let id = CString::new(plugin_id)?;
    let (sender, _receiver) = crossbeam_channel::unbounded();
    Ok(PluginInstance::<MiniHost>::new(
        |_| MiniHostShared::new(sender.clone()),
        |shared| MiniHostMainThread::new(shared),
        entry,
        &id,
        &host_info,
    )?)
}
