//! Ogg/Opus の実験。本体には一切触らない。
//!
//! 2つのことに使った。
//!
//! 1. **`opus-rs` の生パケットを Ogg で包めるか** (フェーズ0)。ヘッダ
//!    (`OpusHead` / `OpusTags`) とグラニュール位置は自分で組み立てる必要がある
//! 2. **高いビットレートで壊れる件の再現** (下記)
//!
//! # 高ビットレートで壊れる
//!
//! `opus-rs` 0.1.26 は、内容によって**壊れたストリームを吐く**。本家 libopus で
//! 復号すると範囲外の値が散発し、聴くとクリックノイズになる。
//!
//! ```text
//! cargo run
//! for k in 48 96 128 160 176 192; do
//!   ffmpeg -v error -y -i spike_$k.opus -f f32le -ac 2 -ar 48000 dec_$k.raw
//! done
//! # dec_*.raw を f32 として読み、|x| > 1.0 の個数を数える
//! #   96/128/160 → 0 個 / 176 → 160 個 / 192 → 111 個
//! ```
//!
//! **信号依存**で、和音 + ノイズのような複雑な信号では 192kbps でも出なかった。
//! `complexity` を変えても改善しない。本体では実測で問題の無かった範囲
//! (48 / 96kbps) だけを出している。

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

/// 壊れ方を見るためのビットレート。純音では 176 以上で範囲外の値が出る
const BITRATES_KBPS: [i32; 6] = [48, 96, 128, 160, 176, 192];

fn main() -> Result<(), Box<dyn std::error::Error>> {
    report_device_rates();

    // 純音のほうが壊れやすい (複雑な信号では再現しなかった)
    let total_frames = SAMPLE_RATE as usize * SECONDS;
    let mut input = Vec::with_capacity(total_frames * CHANNELS);
    for frame in 0..total_frames {
        let t = frame as f32 / SAMPLE_RATE as f32;
        input.push((t * 440.0 * std::f32::consts::TAU).sin() * 0.5);
        input.push((t * 660.0 * std::f32::consts::TAU).sin() * 0.5);
    }

    for kbps in BITRATES_KBPS {
        let bytes = encode_ogg_opus(&input, kbps * 1000)?;
        let name = format!("spike_{kbps}.opus");
        std::fs::write(&name, &bytes)?;
        println!("書き出し: {name} ({} バイト)", bytes.len());
    }
    println!("\nffmpeg で復号し |x| > 1.0 を数えると壊れ方が分かる (冒頭のコメント参照)");
    Ok(())
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
    const VENDOR: &[u8] = b"clap-host-test spike";
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
