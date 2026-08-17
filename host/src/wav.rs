//! WAV (RIFF) ファイルの書き出し。
//!
//! 依存クレートを増やしたくないので自前で組む。形式は 16bit リニア PCM のみ。
//! (32bit float WAV は再生できないアプリがあるため、素直に再生できる方を選ぶ)

/// WAV ヘッダの固定長 (RIFF 12 + fmt 24 + data 8)
const HEADER_LEN: usize = 44;

/// 1サンプルあたりのビット数
const BITS_PER_SAMPLE: u16 = 16;

/// RIFF のサイズ欄が u32 なので、データ部はこれを超えられない
const MAX_DATA_BYTES: usize = u32::MAX as usize - HEADER_LEN;

/// 16bit PCM の WAV バイト列を作る。
///
/// `samples` はインターリーブ済みで -1.0..=1.0 を想定する。範囲外は切り詰める
/// (呼び出し側でピークを均してから渡すこと)。
pub fn to_bytes_16bit(samples: &[f32], channels: u16, sample_rate: u32) -> Result<Vec<u8>, String> {
    if channels == 0 {
        return Err("チャンネル数が 0 です".into());
    }

    let bytes_per_sample = (BITS_PER_SAMPLE / 8) as usize;
    let data_len = samples.len() * bytes_per_sample;
    if data_len > MAX_DATA_BYTES {
        return Err("音声が長すぎて WAV に収まりません (4GB 超)".into());
    }

    let block_align = channels * BITS_PER_SAMPLE / 8;
    let byte_rate = sample_rate * block_align as u32;

    let mut out = Vec::with_capacity(HEADER_LEN + data_len);

    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&((HEADER_LEN - 8 + data_len) as u32).to_le_bytes());
    out.extend_from_slice(b"WAVE");

    out.extend_from_slice(b"fmt ");
    out.extend_from_slice(&16u32.to_le_bytes()); // fmt チャンクの中身の長さ
    out.extend_from_slice(&1u16.to_le_bytes()); // 1 = リニア PCM
    out.extend_from_slice(&channels.to_le_bytes());
    out.extend_from_slice(&sample_rate.to_le_bytes());
    out.extend_from_slice(&byte_rate.to_le_bytes());
    out.extend_from_slice(&block_align.to_le_bytes());
    out.extend_from_slice(&BITS_PER_SAMPLE.to_le_bytes());

    out.extend_from_slice(b"data");
    out.extend_from_slice(&(data_len as u32).to_le_bytes());
    for sample in samples {
        out.extend_from_slice(&to_i16(*sample).to_le_bytes());
    }

    Ok(out)
}

/// -1.0..=1.0 を 16bit 整数にする。
/// 正負で係数を変えず 32767 側に合わせるので、-1.0 は -32767 になる
/// (-32768 まで使うと正側だけ 1LSB 手前で頭打ちになり、非対称な歪みが出る)。
fn to_i16(sample: f32) -> i16 {
    (sample.clamp(-1.0, 1.0) * i16::MAX as f32).round() as i16
}

#[cfg(test)]
mod tests {
    use super::*;

    fn u32_at(bytes: &[u8], offset: usize) -> u32 {
        u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap())
    }

    fn u16_at(bytes: &[u8], offset: usize) -> u16 {
        u16::from_le_bytes(bytes[offset..offset + 2].try_into().unwrap())
    }

    /// ヘッダの各欄が仕様どおりの位置と値になっていること。
    /// (ここがずれると再生アプリが読めない、または速度が変わる)
    #[test]
    fn header_describes_the_stream() {
        let samples = vec![0.0f32; 8]; // ステレオ4フレーム
        let bytes = to_bytes_16bit(&samples, 2, 44_100).unwrap();

        assert_eq!(&bytes[0..4], b"RIFF");
        assert_eq!(&bytes[8..12], b"WAVE");
        assert_eq!(&bytes[12..16], b"fmt ");
        assert_eq!(&bytes[36..40], b"data");

        assert_eq!(u32_at(&bytes, 16), 16, "fmt チャンクの長さ");
        assert_eq!(u16_at(&bytes, 20), 1, "リニア PCM");
        assert_eq!(u16_at(&bytes, 22), 2, "チャンネル数");
        assert_eq!(u32_at(&bytes, 24), 44_100, "サンプルレート");
        assert_eq!(u32_at(&bytes, 28), 44_100 * 4, "バイト毎秒 = レート×4");
        assert_eq!(u16_at(&bytes, 32), 4, "ブロックアライン = 2ch×2byte");
        assert_eq!(u16_at(&bytes, 34), 16, "ビット深度");

        let data_len = samples.len() * 2;
        assert_eq!(u32_at(&bytes, 40), data_len as u32, "データ長");
        assert_eq!(u32_at(&bytes, 4), (36 + data_len) as u32, "RIFF サイズ");
        assert_eq!(bytes.len(), HEADER_LEN + data_len);
    }

    /// モノラルでもブロックアラインとバイト毎秒が合うこと
    #[test]
    fn mono_header_is_consistent() {
        let bytes = to_bytes_16bit(&[0.0; 4], 1, 48_000).unwrap();
        assert_eq!(u16_at(&bytes, 22), 1);
        assert_eq!(u16_at(&bytes, 32), 2, "1ch×2byte");
        assert_eq!(u32_at(&bytes, 28), 48_000 * 2);
    }

    /// 振幅の変換と、範囲外の切り詰め
    #[test]
    fn samples_are_clamped_and_scaled() {
        let bytes = to_bytes_16bit(&[0.0, 1.0, -1.0, 2.0, -2.0, 0.5], 1, 44_100).unwrap();
        let decoded: Vec<i16> = bytes[HEADER_LEN..]
            .chunks_exact(2)
            .map(|c| i16::from_le_bytes([c[0], c[1]]))
            .collect();

        assert_eq!(decoded[0], 0);
        assert_eq!(decoded[1], 32767);
        assert_eq!(decoded[2], -32767);
        assert_eq!(decoded[3], 32767, "1.0 を超えても飽和するだけ");
        assert_eq!(decoded[4], -32767);
        assert_eq!(decoded[5], 16384, "0.5 → 32767/2 を四捨五入");
    }

    #[test]
    fn zero_channels_is_rejected() {
        assert!(to_bytes_16bit(&[0.0], 0, 44_100).is_err());
    }
}
