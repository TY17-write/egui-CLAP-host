//! ITU-R BS.1770-4 のラウドネス測定 (Momentary / Short-term)。
//!
//! **係数はサンプリングレートから毎回引き直す。** 規格が表で載せているのは
//! 48kHz のものだけで、44.1kHz のデバイスにそのまま当てると測定値がずれる。
//! ここでは規格が元にしているアナログ原型から双一次変換で組み立てるので、
//! どのレートでも同じ特性になる (48kHz では規格の表と一致する。テスト参照)。
//!
//! **Integrated (積算) は「頭から今まで」を測る。** 起点は呼び出し側が
//! [`LoudnessMeter::restart_integrated`] で決める (本体は再生開始とループの
//! 折り返し)。M と S は**巻き戻さない** — 直近を見るためのものなので、
//! 折り返しのたびに -∞ へ戻ると読めなくなる。

/// 積算の単位 (100ms)。Momentary も Short-term もこの倍数で切る
const BLOCK_MS: f64 = 100.0;
/// Momentary の窓 (400ms)
const MOMENTARY_BLOCKS: usize = 4;
/// Short-term の窓 (3秒)
const SHORT_TERM_BLOCKS: usize = 30;

/// Integrated の絶対ゲート。これ未満の区間は積算に入れない
const ABSOLUTE_GATE_LUFS: f64 = -70.0;
/// Integrated の相対ゲート。ゲート後の平均からこれだけ下を切る
const RELATIVE_GATE_LU: f64 = 10.0;

/// 積算に溜める区間の上限 (100ms 刻みなので1時間ぶん)。
///
/// 起点は再生開始かループの折り返しなので、普通はここに届かない。
/// **届いたらそこで積算を止める** (古いものを捨てると「頭からの積算」で
/// なくなり、意味の違う数字が出てしまう)。
const MAX_GATING_BLOCKS: usize = 36_000;

/// これより下は無音として扱う。対数なので本当の無音は -∞ になる
pub const SILENCE_LUFS: f32 = -70.0;

/// 配信で事実上の基準になっている目標値。画面の目盛りの中心に使う
pub const REFERENCE_LUFS: f32 = -14.0;

/// BS.1770 の定数項。1kHz の正弦波が入力と同じ値を示すよう決められている
const OFFSET_LU: f64 = -0.691;

/// 双二次フィルタ1段 (直接形 I)。
///
/// `f64` で持つのは、K特性の1段目が低域で係数の差が小さく、`f32` だと
/// 長く回すうちに誤差が積もるため。1サンプルあたり数演算なので費用は問題にならない。
#[derive(Clone, Copy, Default)]
struct Biquad {
    b0: f64,
    b1: f64,
    b2: f64,
    a1: f64,
    a2: f64,
    x1: f64,
    x2: f64,
    y1: f64,
    y2: f64,
}

impl Biquad {
    fn process(&mut self, x: f64) -> f64 {
        let y = self.b0 * x + self.b1 * self.x1 + self.b2 * self.x2
            - self.a1 * self.y1
            - self.a2 * self.y2;
        self.x2 = self.x1;
        self.x1 = x;
        self.y2 = self.y1;
        self.y1 = y;
        y
    }

    fn reset(&mut self) {
        self.x1 = 0.0;
        self.x2 = 0.0;
        self.y1 = 0.0;
        self.y2 = 0.0;
    }
}

/// K特性の1段目 (高域シェルビング)。頭部の音響的な影響を模したもの。
///
/// 規格の表は 48kHz のものしか無いので、原型のパラメータから組み立てる。
/// この4つの定数は規格の表を 48kHz で再現する値で、
/// [`tests::matches_the_published_table_at_48k`] が毎回突き合わせている。
fn shelving(sample_rate: f64) -> Biquad {
    const F0: f64 = 1681.974450955533;
    const GAIN_DB: f64 = 3.999843853973347;
    const Q: f64 = 0.7071752369554196;

    let k = (std::f64::consts::PI * F0 / sample_rate).tan();
    let vh = 10f64.powf(GAIN_DB / 20.0);
    let vb = vh.powf(0.4996667741545416);
    let kk = k * k;

    let a0 = 1.0 + k / Q + kk;
    Biquad {
        b0: (vh + vb * k / Q + kk) / a0,
        b1: 2.0 * (kk - vh) / a0,
        b2: (vh - vb * k / Q + kk) / a0,
        a1: 2.0 * (kk - 1.0) / a0,
        a2: (1.0 - k / Q + kk) / a0,
        ..Default::default()
    }
}

