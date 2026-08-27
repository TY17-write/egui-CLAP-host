//! CLAP と VST3 を同じシーケンスの別トラックに載せて同時に鳴らす検証。
//! オーディオデバイス不要。
//!
//! 形式ごとの smoke (`seq_smoke` / `vst3_smoke`) は、それぞれ1形式だけを見ている。
//! 混ぜたときにしか出ない壊れ方があるので、ここで別に確かめる。
//!
//! - **1本のトランスポートで両方が同じ位置を鳴らすか。** CLAP は `steady_time` を
//!   受け取り、VST3 は自分で時刻を組み立てるので、ずれるとしたらここ
//! - **足し合わせが効いているか。** 片方だけ鳴る区間と、両方鳴る区間を作って測る
//! - **書き出しで借りて返す経路が両形式で通るか。** `offline::render` は
//!   `TrackProcessor` しか見ないが、返し方 (`RetiredProcessor`) は形式ごとに違う
//!
//! 使い方:
//!   cargo run -p egui-clap-host --bin mixed_smoke -- <path\to\plugin.clap> <path\to\plugin.vst3> [--same-plugin]
//!
//! `--same-plugin` は、2つが**同じ DSP の別形式**であるときに付ける
//! (clap-wrapper で `test-plugin` を VST3 化したものなど。作り方は README を参照)。
//!
//! 見ているのは**音の大きさが揃うこと**であって、波形が一致することではない。
//! 2つの区間は別の音程を鳴らしているので、波形どうしは比べられない。
//! ベロシティの換算やゲインの取り違えのように「片方の形式だけ音量が変わる」
//! 壊れ方を捕まえるためのもの。逆に言うと、**たまたま音量が近い別の音源を
//! 渡しても通ってしまう** (実測: Surge XT と test-plugin は 1.1% しか違わなかった)。

use clack_host::prelude::*;
use egui_clap_host::audio::config::StreamAudioConfig;
use egui_clap_host::audio::graph::{Graph, MidiSources};
use egui_clap_host::audio::offline::{self, RenderSetup};
use egui_clap_host::audio::transport::{Transport, TransportMsg, TransportShared};
use egui_clap_host::audio::{self};
use egui_clap_host::discovery;
use egui_clap_host::host::{MiniHost, MiniHostMainThread, MiniHostShared};
use egui_clap_host::sequencer::{MidiEditor, Note};
use std::error::Error;
use std::ffi::CString;
use std::path::Path;

const SAMPLE_RATE: f64 = 44_100.0;
const BLOCK_SIZE: usize = 512;
const CHANNELS: usize = 2;

/// 無音とみなす上限 / 鳴っているとみなす下限 (他の smoke と同じ基準)
const SILENT: f32 = 0.001;
const AUDIBLE: f32 = 0.01;

/// 区間の境界がぼやけるぶんを避けるための余白 (ブロック単位)
const MARGIN_BLOCKS: u64 = 3;

/// `--same-plugin` のときに、両形式のピークが一致しているとみなす相対差。
///
/// 同じ DSP なら本来ぴったり揃う (実測では小数4桁まで一致した)。
/// 2% にしてあるのは丸めのためで、ベロシティの換算やゲインの取り違えのような
/// 実際の壊れ方はこれよりずっと大きくずれる。
const SAME_PLUGIN_TOLERANCE: f32 = 0.02;

