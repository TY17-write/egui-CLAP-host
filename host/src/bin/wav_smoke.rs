//! WAV 書き出し (オフラインレンダリング) の検証。オーディオデバイス不要。
//!
//! 同じプラグインを2トラックに載せ、
//!   トラック1: C4 [0〜1拍) / トラック2: G4 [2〜3拍)
//! というシーケンスをレンダリングして、
//! 「鳴るべき区間で鳴り、休符区間は無音」「末尾の無音が落ちている」
//! 「WAV のヘッダが中身と一致する」をサンプル位置で検証する。
//!
//! 使い方: cargo run -p clap-host-test --bin wav_smoke -- <path\to\plugin.clap> [out.wav]

use clack_host::prelude::*;
use clap_host_test::audio::config::StreamAudioConfig;
use clap_host_test::audio::offline::{self, RenderSetup};
use clap_host_test::audio::{activate_track, TrackProcessor};
use clap_host_test::discovery;
use clap_host_test::host::{MiniHost, MiniHostMainThread, MiniHostShared};
use clap_host_test::sequencer::{MidiEditor, Note};
use clap_host_test::wav;
use std::error::Error;
use std::ffi::CString;
use std::path::{Path, PathBuf};

const SAMPLE_RATE: u32 = 44_100;
const BLOCK_SIZE: u32 = 512;
const CHANNELS: usize = 2;

/// 無音とみなす上限 / 鳴っているとみなす下限
const SILENT: f32 = 0.001;
const AUDIBLE: f32 = 0.01;

fn main() -> Result<(), Box<dyn Error>> {
    let mut args = std::env::args().skip(1);
    let plugin_path = args
        .next()
        .ok_or("使い方: wav_smoke <path\\to\\plugin.clap> [out.wav]")?;
    let out_path = args
        .next()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("target/wav_smoke.wav"));

    let (entry, plugins) = discovery::load_clap_file(Path::new(&plugin_path))?;
    let target = &plugins[0];
    println!("プラグイン: {} ({})", target.name, target.id);

    // デバイスを開かずに構成を作る (GUI ホストが使うものと同じ形)
    let stream_config = StreamAudioConfig {
        output_channel_count: CHANNELS,
        min_buffer_size: 1,
        max_likely_buffer_size: BLOCK_SIZE,
        sample_rate: SAMPLE_RATE,
        sample_format: cpal::SampleFormat::F32,
    };

    // トラックごとに1インスタンス (ホスト本体と同じ構成にする)
    let mut instances = Vec::new();
    let mut processors: Vec<(usize, Box<TrackProcessor>)> = Vec::new();
    for track in 0..2 {
        let mut instance = instantiate(&entry, &target.id)?;
        let processor = activate_track(&mut instance, &stream_config)?;
        instances.push(instance);
        processors.push((track, processor));
    }

    // ---- シーケンス: トラック1 に C4 [0,1拍) / トラック2 に G4 [2,3拍) @120bpm ----
    let mut editor = MidiEditor::default(); // 120bpm → 四分音符 = 22050 サンプル
    editor.add_track();
    editor.notes = vec![
        Note {
            start_tick: 0.0,
            duration: 1.0,
            semitone: 0,
            octave: 4,
            velocity: 127,
            track: 0,
            lane: 0,
        },
        Note {
            start_tick: 2.0,
            duration: 1.0,
            semitone: 7,
            octave: 4,
            velocity: 127,
            track: 1,
            lane: 0,
        },
    ];

    let rate = SAMPLE_RATE as f64;
    let spq = editor.samples_per_quarter(rate); // 22050
    let end_sample = (editor.length_quarters_bar_aligned() as f64 * spq) as u64; // 4拍 = 88200

    let setup = RenderSetup {
        sequences: (0..editor.track_count())
            .map(|track| editor.to_events_for_track(track, rate).into_boxed_slice())
            .collect(),
        end_sample,
        tail_samples: (offline::TAIL_SECONDS * rate) as u64,
        block_frames: BLOCK_SIZE as usize,
        channels: CHANNELS,
        sample_rate: SAMPLE_RATE,
    };

    let rendered = offline::render(&mut processors, setup);

    // 使い終わった処理器はメインスレッドで停止・解放する
    for ((_, processor), mut instance) in processors.into_iter().zip(instances) {
        let Some(clap_host_test::audio::RetiredProcessor::Clap(stopped)) =
            processor.into_single_retired()
        else {
            return Err("CLAP 1段を載せたのに別のものが返ってきた".into());
        };
        instance.deactivate(stopped);
    }

    // ---- 区間ごとのピークを測る ----
    // [C4区間, 休符区間, G4区間, 終端手前]
    let boundaries = [0u64, 22_050, 44_100, 66_150, 88_200];
    let margin = BLOCK_SIZE as u64 * 2; // 境界ブロックの曖昧さを除く
    let peaks: Vec<f32> = (0..4)
        .map(|region| {
            peak_between(
                &rendered.samples,
                CHANNELS,
                boundaries[region] + margin,
                boundaries[region + 1] - margin,
            )
        })
        .collect();

    let frames = rendered.samples.len() / CHANNELS;
    println!(
        "ピーク値: C4区間={:.4} 休符区間={:.4} G4区間={:.4} 終端手前={:.4}",
        peaks[0], peaks[1], peaks[2], peaks[3]
    );
    println!(
        "長さ: {frames} フレーム ({:.2} 秒) / 全体ピーク: {:.4}",
        rendered.seconds(),
        rendered.peak
    );

    // ---- WAV に書き出して読み直す ----
    let bytes = wav::to_bytes_16bit(&rendered.samples, CHANNELS as u16, SAMPLE_RATE)?;
    if let Some(parent) = out_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&out_path, &bytes)?;
    let written = std::fs::read(&out_path)?;
    println!(
        "書き出し: {} ({} バイト)",
        out_path.display(),
        written.len()
    );

    // ---- 判定 ----
    let mut failures = Vec::new();
    // 音源の処理そのものが失敗していたら、以降のピーク判定は意味を持たない
    for failure in &rendered.failures {
        failures.push(format!(
            "トラック {} の処理が失敗: {} ({} ブロック)",
            failure.track + 1,
            failure.message,
            failure.blocks
        ));
    }
    if peaks[0] < AUDIBLE {
        failures.push("トラック1 の C4 区間が無音".to_string());
    }
    if peaks[1] > SILENT {
        failures.push("休符区間で音が出ている".to_string());
    }
    if peaks[2] < AUDIBLE {
        failures.push("トラック2 の G4 区間が無音".to_string());
    }
    if peaks[3] > SILENT {
        failures.push("終端手前で音が出ている".to_string());
    }
    // 最後のノートは 66150 で終わるので、末尾の無音が落ちて終端ちょうどになるはず
    if frames as u64 != end_sample {
        failures.push(format!(
            "末尾の無音が落ちていない (期待 {end_sample} フレーム, 実際 {frames})"
        ));
    }
    if rendered.peak > 1.0 {
        failures.push(format!("ピークが 1.0 を超えている ({:.3})", rendered.peak));
    }
    if let Err(e) = check_wav_header(&written, CHANNELS as u16, SAMPLE_RATE, frames) {
        failures.push(e);
    }

    if failures.is_empty() {
        println!("✅ WAV 書き出しテスト成功");
        Ok(())
    } else {
        Err(format!("❌ 失敗: {}", failures.join(", ")).into())
    }
}

