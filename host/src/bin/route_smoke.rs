//! オーディオトラック同士のルーティングの検証。オーディオデバイス不要。
//!
//! `chain_smoke` は**1本のトラックの中**を見るもの。こちらは**トラックの間**を見る。
//!
//! 治具は `test_plugin.clap` の2つ。オーディオトラック1 に音源、
//! トラック2・3 にエフェクト (`out = in * 0.5`) を置き、繋ぎ方だけを変えて
//! 出てくる音を突き合わせる。
//!
//! 見るのは6つ。
//!
//! 1. **直結** — 1 → 0。音源の音がそのまま出る (基準)
//! 2. **直列** — 1 → 2 → 0。トラック2 のエフェクトを通って半分になる
//! 3. **枝分かれ (センド)** — 1 → 0 と 1 → 2、2 → 0。
//!    原音と加工が**足し合わさる**ので 1.5 倍になる。**上書きなら 1.0 のまま**
//! 4. **鳴らないトラック** — 1 → 2 だけ (2 の先が無い)。マスターは無音
//! 5. **輪を拒否すること** — `1 → 2 → 3 → 1` は繋ぎ方の組み立てで落ちる
//! 6. **打ち込み2本を1つの音源で受けること** — キックとハイハットを別の
//!    打ち込みに分けて1つの音源へ流したものが、同じ音を1本にまとめて
//!    流したものと**波形ごと一致する**こと
//!
//! ```text
//! cargo run -p egui-clap-host --bin route_smoke -- target\debug\test_plugin.clap
//! ```

use clack_host::prelude::*;
use egui_clap_host::audio;
use egui_clap_host::audio::config::StreamAudioConfig;
use egui_clap_host::audio::events::BlockEvent;
use egui_clap_host::audio::graph::{self, Graph, Mixer, Routing};
use egui_clap_host::audio::transport::{Transport, TransportMsg, TransportShared};
use egui_clap_host::discovery;
use egui_clap_host::host::{MiniHost, MiniHostMainThread, MiniHostShared};
use egui_clap_host::sequencer::{MidiEditor, Note};
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

    // ---- 6. 打ち込み2本を1つの音源で受ける ----
    // キックとハイハットを別の打ち込みに書き、音源1つで受ける形。
    // 同じ音を1本にまとめて流したものと**波形ごと一致**しなければならない。
    let split = render_master(&entry, &stream_config, &drum_split(), {
        let mut midi = graph::MidiSources::default();
        midi.insert(0);
        midi.insert(1);
        midi
    })?;
    let merged = render_master(
        &entry,
        &stream_config,
        &drum_merged(),
        graph::MidiSources::one(0),
    )?;

    let split_peak = split.iter().fold(0.0f32, |max, s| max.max(s.abs()));
    let diff = split
        .iter()
        .zip(merged.iter())
        .fold(0.0f32, |max, (a, b)| max.max((a - b).abs()));
    println!("打ち込み2本 → 音源1つ    : ピーク {split_peak:.4} / まとめたものとの差 {diff:.8}");

    if split.len() != merged.len() {
        failures.push(format!(
            "長さが違います (2本 {} / まとめ {})",
            split.len(),
            merged.len()
        ));
    } else if split_peak < 0.01 {
        failures.push("2本に分けると音が出ていません".into());
    } else if diff > SILENT {
        // 継ぎ足しで済ませると、同じブロックに入った2本目のイベントが
        // 時刻順から外れる。それが波形の差として出る
        failures.push(format!(
            "2本に分けたときの音が、まとめたときと違います (最大の差 {diff:.6})"
        ));
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

/// ドラムの叩き位置 (四分音符単位)。**1ブロックの中に複数入る近さにしてある。**
///
/// 48kHz・120bpm で四分音符は 24000 サンプル、ブロックは 256 サンプルなので、
/// 0.005 拍 = 120 サンプル。1ブロックに2〜3発入るので、2本の打ち込みが
/// **同じブロックの中で交互に並ぶ**ことになる。ここが離れていると、
/// 継ぎ足しでも並びが崩れず、検証にならない。
const KICK_HITS: [f32; 4] = [0.0, 0.01, 0.02, 0.03];
const HAT_HITS: [f32; 4] = [0.005, 0.015, 0.025, 0.035];

/// キック (C4) とハイハット (E4) を、それぞれ別の打ち込みトラックに書いたもの
fn drum_split() -> MidiEditor {
    let mut editor = MidiEditor::default();
    editor.add_track(); // 打ち込み1 (ハイハット用)
    editor.notes = hits(&KICK_HITS, 0, 0)
        .chain(hits(&HAT_HITS, 4, 1))
        .collect();
    editor
}

/// 上と同じ音を、1本の打ち込みトラックにまとめたもの
fn drum_merged() -> MidiEditor {
    let mut editor = MidiEditor::default();
    editor.notes = hits(&KICK_HITS, 0, 0)
        .chain(hits(&HAT_HITS, 4, 0))
        .collect();
    editor
}

/// 叩き位置の並びをノートにする
fn hits(at: &[f32], semitone: i32, track: usize) -> impl Iterator<Item = Note> + '_ {
    at.iter().map(move |start| Note {
        start_tick: *start,
        duration: 0.05, // 重なるくらいの長さ (音源は多声)
        semitone,
        octave: 4,
        velocity: 100,
        track,
        lane: 0,
    })
}

