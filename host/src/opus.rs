//! Ogg/Opus への書き出し。
//!
//! 符号化は [`opus-rs`](https://crates.io/crates/opus_rs)、容器は
//! [`ogg`](https://crates.io/crates/ogg)。**どちらも純 Rust** なので、
//! libopus を直接使う道 (CMake が要る) を採らずに済んでいる。
//!
//! `.opus` は生のパケットをそのまま並べたものではなく、Ogg で包んで先頭に
//! `OpusHead` と `OpusTags` を置いた形 (RFC 7845)。ヘッダとグラニュール位置は
//! ここで組み立てる。

use ogg::writing::{PacketWriteEndInfo, PacketWriter};
use opus_rs::{Application, OpusEncoder};

/// Opus が鳴らせる唯一のレート (このプロジェクトでは 48kHz に固定する)。
///
/// 規格上は 8/12/16/24/48kHz を受けるが、内部は常に 48kHz で、それ以外は
/// 符号化器の中で変換される。**書き出し前に音源を 48kHz で動かし直す**ので、
/// ここへ来る時点で 48kHz になっている。
pub const SAMPLE_RATE: u32 = 48_000;

/// 1パケットの長さ (48kHz で 20ms)。
/// Opus の標準的なフレーム長で、パケット数と効率の釣り合いが良い。
const FRAME_SIZE: usize = 960;

/// 符号化の先読み (プリスキップ)。復号側は頭からこのぶんを捨てる。
///
/// **入れた音を最後まで取り出すには、そのぶん余分に流し込む必要がある。**
/// 忘れると出来上がりが短くなる (フェーズ0 では 2.000秒 が 1.9935秒 になった)。
const PRE_SKIP: u16 = 312;

/// 1パケットの上限。20ms・最高ビットレートでもここには収まる。
const MAX_PACKET: usize = 4000;

/// 選べるビットレート (kbps)。
///
/// **かつて 48 / 96 に絞っていた。** `opus-rs` 0.1.26 が、あるビットレートを
/// 超えると壊れたストリームを吐いたため (本家 libopus で復号するとノイズ)。
/// **0.1.28 で修正された。** 試験信号 2種では 510kbps まで正常になっている。
/// 測定と経緯は `docs/export_rate_plan.md` の「高ビットレートは出さない」と
/// `spike/opus/upstream-issue.md`。
///
/// **192 が上限なのは、それ以上を必要としていないから** (試験信号では 510kbps
/// まで正常だったので、クレート側の制限ではない)。**元の不具合が発覚したのは
/// 実際の曲の 192kbps** だったので、そこは曲を書き出して聴いて確かめてある
/// (ノイズにならず、落ちもしない)。
pub const BITRATES_KBPS: [u32; 4] = [48, 96, 128, 192];

/// 既定のビットレート。
///
/// **Opus の利点は低いビットレートでも音質が保たれること**なので、そこを
/// 活かせる値を既定にする。上げたい場合はメニューで選ぶ。
pub const DEFAULT_BITRATE_KBPS: u32 = 96;

/// 符号化に使うスレッドのスタック。
///
/// **`OpusEncoder::new` が大量にスタックを使う。** 0.1.26 は 64KiB でも足りたが、
/// **0.1.27 以降は release で約 0.85MiB、debug では 2〜3MiB 要る** (実測は
/// `spike/opus/src/bin/stack_probe.rs` と `bin/opus_smoke.rs`)。
///
/// 0.1.27 で符号化器の状態がヒープからインラインの固定長へ移り、
/// `size_of::<OpusEncoder>()` が **1,288 → 254,608 バイト**になった。
/// `new` は `Self` を値で返すので、**呼ぶだけで複製が数回積まれる**。
///
/// 上流へは「ヒープに置く経路も欲しい」と出してある:
/// <https://github.com/restsend/opus-rs/issues/12>。
/// **入れば、このスレッドは要らなくなる。**
///
/// **書き出しは `export_opus` からメインスレッド上で呼ばれ、Windows の
/// メインスレッドは既定で 1MiB しか無い。** 必要量がその 1MiB のすぐ際にあるので、
/// **ビルドの些細な違いで落ちたり落ちなかったりする** — 実際、本体の経路は
/// release でも落ち、spike の同等コードは通った。**際どい線に賭けない。**
///
/// **符号化1回ぶんの一時的な確保**なので、余裕を取って困ることはない。
const ENCODE_STACK: usize = 8 * 1024 * 1024;

