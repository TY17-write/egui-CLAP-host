//! 検証用エフェクト (`Test Gain Effect`) が主張どおり計算するかを確かめる。
//! オーディオデバイスも音源も不要。
//!
//! **本体の経路を通さず、clack を直接叩く。** エフェクトの計算そのものだけを
//! 見たいので、**入力バッファに既知の信号を置き**、出てきた値を突き合わせる
//! (本体のチェーンを通した検証は `chain_smoke` の担当)。
//!
//! 見るのは3つ。
//!
//! 1. **1つの `.clap` から2つのプラグインが見えること** (音源とエフェクト)
//! 2. **`out = in * gain + offset` になっていること**
//! 3. **2段の順序を入れ替えると結果が変わること** — この治具が存在する理由。
//!    掛け算だけだと順序が結果に出ず、チェーンの順が守られているか分からない
//!
//! ```text
//! cargo run -p egui-clap-host --bin gain_smoke -- target\debug\test_plugin.clap
//! ```

use clack_host::events::event_types::ParamValueEvent;
use clack_host::prelude::*;
use egui_clap_host::discovery;
use egui_clap_host::host::{MiniHost, MiniHostMainThread, MiniHostShared};
use egui_clap_host::params;
use std::error::Error;
use std::ffi::CString;
use std::path::Path;

const SAMPLE_RATE: f64 = 48_000.0;
/// 短くてよい。エフェクトは状態を持たない
const BLOCK_SIZE: usize = 64;
/// エフェクトが宣言しているチャンネル数 (モノラル)
const CHANNELS: usize = 1;

/// 検証用エフェクトのプラグイン ID
const GAIN_ID: &str = "com.example.test-gain";

/// 突き合わせの許容差。f32 の演算誤差だけを吸収する幅
const TOLERANCE: f32 = 1e-6;

fn main() -> Result<(), Box<dyn Error>> {
    let path = std::env::args()
        .nth(1)
        .ok_or("使い方: gain_smoke <path\\to\\test_plugin.clap>")?;

    println!("ロード中: {path}");
    let (entry, plugins) = discovery::load_clap_file(Path::new(&path))?;
    for plugin in &plugins {
        println!("  発見: {} ({})", plugin.name, plugin.id);
    }

    // 1. 1つのファイルに2つ入っていること
    if plugins.len() != 2 {
        return Err(format!("プラグインが2つ見つかるはずが {} 個でした", plugins.len()).into());
    }
    if !plugins.iter().any(|plugin| plugin.id == GAIN_ID) {
        return Err(format!("{GAIN_ID} がこのファイルにありません").into());
    }
    println!("✅ 1つの .clap から2つ見えている\n");

    let host_info = HostInfo::new(
        "Gain Smoke",
        "egui-clap-host",
        "https://example.com",
        "0.1.0",
    )?;
    let plugin_id = CString::new(GAIN_ID)?;
    let (sender, _receiver) = crossbeam_channel::unbounded();

    let mut instance = PluginInstance::<MiniHost>::new(
        |_| MiniHostShared::new(sender.clone()),
        |shared| MiniHostMainThread::new(shared),
        &entry,
        &plugin_id,
        &host_info,
    )?;

    // パラメータの ID は名前から引く (プラグイン側の定数を写し取らないため)
    let found = params::read_params(&mut instance);
    for param in &found {
        println!(
            "  パラメータ: {} (id={:?}, {}..{}, 既定={})",
            param.name, param.id, param.min, param.max, param.value
        );
    }
    let param_id = |name: &str| -> Result<ClapId, String> {
        found
            .iter()
            .find(|param| param.name == name)
            .map(|param| param.id)
            .ok_or_else(|| format!("パラメータ {name} がありません"))
    };
    let gain_id = param_id("Gain")?;
    let offset_id = param_id("Offset")?;

    let config = PluginAudioConfiguration {
        sample_rate: SAMPLE_RATE,
        min_frames_count: 1,
        max_frames_count: BLOCK_SIZE as u32,
    };
    let mut processor = instance.activate(|_, _| (), config)?.start_processing()?;
    println!("アクティベート + 処理開始: OK\n");

    // 入力は一定値にする。段を通るたびに何が起きたかが値で追えるため
    let input = vec![1.0f32; BLOCK_SIZE];

    // 2. out = in * gain + offset になっていること
    println!("-- 掛け算と足し算 --");
    let mut failures = Vec::new();
    for (gain, offset) in [(0.5, 0.0), (2.0, 0.25), (0.0, -0.5), (1.0, 0.0)] {
        let output = run_block(&mut processor, gain_id, gain, offset_id, offset, &input)?;
        let expected = input[0] * gain + offset;
        let actual = output[0];
        let uniform = output.iter().all(|s| (s - actual).abs() < TOLERANCE);

        let ok = (actual - expected).abs() < TOLERANCE && uniform;
        println!(
            "  gain={gain:.2} offset={offset:+.2} → {actual:.4} (期待 {expected:.4}) {}",
            if ok { "OK" } else { "NG" }
        );
        if !ok {
            failures.push(format!(
                "gain={gain} offset={offset} で {actual} (期待 {expected}, 全域一定={uniform})"
            ));
        }
    }

    // 3. 2段の順序が結果に出ること。
    //    段A (×2.0) と 段B (+0.25) を、順を入れ替えて通す
    println!("\n-- 2段の順序 --");
    let first = run_block(&mut processor, gain_id, 2.0, offset_id, 0.0, &input)?;
    let a_then_b = run_block(&mut processor, gain_id, 1.0, offset_id, 0.25, &first)?;

    let first = run_block(&mut processor, gain_id, 1.0, offset_id, 0.25, &input)?;
    let b_then_a = run_block(&mut processor, gain_id, 2.0, offset_id, 0.0, &first)?;

    // (1*2)+0.25 = 2.25 / (1+0.25)*2 = 2.5
    println!("  ×2.0 → +0.25 : {:.4} (期待 2.2500)", a_then_b[0]);
    println!("  +0.25 → ×2.0 : {:.4} (期待 2.5000)", b_then_a[0]);

    if (a_then_b[0] - 2.25).abs() >= TOLERANCE || (b_then_a[0] - 2.5).abs() >= TOLERANCE {
        failures.push("2段を通した値が期待と違います".into());
    } else if (a_then_b[0] - b_then_a[0]).abs() < TOLERANCE {
        // ここが同じ値になると、チェーンの順が守られているかを
        // この治具では言い当てられない
        failures.push("順序を入れ替えても結果が同じです".into());
    } else {
        println!("✅ 順序が結果に出ている");
    }

    if failures.is_empty() {
        println!("\n✅ 検証用エフェクトは主張どおり動いている");
        Ok(())
    } else {
        for failure in &failures {
            eprintln!("❌ {failure}");
        }
        Err("検証用エフェクトの挙動が期待と違います".into())
    }
}

