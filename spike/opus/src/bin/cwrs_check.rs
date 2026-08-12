//! PVQ の索引付けが破綻していないかを見る。
//!
//! CELT はパルス配置 `y` を「符号帳の何番目か」という整数 `i` にして送る。
//! 成り立っていなければならないのは2つ。
//!
//! - `i < V(N,K)` (符号帳の外を指さない)
//! - `cwrsi(N, K, i) == y` (往復する)
//!
//! **`V(N,K)` は N と K が大きいと 32bit を溢れる。** 本家 libopus は溢れない
//! 範囲しか使わないよう帯域を分割するが、`opus-rs` が同じ制約を守れていなければ、
//! 索引が符号帳の外へ出て**本家の復号器と食い違う**。

use opus_rs::pvq::{celt_pvq_v, cwrsi, icwrs, pvq_search};

fn main() {
    let sizes = [2usize, 3, 4, 6, 8, 12, 16, 24, 32, 48, 64, 96, 128, 176, 200];
    // k >= 129 は compute_u が範囲外アクセスでパニックする (別途報告する)
    let ks = [1i32, 2, 4, 8, 16, 32, 64, 96, 112, 127];

    let mut checked = 0usize;
    let mut out_of_range = Vec::new();
    let mut round_trip_failed = Vec::new();
    let mut overflowed = Vec::new();

    for &n in &sizes {
        for &k in &ks {
            for seed in 0..4u32 {
                let x = make_input(n, seed);
                let mut y = vec![0i32; n];
                pvq_search(&x, &mut y, k, n);

                let v = celt_pvq_v(n as u32, k as u32);
                let i = icwrs(n as u32, k as u32, &y);
                checked += 1;

                // V が 0 になるのは溢れた証拠 (本来 1 以上)
                if v == 0 {
                    overflowed.push((n, k));
                    continue;
                }
                if i >= v {
                    out_of_range.push((n, k, i, v));
                    continue;
                }
                let mut back = vec![0i32; n];
                cwrsi(n as u32, k as u32, i, &mut back);
                if back != y {
                    round_trip_failed.push((n, k, seed));
                }
            }
        }
    }

    println!("調べた組み合わせ: {checked}");
    report("V(N,K) が 0 (32bit を溢れた)", &overflowed);
    report4("索引が符号帳の外 (i >= V)", &out_of_range);
    report3("往復しない", &round_trip_failed);
    if overflowed.is_empty() && out_of_range.is_empty() && round_trip_failed.is_empty() {
        println!("索引付けは健全");
    }
}

fn report(title: &str, items: &[(usize, i32)]) {
    if items.is_empty() {
        return;
    }
    println!("**{title}: {} 件**", items.len());
    let mut seen = Vec::new();
    for (n, k) in items {
        if seen.contains(&(*n, *k)) {
            continue;
        }
        seen.push((*n, *k));
        println!("  n={n:>4} k={k:>4}");
    }
}

fn report4(title: &str, items: &[(usize, i32, u32, u32)]) {
    if items.is_empty() {
        return;
    }
    println!("**{title}: {} 件**", items.len());
    for (n, k, i, v) in items.iter().take(12) {
        println!("  n={n:>4} k={k:>4} i={i} V={v}");
    }
}

fn report3(title: &str, items: &[(usize, i32, u32)]) {
    if items.is_empty() {
        return;
    }
    println!("**{title}: {} 件**", items.len());
    for (n, k, seed) in items.iter().take(12) {
        println!("  n={n:>4} k={k:>4} (seed {seed})");
    }
}

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
