//! 符号化に要るスタックを測る。
//!
//! **0.1.28 は 0.1.26 より大幅に多くスタックを使う。** 本体は書き出しを
//! メインスレッド上で行い、Windows のメインスレッドは既定で 1MiB しか無いので、
//! 版を上げただけで**書き出しの最初で落ちる**ようになった。
//!
//! ここでは「符号化器を作る」「1フレーム符号化する」を細いスタックの上で
//! 順に行い、**どこで落ちるか**を見る。上流に出す最小の再現形でもある。
//!
//! # 分かったこと
//!
//! **犯人は `OpusEncoder::new`。** 符号化ではなく生成の時点で落ちる。
//! **原因は構造体そのものが巨大になったこと**で、**0.1.27 で入った**。
//!
//! | 版 | `size_of::<OpusEncoder>()` | release で要るスタック |
//! |---|---|---|
//! | 0.1.26 | 1,288 バイト | 64KiB でも足りる |
//! | 0.1.27 | 254,608 バイト | 832KiB では落ち、864KiB で足りる |
//! | 0.1.28 | 254,608 バイト | 同上 (debug は 1MiB で落ち、2MiB で足りる) |
//!
//! 0.1.26 は状態をヒープに置いていた (`Box<SilkEncoderState>`、`Vec<_>`) が、
//! 0.1.27 で**インラインの固定長**になった (`SilkEncoderState` を直に持ち、
//! `Vec<T>` → `FixedVec<T, N>` = `[MaybeUninit<T>; N]`)。`new` は `Self` を
//! 値で返すので、**呼ぶだけで 254KB の複製が数回スタックに積まれる**。
//!
//! **Windows のメインスレッドの既定 (1MiB) のすぐ際**なので、ビルドの些細な
//! 違いで落ちたり落ちなかったりする。実際、本体の書き出し経路は release でも
//! 1MiB で落ちたが、ここの同等コードは通った。
//!
//! スタックオーバーフローは捕まえられない (プロセスごと落ちる) ので、
//! **最後に出た行が落ちた場所**になる。
//!
//! ```text
//! cargo run --release --bin stack_probe            # 1MiB
//! cargo run --bin stack_probe -- 2097152           # debug で 2MiB
//! ```

use opus_rs::{Application, OpusEncoder};

const SAMPLE_RATE: i32 = 48_000;
const CHANNELS: usize = 2;
const FRAME_SIZE: usize = 960;
/// 2秒ぶん。落ちるなら最初の数フレームで落ちるので、これで十分
const FRAMES: usize = 100;

fn main() {
    let stack: usize = std::env::args()
        .nth(1)
        .and_then(|a| a.parse().ok())
        .unwrap_or(1024 * 1024);

    // **構造体そのものの大きさを先に出す。** これが大きければ、
    // 「戻り値をスタックに積む」だけで溢れるという話になる
    println!(
        "size_of::<OpusEncoder>() = {} バイト ({:.2} MiB)",
        std::mem::size_of::<OpusEncoder>(),
        std::mem::size_of::<OpusEncoder>() as f64 / (1024.0 * 1024.0)
    );
    println!(
        "スタック {} バイト ({:.2} MiB) のスレッドで試す",
        stack,
        stack as f64 / (1024.0 * 1024.0)
    );
    println!("(落ちた場合、最後に出た行が落ちた場所)\n");

    let worker = std::thread::Builder::new()
        .stack_size(stack)
        .spawn(|| {
            println!("  符号化器を作る...");
            let mut encoder = OpusEncoder::new(SAMPLE_RATE, CHANNELS, Application::Audio)
                .expect("符号化器を作れること");
            encoder.bitrate_bps = 48_000;
            println!("  作れた");

            // **まず無音**。これで落ちるなら入力の中身は関係ないと言える
            println!("  無音を1フレーム符号化する...");
            let mut packet = vec![0u8; 4000];
            let silence = vec![0.0f32; FRAME_SIZE * CHANNELS];
            let len = encoder
                .encode(&silence, FRAME_SIZE, &mut packet)
                .expect("符号化できること");
            println!("  符号化できた ({len} バイト)");

            // **次に実際の信号**。無音が通ってこちらで落ちるなら、
            // 中身によって通る経路が違うということ
            println!("  和音+ノイズを{FRAMES}フレーム符号化する...");
            for frame in 0..FRAMES {
                let block = chord_frame(frame);
                encoder
                    .encode(&block, FRAME_SIZE, &mut packet)
                    .expect("符号化できること");
                if frame % 10 == 0 {
                    println!("    {frame} フレーム目まで通過");
                }
            }
            println!("  符号化できた");
        })
        .expect("スレッドを立てられること");

    match worker.join() {
        Ok(()) => println!("\nこのスタックで足りる"),
        Err(_) => {
            println!("\nスレッドが落ちた");
            std::process::exit(1);
        }
    }
}

/// 和音 + 微小ノイズの1フレーム。無音より広く帯域を使う
fn chord_frame(frame_index: usize) -> Vec<f32> {
    let start = frame_index * FRAME_SIZE;
    let mut block = Vec::with_capacity(FRAME_SIZE * CHANNELS);
    for i in 0..FRAME_SIZE {
        let n = start + i;
        let t = n as f32 / SAMPLE_RATE as f32;
        let noise = ((n as f32 * 12.9898).sin() * 43758.547).fract() - 0.5;
        let chord = |f: f32| (t * f * std::f32::consts::TAU).sin();
        block.push((chord(220.0) + chord(277.2) + chord(329.6)) * 0.15 + noise * 0.05);
        block.push((chord(261.6) + chord(392.0) + chord(523.3)) * 0.15 + noise * 0.05);
    }
    block
}
