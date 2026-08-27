//! モニタが自分で測るためのスペクトルとラウドネス。
//!
//! # ホストとコードを共有しない
//!
//! **これは意図的な重複。** 検証治具がホスト本体と同じ実装を使うと、二つの
//! 表示を見比べても「同じコードが同じ値を出している」ことしか言えない。
//! 別々に書いた二つの実装が同じ値を出すことに意味がある。
//!
//! 依存も持たない (FFT も K特性も `std` だけで組む)。**他の DAW に読ませても
//! 同じように動く**ことがこの治具の前提なので、ホスト側の都合を一切持ち込まない。
//!
//! # 測り方
//!
//! - スペクトル: 2048点の基数2 FFT に Hann 窓。対数で帯に割り、山に落ちを付ける
//! - ラウドネス: ITU-R BS.1770-4。K特性を通してから 100ms 単位で二乗平均を取り、
//!   Momentary (400ms) / Short-term (3秒) / Integrated (ゲート付きの積算) を出す
//!
//! **係数はサンプリングレートから引き直す。** 規格が表で載せているのは 48kHz の
//! ものだけなので、原型のパラメータから双一次変換で組み立てる
//! (48kHz では規格の表と一致することをテストで縛ってある)。

use std::collections::VecDeque;
use std::f64::consts::PI;

/// 画面に出す帯の数
pub const BANDS: usize = 56;
/// 表示の下限 (これより静かな帯は底に張り付く)
pub const FLOOR_DB: f32 = -90.0;
/// 表示の上限
pub const CEIL_DB: f32 = 0.0;

/// これ以下は無音として扱う (BS.1770 の絶対ゲート)
pub const SILENCE_LUFS: f32 = -70.0;
/// 配信でよく使われる基準値。目盛りの中心に置く
pub const REFERENCE_LUFS: f32 = -14.0;

// ---------------------------------------------------------------- スペクトル

/// 変換の点数。48kHz で 23Hz 刻み・窓の長さ 43ms
const FFT_SIZE: usize = 2048;
/// 帯を割り当てる範囲
const MIN_HZ: f32 = 30.0;
const MAX_HZ: f32 = 18_000.0;
/// 山の落ちる速さ (dB/秒)
const DECAY_DB_PER_SECOND: f32 = 90.0;

/// 前回の変換からこれだけ溜まったら回し直す (2048点の 1/8 = 5.3ms @48kHz)。
///
/// **窓が満杯になるのを待ってはいけない。** 待つと 42.7ms に1回しか絵が変わらず、
/// 再描画の間隔と拍が合わずにカクつく。直近の窓を滑らせながら、絵の更新より
/// 細かい間隔で回し直す。溜まっていないときは変換しない (止めている間に
/// 同じ入力を何度も回さないための足切り)。
const REFRESH_FRAMES: usize = FFT_SIZE / 8;

/// 簡易スペクトル。**見るためのもので、測るためのものではない**
pub struct Spectrum {
    sample_rate: u32,
    /// モノラルに落とした入力の輪。**常に直近 [`FFT_SIZE`] サンプルを持つ**
    ring: [f32; FFT_SIZE],
    /// 輪の次の書き込み位置 (ここが最も古いサンプルでもある)
    at: usize,
    /// 前回の変換から入った数
    since_transform: usize,
    /// Hann 窓の係数
    hann: [f32; FFT_SIZE],
    /// FFT の作業領域
    re: [f32; FFT_SIZE],
    im: [f32; FFT_SIZE],
    /// 帯ごとの [開始ビン, 終了ビン)
    bins: [(usize, usize); BANDS],
    /// 帯ごとの表示値 (dB)
    levels: [f32; BANDS],
}