fn main() -> Result<(), Box<dyn Error>> {
    let mut args = std::env::args().skip(1);
    let clap_path = args
        .next()
        .ok_or("使い方: mixed_smoke <path\\to\\plugin.clap> <path\\to\\plugin.vst3>")?;
    let vst3_path = args
        .next()
        .ok_or("使い方: mixed_smoke <path\\to\\plugin.clap> <path\\to\\plugin.vst3>")?;
    let same_plugin = args.any(|arg| arg == "--same-plugin");

    let stream_config = StreamAudioConfig {
        output_channel_count: CHANNELS,
        min_buffer_size: 1,
        max_likely_buffer_size: BLOCK_SIZE as u32,
        sample_rate: SAMPLE_RATE as u32,
        sample_format: cpal::SampleFormat::F32,
    };

    // ---- トラック0: CLAP ----
    let (entry, clap_found) = discovery::load_clap_file(Path::new(&clap_path))?;
    let clap_target = &clap_found[0];
    println!(
        "トラック1 (CLAP): {} ({})",
        clap_target.name, clap_target.id
    );
    let mut clap_instance = instantiate_clap(&entry, &clap_target.id)?;
    let clap_node = audio::activate_node(&mut clap_instance, &stream_config)?;

    // ---- トラック1: VST3 ----
    let vst3_path = Path::new(&vst3_path);
    let vst3_found = discovery::load_vst3_file(vst3_path)?;
    let vst3_target = &vst3_found[0];
    println!(
        "トラック2 (VST3): {} ({})",
        vst3_target.name, vst3_target.id
    );
    let (vst3_shared, vst3_node) =
        audio::activate_vst3_node(vst3_path, &vst3_target.id, &stream_config)?;

    // オーディオトラックは 0 がマスターなので、打ち込み 0/1 は 1/2 に載る
    let mut graph = Graph::new();
    graph.place_chain(1, MidiSources::one(0), vec![clap_node]);
    graph.place_chain(2, MidiSources::one(1), vec![vst3_node]);

    // ---- シーケンス ----
    // 0〜1拍: CLAP だけ / 1〜2拍: 両方 / 2〜3拍: VST3 だけ / 3〜4拍: 休み
    // 「片方だけ」の区間があるので、混ざっていないトラックはそこで露見する。
    let mut editor = MidiEditor::default(); // 120bpm → 四分音符 = 22050 サンプル
    editor.add_track();
    editor.notes = vec![
        Note {
            start_tick: 0.0,
            duration: 2.0,
            semitone: 0,
            octave: 4,
            velocity: 127,
            track: 0,
            lane: 0,
        },
        Note {
            start_tick: 1.0,
            duration: 2.0,
            semitone: 7,
            octave: 4,
            velocity: 127,
            track: 1,
            lane: 0,
        },
    ];

    let spq = editor.samples_per_quarter(SAMPLE_RATE);
    let end_sample = (editor.length_quarters_bar_aligned() as f64 * spq) as u64;

    // ---- 1. 実時間の経路: 1本のトランスポートで両方を回す ----
    let realtime = run_realtime(&mut graph, &editor, end_sample)?;

    // ---- 2. 書き出しの経路: 同じシーケンスをオフラインで回す ----
    let setup = RenderSetup {
        sequences: (0..editor.track_count())
            .map(|track| {
                editor
                    .to_events_for_track(track, SAMPLE_RATE)
                    .into_boxed_slice()
            })
            .collect(),
        end_sample,
        tail_samples: (offline::TAIL_SECONDS * SAMPLE_RATE) as u64,
        block_frames: BLOCK_SIZE,
        sample_rate: SAMPLE_RATE as u32,
    };
    let rendered = offline::render(&mut graph, setup);
    let offline_peaks = region_peaks(&rendered.samples, spq);

    // ---- 後始末: 形式ごとに返し方が違う ----
    let mut clap_stopped = None;
    for (addr, node) in graph.take_nodes() {
        match node.into_retired() {
            audio::RetiredProcessor::Clap(stopped) => clap_stopped = Some(stopped),
            audio::RetiredProcessor::Vst3(shared) => {
                if addr.track != 2 {
                    return Err("VST3 の処理器がオーディオトラック2 以外から返ってきた".into());
                }
                shared.lock().stop_processing()?;
            }
        }
    }
    let clap_stopped = clap_stopped.ok_or("CLAP の処理器が返ってこなかった")?;
    clap_instance.deactivate(clap_stopped);
    drop(vst3_shared);

    // ---- 判定 ----
    println!(
        "実時間  : CLAPのみ={:.4} 両方={:.4} VST3のみ={:.4} 休み={:.4}",
        realtime[0], realtime[1], realtime[2], realtime[3]
    );
    println!(
        "書き出し: CLAPのみ={:.4} 両方={:.4} VST3のみ={:.4} 休み={:.4}",
        offline_peaks[0], offline_peaks[1], offline_peaks[2], offline_peaks[3]
    );
    println!(
        "書き出しの長さ: {:.2} 秒 / ピーク {:.4}",
        rendered.seconds(),
        rendered.peak
    );

    let mut failures = Vec::new();
    for failure in &rendered.failures {
        failures.push(format!(
            "書き出しでトラック {} の処理が失敗: {} ({} ブロック)",
            failure.track + 1,
            failure.message,
            failure.blocks
        ));
    }
    for (label, peaks) in [("実時間", &realtime), ("書き出し", &offline_peaks)] {
        if peaks[0] < AUDIBLE {
            failures.push(format!(
                "{label}: CLAP だけの区間が無音 (トラック1 が鳴っていない)"
            ));
        }
        if peaks[2] < AUDIBLE {
            failures.push(format!(
                "{label}: VST3 だけの区間が無音 (トラック2 が鳴っていない)"
            ));
        }
        if peaks[3] > SILENT {
            failures.push(format!("{label}: 休みの区間で音が出ている"));
        }
        // 足し合わせが効いていれば、重なった区間は片方より大きくなる。
        // 片方を上書きしていると、ここが max(片方) 止まりになる。
        let louder = peaks[0].max(peaks[2]);
        if peaks[1] <= louder {
            failures.push(format!(
                "{label}: 重なった区間 ({:.4}) が片方だけの区間 ({louder:.4}) を超えていない。\
                 足し合わせではなく上書きになっている可能性がある",
                peaks[1]
            ));
        }

        // 同じ DSP の別形式なら、片方だけの区間は同じ大きさになるはず
        if same_plugin {
            let difference = (peaks[0] - peaks[2]).abs();
            if difference > louder * SAME_PLUGIN_TOLERANCE {
                failures.push(format!(
                    "{label}: 同じ音源のはずなのに CLAP ({:.4}) と VST3 ({:.4}) で音量が違う \
                     (差 {difference:.4})。どちらかの形式でベロシティかゲインの\
                     扱いが食い違っている",
                    peaks[0], peaks[2]
                ));
            }
        }
    }

    if failures.is_empty() {
        if same_plugin {
            println!("✅ CLAP と VST3 の混在テスト成功 (同じ音源で音量も一致)");
        } else {
            println!("✅ CLAP と VST3 の混在テスト成功");
        }
        Ok(())
    } else {
        Err(format!("❌ 失敗: {}", failures.join(", ")).into())
    }
}

