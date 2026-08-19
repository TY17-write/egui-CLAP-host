//! マスター出力の監視 (スペクトルとラウドネス)。
//!
//! **オーディオスレッドでは何も測らない。** マスターのサンプルをそのまま
//! リングバッファへ流し、計算は全部メインスレッドで行う。オーディオ側の負担が
//! 「値のコピー」だけで済み、追いつかなければ捨てるだけでよくなる
//! (メーターは欠けても音に影響しない)。
//!
//! ```text
//! [オーディオスレッド]                    [メインスレッド]
//!   graph.master() ── rtrb (f32) ──▶  Meters::drain
//!                                        ├─ LoudnessMeter (BS.1770 K特性)
//!                                        └─ Spectrum (FFT 2048点)
//! ```

pub mod loudness;
pub mod spectrum;

pub use loudness::{LoudnessMeter, REFERENCE_LUFS, SILENCE_LUFS};
pub use spectrum::Spectrum;

/// 一度に取り込む上限 (フレーム数 × 2ch)。
///
/// 画面が長く止まっていた (ファイルダイアログを開いていた等) ときに、
/// 溜まったぶんを全部回すと1フレームが伸びる。**古いものは捨ててよい**ので、
/// 上限を決めて残りは読み飛ばす。0.5秒ぶんあれば普段は溢れない。
const MAX_DRAIN_SAMPLES: usize = 48_000;

/// マスターの監視一式。
///
/// サンプリングレートはエンジンが立ってから決まるので、[`drain`](Self::drain) に
/// 渡す。**変わったら中身を作り直す** (K特性の係数と帯の割り当てが変わるため)。
pub struct Meters {
    loudness: LoudnessMeter,
    spectrum: Spectrum,
}

impl Meters {
    pub fn new(sample_rate: u32) -> Self {
        Self {
            loudness: LoudnessMeter::new(sample_rate),
            spectrum: Spectrum::new(sample_rate),
        }
    }

    /// リングバッファから取り込んで、画面に出す値を更新する。
    ///
    /// `sample_rate` が前回と違えば作り直す。**書き出しで一時的に 48kHz へ
    /// 切り替えたときも通る**ので、そのたびに測り直しになる (書き出し中は
    /// 画面が止まっているので、見た目には現れない)。
    pub fn drain(&mut self, source: &mut rtrb::Consumer<f32>, sample_rate: u32, dt: f32) {
        if sample_rate != 0 && sample_rate != self.loudness.sample_rate() {
            *self = Self::new(sample_rate);
        }

        // **偶数個ずつ取る。** 半端に切ると次の塊で L と R が入れ替わる
        let available = source.slots().min(MAX_DRAIN_SAMPLES) & !1;
        if available > 0 {
            if let Ok(chunk) = source.read_chunk(available) {
                let (first, second) = chunk.as_slices();
                // 輪の折り返しで2つに割れることがある。
                // **境目がフレームの途中なら、揃うところまでで区切る**
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

    /// 溜めたものを捨てる (エンジンを止めたとき)
    pub fn reset(&mut self) {
        self.loudness.reset();
        self.spectrum.reset();
    }

    /// **Integrated だけ**を測り直す (再生開始・ループの折り返し)。
    ///
    /// M と S には触れない。直近を見るためのものなので、折り返しのたびに
    /// -∞ へ戻ると読めなくなる。
    pub fn restart_integrated(&mut self) {
        self.loudness.restart_integrated();
    }

    /// Momentary (400ms) のラウドネス
    pub fn momentary_lufs(&self) -> f32 {
        self.loudness.momentary()
    }

    /// Short-term (3秒) のラウドネス
    pub fn short_term_lufs(&self) -> f32 {
        self.loudness.short_term()
    }

    /// Integrated (測り直してから今まで) のラウドネス
    pub fn integrated_lufs(&self) -> f32 {
        self.loudness.integrated()
    }

    /// 帯ごとのレベル (dB)。左が低域
    pub fn spectrum_levels(&self) -> &[f32; spectrum::BANDS] {
        self.spectrum.levels()
    }
}

impl Default for Meters {
    fn default() -> Self {
        // エンジンが立つ前の見た目用。最初の drain で本当のレートに入れ替わる
        Self::new(48_000)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// リングバッファ越しに測れること (取り込みの経路そのものの担保)
    #[test]
    fn it_measures_through_the_ring() {
        let (mut producer, mut consumer) = rtrb::RingBuffer::<f32>::new(1 << 16);
        let mut meters = Meters::new(48_000);

        // -23 dBFS の 1kHz を 4秒ぶん、少しずつ流し込む
        let amplitude = 10f32.powf(-23.0 / 20.0);
        let mut frame = 0usize;
        while frame < 48_000 * 4 {
            let block = 256.min(48_000 * 4 - frame);
            for _ in 0..block {
                let t = frame as f32 / 48_000.0;
                let value = amplitude * (2.0 * std::f32::consts::PI * 1000.0 * t).sin();
                let _ = producer.push(value);
                let _ = producer.push(value);
                frame += 1;
            }
            meters.drain(&mut consumer, 48_000, 1.0 / 60.0);
        }

        assert!(
            (meters.short_term_lufs() - (-23.0)).abs() < 0.1,
            "{} LUFS (期待 -23.0)",
            meters.short_term_lufs()
        );
    }

    /// レートが変わったら作り直されること
    #[test]
    fn a_new_sample_rate_rebuilds_everything() {
        let (_producer, mut consumer) = rtrb::RingBuffer::<f32>::new(16);
        let mut meters = Meters::new(48_000);
        meters.drain(&mut consumer, 44_100, 0.0);
        assert_eq!(meters.loudness.sample_rate(), 44_100);
        assert_eq!(meters.spectrum.sample_rate(), 44_100);
    }

    /// **フレームの途中で切らないこと。**
    ///
    /// 奇数個だけ取ると、次の塊で L と R が入れ替わったまま測り続ける。
    /// 片側だけ鳴らして、左右が混ざらないことで確かめる。
    #[test]
    fn it_never_splits_a_frame() {
        let (mut producer, mut consumer) = rtrb::RingBuffer::<f32>::new(1 << 16);
        let mut meters = Meters::new(48_000);

        // 左だけ鳴らす。取り込みがずれると右にも漏れる
        let amplitude = 10f32.powf(-23.0 / 20.0);
        for frame in 0..48_000 * 4 {
            let t = frame as f32 / 48_000.0;
            let _ = producer.push(amplitude * (2.0 * std::f32::consts::PI * 1000.0 * t).sin());
            let _ = producer.push(0.0);
            // 奇数個溜まった時点でも取り込みに行く
            if frame % 3 == 0 {
                meters.drain(&mut consumer, 48_000, 0.0);
            }
        }
        meters.drain(&mut consumer, 48_000, 0.0);

        // 片側だけなら両側より 3.01 LU 低い。混ざると値がぶれる
        assert!(
            (meters.short_term_lufs() - (-26.01)).abs() < 0.1,
            "{} LUFS (片側だけなので -26.01 のはず)",
            meters.short_term_lufs()
        );
    }
}
