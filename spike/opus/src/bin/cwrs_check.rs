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
//!
//! # 1件ずつ別スレッドで回す理由
//!
//! 0.1.26 では表外の `(N,K)` で**パニック**し、0.1.28 では**戻ってこなくなる**
//! (`compute_u` の範囲外アクセスが飽和に変わり、パニックの代わりに `cwrsi` が
//! 抜けられない)。どちらも main で直に呼ぶと**そこで打ち切られて残りが測れない**
//! ので、時間切れを見て次の組み合わせへ進む。
//!
//! 時間切れになったスレッドは止められないので放置し、最後に `exit` で畳む。

use std::sync::mpsc;
use std::time::Duration;

use opus_rs::pvq::{celt_pvq_v, cwrsi, icwrs, pvq_search};

/// 1件あたりの制限時間。正常なら 1ms も掛からないので、これで十分見分けられる
const LIMIT: Duration = Duration::from_secs(2);

/// 1件の判定結果
enum Outcome {
    Ok,
    /// V(N,K) が 0 (32bit を溢れた)
    Overflowed,
    /// V(N,K) が u32 の上限に張り付いている (溢れを飽和で潰している)
    Saturated,
    /// 索引が符号帳の外 (i >= V)
    OutOfRange(u32, u32),
    /// 往復しない
    RoundTrip,
}

fn main() {
    // パニックの中身は出さない (どの組み合わせかだけ分かればよい)
    std::panic::set_hook(Box::new(|_| {}));

    let sizes = [
        2usize, 3, 4, 6, 8, 12, 16, 24, 32, 48, 64, 96, 128, 176, 200,
    ];
    let ks = [1i32, 2, 4, 8, 16, 32, 64, 96, 112, 127];

    let mut checked = 0usize;
    let mut out_of_range = Vec::new();
    let mut round_trip_failed = Vec::new();
    let mut overflowed = Vec::new();
    let mut saturated = Vec::new();
    let mut hung = Vec::new();
    let mut panicked = Vec::new();

    for &n in &sizes {
        for &k in &ks {
            for seed in 0..4u32 {
                checked += 1;
                match run_with_limit(n, k, seed) {
                    Ok(Some(Outcome::Ok)) => {}
                    Ok(Some(Outcome::Overflowed)) => overflowed.push((n, k)),
                    Ok(Some(Outcome::Saturated)) => saturated.push((n, k)),
                    Ok(Some(Outcome::OutOfRange(i, v))) => out_of_range.push((n, k, i, v)),
                    Ok(Some(Outcome::RoundTrip)) => round_trip_failed.push((n, k, seed)),
                    Ok(None) => panicked.push((n, k)),
                    Err(_) => hung.push((n, k)),
                }
            }
        }
    }

    println!("調べた組み合わせ: {checked}");
    report("V(N,K) が 0 (32bit を溢れた)", &overflowed);
    report("V(N,K) が u32 の上限に張り付き (飽和)", &saturated);
    report("戻ってこない (2秒で打ち切り)", &hung);
    report("パニック", &panicked);
    report4("索引が符号帳の外 (i >= V)", &out_of_range);
    report3("往復しない", &round_trip_failed);
    if overflowed.is_empty()
        && saturated.is_empty()
        && hung.is_empty()
        && panicked.is_empty()
        && out_of_range.is_empty()
        && round_trip_failed.is_empty()
    {
        println!("索引付けは健全");
    }

    // 時間切れのスレッドが残っていると終われないので、ここで畳む
    std::process::exit(0);
}

/// 1件を別スレッドで回す。
///
/// `Ok(Some(_))` = 判定できた / `Ok(None)` = パニック / `Err(_)` = 制限時間切れ
fn run_with_limit(n: usize, k: i32, seed: u32) -> Result<Option<Outcome>, mpsc::RecvTimeoutError> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let result = std::panic::catch_unwind(|| check(n, k, seed));
        let _ = tx.send(result.ok());
    });
    rx.recv_timeout(LIMIT)
}

fn check(n: usize, k: i32, seed: u32) -> Outcome {
    let x = make_input(n, seed);
    let mut y = vec![0i32; n];
    pvq_search(&x, &mut y, k, n);

    let v = celt_pvq_v(n as u32, k as u32);
    // V が 0 になるのは溢れた証拠 (本来 1 以上)
    if v == 0 {
        return Outcome::Overflowed;
    }
    if v == u32::MAX {
        return Outcome::Saturated;
    }

    let i = icwrs(n as u32, k as u32, &y);
    if i >= v {
        return Outcome::OutOfRange(i, v);
    }

    let mut back = vec![0i32; n];
    cwrsi(n as u32, k as u32, i, &mut back);
    if back != y {
        return Outcome::RoundTrip;
    }
    Outcome::Ok
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
