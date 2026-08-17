//! 不均等な拍 (ウィンナ・ワルツ風) の計算。
//!
//! 小節の中で**拍の長さ自体を変える**。基本は中央の拍を山とする左右対称で、
//! 3拍子なら「短・長・短」——**2拍目が前に出る**形。小節の総和は変えないので、
//! テンポもループ位置も動かない。
//!
//! **山と谷の両方を1つの比で扱う。** `ratio` は「端の拍 ÷ 中央の拍」で、
//! 1.0 未満が山 (ウィンナ風)、1.0 で無効、1.0 超が谷。
//!
//! ここは f32 の数値だけを扱い、ノートのことは知らない
//! (ノートへの適用は `sequencer::MidiEditor::performed_notes`)。
//!
//! # スウィングとの違い
//!
//! スウィングは**位置ベース**で、拍頭と裏拍だけを動かす。こちらは
//! **小節内の時間軸ごとの写像**なので、拍の中の音符も一緒に伸縮する。
//! 写像が狭義単調で小節線が不動点になるため、**スウィングで手当てが要った
//! 「音価が負になる」「順序が入れ替わる」「終端をはみ出す」が起きない**。
//!
//! 計画と実測は `docs/waltz-plan.md`。

/// 比の下限。中央の拍がいちばん長い (山) 側の端。
pub const MIN_RATIO: f32 = 0.5;

/// 比の上限。中央の拍がいちばん短い (谷) 側の端。
pub const MAX_RATIO: f32 = 2.0;

/// 既定の比。控えめな山。
///
/// **典拠は無く、耳で決めた出発点。** スウィングと違って基にした論文が
/// 無いので、鳴らして詰める前提の値。
pub const DEFAULT_RATIO: f32 = 0.85;

/// これ以上 1.0 に近ければ無効として扱う。
///
/// スライダーが厳密な 1.0 を出せるとは限らないため、恒等写像になる幅を持たせる。
const NEUTRAL_TOLERANCE: f32 = 1e-4;

/// この拍子で適用するか。
///
/// **奇数の N/4 (N≥3) だけ。** 中央の拍が1つに定まる形なので偶数は対象外、
/// 1拍子は「中央」と「端」が同じなので比が意味を持たない。
/// N/8 や N/16 は 1拍 = 四分音符 の前提から外れるので、スウィングと同じく外す。
pub fn applies_to(beats: u32, beat_type: u32) -> bool {
    beat_type == 4 && beats >= 3 && beats % 2 == 1
}

/// 中央の拍の番号 (0 始まり)。奇数拍子なので必ず1つに定まる。
fn center(beats: u32) -> u32 {
    (beats - 1) / 2
}

/// 中央からの距離の平均 `k̄ = m(m+1)/N`
fn mean_distance(beats: u32) -> f32 {
    let m = center(beats);
    (m * (m + 1)) as f32 / beats as f32
}

/// 拍 `index` の長さ (四分音符単位)。適用外なら 1.0。
///
/// `d_i = (m + (R−1)·k_i) / (m + (R−1)·k̄)`
///
/// **総和が N になることが式から従う** (`Σk_i = N·k̄` なので分子の総和が
/// `N(m + (R−1)k̄)`)。小節の長さは1 tick も動かない。
///
/// **`ratio` が正である限り拍長も正。** 分子の最小値は山側 (`R<1`) で `mR`、
/// 谷側 (`R>1`) で `m`。分母は `k̄ < m` から正。範囲外を渡されても拍は消えない。
pub fn beat_duration(index: usize, beats: u32, ratio: f32) -> f32 {
    if !is_active(beats, ratio) || index >= beats as usize {
        return 1.0;
    }
    let m = center(beats) as f32;
    let distance = (index as f32 - m).abs();
    let shift = clamp_ratio(ratio) - 1.0;
    (m + shift * distance) / (m + shift * mean_distance(beats))
}