/// 打ち込みを鳴らして、マスターに出た波形をそのまま返す。
///
/// **本体と同じ経路を通す** ([`Graph::emit_from`] が配り、`process` が回す)。
/// 手組みで配ると、配り方を変えたときにここだけ古い形のまま通ってしまう。
fn render_master(
    entry: &PluginEntry,
    stream_config: &StreamAudioConfig,
    editor: &MidiEditor,
    midi: graph::MidiSources,
) -> Result<Vec<f32>, Box<dyn Error>> {
    let mut lists = vec![Vec::new(); graph::AUDIO_TRACKS];
    lists[1] = vec![graph::MASTER];
    let routing = Routing::from_lists(&lists).map_err(|problems| problems.join("\n"))?;

    let mut graph = Graph::new();
    graph.set_mixer(Mixer::build(
        routing,
        &[1.0; graph::AUDIO_TRACKS],
        &[0.0; graph::AUDIO_TRACKS],
        0,
        0,
    ));
    graph.reserve(BLOCK_SIZE);

    let mut instance = instantiate(entry, SINE_ID)?;
    let node = audio::activate_node(&mut instance, stream_config)?;
    graph.place_chain(1, midi, vec![node]);

    // 打ち込みは本数ぶんすべて渡す (受け取るかどうかは割り当てが決める)
    let sample_rate = SAMPLE_RATE as f64;
    let spq = editor.samples_per_quarter(sample_rate);
    let end_sample = (editor.length_quarters_bar_aligned() as f64 * spq) as u64;
    let mut transport = Transport::new(TransportShared::new());
    for track in 0..editor.track_count() {
        let _ = transport.handle_msg(TransportMsg::SetSequence {
            track,
            events: editor
                .to_events_for_track(track, sample_rate)
                .into_boxed_slice(),
            end_sample,
        });
    }
    let _ = transport.handle_msg(TransportMsg::Play);

    let mut master = Vec::new();
    let blocks = (end_sample as usize).div_ceil(BLOCK_SIZE);
    for block in 0..blocks {
        let pos = (block * BLOCK_SIZE) as u64;
        graph.clear_events();
        let plan = transport.plan_block(BLOCK_SIZE as u64);
        graph.emit_from(&mut transport, &plan);

        let mut error = None;
        graph.process(pos, BLOCK_SIZE, &mut |track, e| {
            error.get_or_insert_with(|| format!("オーディオトラック {track}: {e}"));
        });
        if let Some(error) = error {
            return Err(error.into());
        }
        master.extend_from_slice(graph.master(BLOCK_SIZE));
    }

    // 借りた処理器をインスタンスへ返す (返さないと解放できない)
    let mut retired = graph.take_nodes();
    let (_, node) = retired.pop().ok_or("処理器が戻ってこなかった")?;
    let audio::RetiredProcessor::Clap(stopped) = node.into_retired() else {
        return Err("CLAP を載せたのに別形式が返ってきた".into());
    };
    instance.deactivate(stopped);

    Ok(master)
}

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
    let mut used = [false; graph::AUDIO_TRACKS];
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
    for (index, in_use) in used.iter().enumerate() {
        if !in_use || index == graph::MASTER {
            continue;
        }
        let id = if index == 1 { SINE_ID } else { GAIN_ID };
        let mut instance = instantiate(entry, id)?;
        let node = audio::activate_node(&mut instance, stream_config)?;
        // 音源だけが MIDI を受け取る
        let midi = if index == 1 {
            graph::MidiSources::one(0)
        } else {
            graph::MidiSources::default()
        };
        graph.place_chain(index, midi, vec![node]);
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