/// K特性の2段目 (RLB ハイパス)。低域が測定値を支配しないよう落とす。
fn highpass(sample_rate: f64) -> Biquad {
    const F0: f64 = 38.13547087602444;
    const Q: f64 = 0.5003270373238773;

    let k = (std::f64::consts::PI * F0 / sample_rate).tan();
    let kk = k * k;

    let a0 = 1.0 + k / Q + kk;
    Biquad {
        b0: 1.0,
        b1: -2.0,
        b2: 1.0,
        a1: 2.0 * (kk - 1.0) / a0,
        a2: (1.0 - k / Q + kk) / a0,
        ..Default::default()
    }
}

/// ステレオのラウドネス計。**入力は L/R の交互 (グラフの出力そのまま)**。
pub struct LoudnessMeter {
    sample_rate: u32,
    /// K特性 (チャンネルごとに独立した状態を持つ)
    shelving: [Biquad; 2],
    highpass: [Biquad; 2],
    /// 100ms ぶんのフレーム数
    block_frames: usize,
    /// 今の区間に入れたフレーム数
    filled: usize,
    /// 今の区間の二乗和 (L と R を足したもの)
    sum: f64,
    /// 直近の区間ごとの平均二乗 (L+R)。**古いものから上書きする輪**
    history: [f64; SHORT_TERM_BLOCKS],
    /// 次に書く位置
    at: usize,
    /// 積んだ区間の数 (窓が埋まるまでの判定に使う)
    filled_blocks: usize,
    /// Integrated 用に溜めた 400ms 窓の平均二乗。
    /// **[`restart_integrated`](Self::restart_integrated) で捨てる**
    gating: Vec<f64>,
    /// 積算の起点から閉じた区間の数。
    /// 400ms 窓は4区間ぶんなので、**起点直後の窓に前の周のぶんが混じらない**
    /// よう、これが 4 に届くまでは積まない
    blocks_since_restart: usize,
    /// 計算済みの Integrated (区間が増えたときだけ求め直す)
    integrated: f32,
}

impl LoudnessMeter {
    pub fn new(sample_rate: u32) -> Self {
        let rate = sample_rate.max(1);
        Self {
            sample_rate: rate,
            shelving: [shelving(rate as f64); 2],
            highpass: [highpass(rate as f64); 2],
            block_frames: ((rate as f64) * BLOCK_MS / 1000.0).round().max(1.0) as usize,
            filled: 0,
            sum: 0.0,
            history: [0.0; SHORT_TERM_BLOCKS],
            at: 0,
            filled_blocks: 0,
            gating: Vec::new(),
            blocks_since_restart: 0,
            integrated: SILENCE_LUFS,
        }
    }

    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    /// 溜めたものを全部捨てる (レートが変わったとき・エンジンを止めたとき)
    pub fn reset(&mut self) {
        for filter in self.shelving.iter_mut().chain(self.highpass.iter_mut()) {
            filter.reset();
        }
        self.filled = 0;
        self.sum = 0.0;
        self.history = [0.0; SHORT_TERM_BLOCKS];
        self.at = 0;
        self.filled_blocks = 0;
        self.restart_integrated();
    }

    /// **Integrated だけ**を測り直す (再生開始・ループの折り返し)。
    ///
    /// M と S には触れない。直近を見るためのものなので、折り返しのたびに
    /// -∞ へ戻ると読めなくなる。
    pub fn restart_integrated(&mut self) {
        self.gating.clear();
        self.blocks_since_restart = 0;
        self.integrated = SILENCE_LUFS;
    }

    /// L/R 交互のサンプルを流し込む。**長さは2の倍数**であること
    /// (半端な分は捨てる。次のブロックで頭が入れ替わると左右が入れ替わるため)。
    pub fn push(&mut self, interleaved: &[f32]) {
        for frame in interleaved.chunks_exact(2) {
            for (channel, sample) in frame.iter().enumerate() {
                let filtered =
                    self.highpass[channel].process(self.shelving[channel].process(*sample as f64));
                self.sum += filtered * filtered;
            }
            self.filled += 1;
            if self.filled >= self.block_frames {
                self.close_block();
            }
        }
    }