/// 記譜上の位置 (四分音符単位) を、演奏上の位置へ写す。
///
/// 拍頭かどうかは見ない。**位置がどの拍の中にあるか**だけで決まるので、
/// 8分も16分も連符も、拍と一緒に伸縮する。
///
/// 小節線は不動点 (`map(k·N) == k·N`) で、写像は狭義単調。
pub fn map(tick: f32, beats: u32, ratio: f32) -> f32 {
    if !tick.is_finite() || !is_active(beats, ratio) {
        return tick;
    }
    let bar_length = beats as f32;
    let bar = (tick / bar_length).floor();
    let local = tick - bar * bar_length;

    // 誤差で local が小節長ちょうどに届くことがあるので、最後の拍に丸める
    let index = (local.floor() as usize).min(beats as usize - 1);
    let fraction = local - index as f32;

    let mut onset = 0.0;
    for i in 0..index {
        onset += beat_duration(i, beats, ratio);
    }
    bar * bar_length + onset + fraction * beat_duration(index, beats, ratio)
}

/// 比が有効な範囲にあり、かつ恒等でないか
fn is_active(beats: u32, ratio: f32) -> bool {
    beats >= 3 && beats % 2 == 1 && ratio.is_finite() && (ratio - 1.0).abs() > NEUTRAL_TOLERANCE
}

