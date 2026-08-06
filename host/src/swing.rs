//! ジャズのスウィングの計算。
//!
//! 典拠は Nelias らの *Downbeat delays are a key component of swing in jazz*
//! (Communications Physics, 2022)。その枠組みでは **伴奏は正確な拍を刻み、
//! ソリストが表拍だけを遅らせる**。裏拍は伴奏と同期したまま、スウィング比の
//! 位置に置かれる。係数はこの論文をもとに調整した経験則。
//!
//! ここは f32 の数値だけを扱い、ノートのことは知らない
//! (ノートへの適用は `sequencer::MidiEditor::performed_notes`)。

/// 裏拍の比のピーク値 (200BPM 時の比そのもの) の下限。
/// 1.0 は全テンポで直線になり、スウィング無効と等価。
pub const MIN_PEAK_RATIO: f32 = 1.0;

/// 同じく上限。2.0 でピーク時にちょうど三連スウィングになる。
pub const MAX_PEAK_RATIO: f32 = 2.0;

/// 既定のピーク値
pub const DEFAULT_PEAK_RATIO: f32 = 1.5;

/// 表拍の遅れの上限 (ミリ秒)。
///
/// 遅れの式は tick (相対時間) での線形なので、そのまま使うと低速側で絶対時間が
/// 伸びすぎる (60BPM で 117ms)。知覚されるのは絶対時間なのでここで頭打ちにする。
/// 実際に効き始めるのは約 103BPM 以下。
const MAX_DOWNBEAT_DELAY_MS: f32 = 60.0;

/// 元の式が使っている1四分音符あたりの tick 数
const TICKS_PER_QUARTER: f32 = 960.0;

/// 位置判定の許容誤差 (四分音符単位)。960 TPQ で約1 tick。
///
/// 連符のスナップ幅 (`snap * 2 / n`) は割り切れないので、完全一致で見ると
/// 拍頭を取りこぼす。三連符の位置 (0.333) とは十分離れているので誤検出はしない。
const TOLERANCE: f32 = 1e-3;

/// 変換後の最小音価 (四分音符単位)。
///
/// 表拍の遅れで終端が開始を追い越す極端に短いノート (120BPM なら32分音符より短いもの)
/// のための下限。この場合だけ次のノートとわずかに重なりうるが、これは避けられない。
pub const MIN_PERFORMED_DURATION: f32 = 1.0 / 64.0;

/// この拍子でスウィングを適用するか。
///
/// 数式は「1拍 = 四分音符」を前提にしているので N/4 だけを対象にする。
/// N/8 や N/16 はそもそも均等に近いので対象外。
/// (2/2 は 4/4 と同じ音楽の別記法なので、将来対象へ入れるならここを変える)
pub fn applies_to(beat_type: u32) -> bool {
    beat_type == 4
}

/// 裏拍の比。1.0 を下回らせない。
///
/// 上に凸の放物線なので、そのままだと 71BPM 未満と 329BPM 超で 1.0 を割り、
/// 裏拍が表拍より早く来る (逆スウィング) 。そこは直線に留める。
pub fn ratio(tempo: u32, peak_ratio: f32) -> f32 {
    let peak = peak_ratio.clamp(MIN_PEAK_RATIO, MAX_PEAK_RATIO);
    let tempo = tempo.max(1) as f32;
    (-0.00003 * (tempo - 200.0).powi(2) + peak).max(1.0)
}

/// 裏拍が置かれる位置 (拍頭からの四分音符単位)。直線なら 0.5。
pub fn offbeat_position(tempo: u32, peak_ratio: f32) -> f32 {
    let ratio = ratio(tempo, peak_ratio);
    ratio / (ratio + 1.0)
}