    /// 100ms ぶんが埋まったので、区間の平均二乗を輪へ積む
    fn close_block(&mut self) {
        // 二乗和はチャンネル分あるが、区間の長さで割るのは**フレーム数**。
        // これで L と R それぞれの平均二乗を足したものになる (BS.1770 の Σ G_i z_i)
        self.history[self.at] = self.sum / self.filled as f64;
        self.at = (self.at + 1) % SHORT_TERM_BLOCKS;
        self.filled_blocks = (self.filled_blocks + 1).min(SHORT_TERM_BLOCKS);
        self.filled = 0;
        self.sum = 0.0;

        // Integrated のゲート判定は 400ms 窓を 100ms ずつずらして行う。
        // **起点から4区間そろうまで積まない** (前の周のぶんが混じるため)
        self.blocks_since_restart += 1;
        if self.blocks_since_restart >= MOMENTARY_BLOCKS && self.gating.len() < MAX_GATING_BLOCKS {
            self.gating.push(self.mean_of_last(MOMENTARY_BLOCKS));
            self.integrated = self.compute_integrated();
        }
    }

    /// 直近 `blocks` 区間の平均二乗 (L+R)
    fn mean_of_last(&self, blocks: usize) -> f64 {
        let mut total = 0.0;
        for step in 1..=blocks {
            let index = (self.at + SHORT_TERM_BLOCKS - step) % SHORT_TERM_BLOCKS;
            total += self.history[index];
        }
        total / blocks as f64
    }

    /// Momentary (直近 400ms)。まだ埋まっていなければ [`SILENCE_LUFS`]
    pub fn momentary(&self) -> f32 {
        self.window(MOMENTARY_BLOCKS)
    }

    /// Short-term (直近3秒)。まだ埋まっていなければ [`SILENCE_LUFS`]
    pub fn short_term(&self) -> f32 {
        self.window(SHORT_TERM_BLOCKS)
    }

    /// 直近 `blocks` 区間のラウドネス。
    ///
    /// **窓が埋まるまでは測らない。** 途中まででも出せるが、再生を始めた直後に
    /// 大きく振れた値が出て、目盛りを読み間違える元になる。
    fn window(&self, blocks: usize) -> f32 {
        if self.filled_blocks < blocks {
            return SILENCE_LUFS;
        }
        to_lufs(self.mean_of_last(blocks))
    }

    /// Integrated (積算の起点から今まで)。
    ///
    /// 起点は [`restart_integrated`](Self::restart_integrated) を呼んだところ。
    /// 400ms 窓が4つ揃うまでは [`SILENCE_LUFS`]。
    pub fn integrated(&self) -> f32 {
        self.integrated
    }

    /// 積算に入っている 400ms 窓の数 (画面で「まだ測り始めたばかり」を出すのに使う)
    pub fn integrated_blocks(&self) -> usize {
        self.gating.len()
    }

    /// BS.1770 の2段ゲートを掛けて積算する。
    ///
    /// 1. **絶対ゲート** — [`ABSOLUTE_GATE_LUFS`] 未満の区間を捨てる (無音の除外)
    /// 2. **相対ゲート** — 残りの平均から [`RELATIVE_GATE_LU`] 下を切る
    ///    (静かな部分に引きずられて全体が低く出るのを防ぐ)
    ///
    /// 中間の一覧を作らずに2周する。毎回の区間追加で呼ぶので、確保を挟みたくない。
    fn compute_integrated(&self) -> f32 {
        let absolute = power_at(ABSOLUTE_GATE_LUFS);

        let mut sum = 0.0;
        let mut count = 0usize;
        for power in &self.gating {
            if *power > absolute {
                sum += *power;
                count += 1;
            }
        }
        if count == 0 {
            return SILENCE_LUFS;
        }

        // 相対ゲートの敷居は、絶対ゲートを通ったものの平均から決める
        let relative = power_at(to_lufs(sum / count as f64) as f64 - RELATIVE_GATE_LU);
        let gate = absolute.max(relative);

        let mut sum = 0.0;
        let mut count = 0usize;
        for power in &self.gating {
            if *power > gate {
                sum += *power;
                count += 1;
            }
        }
        if count == 0 {
            return SILENCE_LUFS;
        }
        to_lufs(sum / count as f64)
    }
}

