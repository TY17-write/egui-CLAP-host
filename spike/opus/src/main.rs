//! Ogg/Opus の実験。本体には一切触らない。
//!
//! 2つのことに使った。
//!
//! 1. **`opus-rs` の生パケットを Ogg で包めるか** (フェーズ0)。ヘッダ
//!    (`OpusHead` / `OpusTags`) とグラニュール位置は自分で組み立てる必要がある
//! 2. **高いビットレートで壊れる件の再現** (下記)
//!
//! # 高ビットレートで壊れる (0.1.26。**0.1.28 で直った**)
//!
//! `opus-rs` 0.1.26 は壊れたストリームを吐くことがある。本家 libopus で復号すると
//! 中身が崩れ、聴くとノイズになる。**176kbps 以上で再現する** (サイン波の場合)。
//! 内容にもよる (和音+ノイズは 160kbps、実際の曲は 192kbps で発覚)。
//! `complexity` を変えても改善しない。上流へ報告し、**0.1.28 で修正された**。
//!
//! 測り方と実測値は `src/bin/spectral_match.rs`、経緯は `upstream-issue.md`。
//!
//! ```text
//! cargo run --release
//! for s in sine complex; do
//!   for k in 48 96 128 160 176 192 224 256 320 384 448 510; do
//!     ffmpeg -v error -y -i ${s}_${k}.opus -f f32le -ac 2 -ar 48000 dec_${s}_${k}.raw
//!   done
//! done
//! cargo run --release --bin spectral_match
//! ```
//!
//! **「範囲外サンプル (|x|>1.0) の数」だけで測ってはいけない。** 範囲内に収まった
//! まま中身が壊れる場合を取りこぼす (和音+ノイズが実際にそうだった)。
//!
//! 使うクレートの版は `Cargo.lock` で決まる。切り替えて比べるときは
//! `cargo update -p opus-rs --precise 0.1.26` のように指定する。

use ogg::writing::{PacketWriteEndInfo, PacketWriter};
use opus_rs::{Application, OpusEncoder};

/// Opus が鳴らせる唯一のレート
const SAMPLE_RATE: i32 = 48_000;
const CHANNELS: usize = 2;
/// 20ms。Opus の標準的なフレーム長
const FRAME_SIZE: usize = 960;
const SECONDS: usize = 2;

/// 符号化の先読み。復号側は頭からこのぶんを捨てる。
///
/// **入れた音を最後まで取り出すには、そのぶん余分に流し込む必要がある。**
/// 忘れると出来上がりが短くなる (2.000秒 が 1.9935秒 になった)。
const PRE_SKIP: u16 = 312;

/// 壊れ方を見るためのビットレート。
///
/// 前半 6つは 0.1.26 で崩れる範囲を測ったときのもの (純音は 176 以上で崩れた)。
/// 後半は 0.1.28 で直ったあと、**どこまで上げられるか**を見るために足した。
/// 510kbps は Opus のステレオ上限。
const BITRATES_KBPS: [i32; 12] = [48, 96, 128, 160, 176, 192, 224, 256, 320, 384, 448, 510];

fn main() -> Result<(), Box<dyn std::error::Error>> {
    report_device_rates();

    // **入力もそのまま書き出す。** 復号結果と突き合わせないと、純音以外の信号で
    // 壊れているかどうかを測れない (純度は純音にしか使えない)。
    for (label, input) in [("sine", sine_input()), ("complex", complex_input())] {
        std::fs::write(format!("{label}_input.raw"), as_bytes(&input))?;
        for kbps in BITRATES_KBPS {
            let bytes = encode_ogg_opus(&input, kbps * 1000)?;
            let name = format!("{label}_{kbps}.opus");
            std::fs::write(&name, &bytes)?;
            println!("書き出し: {name} ({} バイト)", bytes.len());
        }
    }
    println!("\n復号して入力と突き合わせると壊れ方が分かる (冒頭のコメント参照)");
    Ok(())
}

/// 純音 (左 440Hz / 右 660Hz)。いちばん小さく再現できる形
fn sine_input() -> Vec<f32> {
    let total_frames = SAMPLE_RATE as usize * SECONDS;
    let mut input = Vec::with_capacity(total_frames * CHANNELS);
    for frame in 0..total_frames {
        let t = frame as f32 / SAMPLE_RATE as f32;
        input.push((t * 440.0 * std::f32::consts::TAU).sin() * 0.5);
        input.push((t * 660.0 * std::f32::consts::TAU).sin() * 0.5);
    }
    input
}

