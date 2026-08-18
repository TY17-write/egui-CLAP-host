//! 簡易スペクトルモニター (FFT + 対数の帯分け)。
//!
//! **見るためのもので、測るためのものではない。** 帯ごとの最大値を取って
//! 落ちを付けているので、絶対値の精度は求めていない (それが要る場面では
//! ラウドネス側を見ること)。
//!
//! FFT は 2048 点の基数2で、外部クレートを足していない。この大きさを
//! 毎フレーム1回まわす程度なので速度は問題にならず、依存を1つ増やすほうが高くつく。

/// 変換の点数。48kHz で 23Hz 刻み・窓の長さ 43ms。
/// 細かくすると低域は見やすくなるが、その分だけ時間の追従が鈍る
const FFT_SIZE: usize = 2048;

/// 画面に出す帯の数
pub const BANDS: usize = 56;

/// 帯を割り当てる範囲。下は低すぎると分解能が足りず、上は聞こえる範囲で切る
const MIN_HZ: f32 = 30.0;
const MAX_HZ: f32 = 18_000.0;

/// 表示の下限 (これより静かな帯は底に張り付く)
pub const FLOOR_DB: f32 = -90.0;
/// 表示の上限
pub const CEIL_DB: f32 = 0.0;

/// 山の落ちる速さ (dB/秒)。速いとちらつき、遅いと張り付いて見える
const DECAY_DB_PER_SECOND: f32 = 90.0;

/// 前回の変換からこれだけ溜まったら回し直す。
/// 止まっているときに同じ入力を何度も変換しないための足切り
const REFRESH_FRAMES: usize = FFT_SIZE / 8;

/// 基数2の複素 FFT (定位置変換)。回転因子とビット反転は作るときに用意する。
struct Fft {
    re: [f32; FFT_SIZE],
    im: [f32; FFT_SIZE],
    /// ビット反転した並び替え先
    reversed: [u16; FFT_SIZE],
    /// 回転因子 (`-2πk/N` の cos と sin)
    cos: [f32; FFT_SIZE / 2],
    sin: [f32; FFT_SIZE / 2],
}

impl Fft {
    fn new() -> Self {
        let bits = FFT_SIZE.trailing_zeros();
        let mut reversed = [0u16; FFT_SIZE];
        for (index, slot) in reversed.iter_mut().enumerate() {
            // `u16` に収まるのは FFT_SIZE <= 65536 だから。広げるならここも直すこと
            *slot = ((index as u32).reverse_bits() >> (32 - bits)) as u16;
        }

        let mut cos = [0f32; FFT_SIZE / 2];
        let mut sin = [0f32; FFT_SIZE / 2];
        for k in 0..FFT_SIZE / 2 {
            let angle = -2.0 * std::f32::consts::PI * k as f32 / FFT_SIZE as f32;
            cos[k] = angle.cos();
            sin[k] = angle.sin();
        }

        Self {
            re: [0.0; FFT_SIZE],
            im: [0.0; FFT_SIZE],
            reversed,
            cos,
            sin,
        }
    }

    /// `re` に入力を入れてから呼ぶ。`im` は 0 で埋めること
    fn transform(&mut self) {
        for index in 0..FFT_SIZE {
            let target = self.reversed[index] as usize;
            if index < target {
                self.re.swap(index, target);
                self.im.swap(index, target);
            }
        }

        let mut span = 2;
        while span <= FFT_SIZE {
            let half = span / 2;
            let step = FFT_SIZE / span;
            let mut start = 0;
            while start < FFT_SIZE {
                for k in 0..half {
                    let twiddle = k * step;
                    let (wr, wi) = (self.cos[twiddle], self.sin[twiddle]);
                    let low = start + k;
                    let high = low + half;

                    let tr = self.re[high] * wr - self.im[high] * wi;
                    let ti = self.re[high] * wi + self.im[high] * wr;

                    self.re[high] = self.re[low] - tr;
                    self.im[high] = self.im[low] - ti;
                    self.re[low] += tr;
                    self.im[low] += ti;
                }
                start += span;
            }
            span <<= 1;
        }
    }
}