impl Spectrum {
    pub fn new(sample_rate: u32) -> Self {
        let mut hann = [0.0f32; FFT_SIZE];
        for (index, value) in hann.iter_mut().enumerate() {
            let phase = 2.0 * PI * index as f64 / (FFT_SIZE - 1) as f64;
            *value = (0.5 - 0.5 * phase.cos()) as f32;
        }

        let mut bins = [(0usize, 0usize); BANDS];
        let hz_per_bin = sample_rate as f32 / FFT_SIZE as f32;
        let last_bin = FFT_SIZE / 2;
        for (band, slot) in bins.iter_mut().enumerate() {
            // 対数で等分する (低い帯ほど狭くなる)
            let edge = |at: usize| {
                let t = at as f32 / BANDS as f32;
                MIN_HZ * (MAX_HZ / MIN_HZ).powf(t)
            };
            let low = (edge(band) / hz_per_bin).floor().max(1.0) as usize;
            let high = (edge(band + 1) / hz_per_bin).ceil() as usize;
            // **必ず1本は入れる。** 低い帯は分解能が足りず空になりうる
            *slot = (low.min(last_bin - 1), high.clamp(low + 1, last_bin));
        }

        Self {
            sample_rate,
            ring: [0.0; FFT_SIZE],
            at: 0,
            since_transform: 0,
            hann,
            re: [0.0; FFT_SIZE],
            im: [0.0; FFT_SIZE],
            bins,
            levels: [FLOOR_DB; BANDS],
        }
    }

    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    pub fn reset(&mut self) {
        self.ring = [0.0; FFT_SIZE];
        self.at = 0;
        self.since_transform = 0;
        self.levels = [FLOOR_DB; BANDS];
    }

    /// ステレオのインターリーブを輪へ流し込む (**ここでは変換しない**)
    pub fn push(&mut self, interleaved: &[f32]) {
        for frame in interleaved.chunks_exact(2) {
            self.ring[self.at] = 0.5 * (frame[0] + frame[1]);
            self.at = (self.at + 1) % FFT_SIZE;
            self.since_transform += 1;
        }
    }

    /// 画面に出す値を更新する。`dt` は前回からの秒数 (落ちの量に使う)。
    ///
    /// 新しい入力が [`REFRESH_FRAMES`] ぶん溜まっていれば回し直し、
    /// 溜まっていなければ落ちだけ進める。
    pub fn update(&mut self, dt: f32) {
        if self.since_transform >= REFRESH_FRAMES {
            self.since_transform = 0;
            self.transform();
        }
        // **長く止まっていたぶんを一度に落とさない。** 画面が止まっていた
        // あとに一気に底へ張り付くと、鳴っているのに消えたように見える
        let drop = DECAY_DB_PER_SECOND * dt.clamp(0.0, 0.5);
        for level in self.levels.iter_mut() {
            *level = (*level - drop).max(FLOOR_DB);
        }
    }

    pub fn levels(&self) -> &[f32; BANDS] {
        &self.levels
    }

    /// 帯の中心周波数 (目盛りに使う)
    pub fn center_hz(band: usize) -> f32 {
        let t = (band as f32 + 0.5) / BANDS as f32;
        MIN_HZ * (MAX_HZ / MIN_HZ).powf(t)
    }

    fn transform(&mut self) {
        // 輪の古い側から並べ直して窓を掛ける (`at` が最も古い位置)
        for index in 0..FFT_SIZE {
            let source = (self.at + index) % FFT_SIZE;
            self.re[index] = self.ring[source] * self.hann[index];
            self.im[index] = 0.0;
        }
        fft(&mut self.re, &mut self.im);

        // 窓で減るぶんを戻す (Hann のコヒーレントゲインは 0.5)
        let scale = 2.0 / (FFT_SIZE as f32 * 0.5);
        for (band, (low, high)) in self.bins.iter().copied().enumerate() {
            let mut peak = 0.0f32;
            for bin in low..high {
                let magnitude = (self.re[bin] * self.re[bin] + self.im[bin] * self.im[bin]).sqrt();
                peak = peak.max(magnitude * scale);
            }
            let db = if peak > 0.0 {
                20.0 * peak.log10()
            } else {
                FLOOR_DB
            };
            // **上がるのは即座、下がるのは `update` に任せる**
            self.levels[band] = self.levels[band].max(db.clamp(FLOOR_DB, CEIL_DB));
        }
    }
}

