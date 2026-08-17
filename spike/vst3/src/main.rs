//! フェーズ0: VST3 に進めるかの可否判断用スパイク。**本体には一切触らない。**
//!
//! 確かめること (docs/vst3_host_plan.md 参照):
//!
//! 1. `vst3-host` が Windows でビルドできるか
//! 2. VST3 を読み込んで、オフラインで音を出せるか
//! 3. 処理経路がこのプロジェクトの設計に載るか
//!    - 音源をオーディオスレッドが所有し、指示はロックフリーで渡せるか
//!    - ブロックごとの処理で確保が起きない形か
//!    - サンプル精度でノートを置けるか
//!
//! 使い方:
//!   cargo run -- "C:\Program Files\Common Files\VST3\Surge Synth Team\Surge XT.vst3"

use vst3_host::audio::AudioBuffers;
use vst3_host::midi::{MidiChannel, MidiEvent};
use vst3_host::realtime::RealtimePluginRunner;
use vst3_host::simple;

const SAMPLE_RATE: f64 = 48_000.0;
const BLOCK: usize = 512;
const CHANNELS: usize = 2;

/// 無音とみなす上限 / 鳴っているとみなす下限 (既存の smoke と同じ基準)
const SILENT: f32 = 0.001;
const AUDIBLE: f32 = 0.01;

/// ノートを鳴らすブロック数と、消してから確かめるブロック数
const SOUNDING_BLOCKS: usize = 40;
const RELEASE_BLOCKS: usize = 60;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = std::env::args()
        .nth(1)
        .ok_or("使い方: vst3-spike <path\\to\\plugin.vst3>")?;
    println!("読み込み: {path}");

    // ---- 1. 読み込み ----
    let plugin = simple::load_plugin_with_settings(&path, SAMPLE_RATE, BLOCK)?;
    let info = plugin.info().clone();
    println!("プラグイン: {} / {}", info.name, info.vendor);

    // ---- 2. ロックフリーの実行器を用意する ----
    // 音源は runner が所有し (= オーディオスレッド側)、指示は control 経由で渡す。
    // 本体の「処理器をリングバッファでオーディオスレッドへ渡す」設計と同じ形。
    let (mut runner, mut control) = RealtimePluginRunner::new(plugin, 1024);
    runner.start()?;

    // バッファは一度だけ確保して使い回す (ブロックごとの確保を避けられるか)
    let mut buffers = AudioBuffers::new(0, CHANNELS, BLOCK, SAMPLE_RATE);

    // ---- 3. 発音前・発音中・消音後のピークを測る ----
    let mut silence_peak = 0.0f32;
    for _ in 0..4 {
        buffers.clear();
        runner.process(&mut buffers)?;
        silence_peak = silence_peak.max(peak(&buffers));
    }

    // サンプル精度でノートを置けるか (ブロック内オフセット指定)
    let queued = control.send_midi_at(
        MidiEvent::NoteOn {
            channel: MidiChannel::Ch1,
            note: 60,
            velocity: 100,
        },
        128,
    );

    let mut sounding_peak = 0.0f32;
    for _ in 0..SOUNDING_BLOCKS {
        buffers.clear();
        runner.process(&mut buffers)?;
        sounding_peak = sounding_peak.max(peak(&buffers));
    }

    control.send_midi(MidiEvent::NoteOff {
        channel: MidiChannel::Ch1,
        note: 60,
        velocity: 0,
    });

    // リリースが落ちきるまで回してから、最後の数ブロックを見る
    let mut tail_peak = 0.0f32;
    for block in 0..RELEASE_BLOCKS {
        buffers.clear();
        runner.process(&mut buffers)?;
        if block >= RELEASE_BLOCKS - 4 {
            tail_peak = tail_peak.max(peak(&buffers));
        }
    }

    let dropped = control.dropped_command_count();

    // ---- 4. 後始末 (COM の破棄はスレッド固定なので手順どおりに) ----
    runner.stop()?;
    drop(runner);
    let destroyed = control.service_teardown();

    println!("ピーク: 発音前={silence_peak:.4} 発音中={sounding_peak:.4} 消音後={tail_peak:.4}");
    println!("取りこぼした指示: {dropped} / 破棄完了: {destroyed}");

    // ---- 判定 ----
    let mut failures = Vec::new();
    if !queued {
        failures.push("サンプル精度のノート指定を受け付けなかった".to_string());
    }
    if silence_peak > SILENT {
        failures.push(format!("発音前が無音でない ({silence_peak:.4})"));
    }
    if sounding_peak < AUDIBLE {
        failures.push(format!("ノートを鳴らせていない ({sounding_peak:.4})"));
    }
    if tail_peak > SILENT {
        failures.push(format!("消音後に音が残っている ({tail_peak:.4})"));
    }
    if dropped > 0 {
        failures.push(format!("指示を {dropped} 件取りこぼした"));
    }

    if failures.is_empty() {
        println!("✅ VST3 スパイク成功: 読み込み・発音・消音・後始末が通った");
        Ok(())
    } else {
        Err(format!("❌ 失敗: {}", failures.join(", ")).into())
    }
}

/// 全チャンネルの絶対値ピーク
fn peak(buffers: &AudioBuffers) -> f32 {
    buffers
        .outputs
        .iter()
        .flat_map(|channel| channel.iter())
        .fold(0.0f32, |max, sample| max.max(sample.abs()))
}
