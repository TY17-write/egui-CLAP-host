//! 符号化に要るスタックを測る。
//!
//! **0.1.27〜0.1.28 は 0.1.26 より大幅に多くスタックを使う。** 本体は書き出しを
//! メインスレッド上で行い、Windows のメインスレッドは既定で 1MiB しか無いので、
//! 版を上げただけで**書き出しの最初で落ちる**ようになった。
//!
//! ここでは「符号化器を作る」「1フレーム符号化する」を細いスタックの上で
//! 順に行い、**どこで落ちるか**を見る。上流に出した最小の再現形でもある
//! (<https://github.com/restsend/opus-rs/issues/12>)。
//!
//! # 分かったこと
//!
//! **犯人は `OpusEncoder::new`。** 符号化ではなく生成の時点で落ちる。
//! **原因は構造体そのものが巨大になったこと**で、**0.1.27 で入った**。
//!
//! | 版 | `size_of::<OpusEncoder>()` | release | debug |
//! |---|---|---|---|
//! | 0.1.26 | 1,288 バイト | 64KiB でも足りる | — |
//! | 0.1.27 | 254,608 バイト | 832KiB は落ち、864KiB で足りる | — |
//! | 0.1.28 | 254,608 バイト | 同上 | 1MiB は落ち、2MiB で足りる |
//! | 0.1.29 | **2,416 バイト** | **384KiB は落ち、386KiB で足りる** | **512KiB は落ち、544KiB で足りる** |
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
//! # `Box` で包んでも逃げられない (実測)
//!
//! `Box::new(OpusEncoder::new(..))` と書いても、**閾値は1バイトも動かない。**
//!
//! | 作り方 | 0.1.28 release | 0.1.28 debug |
//! |---|---|---|
//! | そのまま | 832KiB は落ち、864KiB で足りる | 1MiB は落ち、2MiB で足りる |
//! | `Box` で包む | **同じ** | **同じ** |
//!
//! `new` が `Self` を値で返す以上、**`Box::new` に渡す時点で既にスタック上に
//! 出来上がっている**。Rust は「戻り値をヒープの確保先へ直接書く」ことを
//! 保証しないので、包むのは手遅れ。
//!
//! **クレート側で `Box` を返す構築子を足すだけでも同じこと**で、中で
//! `Box::new(Self { .. })` と書けば結局スタックに積まれる。避けるには
//! `Box<MaybeUninit<Self>>` を確保して**その場で埋める**必要がある。
//!
//! # 0.1.29 で issue #12 が入った (2026-08-17)
//!
//! 既定で有効な `heap` フィーチャが足され、`OpusEncoder` / `OpusDecoder` の
//! 大きなフィールドが `Box` 越しになった (`celt_enc`、`silk_enc`、各 `FixedVec`)。
//! **構造体は 254,608 → 2,416 バイト**、必要量は release で約 0.85MiB → 約
//! 0.38MiB、**debug は 2MiB 超 → 0.53MiB** になり、Windows の 1MiB に収まった。
//!
//! **ただし「包むのは手遅れ」はそのまま残っている。** `CeltEncoder` と
//! `SilkEncoderState` の中身は今もインラインの固定長で、`new` は
//!
//! ```ignore
//! let celt_enc = Box::new(CeltEncoder::new(mode, channels));
//! let mut silk_enc = Box::new(SilkEncoderState::default());
//! ```
//!
//! と書いている。**`Box::new` に渡す時点で一度スタックに載る**ので、
//! 2.4KB の構造体に対して 386KiB (release) を要求する。**構造体の大きさから
//! 予想される量とは2桁違う**ことに注意。
//!
//! 本体側は専用スレッドを畳まず残す。**畳んでも得が無く、際どい線に賭ける
//! 理由が無い**ため (`host/src/opus.rs` の `ENCODE_STACK`)。
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
    // 第2引数に box を渡すと `Box::new(OpusEncoder::new(..))` を測る。
    // **呼び出し側で包めば逃げられるのか**を確かめるため
    let boxed = std::env::args().nth(2).is_some_and(|a| a == "box");

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
    println!(
        "作り方: {}",
        if boxed {
            "Box で包む"
        } else {
            "そのまま"
        }
    );
    println!("(落ちた場合、最後に出た行が落ちた場所)\n");

    let worker = std::thread::Builder::new()
        .stack_size(stack)
        .spawn(move || {
            println!("  符号化器を作る...");
            // **`Box::new` に渡す前に、一度スタック上へ作られる。**
            // Rust は「戻り値をヒープの確保先へ直接書く」ことを保証しないので、
            // 呼び出し側で包んでも逃げられるとは限らない。実測で確かめる。
            if boxed {
                let mut encoder = Box::new(
                    OpusEncoder::new(SAMPLE_RATE, CHANNELS, Application::Audio)
                        .expect("符号化器を作れること"),
                );
                println!("  作れた");
                encode_all(&mut encoder);
            } else {
                let mut encoder = OpusEncoder::new(SAMPLE_RATE, CHANNELS, Application::Audio)
                    .expect("符号化器を作れること");
                println!("  作れた");
                encode_all(&mut encoder);
            }
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

/// 無音1フレームと、和音+ノイズを `FRAMES` フレーム符号化する。
///
/// 生成の仕方 (そのまま / `Box`) で共用したいので `&mut` で受ける。
fn encode_all(encoder: &mut OpusEncoder) {
    encoder.bitrate_bps = 48_000;
    let mut packet = vec![0u8; 4000];

    // **まず無音**。これで落ちるなら入力の中身は関係ないと言える
    println!("  無音を1フレーム符号化する...");
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