/// 基数2の Cooley-Tukey (その場で書き換える)。長さは2のべき乗であること
fn fft(re: &mut [f32], im: &mut [f32]) {
    let n = re.len();
    debug_assert!(n.is_power_of_two());

    // ビット反転で並べ替える
    let mut target = 0usize;
    for source in 1..n {
        let mut bit = n >> 1;
        while target & bit != 0 {
            target ^= bit;
            bit >>= 1;
        }
        target |= bit;
        if source < target {
            re.swap(source, target);
            im.swap(source, target);
        }
    }

    let mut len = 2;
    while len <= n {
        let angle = -2.0 * PI / len as f64;
        let (step_sin, step_cos) = angle.sin_cos();
        for start in (0..n).step_by(len) {
            let (mut wr, mut wi) = (1.0f64, 0.0f64);
            for offset in 0..len / 2 {
                let a = start + offset;
                let b = a + len / 2;
                let tr = wr as f32 * re[b] - wi as f32 * im[b];
                let ti = wr as f32 * im[b] + wi as f32 * re[b];
                re[b] = re[a] - tr;
                im[b] = im[a] - ti;
                re[a] += tr;
                im[a] += ti;
                let next_wr = wr * step_cos - wi * step_sin;
                wi = wr * step_sin + wi * step_cos;
                wr = next_wr;
            }
        }
        len <<= 1;
    }
}

// -------------------------------------------------------------- ラウドネス

/// 積算の単位 (100ms)。Momentary も Short-term もこの倍数で切る
const BLOCK_MS: f64 = 100.0;
/// Momentary の窓 (400ms)
const MOMENTARY_BLOCKS: usize = 4;
/// Short-term の窓 (3秒)
const SHORT_TERM_BLOCKS: usize = 30;
/// 相対ゲートの下げ幅 (LU)
const RELATIVE_GATE_LU: f64 = 10.0;

/// K特性 高域シェルビングの原型 (BS.1770-4)
const SHELF_HZ: f64 = 1681.974450955533;
const SHELF_GAIN_DB: f64 = 3.999843853973347;
const SHELF_Q: f64 = 0.7071752369554196;
/// シェルビングの帯域側の利得を出す指数。
///
/// **規格の導出そのものに出てくる値**で、一般的なシェルビングの式には無い。
/// ここを普通の `sqrt` にすると 48kHz の表と 0.4% ずれる (実際にずらして確認した)
const SHELF_BAND_EXPONENT: f64 = 0.499_666_774_155;

/// K特性 低域を落とす段 (RLB) の原型
const HIGHPASS_HZ: f64 = 38.13547087602444;
const HIGHPASS_Q: f64 = 0.5003270373238773;

/// 双二次フィルタ1つ (直接形II転置)
#[derive(Clone, Copy, Default)]
struct Biquad {
    b0: f64,
    b1: f64,
    b2: f64,
    a1: f64,
    a2: f64,
    z1: f64,
    z2: f64,
}

impl Biquad {
    /// 正規化済みの係数から作る
    fn new(b: [f64; 3], a: [f64; 3]) -> Self {
        Self {
            b0: b[0] / a[0],
            b1: b[1] / a[0],
            b2: b[2] / a[0],
            a1: a[1] / a[0],
            a2: a[2] / a[0],
            z1: 0.0,
            z2: 0.0,
        }
    }

