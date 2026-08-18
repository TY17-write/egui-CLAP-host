//! オーディオトラック同士のルーティングの検証。オーディオデバイス不要。
//!
//! `chain_smoke` は**1本のトラックの中**を見るもの。こちらは**トラックの間**を見る。
//!
//! 治具は `test_plugin.clap` の2つ。オーディオトラック1 に音源、
//! トラック2・3 にエフェクト (`out = in * 0.5`) を置き、繋ぎ方だけを変えて
//! 出てくる音を突き合わせる。
//!
//! 見るのは5つ。
//!
//! 1. **直結** — 1 → 0。音源の音がそのまま出る (基準)
//! 2. **直列** — 1 → 2 → 0。トラック2 のエフェクトを通って半分になる
//! 3. **枝分かれ (センド)** — 1 → 0 と 1 → 2、2 → 0。
//!    原音と加工が**足し合わさる**ので 1.5 倍になる。**上書きなら 1.0 のまま**
//! 4. **鳴らないトラック** — 1 → 2 だけ (2 の先が無い)。マスターは無音
//! 5. **輪を拒否すること** — `1 → 2 → 3 → 1` は繋ぎ方の組み立てで落ちる
//!
//! ```text
//! cargo run -p clap-host-test --bin route_smoke -- target\debug\test_plugin.clap
//! ```

use clack_host::prelude::*;
use clap_host_test::audio;
use clap_host_test::audio::config::StreamAudioConfig;
use clap_host_test::audio::events::BlockEvent;
use clap_host_test::audio::graph::{self, Graph, Mixer, Routing};
use clap_host_test::discovery;
use clap_host_test::host::{MiniHost, MiniHostMainThread, MiniHostShared};
use std::error::Error;
use std::ffi::CString;
use std::path::Path;

const SAMPLE_RATE: u32 = 48_000;
const BLOCK_SIZE: usize = 256;

/// 波形が安定するまで回すブロック数
const BLOCKS: usize = 8;

const SINE_ID: &str = "com.example.test-sine";
const GAIN_ID: &str = "com.example.test-gain";

/// 比率の許容差
const TOLERANCE: f32 = 0.02;

/// 無音とみなす上限
const SILENT: f32 = 0.001;

fn main() -> Result<(), Box<dyn Error>> {
    let path = std::env::args()
        .nth(1)
        .ok_or("使い方: route_smoke <path\\to\\test_plugin.clap>")?;

    let (entry, plugins) = discovery::load_clap_file(Path::new(&path))?;
    if !plugins.iter().any(|plugin| plugin.id == GAIN_ID) {
        return Err(format!("{GAIN_ID} がありません。test-plugin を作り直してください").into());
    }

    let stream_config = StreamAudioConfig {
        output_channel_count: graph::BUS_CHANNELS,
        min_buffer_size: 1,
        max_likely_buffer_size: BLOCK_SIZE as u32,
        sample_rate: SAMPLE_RATE,
        sample_format: cpal::SampleFormat::F32,
    };

    let mut failures = Vec::new();

    // ---- 1. 1 → 0 (基準) ----
    let plain = run(&entry, &stream_config, &[(1, MASTER_LIST)])?;
    println!("1 → 0                    : ピーク {plain:.4}");
    if plain < 0.01 {
        return Err("音源が鳴っていません。先に smoke で確かめてください".into());
    }

    // ---- 2. 1 → 2 → 0 (直列) ----
    let serial = run(&entry, &stream_config, &[(1, &[2]), (2, MASTER_LIST)])?;
    let ratio = serial / plain;
    println!("1 → 2 → 0                : ピーク {serial:.4} (基準比 {ratio:.3}、期待 0.500)");
    if (ratio - 0.5).abs() > TOLERANCE {
        let hint = if ratio > 0.9 {
            " ← トラック2 を素通りしています"
        } else {
            ""
        };
        failures.push(format!(
            "トラック間の直列が効いていません (基準比 {ratio:.3}、期待 0.5){hint}"
        ));
    }

    // ---- 3. 1 → 0 と 1 → 2 → 0 (枝分かれ) ----
    // 原音 1.0 と 加工 0.5 が足し合わさって 1.5
    let branched = run(
        &entry,
        &stream_config,
        &[(1, &[graph::MASTER, 2]), (2, MASTER_LIST)],
    )?;
    let ratio = branched / plain;
    println!("1 → 0 と 1 → 2 → 0       : ピーク {branched:.4} (基準比 {ratio:.3}、期待 1.500)");
    if (ratio - 1.5).abs() > TOLERANCE {
        let hint = if (ratio - 1.0).abs() < TOLERANCE {
            " ← 足し合わせでなく上書きになっています"
        } else {
            ""
        };
        failures.push(format!(
            "センドが混ざっていません (基準比 {ratio:.3}、期待 1.5){hint}"
        ));
    }

    // ---- 4. 1 → 2 だけ (マスターへ届かない) ----
    let orphan = run(&entry, &stream_config, &[(1, &[2])])?;
    println!("1 → 2 (行き止まり)       : ピーク {orphan:.4} (期待 0.0000)");
    if orphan > SILENT {
        failures.push(format!(
            "マスターへ届かないトラックの音が出ています (ピーク {orphan:.4})"
        ));
    }

    // ---- 5. 輪は組み立てで拒否されること ----
    let mut lists = vec![Vec::new(); graph::AUDIO_TRACKS];
    lists[1] = vec![2];
    lists[2] = vec![3];
    lists[3] = vec![1];
    match Routing::from_lists(&lists) {
        Ok(_) => failures.push("輪になった繋ぎ方が通ってしまいました".into()),
        Err(problems) => {
            println!("1 → 2 → 3 → 1            : 拒否 ({})", problems.join(" / "));
            if !problems
                .iter()
                .any(|line| line.contains("輪になっています"))
            {
                failures.push(format!("輪として断っていません: {problems:?}"));
            }
        }
    }

    // 「届かない」が分かること (フェーズ4 の赤枠のもと)
    let routing = Routing::from_lists(&{
        let mut lists = vec![Vec::new(); graph::AUDIO_TRACKS];
        lists[1] = vec![2];
        lists
    })
    .map_err(|problems| problems.join("\n"))?;
    if routing.reaches_master(1) || routing.reaches_master(2) {
        failures.push("行き止まりのトラックを「届く」と言っています".into());
    }

    if failures.is_empty() {
        println!("\n✅ 繋ぎ方どおりに流れ、輪は断られている");
        Ok(())
    } else {
        for failure in &failures {
            eprintln!("❌ {failure}");
        }
        Err("ルーティングの挙動が期待と違います".into())
    }
}