/// 和音 + 微小ノイズ。純音より符号化が難しい信号として比較に使う
fn complex_input() -> Vec<f32> {
    let total_frames = SAMPLE_RATE as usize * SECONDS;
    let mut input = Vec::with_capacity(total_frames * CHANNELS);
    for frame in 0..total_frames {
        let t = frame as f32 / SAMPLE_RATE as f32;
        let noise = ((frame as f32 * 12.9898).sin() * 43758.547).fract() - 0.5;
        let chord = |f: f32| (t * f * std::f32::consts::TAU).sin();
        input.push((chord(220.0) + chord(277.2) + chord(329.6)) * 0.15 + noise * 0.05);
        input.push((chord(261.6) + chord(392.0) + chord(523.3)) * 0.15 + noise * 0.05);
    }
    input
}

fn as_bytes(samples: &[f32]) -> Vec<u8> {
    samples.iter().flat_map(|v| v.to_le_bytes()).collect()
}

/// f32 のインターリーブ列を Ogg/Opus のバイト列にする
fn encode_ogg_opus(input: &[f32], bitrate_bps: i32) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    let mut encoder = OpusEncoder::new(SAMPLE_RATE, CHANNELS, Application::Audio)?;
    encoder.bitrate_bps = bitrate_bps;

    let mut out = Vec::new();
    {
        let mut writer = PacketWriter::new(&mut out);
        let serial = 0x5350_494b;

        writer.write_packet(opus_head(), serial, PacketWriteEndInfo::EndPage, 0)?;
        writer.write_packet(opus_tags(), serial, PacketWriteEndInfo::EndPage, 0)?;

        let source_frames = (input.len() / CHANNELS) as u64;
        let target_granule = source_frames + PRE_SKIP as u64;

        let mut packet = vec![0u8; 4000];
        let mut fed: u64 = 0;
        let mut index = 0usize;

        while fed < target_granule {
            let start = index * FRAME_SIZE * CHANNELS;
            let end = (start + FRAME_SIZE * CHANNELS).min(input.len());
            let mut frame: Vec<f32> = input.get(start..end).unwrap_or(&[]).to_vec();
            frame.resize(FRAME_SIZE * CHANNELS, 0.0);

            let len = encoder.encode(&frame, FRAME_SIZE, &mut packet)?;
            fed += FRAME_SIZE as u64;
            index += 1;

            let last = fed >= target_granule;
            writer.write_packet(
                packet[..len].to_vec(),
                serial,
                if last {
                    PacketWriteEndInfo::EndStream
                } else {
                    PacketWriteEndInfo::NormalPacket
                },
                fed.min(target_granule),
            )?;
        }
    }
    Ok(out)
}

/// `OpusHead` パケット (RFC 7845)
fn opus_head() -> Vec<u8> {
    let mut head = Vec::with_capacity(19);
    head.extend_from_slice(b"OpusHead");
    head.push(1); // バージョン
    head.push(CHANNELS as u8);
    head.extend_from_slice(&PRE_SKIP.to_le_bytes());
    head.extend_from_slice(&(SAMPLE_RATE as u32).to_le_bytes());
    head.extend_from_slice(&0i16.to_le_bytes()); // 出力ゲイン
    head.push(0); // チャンネルマッピング: 0 = モノラル/ステレオ
    head
}

/// `OpusTags` パケット (RFC 7845)
fn opus_tags() -> Vec<u8> {
    const VENDOR: &[u8] = b"egui-clap-host spike";
    let mut tags = Vec::new();
    tags.extend_from_slice(b"OpusTags");
    tags.extend_from_slice(&(VENDOR.len() as u32).to_le_bytes());
    tags.extend_from_slice(VENDOR);
    tags.extend_from_slice(&0u32.to_le_bytes()); // コメント数
    tags
}

/// 実機が 48kHz を出せるか (書き出しレートの案を決めるのに使った)
fn report_device_rates() {
    use cpal::traits::{DeviceTrait, HostTrait};

    let Some(device) = cpal::default_host().default_output_device() else {
        println!("出力デバイスが見つかりません");
        return;
    };
    println!(
        "デバイス: {}",
        device.name().unwrap_or_else(|_| "(不明)".into())
    );

    let Ok(configs) = device.supported_output_configs() else {
        return;
    };
    let supports_48k = configs
        .map(|config| (config.min_sample_rate().0, config.max_sample_rate().0))
        .any(|(min, max)| (min..=max).contains(&48_000));
    println!(
        "48kHz を出せるか: {}\n",
        if supports_48k { "はい" } else { "いいえ" }
    );
}