/// 実時間と同じ手順 (1本のトランスポート + ブロックごとの発行) で最後まで回し、
/// 区間ごとのピークを返す。
fn run_realtime(
    graph: &mut Graph,
    editor: &MidiEditor,
    end_sample: u64,
) -> Result<[f32; 4], Box<dyn Error>> {
    let mut transport = Transport::new(TransportShared::new());
    for track in 0..editor.track_count() {
        let _ = transport.handle_msg(TransportMsg::SetSequence {
            track,
            events: editor
                .to_events_for_track(track, SAMPLE_RATE)
                .into_boxed_slice(),
            end_sample,
        });
    }
    let _ = transport.handle_msg(TransportMsg::Play);

    // **本体の再生と同じ `Graph` を通す。** ここだけ違う組み方をすると、
    // 実時間と書き出しを突き合わせる意味が無くなる
    graph.reserve(BLOCK_SIZE);
    let mut samples = Vec::with_capacity(end_sample as usize * CHANNELS + BLOCK_SIZE * CHANNELS);

    let mut position = 0u64;
    while position < end_sample {
        let plan = transport.plan_block(BLOCK_SIZE as u64);
        graph.clear_events();
        graph.emit_from(&mut transport, &plan);

        let mut failure = None;
        graph.process(position, BLOCK_SIZE, &mut |track, e| {
            failure.get_or_insert_with(|| format!("オーディオトラック {track} の処理に失敗: {e}"));
        });
        if let Some(failure) = failure {
            return Err(failure.into());
        }

        samples.extend_from_slice(graph.master(BLOCK_SIZE));
        position += BLOCK_SIZE as u64;
    }

    let spq = editor.samples_per_quarter(SAMPLE_RATE);
    Ok(region_peaks(&samples, spq))
}

/// 拍ごとの4区間 [CLAPのみ, 両方, VST3のみ, 休み] のピーク。
/// 境界のブロックは数えない (発音・消音がそこに乗るため)。
fn region_peaks(samples: &[f32], samples_per_quarter: f64) -> [f32; 4] {
    let margin = MARGIN_BLOCKS * BLOCK_SIZE as u64;
    std::array::from_fn(|region| {
        let from = (region as f64 * samples_per_quarter) as u64 + margin;
        let to = ((region + 1) as f64 * samples_per_quarter) as u64;
        peak_between(samples, from, to.saturating_sub(margin))
    })
}

/// インターリーブ済みバッファの、フレーム [from, to) の絶対値ピーク
fn peak_between(samples: &[f32], from: u64, to: u64) -> f32 {
    let start = (from as usize * CHANNELS).min(samples.len());
    let end = (to as usize * CHANNELS).min(samples.len());
    if start >= end {
        return 0.0;
    }
    samples[start..end]
        .iter()
        .fold(0.0f32, |max, sample| max.max(sample.abs()))
}

fn instantiate_clap(
    entry: &PluginEntry,
    plugin_id: &str,
) -> Result<PluginInstance<MiniHost>, Box<dyn Error>> {
    let host_info = HostInfo::new(
        "Mixed Smoke",
        "egui-clap-host",
        "https://example.com",
        "0.1.0",
    )?;
    let plugin_id = CString::new(plugin_id)?;
    let (sender, _receiver) = crossbeam_channel::unbounded();

    Ok(PluginInstance::<MiniHost>::new(
        |_| MiniHostShared::new(sender.clone()),
        |shared| MiniHostMainThread::new(shared),
        entry,
        &plugin_id,
        &host_info,
    )?)
}