    /// K特性の1段目 (高域シェルビング)。
    ///
    /// **双一次変換をプリワープ込みで直に書く** (`K = tan(pi f0 / fs)`)。
    /// 規格の載せている 48kHz の表はこの形から出ている。
    fn shelf(sample_rate: u32) -> Self {
        let k = (PI * SHELF_HZ / sample_rate as f64).tan();
        let k2 = k * k;
        let high_gain = 10f64.powf(SHELF_GAIN_DB / 20.0);
        let band_gain = high_gain.powf(SHELF_BAND_EXPONENT);
        let denominator = 1.0 + k / SHELF_Q + k2;

        Self::new(
            [
                high_gain + band_gain * k / SHELF_Q + k2,
                2.0 * (k2 - high_gain),
                high_gain - band_gain * k / SHELF_Q + k2,
            ],
            [denominator, 2.0 * (k2 - 1.0), 1.0 - k / SHELF_Q + k2],
        )
    }

    /// K特性の2段目 (低域を落とす)。
    ///
    /// **分子は正規化済みで `[1, -2, 1]` に固定**。規格の表もそうなっている
    /// (ナイキストで利得1になる形)
    fn highpass(sample_rate: u32) -> Self {
        let k = (PI * HIGHPASS_HZ / sample_rate as f64).tan();
        let k2 = k * k;
        let denominator = 1.0 + k / HIGHPASS_Q + k2;

        Self::new(
            [1.0, -2.0, 1.0],
            [
                1.0,
                2.0 * (k2 - 1.0) / denominator,
                (1.0 - k / HIGHPASS_Q + k2) / denominator,
            ],
        )
    }

    fn process(&mut self, input: f64) -> f64 {
        let output = self.b0 * input + self.z1;
        self.z1 = self.b1 * input - self.a1 * output + self.z2;
        self.z2 = self.b2 * input - self.a2 * output;
        output
    }

    fn reset(&mut self) {
        self.z1 = 0.0;
        self.z2 = 0.0;
    }
}

/// BS.1770-4 のラウドネス測定 (ステレオ固定)
pub struct Loudness {
    sample_rate: u32,
    shelf: [Biquad; 2],
    highpass: [Biquad; 2],
    /// 積み上げ中の 100ms ブロック
    block_square_sum: [f64; 2],
    block_frames: usize,
    block_len: usize,
    /// 直近のブロックの二乗平均 (チャンネルごと)
    recent: VecDeque<[f64; 2]>,
    /// 積算用に取っておく 400ms 窓の合算エネルギー
    gating: Vec<f64>,
}

impl Loudness {
    pub fn new(sample_rate: u32) -> Self {
        let rate = sample_rate.max(1);
        Self {
            sample_rate: rate,
            shelf: [Biquad::shelf(rate); 2],
            highpass: [Biquad::highpass(rate); 2],
            block_square_sum: [0.0; 2],
            block_frames: 0,
            block_len: ((rate as f64 * BLOCK_MS / 1000.0).round() as usize).max(1),
            recent: VecDeque::with_capacity(SHORT_TERM_BLOCKS),
            gating: Vec::new(),
        }
    }

    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    pub fn reset(&mut self) {
        for channel in 0..2 {
            self.shelf[channel].reset();
            self.highpass[channel].reset();
        }
        self.block_square_sum = [0.0; 2];
        self.block_frames = 0;
        self.recent.clear();
        self.gating.clear();
    }

    /// 積算だけを測り直す (再生を始めたときなど)
    pub fn restart_integrated(&mut self) {
        self.gating.clear();
    }

    /// ステレオのインターリーブを流し込む
    pub fn push(&mut self, interleaved: &[f32]) {
        for frame in interleaved.chunks_exact(2) {
            for (channel, sample) in frame.iter().enumerate() {
                let filtered =
                    self.highpass[channel].process(self.shelf[channel].process(*sample as f64));
                self.block_square_sum[channel] += filtered * filtered;
            }
            self.block_frames += 1;
            if self.block_frames >= self.block_len {
                self.close_block();
            }
        }
    }

