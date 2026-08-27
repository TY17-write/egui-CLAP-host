//! トラック内のチェーン (音源 → エフェクト → …) の検証。オーディオデバイス不要。
//!
//! **ここまでホストはエフェクトに音を入れられなかった** (入力バッファを 0 で
//! 埋めていた)。`docs/archive/routing_plan.md` のフェーズ1 で通した経路を、
//! 本体と同じ `TrackProcessor` を使って確かめる。
//!
//! 治具は `test_plugin.clap` の3つ。音源 (`Test Sine Synth`)、
//! エフェクト (`Test Gain Effect`、`out = in * gain + offset`、`gain` の既定 0.5)、
//! **出力を持たない**モニタ (`Test Monitor`)。
//!
//! 見るのは5つ。
//!
//! 1. **エフェクトに音が入ること** — 音源だけの音量に対して、後ろに ×0.5 を
//!    1段刺すと半分になる
//! 2. **2段を順に通ること** — ×0.5 を2段で 1/4 になる
//! 3. **パラメータが段ごとに効くこと** — 1段目だけ ×2.0 にすると、
//!    2段目は既定の ×0.5 のままで差し引き等倍になる。**全段へ配ってしまうと
//!    4倍になる**ので、ここで気付ける
//! 4. **入力ポートを持たない段が、そこまでの音を捨てること** — 音源を2段
//!    重ねても2倍にならない (足し込みではなく上書きであること)
//! 5. **出力ポートを持たない段で音が途切れないこと** — モニタリング系
//!    (アナライザ・チューナー) を刺しても音量が変わらず、**なおかつ
//!    モニタ側には音が届いている**こと。段ごと飛ばす実装でも出音は同じに
//!    なるので、モニタが見たピークを本人から聞いて区別する
//!
//! `.vst3` を第2引数に渡すと、**VST3 のエフェクトを混ぜたチェーン**も試す。
//! CLAP と VST3 では入力バッファの渡し方が全く違う (ポート連続配置 vs
//! チャンネルごとの `Vec`) ので、片方が通っても他方は分からない。
//!
//! ```text
//! cargo run -p egui-clap-host --bin chain_smoke -- target\debug\test_plugin.clap
//!
//! # VST3 も見る (clap-wrapper で作った test_plugin.vst3。作り方は guide.md)
//! $env:CLAP_PATH = "$PWD\target\debug"
//! cargo run -p egui-clap-host --bin chain_smoke -- target\debug\test_plugin.clap target\test_plugin.vst3
//! ```

use clack_host::prelude::*;
use egui_clap_host::audio::config::StreamAudioConfig;
use egui_clap_host::audio::events::BlockEvent;
use egui_clap_host::audio::{self, TrackProcessor};
use egui_clap_host::discovery;
use egui_clap_host::host::{MiniHost, MiniHostMainThread, MiniHostShared};
use egui_clap_host::params;
use std::error::Error;
use std::ffi::CString;
use std::path::Path;

const SAMPLE_RATE: u32 = 48_000;
const BLOCK_SIZE: u32 = 256;
const CHANNELS: usize = 2;

/// 波形が安定するまで回すブロック数。サイン波なので数ブロックで足りる
const BLOCKS: usize = 8;

const SINE_ID: &str = "com.example.test-sine";
const GAIN_ID: &str = "com.example.test-gain";
/// 出力ポートを持たないモニタ (アナライザ相当)
const MONITOR_ID: &str = "com.example.test-monitor";

/// 比率の許容差。段ごとの丸めだけを吸収する幅
const TOLERANCE: f32 = 0.02;

/// チェーンの1段の指定
#[derive(Clone, Copy)]
enum Stage<'a> {
    /// CLAP のプラグイン ID
    Clap(&'a str),
    /// VST3 のバンドルと、その中のクラス名 (名前で引く)
    Vst3 { path: &'a Path, name: &'a str },
}