/// インターリーブされた f32 を Ogg/Opus のバイト列にする。
///
/// `sample_rate` は [`SAMPLE_RATE`] でなければならない (呼び出し側で揃えること)。
///
/// **符号化は専用のスレッドで行う** ([`ENCODE_STACK`] の理由を参照)。
/// 呼び出し側から見た振る舞いは変わらない (終わるまで待つ)。
pub fn to_bytes(
    samples: &[f32],
    channels: u16,
    sample_rate: u32,
    bitrate_kbps: u32,
) -> Result<Vec<u8>, String> {
    // 借りたまま渡したいので、スコープ付きスレッドを使う
    std::thread::scope(|scope| {
        let worker = std::thread::Builder::new()
            .stack_size(ENCODE_STACK)
            .spawn_scoped(scope, || {
                encode(samples, channels, sample_rate, bitrate_kbps)
            })
            .map_err(|e| format!("Opus の符号化を始められません: {e}"))?;
        worker
            .join()
            .map_err(|_| "Opus の符号化が異常終了しました".to_string())?
    })
}

fn encode(
    samples: &[f32],
    channels: u16,
    sample_rate: u32,
    bitrate_kbps: u32,
) -> Result<Vec<u8>, String> {
    if sample_rate != SAMPLE_RATE {
        return Err(format!(
            "Opus は {}Hz でのみ書き出せます (渡されたのは {sample_rate}Hz)",
            SAMPLE_RATE
        ));
    }
    // マッピングファミリ0 はモノラルとステレオだけ。それ以上は真面目に
    // チャンネル対応表を書く必要があるので、ここでは断る。
    if channels != 1 && channels != 2 {
        return Err(format!(
            "Opus で書き出せるのはモノラルかステレオだけです ({channels}ch)"
        ));
    }
    if samples.is_empty() {
        return Err("書き出す音がありません".into());
    }

    let channels = channels as usize;
    let mut encoder = OpusEncoder::new(SAMPLE_RATE as i32, channels, Application::Audio)
        .map_err(|e| format!("Opus の符号化器を作れません: {e}"))?;
    encoder.bitrate_bps = (bitrate_kbps * 1000) as i32;

    let mut out = Vec::new();
    {
        let mut writer = PacketWriter::new(&mut out);
        // 1本しか入れないので固定でよい
        const SERIAL: u32 = 0x5350_494b;

        writer
            .write_packet(
                opus_head(channels as u8),
                SERIAL,
                PacketWriteEndInfo::EndPage,
                0,
            )
            .map_err(|e| format!("Opus のヘッダを書けません: {e}"))?;
        writer
            .write_packet(opus_tags(), SERIAL, PacketWriteEndInfo::EndPage, 0)
            .map_err(|e| format!("Opus のタグを書けません: {e}"))?;

        // グラニュール位置は「そこまでに復号できる 48kHz サンプル数」で、
        // プリスキップを含む。終端は「元の長さ + プリスキップ」。
        let source_frames = (samples.len() / channels) as u64;
        let target_granule = source_frames + PRE_SKIP as u64;

        let mut packet = vec![0u8; MAX_PACKET];
        let mut fed: u64 = 0;
        let mut index = 0usize;

        while fed < target_granule {
            let start = index * FRAME_SIZE * channels;
            let end = (start + FRAME_SIZE * channels).min(samples.len());
            let mut frame: Vec<f32> = samples.get(start..end).unwrap_or(&[]).to_vec();
            // 足りないぶんは無音で埋める (フレーム長は固定のため)
            frame.resize(FRAME_SIZE * channels, 0.0);

            let len = encoder
                .encode(&frame, FRAME_SIZE, &mut packet)
                .map_err(|e| format!("Opus の符号化に失敗しました: {e}"))?;
            fed += FRAME_SIZE as u64;
            index += 1;

            // 最後のページで、余分に出したぶんを切り詰める
            let granule = fed.min(target_granule);
            let last = fed >= target_granule;
            writer
                .write_packet(
                    packet[..len].to_vec(),
                    SERIAL,
                    if last {
                        PacketWriteEndInfo::EndStream
                    } else {
                        PacketWriteEndInfo::NormalPacket
                    },
                    granule,
                )
                .map_err(|e| format!("Opus のパケットを書けません: {e}"))?;
        }
    }
    Ok(out)
}