    fn close_block(&mut self) {
        let frames = self.block_frames.max(1) as f64;
        let mean = [
            self.block_square_sum[0] / frames,
            self.block_square_sum[1] / frames,
        ];
        self.block_square_sum = [0.0; 2];
        self.block_frames = 0;

        if self.recent.len() == SHORT_TERM_BLOCKS {
            self.recent.pop_front();
        }
        self.recent.push_back(mean);

        // 積算のゲートブロックは 400ms・75% 重なり = 100ms ごとに1つ
        if self.recent.len() >= MOMENTARY_BLOCKS {
            self.gating.push(self.energy(MOMENTARY_BLOCKS));
        }
    }

    /// 直近 `blocks` 個ぶんの合算エネルギー (チャンネル重み G はステレオでは 1.0)
    fn energy(&self, blocks: usize) -> f64 {
        let take = blocks.min(self.recent.len());
        if take == 0 {
            return 0.0;
        }
        let mut total = 0.0;
        for channel in 0..2 {
            let sum: f64 = self
                .recent
                .iter()
                .rev()
                .take(take)
                .map(|block| block[channel])
                .sum();
            total += sum / take as f64;
        }
        total
    }

    /// Momentary (直近 400ms)
    pub fn momentary(&self) -> f32 {
        loudness_of(self.energy(MOMENTARY_BLOCKS))
    }

    /// Short-term (直近3秒)
    pub fn short_term(&self) -> f32 {
        loudness_of(self.energy(SHORT_TERM_BLOCKS))
    }

    /// Integrated (頭からの積算、二段ゲート付き)
    pub fn integrated(&self) -> f32 {
        // 絶対ゲート
        let above: Vec<f64> = self
            .gating
            .iter()
            .copied()
            .filter(|energy| loudness_of(*energy) > SILENCE_LUFS)
            .collect();
        if above.is_empty() {
            return SILENCE_LUFS;
        }

        // 相対ゲート (絶対ゲートを通ったものの平均から 10 LU 下)
        let mean = above.iter().sum::<f64>() / above.len() as f64;
        let threshold = loudness_of(mean) as f64 - RELATIVE_GATE_LU;
        let gated: Vec<f64> = above
            .into_iter()
            .filter(|energy| loudness_of(*energy) as f64 > threshold)
            .collect();
        if gated.is_empty() {
            return SILENCE_LUFS;
        }

        loudness_of(gated.iter().sum::<f64>() / gated.len() as f64)
    }
}

/// 合算エネルギーを LUFS へ (BS.1770 の -0.691 補正込み)
fn loudness_of(energy: f64) -> f32 {
    if energy <= 0.0 {
        return SILENCE_LUFS;
    }
    let lufs = -0.691 + 10.0 * energy.log10();
    (lufs as f32).max(SILENCE_LUFS)
}

// ------------------------------------------------------------------ まとめ

/// 一度に取り込む上限 (フレーム数 × 2ch)。0.5秒ぶんあれば普段は溢れない
const MAX_DRAIN_SAMPLES: usize = 48_000;

/// スペクトルとラウドネスをまとめて回す
pub struct Meters {
    loudness: Loudness,
    spectrum: Spectrum,
}

impl Meters {
    pub fn new(sample_rate: u32) -> Self {
        Self {
            loudness: Loudness::new(sample_rate),
            spectrum: Spectrum::new(sample_rate),
        }
    }

    /// 輪から取り込んで表示値を更新する。`sample_rate` が変われば作り直す
    pub fn drain(&mut self, source: &mut rtrb::Consumer<f32>, sample_rate: u32, dt: f32) {
        if sample_rate != 0 && sample_rate != self.loudness.sample_rate() {
            *self = Self::new(sample_rate);
        }

        // **偶数個ずつ取る。** 半端に切ると次の塊で L と R が入れ替わる。
        //
        // **上限も要る。** 画面が長く止まっていたあとに溜まったぶんを全部回すと、
        // その1フレームだけ伸びて、そのせいでまた溜まる、という悪循環になる。
        // 古いものは捨ててよい (メーターは欠けても音に影響しない)
        let available = source.slots().min(MAX_DRAIN_SAMPLES) & !1;
        if available > 0 {
            if let Ok(chunk) = source.read_chunk(available) {
                let (first, second) = chunk.as_slices();
                // 輪の折り返しで2つに割れる。境目がフレームの途中なら揃うところで区切る
                let head = first.len() & !1;
                self.loudness.push(&first[..head]);
                self.spectrum.push(&first[..head]);
                if head == first.len() {
                    let tail = second.len() & !1;
                    self.loudness.push(&second[..tail]);
                    self.spectrum.push(&second[..tail]);
                }
                chunk.commit_all();
            }
        }

        self.spectrum.update(dt);
    }