fn main() -> Result<(), Box<dyn Error>> {
    let mut args = std::env::args().skip(1);
    let path = args
        .next()
        .ok_or("使い方: chain_smoke <path\\to\\test_plugin.clap> [path\\to\\test_plugin.vst3]")?;
    let vst3_path = args.next();

    let (entry, plugins) = discovery::load_clap_file(Path::new(&path))?;
    for plugin in &plugins {
        println!("  発見: {} ({})", plugin.name, plugin.id);
    }
    if !plugins.iter().any(|plugin| plugin.id == GAIN_ID) {
        return Err(format!("{GAIN_ID} がありません。test-plugin を作り直してください").into());
    }
    println!();

    let stream_config = StreamAudioConfig {
        output_channel_count: CHANNELS,
        min_buffer_size: 1,
        max_likely_buffer_size: BLOCK_SIZE,
        sample_rate: SAMPLE_RATE,
        sample_format: cpal::SampleFormat::F32,
    };

    // エフェクトのパラメータ ID を名前から引く (プラグイン側の定数を写さない)
    let gain_param = {
        let mut probe = instantiate(&entry, GAIN_ID)?;
        params::read_params(&mut probe)
            .into_iter()
            .find(|param| param.name == "Gain")
            .map(|param| u32::from(param.id))
            .ok_or("エフェクトに Gain パラメータがありません")?
    };

    let sine = Stage::Clap(SINE_ID);
    let gain = Stage::Clap(GAIN_ID);
    let mut failures = Vec::new();

    // ---- 1. 音源だけ (基準) ----
    let plain = run_chain(&entry, &stream_config, &[sine], &[])?.peak;
    println!("音源のみ                   : ピーク {plain:.4}");
    if plain < 0.01 {
        return Err("音源が鳴っていません。先に smoke で確かめてください".into());
    }

    // ---- 2. 音源 → ×0.5 ----
    let one_stage = run_chain(&entry, &stream_config, &[sine, gain], &[])?.peak;
    let ratio = one_stage / plain;
    println!("音源 → ×0.5                : ピーク {one_stage:.4} (基準比 {ratio:.3}、期待 0.500)");
    if (ratio - 0.5).abs() > TOLERANCE {
        failures.push(format!(
            "CLAP エフェクトに音が入っていません (基準比 {ratio:.3}、期待 0.5)"
        ));
    }

    // ---- 3. 音源 → ×0.5 → ×0.5 ----
    let two_stages = run_chain(&entry, &stream_config, &[sine, gain, gain], &[])?.peak;
    let ratio = two_stages / plain;
    println!("音源 → ×0.5 → ×0.5         : ピーク {two_stages:.4} (基準比 {ratio:.3}、期待 0.250)");
    if (ratio - 0.25).abs() > TOLERANCE {
        failures.push(format!(
            "2段目が効いていません (基準比 {ratio:.3}、期待 0.25)"
        ));
    }

    // ---- 4. 1段目だけ ×2.0 にする (2段目は既定の ×0.5 のまま) ----
    // 全段へ配ってしまうと ×2.0 が2段で基準比 4.0 になる
    let addressed = run_chain(
        &entry,
        &stream_config,
        &[sine, gain, gain],
        &[(1, gain_param, 2.0)],
    )?
    .peak;
    let ratio = addressed / plain;
    println!("音源 → ×2.0 → ×0.5         : ピーク {addressed:.4} (基準比 {ratio:.3}、期待 1.000)");
    if (ratio - 1.0).abs() > TOLERANCE {
        let hint = if ratio > 3.0 {
            " ← 全段に配られています"
        } else {
            ""
        };
        failures.push(format!(
            "パラメータの宛先が効いていません (基準比 {ratio:.3}、期待 1.0){hint}"
        ));
    }

    // ---- 5. 音源を2段重ねる (2段目が1段目の音を捨てること) ----
    let stacked = run_chain(&entry, &stream_config, &[sine, sine], &[])?.peak;
    let ratio = stacked / plain;
    println!("音源 → 音源                : ピーク {stacked:.4} (基準比 {ratio:.3}、期待 1.000)");
    if (ratio - 1.0).abs() > TOLERANCE {
        let hint = if ratio > 1.8 {
            " ← 足し込まれています"
        } else {
            ""
        };
        failures.push(format!(
            "入力を持たない段が前の音を捨てていません (基準比 {ratio:.3}、期待 1.0){hint}"
        ));
    }

    // ---- 6. 出力を持たない段 (モニタリング系) ----
    //
    // アナライザ・チューナー・メーターは出力バスを持たない。ホストは器として
    // ステレオのバッファを渡すが、プラグインはそこへ何も書かない。
    // **その空のバッファを採ると、そこから後ろが無音になる。**
    if plugins.iter().any(|plugin| plugin.id == MONITOR_ID) {
        let monitor = Stage::Clap(MONITOR_ID);

        let tapped = run_chain(&entry, &stream_config, &[sine, monitor], &[])?;
        let ratio = tapped.peak / plain;
        println!(
            "音源 → モニタ              : ピーク {:.4} (基準比 {ratio:.3}、期待 1.000)",
            tapped.peak
        );
        if (ratio - 1.0).abs() > TOLERANCE {
            let hint = if ratio < 0.05 {
                " ← 出力を持たない段で音が途切れています"
            } else {
                ""
            };
            failures.push(format!(
                "出力を持たない段が素通しになっていません (基準比 {ratio:.3}、期待 1.0){hint}"
            ));
        }

        // **素通しでも音は見えていること。** 段ごと飛ばす実装でも出音は
        // 同じになるので、出音だけでは区別がつかない
        match tapped.observed {
            Some(observed) => {
                let seen = observed / plain;
                println!(
                    "  └ モニタが見た音         : ピーク {observed:.4} (基準比 {seen:.3}、期待 1.000)"
                );
                if (seen - 1.0).abs() > TOLERANCE {
                    let hint = if observed < 0.001 {
                        " ← 段ごと飛ばされています"
                    } else {
                        ""
                    };
                    failures.push(format!(
                        "モニタに音が届いていません (基準比 {seen:.3}、期待 1.0){hint}"
                    ));
                }
            }
            None => failures.push("モニタの観測値を読めませんでした".into()),
        }

        // 後ろの段が生きていること (素通しが「以降を無視」になっていない)
        let behind = run_chain(&entry, &stream_config, &[sine, monitor, gain], &[])?.peak;
        let ratio = behind / plain;
        println!("音源 → モニタ → ×0.5       : ピーク {behind:.4} (基準比 {ratio:.3}、期待 0.500)");
        if (ratio - 0.5).abs() > TOLERANCE {
            failures.push(format!(
                "モニタの後ろの段が効いていません (基準比 {ratio:.3}、期待 0.5)"
            ));
        }
    } else {
        println!("\n(モニタは未検証。test-plugin を作り直すと一緒に見ます)");
    }

    // ---- 7. VST3 のエフェクトを混ぜる (渡されたときだけ) ----
    //
    // **CLAP が通っても VST3 は分からない。** 入力バッファの渡し方が別物
    // (ポート連続配置 vs チャンネルごとの `Vec`) で、実装も別にある。
    if let Some(vst3_path) = &vst3_path {
        let vst3_path = Path::new(vst3_path);
        let vst3_gain = Stage::Vst3 {
            path: vst3_path,
            name: "Test Gain Effect",
        };

        let mixed = run_chain(&entry, &stream_config, &[sine, vst3_gain], &[])?.peak;
        let ratio = mixed / plain;
        println!("音源 → ×0.5(VST3)          : ピーク {mixed:.4} (基準比 {ratio:.3}、期待 0.500)");
        if (ratio - 0.5).abs() > TOLERANCE {
            let hint = if ratio < 0.05 {
                " ← 音が入っていません"
            } else {
                ""
            };
            failures.push(format!(
                "VST3 エフェクトに音が入っていません (基準比 {ratio:.3}、期待 0.5){hint}"
            ));
        }

        // CLAP と VST3 を混ぜた3段。どちらの向きの受け渡しも通ること
        let both = run_chain(&entry, &stream_config, &[sine, vst3_gain, gain], &[])?.peak;
        let ratio = both / plain;
        println!("音源 → ×0.5(VST3) → ×0.5   : ピーク {both:.4} (基準比 {ratio:.3}、期待 0.250)");
        if (ratio - 0.25).abs() > TOLERANCE {
            failures.push(format!(
                "VST3 の出力を CLAP が受け取れていません (基準比 {ratio:.3}、期待 0.25)"
            ));
        }
    } else {
        println!("\n(VST3 は未検証。第2引数に .vst3 を渡すと一緒に見ます)");
    }

    if failures.is_empty() {
        println!("\n✅ チェーンは順に通り、パラメータは段ごとに効いている");
        Ok(())
    } else {
        for failure in &failures {
            eprintln!("❌ {failure}");
        }
        Err("チェーンの挙動が期待と違います".into())
    }
}