/// スペクトルモニター。**入力は L/R の交互** (グラフの出力そのまま)。
pub struct Spectrum {
    sample_rate: u32,
    fft: Fft,
    /// ハン窓 (両端を落として、周波数のにじみを抑える)
    window: [f32; FFT_SIZE],
    /// 窓の総和 ÷ 2。全振幅の正弦波が 0dB になるよう正規化するのに使う
    normalize: f32,
    /// 直近のモノラル入力 (輪。新しいものが `at` の手前)
    ring: [f32; FFT_SIZE],
    at: usize,
    /// 前回の変換から入ったフレーム数
    since_transform: usize,
    /// 帯ごとに見るビンの範囲
    ranges: [(usize, usize); BANDS],
    /// 画面に出す値 (dB。落ちを付けたあとのもの)
    levels: [f32; BANDS],
}

impl Spectrum {
    pub fn new(sample_rate: u32) -> Self {
        let rate = sample_rate.max(1);

        let mut window = [0f32; FFT_SIZE];
        let mut sum = 0.0;
        for (index, slot) in window.iter_mut().enumerate() {
            // ハン窓
            let value =
                0.5 - 0.5 * (2.0 * std::f32::consts::PI * index as f32 / FFT_SIZE as f32).cos();
            *slot = value;
            sum += value;
        }

        // 帯が丸ごと含むビンの範囲。**低域では1つも含まないことがある**
        // (30〜100Hz は帯10本ぶんあるのに、48kHz ではビンが3つしかない)。
        // そのときは `from > to` になり、中心での補間だけを使う。
        let mut ranges = [(1usize, 0usize); BANDS];
        let ratio = MAX_HZ / MIN_HZ;
        let last_bin = FFT_SIZE / 2 - 1;
        let per_hz = FFT_SIZE as f32 / rate as f32;
        for (index, slot) in ranges.iter_mut().enumerate() {
            let low = MIN_HZ * ratio.powf(index as f32 / BANDS as f32);
            let high = MIN_HZ * ratio.powf((index + 1) as f32 / BANDS as f32);
            let from = (low * per_hz).ceil().max(1.0) as usize;
            let to = (high * per_hz).floor() as usize;
            *slot = (from, to.min(last_bin));
        }

        Self {
            sample_rate: rate,
            fft: Fft::new(),
            window,
            normalize: sum / 2.0,
            ring: [0.0; FFT_SIZE],
            at: 0,
            since_transform: 0,
            ranges,
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

    /// L/R 交互のサンプルを流し込む (中でモノラルに落とす)
    pub fn push(&mut self, interleaved: &[f32]) {
        for frame in interleaved.chunks_exact(2) {
            self.ring[self.at] = (frame[0] + frame[1]) * 0.5;
            self.at = (self.at + 1) % FFT_SIZE;
            self.since_transform += 1;
        }
    }

    /// 画面に出す値を更新する。`dt` は前回からの秒数 (落ちの量に使う)。
    ///
    /// 新しい入力が溜まっていなければ変換はせず、落ちだけ進める
    /// (止めている間もゆっくり下がる)。
    pub fn update(&mut self, dt: f32) {
        let decay = DECAY_DB_PER_SECOND * dt.clamp(0.0, 0.5);

        if self.since_transform >= REFRESH_FRAMES {
            self.since_transform = 0;
            self.transform();
        }
        for level in self.levels.iter_mut() {
            *level = (*level - decay).max(FLOOR_DB);
        }
    }

    fn transform(&mut self) {
        // 輪の古い側から並べ直して窓を掛ける
        for index in 0..FFT_SIZE {
            let source = (self.at + index) % FFT_SIZE;
            self.fft.re[index] = self.ring[source] * self.window[index];
            self.fft.im[index] = 0.0;
        }
        self.fft.transform();

        for (band, (from, to)) in self.ranges.iter().copied().enumerate() {
            // **中心での補間を必ず見る。** 帯がビンより細かい低域では、丸ごと含む
            // ビンが1つも無い。ビンに丸めて拾うと、隣り合う何本もの帯が同じビンを
            // 指して同じ高さになり、**山が本当より低い側に見える**
            // (100Hz の音が 75Hz あたりから立っているように見えた)。
            let mut peak = self.magnitude_at(Self::center_hz(band));
            for bin in from..=to {
                peak = peak.max(self.magnitude(bin));
            }
            // 実数入力なので上半分と対になっている。振幅に直すため2倍する
            let amplitude = peak * 2.0 / self.normalize;
            let db = if amplitude > 0.0 {
                20.0 * amplitude.log10()
            } else {
                FLOOR_DB
            };
            // 落ちの途中でも、新しい山が高ければそちらを採る
            self.levels[band] = self.levels[band].max(db.clamp(FLOOR_DB, CEIL_DB));
        }
    }

    /// 1ビンの大きさ
    fn magnitude(&self, bin: usize) -> f32 {
        let re = self.fft.re[bin];
        let im = self.fft.im[bin];
        (re * re + im * im).sqrt()
    }

    /// その周波数での大きさ (隣り合うビンの線形補間)。
    ///
    /// ハン窓を掛けた正弦波はビン3つ程度に広がるので、ビンの間を補うと
    /// **山の位置が本当の周波数に乗る**。低域の細い帯はこれだけで描く。
    fn magnitude_at(&self, hz: f32) -> f32 {
        let position = hz * FFT_SIZE as f32 / self.sample_rate as f32;
        let last = FFT_SIZE / 2 - 1;
        if position < 1.0 || position >= last as f32 {
            return 0.0;
        }
        let low = position.floor() as usize;
        let fraction = position - low as f32;
        self.magnitude(low) * (1.0 - fraction) + self.magnitude(low + 1) * fraction
    }

    /// 帯ごとの値 (dB)。左が低域
    pub fn levels(&self) -> &[f32; BANDS] {
        &self.levels
    }

    /// 帯の中心周波数 (目盛りの表示に使う)
    pub fn center_hz(band: usize) -> f32 {
        let ratio = MAX_HZ / MIN_HZ;
        MIN_HZ * ratio.powf((band as f32 + 0.5) / BANDS as f32)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stereo_sine(sample_rate: u32, frames: usize, hz: f32, amplitude: f32) -> Vec<f32> {
        let mut out = Vec::with_capacity(frames * 2);
        for frame in 0..frames {
            let t = frame as f32 / sample_rate as f32;
            let value = amplitude * (2.0 * std::f32::consts::PI * hz * t).sin();
            out.push(value);
            out.push(value);
        }
        out
    }

    /// いちばん強い帯を返す
    fn loudest(spectrum: &Spectrum) -> usize {
        let levels = spectrum.levels();
        (0..BANDS).fold(0, |best, band| {
            if levels[band] > levels[best] {
                band
            } else {
                best
            }
        })
    }

    /// **入れた周波数の帯が立つこと。** ここが外れると絵が周波数を表さない
    #[test]
    fn a_tone_lights_up_its_own_band() {
        for hz in [100.0f32, 1000.0, 5000.0] {
            let mut spectrum = Spectrum::new(48_000);
            spectrum.push(&stereo_sine(48_000, FFT_SIZE * 2, hz, 0.5));
            spectrum.update(0.0);

            let band = loudest(&spectrum);
            let center = Spectrum::center_hz(band);
            // 帯は対数で切ってあるので、隣とは十数%しか離れていない
            let ratio = center / hz;
            assert!(
                (0.85..=1.18).contains(&ratio),
                "{hz} Hz が中心 {center} Hz の帯 ({band}) に出た"
            );
        }
    }

    /// **全振幅の正弦波が 0 dB になること。** 窓の正規化が合っている担保
    #[test]
    fn a_full_scale_tone_reads_zero_db() {
        let mut spectrum = Spectrum::new(48_000);
        spectrum.push(&stereo_sine(48_000, FFT_SIZE * 2, 1000.0, 1.0));
        spectrum.update(0.0);

        let peak = spectrum.levels()[loudest(&spectrum)];
        assert!((peak - CEIL_DB).abs() < 1.0, "{peak} dB (0 dB のはず)");
    }

    /// 振幅を半分にすると約 6 dB 下がること
    #[test]
    fn halving_the_amplitude_drops_six_db() {
        let measure = |amplitude: f32| {
            let mut spectrum = Spectrum::new(48_000);
            spectrum.push(&stereo_sine(48_000, FFT_SIZE * 2, 1000.0, amplitude));
            spectrum.update(0.0);
            spectrum.levels()[loudest(&spectrum)]
        };
        let loud = measure(0.5);
        let quiet = measure(0.25);
        assert!(
            (loud - quiet - 6.02).abs() < 0.5,
            "{loud} → {quiet} (6 dB 下がること)"
        );
    }

    /// 無音は底に張り付くこと (0 の対数で -∞ や NaN を出さない)
    #[test]
    fn silence_sits_on_the_floor() {
        let mut spectrum = Spectrum::new(48_000);
        spectrum.push(&vec![0.0; FFT_SIZE * 4]);
        spectrum.update(0.0);
        for (band, level) in spectrum.levels().iter().enumerate() {
            assert_eq!(*level, FLOOR_DB, "帯 {band}");
            assert!(level.is_finite());
        }
    }

    /// 鳴り止んだら落ちること (張り付いたままにしない)
    #[test]
    fn the_display_falls_when_the_sound_stops() {
        let mut spectrum = Spectrum::new(48_000);
        spectrum.push(&stereo_sine(48_000, FFT_SIZE * 2, 1000.0, 1.0));
        spectrum.update(0.0);
        let peak = spectrum.levels()[loudest(&spectrum)];

        // 入力を足さずに 0.1 秒進める
        spectrum.update(0.1);
        let after = spectrum.levels()[loudest(&spectrum)];
        assert!(after < peak, "{peak} → {after} (下がること)");
        assert!(
            (peak - after - DECAY_DB_PER_SECOND * 0.1).abs() < 0.01,
            "落ちの量が決めた速さと合うこと"
        );
    }

    /// レートが変われば帯の割り当ても変わること。
    /// (48kHz の割り当てを 96kHz で使うと、絵の周波数が倍にずれる)
    #[test]
    fn the_bands_follow_the_sample_rate() {
        for rate in [44_100, 48_000, 96_000] {
            let mut spectrum = Spectrum::new(rate);
            spectrum.push(&stereo_sine(rate, FFT_SIZE * 2, 1000.0, 0.5));
            spectrum.update(0.0);

            let center = Spectrum::center_hz(loudest(&spectrum));
            let ratio = center / 1000.0;
            assert!(
                (0.85..=1.18).contains(&ratio),
                "{rate} Hz で 1000 Hz が中心 {center} Hz の帯に出た"
            );
        }
    }

    /// FFT そのものが正しいこと (既知の入力で確かめる)。
    /// 定数の入力は直流成分だけが立ち、それ以外は 0 になる。
    #[test]
    fn the_transform_puts_a_constant_in_the_first_bin() {
        let mut fft = Fft::new();
        fft.re = [1.0; FFT_SIZE];
        fft.im = [0.0; FFT_SIZE];
        fft.transform();

        assert!((fft.re[0] - FFT_SIZE as f32).abs() < 0.01, "{}", fft.re[0]);
        assert!(fft.im[0].abs() < 0.01);
        for bin in 1..FFT_SIZE / 2 {
            let magnitude = (fft.re[bin] * fft.re[bin] + fft.im[bin] * fft.im[bin]).sqrt();
            assert!(magnitude < 0.01, "ビン {bin} が {magnitude}");
        }
    }
}
