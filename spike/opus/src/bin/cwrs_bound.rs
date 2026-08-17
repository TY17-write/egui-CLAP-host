//! `celt_pvq_u` / `celt_pvq_v` が K のどこで壊れるかを特定する。
//!
//! 符号帳の大きさ V(N,K) を引く関数。**本家 libopus は表引きだけ**で、
//! 表に無い組み合わせは使わないよう帯域を分割している。`opus-rs` は表に
//! 無いとき `compute_u` で計算し直す作りになっているが、その中の作業配列が
//! 130 要素しかない。

use opus_rs::pvq::{celt_pvq_u, celt_pvq_v};

fn main() {
    println!("N ごとに、celt_pvq_u / celt_pvq_v がパニックしない上限の K を探す\n");
    // パニックの中身は出さない (境界だけ知りたい)
    std::panic::set_hook(Box::new(|_| {}));

    for n in [2usize, 4, 8, 15, 16, 32, 64, 128, 176, 200] {
        let last_u = (1..300)
            .take_while(|k| ok(|| celt_pvq_u(n as u32, *k)))
            .last();
        let last_v = (1..300)
            .take_while(|k| ok(|| celt_pvq_v(n as u32, *k)))
            .last();
        println!(
            "N={n:>4} : celt_pvq_u は K<={:?} まで / celt_pvq_v は K<={:?} まで",
            last_u, last_v
        );
    }

    println!("\n値が 32bit を溢れていないかも見る (溢れると符号帳の大きさが嘘になる)");
    for n in [32usize, 64, 128, 176] {
        for k in [16u32, 32, 64, 96, 120] {
            if !ok(|| celt_pvq_v(n as u32, k)) {
                println!("N={n:>4} K={k:>4} : パニック");
                continue;
            }
            let v = celt_pvq_v(n as u32, k);
            let u = celt_pvq_u(n as u32, k);
            // **巻き戻りと飽和は分けて出す。** 0.1.26 は wrapping_add で巻き戻り、
            // 0.1.28 は上限に張り付く。どちらも V <= U になるが、意味が違う
            // (巻き戻りは嘘の小さい値、飽和は「32bit に収まらない」の印)
            if v == u32::MAX && u == u32::MAX {
                println!("N={n:>4} K={k:>4} : V も U も u32 の上限 (飽和)");
            } else if v <= u {
                // V は U より必ず大きい。逆転していたら巻き戻っている
                println!("N={n:>4} K={k:>4} : V={v} <= U={u} (巻き戻っている)");
            }
        }
    }
}

fn ok<T>(f: impl FnOnce() -> T + std::panic::UnwindSafe) -> bool {
    std::panic::catch_unwind(f).is_ok()
}