/// `OpusHead` パケット (RFC 7845)
fn opus_head(channels: u8) -> Vec<u8> {
    let mut head = Vec::with_capacity(19);
    head.extend_from_slice(b"OpusHead");
    head.push(1); // バージョン
    head.push(channels);
    head.extend_from_slice(&PRE_SKIP.to_le_bytes());
    // 元のレート。情報として持つだけで、復号は常に 48kHz
    head.extend_from_slice(&SAMPLE_RATE.to_le_bytes());
    head.extend_from_slice(&0i16.to_le_bytes()); // 出力ゲイン
    head.push(0); // チャンネルマッピング: 0 = モノラル/ステレオ
    head
}

/// `OpusTags` パケット (RFC 7845)。コメントは付けない
fn opus_tags() -> Vec<u8> {
    const VENDOR: &[u8] = b"clap-host-test";
    let mut tags = Vec::with_capacity(8 + 4 + VENDOR.len() + 4);
    tags.extend_from_slice(b"OpusTags");
    tags.extend_from_slice(&(VENDOR.len() as u32).to_le_bytes());
    tags.extend_from_slice(VENDOR);
    tags.extend_from_slice(&0u32.to_le_bytes()); // コメント数
    tags
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 2秒ぶんのサイン波 (ステレオ)
    fn sine(seconds: usize) -> Vec<f32> {
        let frames = SAMPLE_RATE as usize * seconds;
        let mut samples = Vec::with_capacity(frames * 2);
        for frame in 0..frames {
            let t = frame as f32 / SAMPLE_RATE as f32;
            samples.push((t * 440.0 * std::f32::consts::TAU).sin() * 0.5);
            samples.push((t * 660.0 * std::f32::consts::TAU).sin() * 0.5);
        }
        samples
    }

    /// Ogg の体裁になっていること (先頭が OggS で、OpusHead が入る)
    #[test]
    fn writes_an_ogg_opus_stream() {
        let bytes = to_bytes(&sine(1), 2, SAMPLE_RATE, 128).expect("書き出せること");
        assert_eq!(&bytes[..4], b"OggS", "Ogg のページで始まること");
        assert!(
            bytes.windows(8).any(|w| w == b"OpusHead"),
            "OpusHead が入っていること"
        );
        assert!(
            bytes.windows(8).any(|w| w == b"OpusTags"),
            "OpusTags が入っていること"
        );
    }

    /// ビットレートが効いていること (上げるとファイルが大きくなる)
    #[test]
    fn higher_bitrate_makes_a_bigger_file() {
        let samples = sine(1);
        let mut previous = 0;
        for kbps in BITRATES_KBPS {
            let size = to_bytes(&samples, 2, SAMPLE_RATE, kbps)
                .expect("書き出せること")
                .len();
            assert!(
                size > previous,
                "{kbps}kbps が前の段階より大きくなること ({size} <= {previous})"
            );
            previous = size;
        }
    }

    /// 48kHz 以外と、扱えないチャンネル数は断ること。
    ///
    /// **黙って別のレートで書くと、再生時に音程が変わる。**
    #[test]
    fn refuses_what_it_cannot_encode() {
        let samples = sine(1);
        assert!(
            to_bytes(&samples, 2, 44_100, 128).is_err(),
            "44.1kHz は断る"
        );
        assert!(
            to_bytes(&samples, 3, SAMPLE_RATE, 128).is_err(),
            "3ch は断る"
        );
        assert!(to_bytes(&[], 2, SAMPLE_RATE, 128).is_err(), "空は断る");
    }

    /// 長さがサンプル数から決まる範囲に収まること。
    ///
    /// 目安として、128kbps・1秒なら 16KB 前後。**桁が違えばフレーム長か
    /// グラニュールの勘定を間違えている。**
    #[test]
    fn output_size_is_in_the_expected_range() {
        let bytes = to_bytes(&sine(1), 2, SAMPLE_RATE, 128).expect("書き出せること");
        assert!(
            (12_000..24_000).contains(&bytes.len()),
            "128kbps・1秒として妥当な大きさであること (実際は {} バイト)",
            bytes.len()
        );
    }
}