/// 範囲外の比を丸める。**0 以下を通すと拍長が壊れる**ので、ここが最後の砦。
pub fn clamp_ratio(ratio: f32) -> f32 {
    if !ratio.is_finite() {
        return 1.0;
    }
    ratio.clamp(MIN_RATIO, MAX_RATIO)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(actual: f32, expected: f32) -> bool {
        (actual - expected).abs() < 1e-4
    }

    fn durations(beats: u32, ratio: f32) -> Vec<f32> {
        (0..beats as usize)
            .map(|i| beat_duration(i, beats, ratio))
            .collect()
    }

    /// 小節の長さが変わらないこと。**これが崩れるとテンポがずれる。**
    #[test]
    fn durations_always_sum_to_the_bar() {
        for beats in [3u32, 5, 7, 9] {
            for ratio in [0.5, 0.7, 0.85, 1.0, 1.2, 1.5, 2.0] {
                let sum: f32 = durations(beats, ratio).iter().sum();
                assert!(close(sum, beats as f32), "{beats}拍 R={ratio}: 総和 {sum}");
            }
        }
    }

    /// 向きの取り違え防止。**計画時に一度取り違えたので明示的に固定する。**
    /// R < 1 は中央が最長 (山＝ウィンナ風)、R > 1 は中央が最短 (谷)。
    #[test]
    fn ratio_below_one_makes_the_middle_longest() {
        for beats in [3u32, 5, 7] {
            let middle = center(beats) as usize;
            let hill = durations(beats, 0.85);
            let valley = durations(beats, 1.2);
            for i in 0..beats as usize {
                if i == middle {
                    continue;
                }
                assert!(hill[middle] > hill[i], "{beats}拍 山: 中央が最長であること");
                assert!(
                    valley[middle] < valley[i],
                    "{beats}拍 谷: 中央が最短であること"
                );
            }
        }
    }

    /// 3拍子が「短・長・短」になること (2拍目が前に出る)
    #[test]
    fn three_four_puts_the_second_beat_early() {
        let d = durations(3, DEFAULT_RATIO);
        assert!(close(d[0], 0.944_444), "1拍目: {}", d[0]);
        assert!(close(d[1], 1.111_111), "2拍目: {}", d[1]);
        assert!(close(d[2], 0.944_444), "3拍目: {}", d[2]);
        // 2拍目の頭が記譜より前へ出ること
        assert!(map(1.0, 3, DEFAULT_RATIO) < 1.0);
        assert!(close(map(1.0, 3, DEFAULT_RATIO), 0.944_444));
        // 3拍目の頭は後ろへ下がる
        assert!(close(map(2.0, 3, DEFAULT_RATIO), 2.055_556));
    }

    /// 中央から端へ向かって狭義単調であること
    #[test]
    fn durations_are_monotonic_from_the_middle() {
        for beats in [5u32, 7, 9] {
            let d = durations(beats, 0.85);
            let middle = center(beats) as usize;
            for i in 0..middle {
                assert!(d[i] < d[i + 1], "{beats}拍: 前半が単調に伸びること");
            }
            for i in middle..beats as usize - 1 {
                assert!(d[i] > d[i + 1], "{beats}拍: 後半が単調に縮むこと");
            }
        }
    }

    /// スライダーの意味が保たれること (端 ÷ 中央 が比そのもの)
    #[test]
    fn edge_over_middle_is_the_ratio() {
        for beats in [3u32, 5, 7, 9] {
            for ratio in [0.5, 0.85, 1.2, 2.0] {
                let d = durations(beats, ratio);
                let actual = d[0] / d[center(beats) as usize];
                assert!(close(actual, ratio), "{beats}拍 R={ratio}: 実際 {actual}");
            }
        }
    }

    /// R = 1.0 が恒等写像であること (無効と等価)
    #[test]
    fn neutral_ratio_changes_nothing() {
        for beats in [3u32, 5, 7] {
            for tick in [0.0, 0.25, 1.0, 1.5, 2.0, 6.75] {
                assert!(close(map(tick, beats, 1.0), tick), "{beats}拍 {tick}");
            }
        }
    }

    /// R と 1/R で長短が入れ替わること。
    /// **ただし拍長そのものは鏡像にならない** (総和を N に保つ正規化のぶんずれる)。
    #[test]
    fn inverse_ratio_flips_the_shape_but_is_not_a_mirror() {
        let hill = durations(3, 0.85);
        let valley = durations(3, 1.0 / 0.85);
        assert!(hill[0] < hill[1], "山: 端が短い");
        assert!(valley[0] > valley[1], "谷: 端が長い");
        // 鏡像ではない
        assert!(
            !close(hill[0], valley[1]),
            "端と中央が入れ替わっただけではないこと ({} vs {})",
            hill[0],
            valley[1]
        );
    }

    /// 範囲外や壊れた比を渡しても拍長が正であること
    #[test]
    fn out_of_range_ratio_keeps_durations_positive() {
        for ratio in [0.0, -1.0, 0.01, 100.0, f32::NAN, f32::INFINITY] {
            for beats in [3u32, 5, 7] {
                for d in durations(beats, ratio) {
                    assert!(d > 0.0, "R={ratio} {beats}拍: 拍長 {d}");
                }
            }
        }
    }

    /// 写像が狭義単調であること。**重なりと順序入れ替わりを防ぐ核心。**
    #[test]
    fn map_is_strictly_increasing() {
        for beats in [3u32, 5, 7] {
            for ratio in [0.5, 0.85, 1.2, 2.0] {
                let mut previous = f32::NEG_INFINITY;
                // 3小節ぶんを細かく刻む
                for step in 0..=(beats * 3 * 64) {
                    let tick = step as f32 / 64.0;
                    let mapped = map(tick, beats, ratio);
                    assert!(
                        mapped > previous,
                        "{beats}拍 R={ratio} tick={tick}: {mapped} <= {previous}"
                    );
                    previous = mapped;
                }
            }
        }
    }

    /// 小節線が不動点であること。**終端のはみ出しが起きない根拠。**
    #[test]
    fn bar_lines_do_not_move() {
        for beats in [3u32, 5, 7] {
            for ratio in [0.5, 0.85, 1.2, 2.0] {
                for bar in 0..4 {
                    let line = (bar * beats) as f32;
                    assert!(
                        close(map(line, beats, ratio), line),
                        "{beats}拍 R={ratio} 小節線 {line}: {}",
                        map(line, beats, ratio)
                    );
                }
            }
        }
    }

    /// 拍頭が拍長の累積に一致すること
    #[test]
    fn beat_heads_land_on_the_accumulated_onsets() {
        for beats in [3u32, 5, 7] {
            let ratio = 0.85;
            let mut onset = 0.0;
            for i in 0..beats as usize {
                assert!(
                    close(map(i as f32, beats, ratio), onset),
                    "{beats}拍 {i}拍目: {} != {onset}",
                    map(i as f32, beats, ratio)
                );
                onset += beat_duration(i, beats, ratio);
            }
        }
    }

    /// 拍の中の連符が、拍の中では等分のまま保たれること。
    /// **拍ごと伸縮する方式なので、比は変わらない。**
    #[test]
    fn tuplets_stay_even_inside_the_beat() {
        let (beats, ratio) = (3u32, 0.85);
        for beat in 0..3usize {
            let base = beat as f32;
            let head = map(base, beats, ratio);
            let first = map(base + 1.0 / 3.0, beats, ratio);
            let second = map(base + 2.0 / 3.0, beats, ratio);
            let next = map(base + 1.0, beats, ratio);
            let spans = [first - head, second - first, next - second];
            assert!(
                close(spans[0], spans[1]) && close(spans[1], spans[2]),
                "{beat}拍目の三連符が等分でない: {spans:?}"
            );
        }
    }

    /// 拍を分け合う音符が、伸縮後も拍を過不足なく埋めること。
    ///
    /// 隙間も重なりも生まれないことの実質的な確認。写像は位置だけの関数なので
    /// 「前の終端 == 次の開始」は自明に成り立つが、**区分線形が崩れていれば
    /// 分割の合計が拍長からずれる**ので、そこを見る。
    #[test]
    fn subdivisions_fill_the_beat_exactly() {
        let (beats, ratio) = (5u32, 0.7);
        for beat in 0..beats as usize {
            let base = beat as f32;
            let expected = beat_duration(beat, beats, ratio);
            for division in [2usize, 3, 4, 8] {
                let step = 1.0 / division as f32;
                let total: f32 = (0..division)
                    .map(|k| {
                        let from = base + k as f32 * step;
                        map(from + step, beats, ratio) - map(from, beats, ratio)
                    })
                    .sum();
                assert!(
                    close(total, expected),
                    "{beat}拍目を{division}分割: 合計 {total} != 拍長 {expected}"
                );
            }
        }
    }

    /// 適用するのは奇数の N/4 だけであること
    #[test]
    fn applies_only_to_odd_quarter_note_meters() {
        assert!(applies_to(3, 4), "3/4");
        assert!(applies_to(5, 4), "5/4");
        assert!(applies_to(7, 4), "7/4");
        assert!(applies_to(9, 4), "9/4");
        assert!(!applies_to(4, 4), "4/4 は偶数");
        assert!(!applies_to(6, 4), "6/4 は偶数");
        assert!(!applies_to(1, 4), "1/4 は中央と端が同じ");
        assert!(!applies_to(3, 8), "3/8 は 1拍 = 四分音符 でない");
        assert!(!applies_to(5, 16), "5/16 も同様");
    }

    /// 適用外の拍子では写像が恒等であること
    #[test]
    fn map_is_identity_outside_the_supported_meters() {
        for beats in [1u32, 2, 4, 6] {
            for tick in [0.0, 0.5, 1.0, 3.25] {
                assert!(close(map(tick, beats, 0.85), tick), "{beats}拍 {tick}");
            }
        }
    }

    /// 有限でない値を渡されても壊れないこと (壊れたファイルからの防御)
    #[test]
    fn map_ignores_non_finite_input() {
        assert!(map(f32::NAN, 3, 0.85).is_nan());
        assert_eq!(map(f32::INFINITY, 3, 0.85), f32::INFINITY);
    }
}