/// パラメータを指定して1ブロック処理し、出てきた値を返す。
///
/// **入力バッファに `input` を置いてから渡す。** ホスト本体はここを 0 で
/// 埋めているので、エフェクトに音を入れられるのはまだこの経路だけ。
fn run_block(
    processor: &mut StartedPluginAudioProcessor<MiniHost>,
    gain_id: ClapId,
    gain: f32,
    offset_id: ClapId,
    offset: f32,
    input: &[f32],
) -> Result<Vec<f32>, Box<dyn Error>> {
    let mut input_buffers = [input.to_vec()];
    let mut output_buffers = [vec![0.0f32; input.len()]];
    let mut input_ports = AudioPorts::with_capacity(CHANNELS, 1);
    let mut output_ports = AudioPorts::with_capacity(CHANNELS, 1);

    // ブロックの先頭で両方を設定する
    let mut events = EventBuffer::with_capacity(4);
    events.push(&ParamValueEvent::new(
        0,
        gain_id,
        Pckn::match_all(),
        gain as f64,
    ));
    events.push(&ParamValueEvent::new(
        0,
        offset_id,
        Pckn::match_all(),
        offset as f64,
    ));

    {
        let inputs = input_ports.with_input_buffers([AudioPortBuffer {
            latency: 0,
            channels: AudioPortBufferType::f32_input_only(input_buffers.iter_mut().map(|buffer| {
                InputChannel {
                    buffer: buffer.as_mut_slice(),
                    // 一定値だが、定数扱いにするとプラグインが読み飛ばせてしまう。
                    // ここでは実際に読ませたい
                    is_constant: false,
                }
            })),
        }]);
        let mut outputs = output_ports.with_output_buffers([AudioPortBuffer {
            latency: 0,
            channels: AudioPortBufferType::f32_output_only(
                output_buffers
                    .iter_mut()
                    .map(|buffer| buffer.as_mut_slice()),
            ),
        }]);

        processor.process(
            &inputs,
            &mut outputs,
            &events.as_input(),
            &mut OutputEvents::void(),
            Some(0),
            None,
        )?;
    }

    Ok(output_buffers.into_iter().next().unwrap_or_default())
}