/// 表拍の遅れ (四分音符単位)。
///
/// ソリストが拍の頭を伴奏より後ろに置く量。これがスウィングの核で、
/// 比だけでは足りない。
pub fn downbeat_delay(tempo: u32) -> f32 {
    let tempo = tempo.max(1) as f32;
    let by_ticks = (-0.3 * tempo + 130.0).max(0.0) / TICKS_PER_QUARTER;
    // ミリ秒の上限を四分音符に直す (1拍 = 60000/T ms)
    let by_ms = MAX_DOWNBEAT_DELAY_MS * tempo / 60_000.0;
    by_ticks.min(by_ms)
}

/// 時刻に足すオフセット (四分音符単位)。
///
/// 動かすのは拍頭と裏拍だけ。連符も16分も `0.0` を返すので、モードによる
/// 場合分けは要らない (判定は位置だけを見る)。
pub fn offset(tick: f32, tempo: u32, peak_ratio: f32) -> f32 {
    if !tick.is_finite() {
        return 0.0;
    }
    let phase = tick.rem_euclid(1.0);
    // 誤差で 0.9999998 になった拍頭も拾う
    if near(phase, 0.0) || near(phase, 1.0) {
        downbeat_delay(tempo)
    } else if near(phase, 0.5) {
        offbeat_position(tempo, peak_ratio) - 0.5
    } else {
        0.0
    }
}

fn near(value: f32, target: f32) -> bool {
    (value - target).abs() < TOLERANCE
}

#[cfg(test)]
mod tests {
    use super::*;

    /// おおよその一致 (小数の比較用)
    fn close(actual: f32, expected: f32) -> bool {
        (actual - expected).abs() < 1e-4
    }

    /// 遅れをミリ秒に直す (上限の確認用)
    fn delay_ms(tempo: u32) -> f32 {
        downbeat_delay(tempo) * 60_000.0 / tempo as f32
    }

    /// ミリ秒どうしの比較。値が数十のオーダーなので許容を広く取る
    fn close_ms(actual: f32, expected: f32) -> bool {
        (actual - expected).abs() < 0.01
    }

    /// 比が経験則の曲線どおりであること。
    /// ピークは 200BPM で、そこでは C がそのまま比になる。
    #[test]
    fn ratio_follows_the_curve() {
        assert!(close(ratio(120, 1.5), 1.308), "実際 {}", ratio(120, 1.5));
        assert!(close(ratio(200, 1.5), 1.5), "200BPM ではピーク値そのもの");
        assert!(close(ratio(200, 2.0), 2.0), "C=2.0 なら三連スウィング");
        // 200BPM から等距離なら同じ比になる (放物線なので左右対称)
        assert!(close(ratio(160, 1.5), ratio(240, 1.5)));
    }

    /// C = 1.0 はどのテンポでも直線になること (スウィング無効と等価)
    #[test]
    fn lowest_peak_ratio_is_straight_everywhere() {
        for tempo in [60, 120, 200, 300] {
            assert!(close(ratio(tempo, 1.0), 1.0), "{tempo}BPM");
            assert!(close(offbeat_position(tempo, 1.0), 0.5), "{tempo}BPM");
        }
    }

    /// 比が 1.0 を割らないこと。
    /// 割ると裏拍が表拍より早く来て逆スウィングになる。
    #[test]
    fn ratio_never_falls_below_straight() {
        // 境界は 200 ± sqrt(0.5 / 0.00003) ≒ 200 ± 129.1
        assert!(ratio(70, 1.5) >= 1.0);
        assert!(ratio(60, 1.5) >= 1.0, "遅い側で clamp されること");
        assert!(ratio(400, 1.5) >= 1.0, "速い側で clamp されること");
        // 境界のすぐ内側ではまだ 1.0 を超えている
        assert!(ratio(75, 1.5) > 1.0);
        assert!(ratio(325, 1.5) > 1.0);
    }

    /// ピーク値は範囲外を渡されても丸められること
    #[test]
    fn peak_ratio_is_clamped() {
        assert!(close(ratio(200, 10.0), MAX_PEAK_RATIO));
        assert!(close(ratio(200, 0.0), MIN_PEAK_RATIO));
    }

