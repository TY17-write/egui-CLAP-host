//! 復号結果が入力とどれだけ一致しているかを測る。
//!
//! 上流への報告 (`upstream-issue.md`) で使った指標を、後から回し直せるように
//! コードにしたもの。**報告時は場外で測っており、コードが残っていなかった。**
//! 修正版の検証で同じ物差しが要るので、ここに置く。
//!
//! # 使い方
//!
//! ```text
//! cargo run --release                     # *_input.raw と *_{kbps}.opus を作る
//! for s in sine complex; do
//!   for k in 48 96 128 160 176 192 224 256 320 384 448 510; do
//!     ffmpeg -v error -y -i ${s}_${k}.opus -f f32le -ac 2 -ar 48000 dec_${s}_${k}.raw
//!   done
//! done
//! cargo run --release --bin spectral_match
//! ```
//!
//! ビットレートを変えたら `main.rs` と両方直すこと。
//!
//! # 指標
//!
//! 20ms フレームごとに入力と復号結果の**振幅スペクトル**を求め、
//! `10*log10( sum(A²) / sum((A-B)²) )` を出す。大きいほど元に近い。
//!
//! **波形の差ではなくスペクトルの差**を見るのは、Opus が位相を保たないため。
//! 波形で引き算すると、正常に符号化できていても大きな差が出てしまう。
//!
//! **「範囲外サンプル (|x|>1.0) の数」で測ってはいけない。** 範囲内に収まった
//! まま中身が壊れる場合を取りこぼす (和音+ノイズが実際にそうだった)。
//!
//! # この指標も万能ではない
//!
//! **0.1.26 の 510kbps は、聴くと明らかに壊れているのに 51.8dB (サイン波) と
//! 高く出る。** 176〜448kbps が 2〜5dB に崩れている中で、いちばん上のレートだけ
//! 正常に見えるという矛盾した形になる。
//!
//! 原因は追っていないが、**数値が高いことを「正常」の証拠に使わないこと。**
//! 崩れの検出 (低く出る) は信用できるが、逆は言えない。判断に使うなら聴く。

use std::f64::consts::TAU;

const CHANNELS: usize = 2;
/// 20ms。符号化と同じ刻みで比べる
const FRAME_SIZE: usize = 960;

/// `main.rs` の同名の定数と揃えること
const BITRATES_KBPS: [i32; 12] = [48, 96, 128, 160, 176, 192, 224, 256, 320, 384, 448, 510];

fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("信号     kbps   一致度(dB)   最悪フレーム(dB)   フレーム数");

    for label in ["sine", "complex"] {
        let input_path = format!("{label}_input.raw");
        let Ok(input) = read_f32(&input_path) else {
            println!("(飛ばす) {input_path} が無い。先に cargo run --release");
            continue;
        };

        for kbps in BITRATES_KBPS {
            let decoded_path = format!("dec_{label}_{kbps}.raw");
            let Ok(decoded) = read_f32(&decoded_path) else {
                println!("(飛ばす) {decoded_path} が無い。先に ffmpeg で復号");
                continue;
            };
            let (mean, worst, frames) = compare(&input, &decoded);
            println!("{label:<8} {kbps:>4}   {mean:>9.1}   {worst:>14.1}   {frames:>9}");
        }
    }
    Ok(())
}

/// フレームごとの一致度を求め、(平均, 最小, フレーム数) を返す。
///
/// **長さが違っても短いほうに合わせる。** 復号結果は容器の端数で数サンプル
/// 前後することがあり、そこで落とすと測れなくなる。
fn compare(input: &[f32], decoded: &[f32]) -> (f64, f64, usize) {
    let frames = input.len().min(decoded.len()) / (FRAME_SIZE * CHANNELS);
    let window = hann(FRAME_SIZE);

    let mut sum = 0.0;
    let mut worst = f64::INFINITY;
    let mut count = 0usize;

    for frame in 0..frames {
        for channel in 0..CHANNELS {
            let a = spectrum(input, frame, channel, &window);
            let b = spectrum(decoded, frame, channel, &window);

            let energy: f64 = a.iter().map(|v| v * v).sum();
            let error: f64 = a.iter().zip(&b).map(|(x, y)| (x - y) * (x - y)).sum();
            // 完全一致 (error = 0) は起きないが、0除算だけは避ける
            let db = 10.0 * (energy / error.max(1e-30)).log10();

            sum += db;
            worst = worst.min(db);
            count += 1;
        }
    }

    if count == 0 {
        return (f64::NAN, f64::NAN, 0);
    }
    (sum / count as f64, worst, frames)
}

/// 1フレーム・1チャンネルぶんの振幅スペクトル (0..N/2)。
///
/// 960点は2の冪ではないので素朴な DFT で回す。**検証用なので速度は問わない**
/// (全体でも数秒)。窓を掛けるのは、フレーム境界の不連続が漏れ込むのを抑えるため。
fn spectrum(samples: &[f32], frame: usize, channel: usize, window: &[f64]) -> Vec<f64> {
    let start = frame * FRAME_SIZE * CHANNELS + channel;
    let x: Vec<f64> = (0..FRAME_SIZE)
        .map(|i| samples[start + i * CHANNELS] as f64 * window[i])
        .collect();

    (0..FRAME_SIZE / 2)
        .map(|bin| {
            let (mut re, mut im) = (0.0, 0.0);
            for (i, v) in x.iter().enumerate() {
                let angle = TAU * bin as f64 * i as f64 / FRAME_SIZE as f64;
                re += v * angle.cos();
                im -= v * angle.sin();
            }
            (re * re + im * im).sqrt()
        })
        .collect()
}

fn hann(n: usize) -> Vec<f64> {
    (0..n)
        .map(|i| 0.5 - 0.5 * (TAU * i as f64 / n as f64).cos())
        .collect()
}

fn read_f32(path: &str) -> std::io::Result<Vec<f32>> {
    let bytes = std::fs::read(path)?;
    Ok(bytes
        .chunks_exact(4)
        .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect())
}