/// 平均二乗 (L+R) をラウドネスへ
fn to_lufs(power: f64) -> f32 {
    if power <= 0.0 {
        return SILENCE_LUFS;
    }
    ((OFFSET_LU + 10.0 * power.log10()) as f32).max(SILENCE_LUFS)
}

/// ラウドネスを平均二乗へ ([`to_lufs`] の逆。ゲートの敷居を作るのに使う)
fn power_at(lufs: f64) -> f64 {
    10f64.powf((lufs - OFFSET_LU) / 10.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 1kHz の正弦波を両チャンネルに入れた交互サンプルを作る
    fn stereo_sine(sample_rate: u32, seconds: f64, hz: f64, db_fs: f64) -> Vec<f32> {
        let amplitude = 10f64.powf(db_fs / 20.0);
        let frames = (sample_rate as f64 * seconds) as usize;
        let mut out = Vec::with_capacity(frames * 2);
        for frame in 0..frames {
            let t = frame as f64 / sample_rate as f64;
            let value = (amplitude * (2.0 * std::f64::consts::PI * hz * t).sin()) as f32;
            out.push(value);
            out.push(value);
        }
        out
    }

    /// **48kHz で規格の表と一致すること。**
    ///
    /// BS.1770-4 が載せている係数は 48kHz のものだけ。ここが合っていれば、
    /// 他のレートで引き直した係数も同じ原型から出ていることになる。
    /// **[`shelving`] と [`highpass`] の定数を触ったら必ずここで落ちる。**
    #[test]
    fn matches_the_published_table_at_48k() {
        let shelf = shelving(48_000.0);
        let close = |actual: f64, expected: f64, name: &str| {
            assert!(
                (actual - expected).abs() < 1e-6,
                "{name}: {actual} != {expected}"
            );
        };
        close(shelf.b0, 1.53512485958697, "シェルビング b0");
        close(shelf.b1, -2.69169618940638, "シェルビング b1");
        close(shelf.b2, 1.19839281085285, "シェルビング b2");
        close(shelf.a1, -1.69065929318241, "シェルビング a1");
        close(shelf.a2, 0.73248077421585, "シェルビング a2");

        let hp = highpass(48_000.0);
        close(hp.b0, 1.0, "ハイパス b0");
        close(hp.b1, -2.0, "ハイパス b1");
        close(hp.b2, 1.0, "ハイパス b2");
        close(hp.a1, -1.99004745483398, "ハイパス a1");
        close(hp.a2, 0.99007225036621, "ハイパス a2");
    }

    /// **EBU Tech 3341 の試験信号1。**
    ///
    /// 1kHz の正弦波 -23.0 dBFS を左右に入れると、Momentary も Short-term も
    /// -23.0 LUFS (±0.1) を示す。**測定の鎖すべて**(フィルタ・区間の積算・
    /// 定数項) が正しくないと通らない。
    #[test]
    fn a_minus_23_dbfs_tone_reads_minus_23_lufs() {
        let mut meter = LoudnessMeter::new(48_000);
        meter.push(&stereo_sine(48_000, 4.0, 1000.0, -23.0));

        assert!(
            (meter.momentary() - (-23.0)).abs() < 0.1,
            "Momentary が {} LUFS (期待 -23.0)",
            meter.momentary()
        );
        assert!(
            (meter.short_term() - (-23.0)).abs() < 0.1,
            "Short-term が {} LUFS (期待 -23.0)",
            meter.short_term()
        );
    }

    /// **44.1kHz でも同じ値になること。**
    ///
    /// 48kHz の係数を流用すると、ここが 0.1 以上ずれる。デバイスは
    /// 44.1kHz のことが多いので、外すと普段使いの表示が丸ごと狂う。
    #[test]
    fn the_reading_does_not_depend_on_the_sample_rate() {
        for rate in [44_100, 48_000, 96_000] {
            let mut meter = LoudnessMeter::new(rate);
            meter.push(&stereo_sine(rate, 4.0, 1000.0, -23.0));
            assert!(
                (meter.short_term() - (-23.0)).abs() < 0.1,
                "{rate} Hz で {} LUFS (期待 -23.0)",
                meter.short_term()
            );
        }
    }

    /// 振幅を倍にすると 6.02 LU 上がること (対数として素直であること)
    #[test]
    fn doubling_the_amplitude_adds_six_lu() {
        let measure = |db: f64| {
            let mut meter = LoudnessMeter::new(48_000);
            meter.push(&stereo_sine(48_000, 4.0, 1000.0, db));
            meter.short_term()
        };
        let quiet = measure(-30.0);
        let loud = measure(-24.0);
        assert!(
            (loud - quiet - 6.0).abs() < 0.05,
            "{quiet} → {loud} (6 LU 上がること)"
        );
    }

    /// 無音は下限に張り付くこと (0 の対数を取って -∞ や NaN にしない)
    #[test]
    fn silence_sits_at_the_bottom() {
        let mut meter = LoudnessMeter::new(48_000);
        meter.push(&vec![0.0; 48_000 * 2 * 4]);
        assert_eq!(meter.momentary(), SILENCE_LUFS);
        assert_eq!(meter.short_term(), SILENCE_LUFS);
    }

    /// 窓が埋まるまでは測らないこと。
    /// (途中まででも数字は出せるが、再生開始直後に大きく振れて読み違える)
    #[test]
    fn the_window_has_to_fill_before_it_reads() {
        let mut meter = LoudnessMeter::new(48_000);
        // 0.2秒 = 区間2つぶん。Momentary (4つ) にも足りない
        meter.push(&stereo_sine(48_000, 0.2, 1000.0, -23.0));
        assert_eq!(meter.momentary(), SILENCE_LUFS);
        assert_eq!(meter.short_term(), SILENCE_LUFS);

        // 0.5秒まで進めば Momentary は出る。Short-term (3秒) はまだ
        meter.push(&stereo_sine(48_000, 0.3, 1000.0, -23.0));
        assert!(meter.momentary() > -30.0, "Momentary は出ること");
        assert_eq!(meter.short_term(), SILENCE_LUFS);
    }

    /// 左右で音量が違っても、両チャンネルの和で測ること。
    /// **片側だけ鳴っていれば、両側で鳴らしたときより 3 LU 低い**
    #[test]
    fn both_channels_add_up() {
        let both = {
            let mut meter = LoudnessMeter::new(48_000);
            meter.push(&stereo_sine(48_000, 4.0, 1000.0, -23.0));
            meter.short_term()
        };
        let left_only = {
            let mut meter = LoudnessMeter::new(48_000);
            let mut samples = stereo_sine(48_000, 4.0, 1000.0, -23.0);
            for frame in samples.chunks_exact_mut(2) {
                frame[1] = 0.0;
            }
            meter.push(&samples);
            meter.short_term()
        };
        assert!(
            (both - left_only - 3.01).abs() < 0.05,
            "両側 {both} / 片側 {left_only} (3.01 LU 差)"
        );
    }

    /// **Integrated も試験信号1に乗ること。**
    /// 一定の水準なら M・S・I が揃うはず (ゲートで何も落ちない)
    #[test]
    fn a_steady_tone_reads_the_same_on_every_window() {
        let mut meter = LoudnessMeter::new(48_000);
        meter.push(&stereo_sine(48_000, 4.0, 1000.0, -23.0));

        for (name, value) in [
            ("Momentary", meter.momentary()),
            ("Short-term", meter.short_term()),
            ("Integrated", meter.integrated()),
        ] {
            assert!(
                (value - (-23.0)).abs() < 0.1,
                "{name} が {value} LUFS (期待 -23.0)"
            );
        }
    }

    /// **EBU Tech 3341 の試験信号3 (相対ゲート)。**
    ///
    /// 静か → 大きい → 静か と並べると、**静かな部分は落とされて
    /// 大きい部分の値になる**。規格は 10秒/60秒/10秒 で -36/-23/-36 dBFS。
    ///
    /// **相対ゲートを外すと通らない。** -36 は平均より 10 LU 以上下なので、
    /// 入れたままだと全体が -24.4 あたりまで下がる。
    ///
    /// 大きい部分を 20秒と長く取っているのは、**境目をまたぐ 400ms 窓**が
    /// あるため。半分だけ静かな窓はゲートを通り抜けて平均を少し下げるので、
    /// 短くすると (6秒だと 0.2 LU) 規格の許容 ±0.1 に収まらない。
    #[test]
    fn the_quiet_parts_are_gated_out() {
        let mut meter = LoudnessMeter::new(48_000);
        meter.push(&stereo_sine(48_000, 3.0, 1000.0, -36.0));
        meter.push(&stereo_sine(48_000, 20.0, 1000.0, -23.0));
        meter.push(&stereo_sine(48_000, 3.0, 1000.0, -36.0));

        assert!(
            (meter.integrated() - (-23.0)).abs() < 0.1,
            "Integrated が {} LUFS (期待 -23.0)",
            meter.integrated()
        );
    }

    /// **無音は絶対ゲートで落ちること。**
    /// 前後に無音を足しても、鳴っている部分の値が変わらない
    /// (長さの理由は [`the_quiet_parts_are_gated_out`] と同じ)
    #[test]
    fn silence_does_not_drag_the_integrated_down() {
        let mut meter = LoudnessMeter::new(48_000);
        meter.push(&vec![0.0; 48_000 * 2 * 2]);
        meter.push(&stereo_sine(48_000, 20.0, 1000.0, -23.0));
        meter.push(&vec![0.0; 48_000 * 2 * 2]);

        assert!(
            (meter.integrated() - (-23.0)).abs() < 0.1,
            "{} LUFS (期待 -23.0)",
            meter.integrated()
        );
    }

    /// **測り直しは Integrated だけに効くこと。**
    ///
    /// ループの折り返しで M と S まで戻すと、そのたびに -∞ になって読めなくなる。
    #[test]
    fn restarting_leaves_the_rolling_windows_alone() {
        let mut meter = LoudnessMeter::new(48_000);
        meter.push(&stereo_sine(48_000, 4.0, 1000.0, -23.0));
        assert!(meter.integrated() > -30.0);

        meter.restart_integrated();
        assert_eq!(meter.integrated(), SILENCE_LUFS, "Integrated は戻ること");
        assert!(
            (meter.short_term() - (-23.0)).abs() < 0.1,
            "Short-term は戻らないこと ({})",
            meter.short_term()
        );
        assert!(
            (meter.momentary() - (-23.0)).abs() < 0.1,
            "Momentary は戻らないこと ({})",
            meter.momentary()
        );
    }

    /// **測り直した後に、前の分が混じらないこと。**
    ///
    /// 400ms 窓は4区間ぶんあるので、起点直後の窓には前の音が残っている。
    /// 大きい音のあとで測り直し、静かな音だけを入れて、静かなほうの値が出ること。
    #[test]
    fn the_previous_pass_does_not_leak_in() {
        let mut meter = LoudnessMeter::new(48_000);
        meter.push(&stereo_sine(48_000, 4.0, 1000.0, -14.0));
        meter.restart_integrated();
        meter.push(&stereo_sine(48_000, 4.0, 1000.0, -30.0));

        assert!(
            (meter.integrated() - (-30.0)).abs() < 0.1,
            "Integrated が {} LUFS (期待 -30.0。前の -14 が混じっていないか)",
            meter.integrated()
        );
    }

    /// 起点から 400ms 経つまでは測らないこと (窓が1つも埋まっていない)
    #[test]
    fn the_integrated_needs_one_full_window() {
        let mut meter = LoudnessMeter::new(48_000);
        meter.push(&stereo_sine(48_000, 0.3, 1000.0, -23.0));
        assert_eq!(meter.integrated(), SILENCE_LUFS);
        assert_eq!(meter.integrated_blocks(), 0);

        meter.push(&stereo_sine(48_000, 0.2, 1000.0, -23.0));
        assert!(meter.integrated_blocks() > 0, "窓が埋まれば測り始めること");
        assert!(meter.integrated() > -30.0);
    }

    /// レートを変えたら作り直せること (係数と溜めたものが両方入れ替わる)
    #[test]
    fn a_new_rate_starts_from_scratch() {
        let mut meter = LoudnessMeter::new(48_000);
        meter.push(&stereo_sine(48_000, 4.0, 1000.0, -23.0));
        assert!(meter.short_term() > -30.0);

        meter.reset();
        assert_eq!(meter.short_term(), SILENCE_LUFS, "溜めたものが消えること");
    }
}