    /// 表拍の遅れが 60ms で頭打ちになること。
    /// tick の式のままだと低速側で伸びすぎる (60BPM で 117ms)。
    #[test]
    fn downbeat_delay_is_capped_in_milliseconds() {
        assert!(close_ms(delay_ms(120), 48.958), "120BPM は無傷: {}", delay_ms(120));
        assert!(close_ms(delay_ms(200), 21.875), "200BPM も無傷: {}", delay_ms(200));
        assert!(close_ms(delay_ms(60), 60.0), "60BPM は頭打ち: {}", delay_ms(60));
        assert!(close_ms(delay_ms(40), 60.0), "さらに遅くても 60ms を超えないこと");
        // 境界は約 103BPM
        assert!(delay_ms(100) >= 59.9, "100BPM は頭打ち側: {}", delay_ms(100));
        assert!(delay_ms(110) < 60.0, "110BPM は素の式側: {}", delay_ms(110));
    }

    /// 四分音符に対する遅れの比率 (120BPM で約 0.098 拍)
    #[test]
    fn downbeat_delay_in_quarters() {
        assert!(close(downbeat_delay(120), 94.0 / 960.0));
        assert!(close(downbeat_delay(200), 70.0 / 960.0));
    }

    /// 極端に速いテンポで遅れが 0 になり、負にならないこと
    #[test]
    fn downbeat_delay_vanishes_at_extreme_tempo() {
        assert_eq!(downbeat_delay(500), 0.0);
        assert_eq!(downbeat_delay(999), 0.0);
    }

    /// 動くのは拍頭と裏拍だけで、連符や16分は動かないこと。
    /// (連符モードとの排他制御が要らないのはこのため)
    #[test]
    fn offset_moves_only_downbeats_and_offbeats() {
        let (t, c) = (120, 1.5);
        assert!(close(offset(0.0, t, c), downbeat_delay(t)), "拍頭");
        assert!(close(offset(3.0, t, c), downbeat_delay(t)), "何拍目でも拍頭");
        assert!(close(offset(0.5, t, c), 0.066_724), "裏拍: {}", offset(0.5, t, c));
        assert!(close(offset(2.5, t, c), offset(0.5, t, c)), "何拍目でも裏拍");

        assert_eq!(offset(1.0 / 3.0, t, c), 0.0, "三連符の2音目");
        assert_eq!(offset(2.0 / 3.0, t, c), 0.0, "三連符の3音目");
        assert_eq!(offset(0.25, t, c), 0.0, "16分の裏");
        assert_eq!(offset(0.75, t, c), 0.0, "16分の裏");
    }

    /// 連符スナップが生む誤差を拍頭として拾い、三連符とは取り違えないこと
    #[test]
    fn offset_tolerates_tuplet_rounding() {
        let (t, c) = (120, 1.5);
        assert!(close(offset(0.999_999_8, t, c), downbeat_delay(t)), "手前側の誤差");
        assert!(close(offset(1.000_000_2, t, c), downbeat_delay(t)), "奥側の誤差");
        assert!(close(offset(0.500_000_2, t, c), offset(0.5, t, c)), "裏拍の誤差");
        // 三連符は許容誤差の外
        assert_eq!(offset(0.333_333_3, t, c), 0.0);
    }

    /// 有限でない値を渡されても壊れないこと (壊れたファイルからの防御)
    #[test]
    fn offset_ignores_non_finite_input() {
        assert_eq!(offset(f32::NAN, 120, 1.5), 0.0);
        assert_eq!(offset(f32::INFINITY, 120, 1.5), 0.0);
    }

    /// 適用するのは N/4 拍子だけであること
    #[test]
    fn applies_only_to_quarter_note_beats() {
        assert!(applies_to(4), "4/4 や 3/4");
        assert!(!applies_to(8), "6/8 など");
        assert!(!applies_to(16));
        assert!(!applies_to(2), "2/2 は今は対象外");
    }
}