/// チェーンを1回まわした結果
struct ChainResult {
    /// トラックから出てきた音のピーク
    peak: f32,
    /// モニタの段が**入力として見た**ピーク。モニタを刺していなければ `None`。
    ///
    /// 素通しになっていれば出音は変わらないので、**出音だけでは
    /// 「見せたうえで素通しした」のか「段ごと飛ばした」のか区別できない**。
    /// 飛ばす実装でも出音は同じになるため、中から言ってもらう。
    observed: Option<f32>,
}

/// `stages` の順にチェーンを組み、C4 を鳴らして結果を返す。
///
/// `params` は `(段, パラメータ ID, 値)`。最初のブロックの先頭で送る。
fn run_chain(
    entry: &PluginEntry,
    stream_config: &StreamAudioConfig,
    stages: &[Stage],
    params: &[(usize, u32, f64)],
) -> Result<ChainResult, Box<dyn Error>> {
    // 段と同じ並びで持つ。VST3 は CLAP のインスタンスを持たないので None
    let mut owners: Vec<Option<PluginInstance<MiniHost>>> = Vec::with_capacity(stages.len());
    for stage in stages {
        owners.push(match stage {
            Stage::Clap(id) => Some(instantiate(entry, id)?),
            Stage::Vst3 { .. } => None,
        });
    }

    let mut nodes = Vec::with_capacity(stages.len());
    for (stage, owner) in stages.iter().zip(owners.iter_mut()) {
        let node = match (stage, owner) {
            (Stage::Clap(_), Some(instance)) => audio::activate_node(instance, stream_config)?,
            (Stage::Vst3 { path, name }, _) => {
                let found = discovery::load_vst3_file(path)?;
                let class = found
                    .iter()
                    .find(|plugin| plugin.name == *name)
                    .ok_or_else(|| format!("{name} が {} にありません", path.display()))?;
                // SharedPlugin は捨ててよい (処理器が同じ音源を指している)
                audio::activate_vst3_node(path, &class.id, stream_config)?.1
            }
            (Stage::Clap(_), None) => return Err("CLAP の段にインスタンスがありません".into()),
        };
        nodes.push(node);
    }

    let mut nodes = nodes.into_iter();
    let mut track = TrackProcessor::from_node(nodes.next().ok_or("チェーンが空です")?);
    for node in nodes {
        track.push_node(node);
    }

    let block_len = BLOCK_SIZE as usize * CHANNELS;
    let mut buffer = vec![0.0f32; block_len];
    let mut peak = 0.0f32;

    for block in 0..BLOCKS {
        let events = track.events_mut();
        events.clear();
        if block == 0 {
            // C4。ベロシティは最大にして、段ごとの減衰を読みやすくする
            events.push(BlockEvent::NoteOn {
                offset: 0,
                key: 60,
                velocity: 1.0,
            });
            for (node, id, value) in params {
                events.push(BlockEvent::Param {
                    offset: 0,
                    node: *node,
                    id: *id,
                    value: *value,
                });
            }
        }

        // 呼び出し側が 0 で埋めてから渡す約束 (本体の再生・書き出しと同じ)
        buffer.fill(0.0);
        track.process((block * BLOCK_SIZE as usize) as u64, &mut buffer)?;

        // 1ブロック目は音の立ち上がりで値が安定しないので数えない
        if block > 0 {
            peak = buffer.iter().fold(peak, |max, s| max.max(s.abs()));
        }
    }

    // モニタの段が何を見たかを聞く。**始末する前に読む**
    let mut observed = None;
    for (stage, owner) in stages.iter().zip(owners.iter_mut()) {
        let (Stage::Clap(MONITOR_ID), Some(instance)) = (stage, owner) else {
            continue;
        };
        observed = params::read_params(instance)
            .into_iter()
            .find(|param| param.name == "Observed Peak")
            .map(|param| param.value as f32);
    }

    // 借りた処理器を始末する。**形式ごとにやり方が違う** (CLAP はインスタンスへ
    // 返して初めて解放でき、VST3 はメインスレッドで停止する)
    for (retired, owner) in track.into_retired().into_iter().zip(owners) {
        match (retired, owner) {
            (audio::RetiredProcessor::Clap(stopped), Some(mut instance)) => {
                instance.deactivate(stopped)
            }
            (audio::RetiredProcessor::Vst3(shared), None) => {
                shared.lock().stop_processing()?;
            }
            _ => return Err("処理器と段の形式が食い違っています".into()),
        }
    }

    Ok(ChainResult { peak, observed })
}

fn instantiate(entry: &PluginEntry, id: &str) -> Result<PluginInstance<MiniHost>, Box<dyn Error>> {
    let host_info = HostInfo::new(
        "Chain Smoke",
        "egui-clap-host",
        "https://example.com",
        "0.1.0",
    )?;
    let plugin_id = CString::new(id)?;
    let (sender, _receiver) = crossbeam_channel::unbounded();

    Ok(PluginInstance::<MiniHost>::new(
        |_| MiniHostShared::new(sender.clone()),
        |shared| MiniHostMainThread::new(shared),
        entry,
        &plugin_id,
        &host_info,
    )?)
}
