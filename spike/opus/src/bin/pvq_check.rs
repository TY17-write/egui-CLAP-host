//! 高ビットレートで壊れる原因を絞るための検証。
//!
//! CELT の PVQ 探索は **ちょうど K 個のパルス**を返さなければならない
//! (`sum(|y|) == K`)。ここが崩れると、符号化した索引が符号帳の外に出て、
//! **本家 libopus の復号器と食い違う** = 散発的なノイズになる。
//!
//! `opus-rs` の `pvq_search` は n と k で複数の実装に分岐しており
//! (n==2 / n==4 / n>=32 の fast_select / AVX2 / スカラ)、**分岐のどれかが
//! 崩れていないか**を総当たりで見る。

use opus_rs::pvq::pvq_search;

fn main() {
    // CELT が実際に使う範囲。K は高ビットレートほど大きくなる
    let sizes = [
        2usize, 3, 4, 6, 8, 12, 16, 24, 32, 48, 64, 96, 128, 176, 200,
    ];
    let ks = [
        1i32, 2, 3, 4, 5, 6, 7, 8, 12, 16, 24, 32, 48, 64, 96, 128, 160, 192, 256,
    ];

    let mut checked = 0usize;
    let mut broken = Vec::new();

    for &n in &sizes {
        for &k in &ks {
            for seed in 0..8u32 {
                let x = make_input(n, seed);
                let mut y = vec![0i32; n];
                pvq_search(&x, &mut y, k, n);

                let sum: i32 = y.iter().map(|v| v.abs()).sum();
                checked += 1;
                if sum != k {
                    broken.push((n, k, seed, sum));
                }
            }
        }
    }

    println!("調べた組み合わせ: {checked}");
    if broken.is_empty() {
        println!("すべて sum(|y|) == K。探索は仕様どおり");
        return;
    }

    println!("**K と合わない組み合わせ: {} 件**", broken.len());
    // 同じ (n,k) は代表1件だけ出す
    let mut seen = Vec::new();
    for (n, k, seed, sum) in &broken {
        if seen.contains(&(*n, *k)) {
            continue;
        }
        seen.push((*n, *k));
        println!("  n={n:>4} k={k:>4} (seed {seed}) → sum(|y|)={sum}");
    }
}

/// 決まった手順で作る入力 (再現できるように乱数は使わない)。
/// seed で「純音的 (1本だけ大きい)」から「平坦」まで変える。
fn make_input(n: usize, seed: u32) -> Vec<f32> {
    let mut x = Vec::with_capacity(n);
    let sharpness = seed as f32;
    for i in 0..n {
        let t = i as f32 / n as f32;
        // seed が小さいほど1点に集中する (純音に近い)
        let peak = (-((t - 0.25) * (t - 0.25)) * (40.0 - sharpness * 4.0)).exp();
        let ripple = (t * 17.0 + seed as f32).sin() * 0.1;
        x.push(peak + ripple);
    }
    // PVQ の入力は正規化されている前提
    let norm = x.iter().map(|v| v * v).sum::<f32>().sqrt().max(1e-9);
    for v in &mut x {
        *v /= norm;
    }
    x
}
