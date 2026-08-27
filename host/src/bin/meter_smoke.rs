//! マスターメーター (ラウドネスとスペクトル) の検証。オーディオデバイス不要。
//!
//! 2つ見る。
//!
//! 1. **規格の試験信号**: EBU Tech 3341 の試験信号1 (1kHz 正弦波 -23.0 dBFS) を
//!    リングバッファ越しに流し、-23.0 LUFS を示すこと。デバイスのレートが
//!    変わっても同じ値になること (K特性の係数を引き直せているかの担保)
//! 2. **実際の音源**: 検証用プラグインを鳴らして、その出力の読みを出す。
//!    ここは数値を目で見る用 (音源の音量が変われば当然変わる)
//!
//! **1 は本体と同じ経路を通す。** `Meters::drain` はオーディオスレッドから
//! 届く塊を偶数個ずつ取る作りなので、そこを含めて確かめる。
//!
//! 使い方: cargo run -p egui-clap-host --bin meter_smoke -- <path\to\plugin.clap>

use clack_host::prelude::*;
use egui_clap_host::audio::activate_node;
use egui_clap_host::audio::config::StreamAudioConfig;
use egui_clap_host::audio::graph::{self, Graph};
use egui_clap_host::audio::offline::{self, RenderSetup};
use egui_clap_host::discovery;
use egui_clap_host::host::{MiniHost, MiniHostMainThread, MiniHostShared};
use egui_clap_host::meter::{spectrum, Meters, REFERENCE_LUFS};
use egui_clap_host::sequencer::{MidiEditor, Note};
use std::error::Error;
use std::ffi::CString;
use std::path::Path;

const BLOCK_SIZE: u32 = 512;
const CHANNELS: usize = 2;

/// 規格の試験信号の水準と、許される誤差
const TEST_TONE_DBFS: f64 = -23.0;
const TOLERANCE_LU: f32 = 0.1;

fn main() -> Result<(), Box<dyn Error>> {
    let plugin_path = std::env::args()
        .nth(1)
        .ok_or("使い方: meter_smoke <path\\to\\plugin.clap>")?;

    let mut failed = false;

    // ---- 1. 規格の試験信号 ----
    println!("-- EBU Tech 3341 試験信号1 (1kHz {TEST_TONE_DBFS} dBFS) --");
    for rate in [44_100u32, 48_000, 96_000] {
        let (short_term, integrated) = measure_tone(rate);
        // 一定の水準なので S と I は揃うはず (ゲートで何も落ちない)
        for (name, lufs) in [("S", short_term), ("I", integrated)] {
            let off = (lufs - TEST_TONE_DBFS as f32).abs();
            let verdict = if off <= TOLERANCE_LU {
                "OK"
            } else {
                failed = true;
                "**ずれている**"
            };
            println!(
                "  {rate:>6} Hz {name} → {lufs:+7.2} LUFS (期待 {TEST_TONE_DBFS:+.1}) {verdict}"
            );
        }
    }
    println!();

    // ---- 2. 実際の音源 ----
    println!("-- 検証用プラグインの出力 --");
    let Plugin {
        momentary,
        short_term,
        integrated,
        peak_band,
    } = measure_plugin(&plugin_path)?;
    println!("  Momentary : {momentary:+7.2} LUFS");
    println!("  Short-term: {short_term:+7.2} LUFS");
    println!("  Integrated: {integrated:+7.2} LUFS");
    println!(
        "  いちばん強い帯: {:.0} Hz",
        spectrum::Spectrum::center_hz(peak_band)
    );
    println!(
        "  基準 {REFERENCE_LUFS:+.1} LUFS との差: {:+.2} LU",
        short_term - REFERENCE_LUFS
    );

    // 音源はサイン波なので、鳴っている間は必ず基準の下限より上に出る。
    // ここが無音なら経路のどこかが切れている
    if short_term <= -70.0 {
        println!("  ⚠ 無音になっている (経路が切れている可能性)");
        failed = true;
    }

    println!();
    if failed {
        return Err("メーターの検証に失敗".into());
    }
    println!("✅ メーターは規格どおりに測れている");
    Ok(())
}

