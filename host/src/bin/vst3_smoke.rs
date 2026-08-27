//! VST3 バックエンドの検証。オーディオデバイス不要。
//!
//! CLAP 側の `choke_smoke` と同じ筋道を VST3 で通す。ロングトーンを鳴らしながら
//! 停止・シークを行い、「鳴る」「止まる」「またに鳴る」をサンプル位置で確かめる。
//!
//! VST3 には消音イベントが無く、鳴っているキーを覚えて個別に note-off する
//! 作りになっている。ここが壊れると停止しても鳴りっぱなしになるので、
//! CLAP 以上に検証の価値がある。
//!
//! あわせて、音源をメインスレッドが握っている間の振る舞い (そのブロックは無音、
//! イベントは持ち越し) も確かめる。
//!
//! 使い方: cargo run -p egui-clap-host --bin vst3_smoke -- <path\to\plugin.vst3>

use egui_clap_host::audio::config::StreamAudioConfig;
use egui_clap_host::audio::graph::{Graph, MidiSources};
use egui_clap_host::audio::offline::{self, RenderSetup};
use egui_clap_host::audio::transport::{self, Transport, TransportMsg, TransportShared};
use egui_clap_host::audio::{self, ProcessError};
use egui_clap_host::discovery;
use egui_clap_host::sequencer::{MidiEditor, Note};
use std::error::Error;
use std::path::Path;

const SAMPLE_RATE: f64 = 44_100.0;
const BLOCK_SIZE: usize = 512;
const CHANNELS: usize = 2;

/// 無音とみなす上限 / 鳴っているとみなす下限 (他の smoke と同じ基準)
const SILENT: f32 = 0.001;
const AUDIBLE: f32 = 0.01;

/// 何ブロック目で停止するか / シークするか (どちらもロングトーンの途中)
const STOP_AT: usize = 40;
const SEEK_AT: usize = 80;
const TOTAL_BLOCKS: usize = 120;

