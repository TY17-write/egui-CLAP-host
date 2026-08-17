//! Opus 書き出しの検証。音源もオーディオデバイスも不要。
//!
//! 見るのは**スタックが足りるか**。`opus-rs` は大きな配列をスタックに置くので、
//! **積む場所によっては書き出しの途中で落ちる**。本体の書き出しは
//! `main.rs` の `export_opus` から**メインスレッド上で**呼ばれ、Windows の
//! メインスレッドは既定で **1MiB しか無い** (`.cargo/config.toml` も
//! `/STACK` の指定も無いため)。
//!
//! **0.1.26 は 1MiB で足りていたが、0.1.27 以降は release でも 1MiB で落ち、
//! debug では 2MiB でも足りない** (<https://github.com/restsend/opus-rs/issues/12>)。
//! そのため `opus::to_bytes` は自前でスレッドを立てて符号化する。ここでは
//! **その保護が効いているか**を、細いスタックから呼んで確かめる。
//!
//! **0.1.29 で上流が直し、必要量は release 約 0.38MiB / debug 約 0.53MiB まで
//! 落ちた**が、スレッドは残してある (理由は `opus.rs` の `ENCODE_STACK`)。
//! つまりここは今も**保護が効いていること**を見る場所で、保護が要るかどうかを
//! 見る場所ではない。
//!
//! スタックオーバーフローは捕まえられない (プロセスごと落ちる) ので、
//! **最後に出た行が落ちた場所**になる。
//!
//! ```text
//! cargo run --bin opus_smoke              # 1MiB のスレッドから呼ぶ
//! cargo run --bin opus_smoke -- 262144    # 256KiB から呼ぶ
//! cargo run --release --bin opus_smoke
//! ```

use clap_host_test::opus;

/// 本体のメインスレッドと同じ細さ (Windows の既定)
const DEFAULT_STACK: usize = 1024 * 1024;

/// 2秒あれば符号化器の内部は一通り動く
const SECONDS: usize = 2;
const CHANNELS: u16 = 2;

fn main() {
    let stack = std::env::args()
        .nth(1)
        .and_then(|a| a.parse().ok())
        .unwrap_or(DEFAULT_STACK);

    println!(
        "呼び出し側のスタック: {} バイト ({:.2} MiB)",
        stack,
        stack as f64 / (1024.0 * 1024.0)
    );
    println!("(落ちた場合、最後に出た行が落ちた場所)\n");

    let samples = signal();
    let worker = std::thread::Builder::new()
        .stack_size(stack)
        .spawn(move || {
            for kbps in opus::BITRATES_KBPS {
                println!("  {kbps:>4} kbps を符号化中...");
                match opus::to_bytes(&samples, CHANNELS, opus::SAMPLE_RATE, kbps) {
                    Ok(bytes) => println!("  {kbps:>4} kbps → {} バイト", bytes.len()),
                    Err(e) => {
                        println!("  {kbps:>4} kbps → 失敗: {e}");
                        return false;
                    }
                }
            }
            true
        })
        .expect("スレッドを立てられること");

    match worker.join() {
        Ok(true) => println!("\n全てのビットレートで書き出せた"),
        Ok(false) => {
            println!("\n符号化に失敗した");
            std::process::exit(1);
        }
        Err(_) => {
            println!("\nスレッドが落ちた");
            std::process::exit(1);
        }
    }
}

/// 和音 + 微小ノイズ。純音より符号化が難しく、帯域を広く使う
fn signal() -> Vec<f32> {
    let total = opus::SAMPLE_RATE as usize * SECONDS;
    let mut out = Vec::with_capacity(total * CHANNELS as usize);
    for frame in 0..total {
        let t = frame as f32 / opus::SAMPLE_RATE as f32;
        let noise = ((frame as f32 * 12.9898).sin() * 43758.547).fract() - 0.5;
        let chord = |f: f32| (t * f * std::f32::consts::TAU).sin();
        out.push((chord(220.0) + chord(277.2) + chord(329.6)) * 0.15 + noise * 0.05);
        out.push((chord(261.6) + chord(392.0) + chord(523.3)) * 0.15 + noise * 0.05);
    }
    out
}