/// 試験信号をリングバッファ越しに流して (Short-term, Integrated) を測る。
/// **本体と同じく、オーディオ側は塊で押し込み、画面側は毎フレーム取り込む。**
fn measure_tone(sample_rate: u32) -> (f32, f32) {
    let (mut producer, mut consumer) = rtrb::RingBuffer::<f32>::new(1 << 16);
    let mut meters = Meters::new(sample_rate);

    let amplitude = 10f64.powf(TEST_TONE_DBFS / 20.0) as f32;
    let total = sample_rate as usize * 4; // Short-term (3秒) が埋まるまで
    let mut frame = 0usize;

    while frame < total {
        // オーディオスレッド1ブロックぶん
        let block = (BLOCK_SIZE as usize).min(total - frame);
        for _ in 0..block {
            let t = frame as f32 / sample_rate as f32;
            let value = amplitude * (2.0 * std::f32::consts::PI * 1000.0 * t).sin();
            let _ = producer.push(value);
            let _ = producer.push(value);
            frame += 1;
        }
        // 画面1フレームぶん
        meters.drain(&mut consumer, sample_rate, 1.0 / 60.0);
    }
    meters.drain(&mut consumer, sample_rate, 1.0 / 60.0);
    (meters.short_term_lufs(), meters.integrated_lufs())
}

/// 検証用プラグインの出力の読み
struct Plugin {
    momentary: f32,
    short_term: f32,
    integrated: f32,
    /// いちばん強い帯
    peak_band: usize,
}

/// 検証用プラグインを鳴らし、その出力を測る
fn measure_plugin(plugin_path: &str) -> Result<Plugin, Box<dyn Error>> {
    const SAMPLE_RATE: u32 = 44_100;

    let (entry, plugins) = discovery::load_clap_file(Path::new(plugin_path))?;
    let target = &plugins[0];
    println!("  プラグイン: {} ({})", target.name, target.id);

    let stream_config = StreamAudioConfig {
        output_channel_count: CHANNELS,
        min_buffer_size: 1,
        max_likely_buffer_size: BLOCK_SIZE,
        sample_rate: SAMPLE_RATE,
        sample_format: cpal::SampleFormat::F32,
    };

    let mut instance = instantiate(&entry, &target.id)?;
    let node = activate_node(&mut instance, &stream_config)?;
    let mut graph = Graph::new();
    let audio_track = graph::audio_track_for(0).expect("1本なら収まる");
    graph.place_chain(audio_track, graph::MidiSources::one(0), vec![node]);

    // 4秒ぶん鳴らしっぱなしにする (Short-term の窓が埋まるまで)
    let mut editor = MidiEditor::default(); // 120bpm → 四分音符 0.5秒
    editor.notes = vec![Note {
        start_tick: 0.0,
        duration: 8.0,
        semitone: 0,
        octave: 4,
        velocity: 127,
        track: 0,
        lane: 0,
    }];

    let rate = SAMPLE_RATE as f64;
    let spq = editor.samples_per_quarter(rate);
    let setup = RenderSetup {
        sequences: (0..editor.track_count())
            .map(|track| editor.to_events_for_track(track, rate).into_boxed_slice())
            .collect(),
        end_sample: (editor.length_quarters_bar_aligned() as f64 * spq) as u64,
        tail_samples: 0,
        block_frames: BLOCK_SIZE as usize,
        sample_rate: SAMPLE_RATE,
    };
    let rendered = offline::render(&mut graph, setup);

    // 使い終わった処理器はメインスレッドで停止・解放する
    for (_, node) in graph.take_nodes() {
        let egui_clap_host::audio::RetiredProcessor::Clap(stopped) = node.into_retired() else {
            return Err("CLAP を載せたのに別形式が返ってきた".into());
        };
        instance.deactivate(stopped);
    }

    // 描画結果をリングバッファ越しに流す (実時間と同じ経路)
    let (mut producer, mut consumer) = rtrb::RingBuffer::<f32>::new(1 << 16);
    let mut meters = Meters::new(SAMPLE_RATE);
    for block in rendered.samples.chunks(BLOCK_SIZE as usize * CHANNELS) {
        for sample in block {
            let _ = producer.push(*sample);
        }
        meters.drain(&mut consumer, SAMPLE_RATE, 1.0 / 60.0);
    }
    meters.drain(&mut consumer, SAMPLE_RATE, 1.0 / 60.0);

    let levels = meters.spectrum_levels();
    let peak_band = (0..spectrum::BANDS).fold(0, |best, band| {
        if levels[band] > levels[best] {
            band
        } else {
            best
        }
    });
    Ok(Plugin {
        momentary: meters.momentary_lufs(),
        short_term: meters.short_term_lufs(),
        integrated: meters.integrated_lufs(),
        peak_band,
    })
}

fn instantiate(
    entry: &PluginEntry,
    plugin_id: &str,
) -> Result<PluginInstance<MiniHost>, Box<dyn Error>> {
    let host_info = HostInfo::new(
        "meter_smoke",
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