fn main() -> Result<(), Box<dyn Error>> {
    let path = std::env::args()
        .nth(1)
        .ok_or("使い方: vst3_smoke <path\\to\\plugin.vst3>")?;
    let path = Path::new(&path);

    let found = discovery::load_vst3_file(path)?;
    // 1つのファイルに複数入りうる。何が見えているかを出しておく
    for plugin in &found {
        println!("  発見: {} ({})", plugin.name, plugin.id);
    }
    let target = &found[0];
    println!("プラグイン: {} ({})", target.name, target.id);

    let stream_config = StreamAudioConfig {
        output_channel_count: CHANNELS,
        min_buffer_size: 1,
        max_likely_buffer_size: BLOCK_SIZE as u32,
        sample_rate: SAMPLE_RATE as u32,
        sample_format: cpal::SampleFormat::F32,
    };

    let (shared, mut processor) = audio::activate_vst3_track(path, &target.id, &stream_config)?;

    // ---- シーケンス: C4 のロングトーン1本 (8拍) ----
    // 途中で止めるためのものなので、検証の間ずっと鳴り続ける長さにする。
    let mut editor = MidiEditor::default(); // 120bpm → 四分音符 = 22050 サンプル
    editor.notes = vec![Note {
        start_tick: 0.0,
        duration: 8.0,
        semitone: 0,
        octave: 4,
        velocity: 127,
        velocity_to: 127,
        track: 0,
        lane: 0,
    }];

    let spq = editor.samples_per_quarter(SAMPLE_RATE);
    let events = editor.to_events(SAMPLE_RATE).into_boxed_slice();
    let end_sample = (editor.length_quarters_bar_aligned() as f64 * spq) as u64;

    let mut transport = Transport::new(TransportShared::new());
    let _ = transport.handle_msg(TransportMsg::SetSequence {
        track: 0,
        events,
        end_sample,
    });
    let _ = transport.handle_msg(TransportMsg::Play);

    let mut mix = vec![0.0f32; BLOCK_SIZE * CHANNELS];
    let mut failures = Vec::new();

    // ---- 1. メインスレッドが音源を握っている間は無音になり、イベントが持ち越されること ----
    // 発音の指示を出しながら音源を握ると、そのブロックは無音のまま返るはず。
    {
        let guard = shared.lock();

        processor.events_mut().clear();
        let plan = transport.plan_block(BLOCK_SIZE as u64);
        transport.emit_track(0, &plan, processor.events_mut());
        let queued = processor.events_mut().len();

        mix.fill(0.0);
        let result = processor.process(0, &mut mix);

        match result {
            Err(ProcessError::Busy) => {}
            Err(e) => failures.push(format!("握っている間のエラーが Busy でない: {e}")),
            Ok(()) => failures.push("音源を握っている間も処理が通ってしまった".to_string()),
        }
        if queued == 0 {
            failures.push("最初のブロックに発音イベントが乗っていない (検証にならない)".into());
        }
        if peak(&mix) > SILENT {
            failures.push("音源を握っている間に音が出た".to_string());
        }
        drop(guard);
    }

    // ---- 2. 再生・停止・シークを通す ----
    // 上のブロックはトランスポートを1つ進めてあるので、その続きから回す。
    let mut peaks = [0.0f32; 3]; // [再生中, 停止後, シーク後]
    let mut steady = BLOCK_SIZE as u64;

    for block in 0..TOTAL_BLOCKS {
        processor.events_mut().clear();

        // 本体と同じ手順: トランスポートを操作し、要求されたら消音イベントを積む
        let msg = match block {
            STOP_AT => Some(TransportMsg::Stop),
            SEEK_AT => Some(TransportMsg::Seek { sample: 0 }),
            _ => None,
        };
        if let Some(msg) = msg {
            if transport.handle_msg(msg) {
                transport::push_choke(processor.events_mut(), 0);
            }
        }
        if block == SEEK_AT {
            let _ = transport.handle_msg(TransportMsg::Play);
        }

        let plan = transport.plan_block(BLOCK_SIZE as u64);
        transport.emit_track(0, &plan, processor.events_mut());

        mix.fill(0.0);
        if let Err(e) = processor.process(steady, &mut mix) {
            failures.push(format!("ブロック {block} の処理に失敗: {e}"));
            break;
        }

        let peak = peak(&mix);
        // 操作したブロック自体は境界なので、どの区間にも数えない。
        // 停止後はリリースが落ちきるまで待ってから測る。
        match block {
            b if b < STOP_AT => peaks[0] = peaks[0].max(peak),
            b if b > STOP_AT + 20 && b < SEEK_AT => peaks[1] = peaks[1].max(peak),
            b if b > SEEK_AT => peaks[2] = peaks[2].max(peak),
            _ => {}
        }

        steady += BLOCK_SIZE as u64;
    }

    // ---- 3. オフラインレンダリング (WAV 書き出しと同じ経路) ----
    // 書き出しは処理器をオーディオスレッドから借りてその場で回す作りなので、
    // 実時間の再生が通っても、こちらが通るとは限らない。
    // オーディオトラックは 0 がマスターなので、打ち込み0 は 1 に載る
    let mut graph = Graph::new();
    graph.place_chain(1, MidiSources::one(0), processor.take_nodes());
    let setup = RenderSetup {
        sequences: vec![editor
            .to_events_for_track(0, SAMPLE_RATE)
            .into_boxed_slice()],
        end_sample,
        tail_samples: (offline::TAIL_SECONDS * SAMPLE_RATE) as u64,
        block_frames: BLOCK_SIZE,
        sample_rate: SAMPLE_RATE as u32,
    };
    let rendered = offline::render(&mut graph, setup);
    let node = graph
        .take_nodes()
        .pop()
        .map(|(_, node)| node)
        .ok_or("借りた処理器が戻ってこなかった")?;

    println!(
        "書き出し: {:.2} 秒 / ピーク {:.4}",
        rendered.seconds(),
        rendered.peak
    );
    for failure in &rendered.failures {
        failures.push(format!(
            "書き出しでトラック {} の処理が失敗: {} ({} ブロック)",
            failure.track + 1,
            failure.message,
            failure.blocks
        ));
    }
    if rendered.peak < AUDIBLE {
        failures.push(format!(
            "書き出した内容が無音 (ピーク {:.4})",
            rendered.peak
        ));
    }

    // ---- 後始末 (メインスレッドで止めてから解放する) ----
    let audio::RetiredProcessor::Vst3(retired) = node.into_retired() else {
        return Err("VST3 を載せたのに別形式が返ってきた".into());
    };
    retired.lock().stop_processing()?;
    drop(retired);
    drop(shared);

    println!(
        "ピーク値: 再生中={:.4} 停止後={:.4} シーク後={:.4}",
        peaks[0], peaks[1], peaks[2]
    );

    if peaks[0] < AUDIBLE {
        failures.push("再生中に音が出ていない (検証になっていない)".to_string());
    }
    if peaks[1] > SILENT {
        failures.push(format!(
            "停止しても鳴り続けている (ピーク {:.4})。\
             鳴っているキーの記録が合っていない可能性がある",
            peaks[1]
        ));
    }
    if peaks[2] < AUDIBLE {
        failures.push("シークして再生し直しても音が出ない".to_string());
    }

    if failures.is_empty() {
        println!("✅ VST3 バックエンドのテスト成功");
        Ok(())
    } else {
        Err(format!("❌ 失敗: {}", failures.join(", ")).into())
    }
}

/// インターリーブ済みバッファの絶対値ピーク
fn peak(mix: &[f32]) -> f32 {
    mix.iter().fold(0.0f32, |max, s| max.max(s.abs()))
}