    pub fn reset(&mut self) {
        self.loudness.reset();
        self.spectrum.reset();
    }

    pub fn momentary_lufs(&self) -> f32 {
        self.loudness.momentary()
    }

    pub fn short_term_lufs(&self) -> f32 {
        self.loudness.short_term()
    }

    pub fn integrated_lufs(&self) -> f32 {
        self.loudness.integrated()
    }

    pub fn spectrum_levels(&self) -> &[f32; BANDS] {
        self.spectrum.levels()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **48kHz の係数が規格の表と一致すること。**
    ///
    /// 原型から組み立てた結果が BS.1770-4 の載せている値になるかを見る。
    /// ここがずれていると、測定値が静かに間違ったまま出続ける。
    #[test]
    fn k_weighting_matches_the_published_table_at_48k() {
        let shelf = Biquad::shelf(48_000);
        let highpass = Biquad::highpass(48_000);

        let close = |actual: f64, expected: f64, what: &str| {
            assert!(
                (actual - expected).abs() < 1e-6,
                "{what}: 実測 {actual} / 規格 {expected}"
            );
        };

        close(shelf.b0, 1.53512485958697, "shelf b0");
        close(shelf.b1, -2.69169618940638, "shelf b1");
        close(shelf.b2, 1.19839281085285, "shelf b2");
        close(shelf.a1, -1.69065929318241, "shelf a1");
        close(shelf.a2, 0.73248077421585, "shelf a2");

        close(highpass.b0, 1.0, "highpass b0");
        close(highpass.b1, -2.0, "highpass b1");
        close(highpass.b2, 1.0, "highpass b2");
        close(highpass.a1, -1.99004745483398, "highpass a1");
        close(highpass.a2, 0.99007225036621, "highpass a2");
    }

    /// 無音は無音として出ること
    #[test]
    fn silence_reads_as_silence() {
        let mut loudness = Loudness::new(48_000);
        loudness.push(&vec![0.0; 48_000 * 2]);
        assert_eq!(loudness.momentary(), SILENCE_LUFS);
        assert_eq!(loudness.integrated(), SILENCE_LUFS);
    }

    /// 1kHz の正弦波が、振幅から計算した値どおりに出ること。
    ///
    /// 見ているのは**ラウドネスの組み立てのほう** (100ms 区切り・チャンネル合算・
    /// -0.691 の補正・二段ゲート)。K特性そのものは上のテストで縛ってあるので、
    /// **その持ち上がりぶんはここで実測して期待値に織り込む**
    /// (1kHz は素通しではなく、シェルビングが少し効き始めている)。
    #[test]
    fn a_sine_reads_close_to_its_calculated_level() {
        const RATE: u32 = 48_000;
        const HZ: f64 = 1000.0;
        let amplitude = 0.5f64;

        let sine = |frame: usize| {
            let phase = 2.0 * std::f64::consts::PI * HZ * frame as f64 / RATE as f64;
            amplitude * phase.sin()
        };

        // K特性が 1kHz で何 dB 持ち上げるかを測る (前半は過渡なので数えない)
        let mut shelf = Biquad::shelf(RATE);
        let mut highpass = Biquad::highpass(RATE);
        let (mut energy_in, mut energy_out) = (0.0f64, 0.0f64);
        for frame in 0..RATE as usize {
            let input = sine(frame);
            let output = highpass.process(shelf.process(input));
            if frame >= RATE as usize / 2 {
                energy_in += input * input;
                energy_out += output * output;
            }
        }
        let weighting_db = 10.0 * (energy_out / energy_in).log10();

        let mut samples = Vec::with_capacity(RATE as usize * 2 * 4);
        for frame in 0..RATE as usize * 4 {
            let value = sine(frame) as f32;
            samples.push(value);
            samples.push(value);
        }
        let mut loudness = Loudness::new(RATE);
        loudness.push(&samples);

        let rms = amplitude / 2f64.sqrt();
        let expected = -0.691 + 10.0 * (2.0 * rms * rms).log10() + weighting_db;
        let actual = loudness.integrated() as f64;
        assert!(
            (actual - expected).abs() < 0.2,
            "実測 {actual:.2} LUFS / 計算 {expected:.2} LUFS (K特性 {weighting_db:+.2} dB 込み)"
        );
    }

    /// 正弦波の山が、その周波数の帯に立つこと
    #[test]
    fn a_sine_shows_up_in_its_own_band() {
        const RATE: u32 = 48_000;
        const HZ: f32 = 1000.0;

        let mut spectrum = Spectrum::new(RATE);
        let mut samples = Vec::new();
        for frame in 0..FFT_SIZE * 2 {
            let phase = 2.0 * std::f64::consts::PI * HZ as f64 * frame as f64 / RATE as f64;
            let value = 0.5 * phase.sin() as f32;
            samples.push(value);
            samples.push(value);
        }
        spectrum.push(&samples);
        // **変換は `update` の仕事。** 押し込んだだけでは絵は変わらない
        spectrum.update(0.0);

        let loudest = spectrum
            .levels()
            .iter()
            .enumerate()
            .max_by(|a, b| a.1.partial_cmp(b.1).unwrap())
            .map(|(band, _)| band)
            .unwrap();

        let center = Spectrum::center_hz(loudest);
        // 帯は対数で割ってあるので、隣の帯までのずれは許す
        assert!(
            (center / HZ).log2().abs() < 0.2,
            "山が {center:.0}Hz の帯に立った (期待 {HZ:.0}Hz あたり)"
        );
    }

    /// 落ちが効いて、鳴り止んだら底へ戻ること
    #[test]
    fn bands_fall_back_to_the_floor() {
        let mut spectrum = Spectrum::new(48_000);
        spectrum.levels = [-10.0; BANDS];

        // **1回では底まで行かない。** 一度に落とす量は 0.5秒ぶんで頭打ちに
        // してある (画面が止まっていたあとに一気に消えないように)
        spectrum.update(1.0);
        assert!(
            spectrum.levels().iter().all(|level| *level > FLOOR_DB),
            "一度に底まで落とさないこと"
        );

        for _ in 0..4 {
            spectrum.update(0.5);
        }
        assert!(spectrum.levels().iter().all(|level| *level == FLOOR_DB));
    }

    /// **絵が窓の満杯を待たないこと。**
    ///
    /// 満杯 (2048サンプル = 42.7ms) を待つと、再描画の間隔と拍が合わずにカクつく。
    /// 窓の 1/8 だけ入れば回し直すことを縛っておく。
    #[test]
    fn the_picture_updates_before_the_window_fills() {
        let mut spectrum = Spectrum::new(48_000);

        // 窓の 1/8 ちょうど。満杯までは程遠い
        let mut samples = Vec::new();
        for frame in 0..REFRESH_FRAMES {
            let phase = 2.0 * std::f64::consts::PI * 1000.0 * frame as f64 / 48_000.0;
            let value = 0.5 * phase.sin() as f32;
            samples.push(value);
            samples.push(value);
        }
        spectrum.push(&samples);
        spectrum.update(0.0);

        assert!(
            spectrum.levels().iter().any(|level| *level > FLOOR_DB),
            "窓が満杯になる前に絵が出ること"
        );
    }
}