/// マスターへ1本だけ送る指定 (書き回すので定数にしてある)
const MASTER_LIST: &[usize] = &[graph::MASTER];

/// 指定の繋ぎ方でグラフを組み、C4 を鳴らしてマスターのピークを返す。
///
/// オーディオトラック1 に音源、それ以外の登場するトラックにエフェクトを置く。
/// `edges` は `(トラック番号, 送り先の一覧)`。
fn run(
    entry: &PluginEntry,
    stream_config: &StreamAudioConfig,
    edges: &[(usize, &[usize])],
) -> Result<f32, Box<dyn Error>> {
    let mut lists = vec![Vec::new(); graph::AUDIO_TRACKS];
    for (from, targets) in edges {
        lists[*from] = targets.to_vec();
    }
    let routing = Routing::from_lists(&lists).map_err(|problems| problems.join("\n"))?;

    // 出てくるトラック番号を集める (送り元と送り先の両方)
    let mut used = vec![false; graph::AUDIO_TRACKS];
    for (from, targets) in edges {
        used[*from] = true;
        for target in *targets {
            used[*target] = true;
        }
    }

    let mut graph = Graph::new();
    // 音量・パンは既定 (等倍・中央・全部鳴る)。見たいのは繋ぎ方だけ
    graph.set_mixer(Mixer::build(
        routing,
        &[1.0; graph::AUDIO_TRACKS],
        &[0.0; graph::AUDIO_TRACKS],
        0,
        0,
    ));
    graph.reserve(BLOCK_SIZE);

    // トラック1 だけ音源。他はエフェクト (マスターには何も載せない)
    let mut instances = Vec::new();
    for index in 0..graph::AUDIO_TRACKS {
        if !used[index] || index == graph::MASTER {
            continue;
        }
        let id = if index == 1 { SINE_ID } else { GAIN_ID };
        let mut instance = instantiate(entry, id)?;
        let node = audio::activate_node(&mut instance, stream_config)?;
        // 音源だけが MIDI を受け取る
        let midi_track = (index == 1).then_some(0);
        graph.place_chain(index, midi_track, vec![node]);
        instances.push((index, instance));
    }

    let mut peak = 0.0f32;
    for block in 0..BLOCKS {
        graph.clear_events();
        if block == 0 {
            if let Some(events) = graph.events_mut(1) {
                events.push(BlockEvent::NoteOn {
                    offset: 0,
                    key: 60,
                    velocity: 1.0,
                });
            }
        }

        let mut error = None;
        graph.process((block * BLOCK_SIZE) as u64, BLOCK_SIZE, &mut |track, e| {
            error.get_or_insert_with(|| format!("オーディオトラック {track}: {e}"));
        });
        if let Some(error) = error {
            return Err(error.into());
        }

        // 1ブロック目は立ち上がりで値が安定しないので数えない
        if block > 0 {
            peak = graph
                .master(BLOCK_SIZE)
                .iter()
                .fold(peak, |max, s| max.max(s.abs()));
        }
    }

    // 借りた処理器をインスタンスへ返す (返さないと解放できない)
    let mut retired = graph.take_nodes();
    for (index, mut instance) in instances {
        let at = retired
            .iter()
            .position(|(addr, _)| addr.track == index)
            .ok_or("処理器が戻ってこなかった")?;
        let (_, node) = retired.remove(at);
        let audio::RetiredProcessor::Clap(stopped) = node.into_retired() else {
            return Err("CLAP を載せたのに別形式が返ってきた".into());
        };
        instance.deactivate(stopped);
    }

    Ok(peak)
}

fn instantiate(entry: &PluginEntry, id: &str) -> Result<PluginInstance<MiniHost>, Box<dyn Error>> {
    let host_info = HostInfo::new(
        "Route Smoke",
        "clap-host-test",
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