fn instantiate(
    entry: &PluginEntry,
    plugin_id: &str,
) -> Result<PluginInstance<MiniHost>, Box<dyn Error>> {
    let host_info = HostInfo::new(
        "WAV Smoke",
        "clap-host-test",
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

/// インターリーブ済みバッファの、フレーム [from, to) の絶対値ピーク
fn peak_between(samples: &[f32], channels: usize, from: u64, to: u64) -> f32 {
    let start = (from as usize * channels).min(samples.len());
    let end = (to as usize * channels).min(samples.len());
    samples[start..end]
        .iter()
        .fold(0.0f32, |max, s| max.max(s.abs()))
}

/// WAV ヘッダが実際の中身と食い違っていないか確かめる
fn check_wav_header(
    bytes: &[u8],
    channels: u16,
    sample_rate: u32,
    frames: usize,
) -> Result<(), String> {
    if bytes.len() < 44 {
        return Err("WAV が短すぎる".into());
    }
    let u32_at = |at: usize| u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap());
    let u16_at = |at: usize| u16::from_le_bytes(bytes[at..at + 2].try_into().unwrap());

    if &bytes[0..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
        return Err("RIFF/WAVE のマジックが違う".into());
    }
    if u16_at(22) != channels {
        return Err(format!("チャンネル数が違う ({})", u16_at(22)));
    }
    if u32_at(24) != sample_rate {
        return Err(format!("サンプルレートが違う ({})", u32_at(24)));
    }
    let expected = frames * channels as usize * 2;
    if u32_at(40) as usize != expected {
        return Err(format!(
            "データ長が違う (ヘッダ {} / 期待 {expected})",
            u32_at(40)
        ));
    }
    if bytes.len() != 44 + expected {
        return Err(format!("ファイル長が違う ({})", bytes.len()));
    }
    Ok(())
}
