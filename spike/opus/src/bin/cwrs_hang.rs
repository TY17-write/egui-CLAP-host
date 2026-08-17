//! `cwrs_check` が固まる件を、関数単位まで絞る。
//!
//! 0.1.28 で `N=200` の小さい `K` が**戻ってこなくなる**のを見つけたので、
//! `pvq_search` / `celt_pvq_v` / `icwrs` / `cwrsi` のどこで起きているのかを分ける。
//!
//! **単純な `y` を自分で作って渡しても再現しない。** 実際に `pvq_search` が
//! 返した `y` を渡さないと起きないので、`cwrs_check` と同じ順に呼び、
//! **途中まで**を制限時間つきで回す。最初に固まる段が犯人。
//!
//! `N=200` は本家 libopus が帯域分割で作る `N` の集合 (176,144,96,...) には
//! 無い。**表の外を引いたときの振る舞い**を見るための値として入れてある。
//!
//! 0.1.26 と 0.1.28 を切り替えて回すと、パニックが無限ループに変わったのか、
//! 元から固まっていたのかが分かる。
//!
//! ```text
//! cargo update -p opus-rs --precise 0.1.26
//! cargo run --release --bin cwrs_hang
//! ```

use std::sync::mpsc;
use std::time::Duration;

use opus_rs::pvq::{celt_pvq_v, cwrsi, icwrs, pvq_search};

const LIMIT: Duration = Duration::from_secs(2);
/// 「重いだけ」を無限ループと言い違えないための、念のための長い待ち時間
const LONG_LIMIT: Duration = Duration::from_secs(120);

/// どこまで進めるか
#[derive(Clone, Copy)]
enum Stage {
    Search,
    Value,
    Index,
    RoundTrip,
}

fn main() {
    std::panic::set_hook(Box::new(|_| {}));

    println!("N    K   seed  pvq_search   celt_pvq_v   icwrs        cwrsi         V(N,K)   索引");
    for n in [176usize, 200] {
        for k in [1i32, 2, 4, 8, 16] {
            for seed in 0..4u32 {
                let s = probe(n, k, seed, Stage::Search);
                let v = probe(n, k, seed, Stage::Value);
                let i = probe(n, k, seed, Stage::Index);
                let c = probe(n, k, seed, Stage::RoundTrip);
                // 全部素通りなら出さない (固まる組み合わせだけ見たい)
                if [s, v, i, c].iter().all(|r| *r == "返る") {
                    continue;
                }
                let (size, index) = size_and_index(n, k, seed);
                println!(
                    "{n:<4} {k:<3} {seed:<5} {s:<12} {v:<12} {i:<12} {c:<12} {size:>8} {index:>6}"
                );
            }
        }
    }
    println!("\n(出ていない組み合わせはすべて素通り)");

    // **最小の再現形を作るため**、索引を直接振る。`pvq_search` を通さずに
    // 固まるなら、報告は `cwrsi` の1行で済む。
    //
    // **境目は決め打ちで探さない。** 固まる i が単調に並んでいる保証が無いので、
    // 二分探索はせず、桁を刻んで「どれが固まったか」をそのまま並べる。
    // N=176 も同じように振る。**本家が実際に作る N なので、こちらが無事で
    // あることを確かめておかないと「書き出しには影響しない」と言えない。**
    for n in [176usize, 200] {
        println!("\ncwrsi(N={n}, K=2, i) : V={}", celt_pvq_v(n as u32, 2));
        for i in [
            0u32, 1, 63, 64, 128, 256, 512, 1024, 2048, 4096, 8192, 16384, 16592,
        ] {
            println!("  i={i:<6} {}", probe_index(n, 2, i, LIMIT));
        }
    }

    // **「遅い」と「返らない」は分けて確かめる。** 2秒で切ると、単に重いだけの
    // 計算を無限ループと言い違える
    println!("\n同じ呼び出しを{}秒待つ", LONG_LIMIT.as_secs());
    println!("  i=16592 {}", probe_index(200, 2, 16592, LONG_LIMIT));

    // 時間切れのスレッドが残っていると終われない
    std::process::exit(0);
}

/// 符号帳の大きさと、`pvq_search` の結果から得た索引
fn size_and_index(n: usize, k: i32, seed: u32) -> (u32, u32) {
    let x = make_input(n, seed);
    let mut y = vec![0i32; n];
    pvq_search(&x, &mut y, k, n);
    (
        celt_pvq_v(n as u32, k as u32),
        icwrs(n as u32, k as u32, &y),
    )
}

/// `cwrsi` に索引を直接渡す (pvq_search を通さない)
fn probe_index(n: usize, k: u32, i: u32, limit: Duration) -> &'static str {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let call = move || {
            let mut back = vec![0i32; n];
            cwrsi(n as u32, k, i, &mut back);
        };
        let _ = tx.send(std::panic::catch_unwind(call).is_ok());
    });
    match rx.recv_timeout(limit) {
        Ok(true) => "返る",
        Ok(false) => "パニック",
        Err(_) => "**返らない**",
    }
}

/// 指定の段まで回して、返ったか・パニックか・固まったかを返す
fn probe(n: usize, k: i32, seed: u32, stage: Stage) -> &'static str {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(std::panic::catch_unwind(move || run(n, k, seed, stage)).is_ok());
    });
    match rx.recv_timeout(LIMIT) {
        Ok(true) => "返る",
        Ok(false) => "パニック",
        Err(_) => "**固まる**",
    }
}

fn run(n: usize, k: i32, seed: u32, stage: Stage) {
    let x = make_input(n, seed);
    let mut y = vec![0i32; n];
    pvq_search(&x, &mut y, k, n);
    if matches!(stage, Stage::Search) {
        return;
    }

    let v = celt_pvq_v(n as u32, k as u32);
    if matches!(stage, Stage::Value) {
        return;
    }

    let i = icwrs(n as u32, k as u32, &y);
    if matches!(stage, Stage::Index) {
        return;
    }

    // 符号帳の外を指しているなら往復させても意味が無い
    if v == 0 || v == u32::MAX || i >= v {
        return;
    }
    let mut back = vec![0i32; n];
    cwrsi(n as u32, k as u32, i, &mut back);
}

/// `cwrs_check` と同じ入力の作り方
fn make_input(n: usize, seed: u32) -> Vec<f32> {
    let mut x = Vec::with_capacity(n);
    for i in 0..n {
        let t = i as f32 / n as f32;
        let peak = (-((t - 0.25) * (t - 0.25)) * (40.0 - seed as f32 * 8.0)).exp();
        x.push(peak + (t * 17.0 + seed as f32).sin() * 0.1);
    }
    let norm = x.iter().map(|v| v * v).sum::<f32>().sqrt().max(1e-9);
    for v in &mut x {
        *v /= norm;
    }
    x
}
