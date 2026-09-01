//! dB (ピーク) メーター。マスターの**サンプルピーク**を dBFS で出す。
//!
//! **真のピーク (dBTP) ではない。** 4倍オーバーサンプリングで山の間を読む
//! 真のピークと違い、サンプル値の最大をそのまま使う。サンプルの谷間に挟まった
//! 山は 0 dBFS を超えていても見えないことがある (最悪 +3 dB 程度)。
//! DAW の標準的なチャンネルメーターと同じ割り切り。
//!
//! 3つの値を持つ:
//!
//! - **バー**: 立ち上がりは即時、戻りは [`RELEASE_DB_PER_SECOND`] で落ちる
//! - **ホールド**: バーの上に残る印。[`HOLD_SECONDS`] 保持してから落ちる
//! - **最大値**: 測り直すまで保持。読み値の数字はこれ
//!
//! クリップ (振幅 1.0 以上) も測り直すまで保持する。瞬間の点灯だと
//! 見ていない間のクリップを取りこぼすため。

/// 表示の下限。これ以下は -∞ 扱い ([`SILENCE_LUFS`](super::SILENCE_LUFS) と同じ値)
pub const SILENCE_DBFS: f32 = -70.0;

/// バーの戻り速度 (dB/秒)。
///
/// ラウドネスメーターの戻り (IEC 60268-18 は 20 dB/1.7秒 ≒ 11.8 dB/秒) より
/// 少し速め。遅いと次の山が来る前に前の山が読めなくなる。
const RELEASE_DB_PER_SECOND: f32 = 20.0;

/// ホールドの保持時間 (秒)。過ぎたらバーと同じ速度で落ちる
const HOLD_SECONDS: f32 = 2.0;

/// クリップとみなす振幅。1.0 ちょうどのサンプル (0 dBFS のフルスケール) から数える
const CLIP_AMPLITUDE: f32 = 1.0;

/// 振幅 (リニア) → dBFS。0 や負は下限へ落とす
fn to_dbfs(amplitude: f32) -> f32 {
    if amplitude <= 0.0 {
        return SILENCE_DBFS;
    }
    (20.0 * amplitude.log10()).max(SILENCE_DBFS)
}

/// マスターのピークメーター (ステレオ)。
///
/// 中身はすべて振幅 (リニア) で持ち、dB へは読むときに変換する。
/// dB で持って毎サンプル log を取ると、push が測る量に対して重くなる。
pub struct PeakMeter {
    /// バーの値 (チャンネルごと)
    bar: [f32; 2],
    /// ホールドの値 (チャンネルごと)
    hold: [f32; 2],
    /// ホールドがその値になってからの経過 (秒)
    hold_age: [f32; 2],
    /// 測り直してからの最大 (両チャンネルの大きいほう)
    max: f32,
    /// 振幅 1.0 以上のサンプルが来たか。測り直すまで保持
    clipped: bool,
}

impl PeakMeter {
    pub fn new() -> Self {
        Self {
            bar: [0.0; 2],
            hold: [0.0; 2],
            hold_age: [0.0; 2],
            max: 0.0,
            clipped: false,
        }
    }

    /// L/R 交互のサンプルを流し込む。**長さは2の倍数**であること
    /// (半端な分は捨てる。次のブロックで頭が入れ替わると左右が入れ替わるため)。
    pub fn push(&mut self, interleaved: &[f32]) {
        for frame in interleaved.chunks_exact(2) {
            for (channel, sample) in frame.iter().enumerate() {
                let amplitude = sample.abs();
                // 立ち上がりは即時。戻りは update が受け持つ
                if amplitude > self.bar[channel] {
                    self.bar[channel] = amplitude;
                }
                if amplitude > self.hold[channel] {
                    self.hold[channel] = amplitude;
                    self.hold_age[channel] = 0.0;
                }
                if amplitude > self.max {
                    self.max = amplitude;
                }
                if amplitude >= CLIP_AMPLITUDE {
                    self.clipped = true;
                }
            }
        }
    }

    /// 画面に出す値を更新する。`dt` は前回からの秒数 (戻りの量に使う)。
    pub fn update(&mut self, dt: f32) {
        // 画面が長く止まっていたぶんまで落とすと、戻った瞬間に全部消えている。
        // スペクトルと同じ理由で上限を入れる
        let dt = dt.clamp(0.0, 0.5);
        let release = 10f32.powf(-RELEASE_DB_PER_SECOND * dt / 20.0);
        for channel in 0..2 {
            self.bar[channel] *= release;
            self.hold_age[channel] += dt;
            if self.hold_age[channel] > HOLD_SECONDS {
                self.hold[channel] *= release;
            }
            // 下限より下は 0 に潰す (無音時に非正規化数まで減衰し続けないため)
            if to_dbfs(self.bar[channel]) <= SILENCE_DBFS {
                self.bar[channel] = 0.0;
            }
            if to_dbfs(self.hold[channel]) <= SILENCE_DBFS {
                self.hold[channel] = 0.0;
            }
        }
    }

    /// 全部消す (エンジンを止めたとき)
    pub fn reset(&mut self) {
        *self = Self::new();
    }

    /// 最大値とクリップとホールドを測り直す (ユーザーの操作)。
    /// バーは触らない — 鳴っている音は鳴ったままなので
    pub fn restart(&mut self) {
        self.max = 0.0;
        self.clipped = false;
        self.hold = [0.0; 2];
        self.hold_age = [0.0; 2];
    }

    /// バーの値 (dBFS)。`channel` は 0 = L / 1 = R
    pub fn bar_dbfs(&self, channel: usize) -> f32 {
        to_dbfs(self.bar[channel.min(1)])
    }

    /// ホールドの値 (dBFS)
    pub fn hold_dbfs(&self, channel: usize) -> f32 {
        to_dbfs(self.hold[channel.min(1)])
    }

    /// 測り直してからの最大 (dBFS)
    pub fn max_dbfs(&self) -> f32 {
        to_dbfs(self.max)
    }

    /// 振幅 1.0 以上のサンプルが来たか
    pub fn clipped(&self) -> bool {
        self.clipped
    }
}

impl Default for PeakMeter {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 振幅 0.5 = -6.02 dBFS。換算の基準そのもの
    #[test]
    fn half_amplitude_reads_minus_six_dbfs() {
        let mut meter = PeakMeter::new();
        meter.push(&[0.5, -0.5]);
        for channel in 0..2 {
            assert!(
                (meter.bar_dbfs(channel) - (-6.02)).abs() < 0.01,
                "ch{channel}: {}",
                meter.bar_dbfs(channel)
            );
        }
        assert!((meter.max_dbfs() - (-6.02)).abs() < 0.01);
        assert!(!meter.clipped());
    }

    /// 立ち上がりは即時、戻りは決めた速度で落ちること
    #[test]
    fn attack_is_instant_and_release_is_timed() {
        let mut meter = PeakMeter::new();
        meter.push(&[1.0, 1.0]);
        assert!((meter.bar_dbfs(0) - 0.0).abs() < 0.01, "立ち上がりは即時");

        meter.update(0.1);
        let expected = -RELEASE_DB_PER_SECOND * 0.1;
        assert!(
            (meter.bar_dbfs(0) - expected).abs() < 0.01,
            "0.1秒で {expected} dB のはずが {}",
            meter.bar_dbfs(0)
        );
    }

    /// ホールドは保持時間のあいだ落ちず、過ぎたら落ち始めること。
    /// (`dt` は1回 0.5 秒までしか進まないので、経過は刻んで作る)
    #[test]
    fn hold_stays_then_falls() {
        let mut meter = PeakMeter::new();
        meter.push(&[0.5, 0.5]);

        for _ in 0..3 {
            meter.update(0.5); // 1.5秒 < HOLD_SECONDS (2.0)
        }
        assert!(
            (meter.hold_dbfs(0) - (-6.02)).abs() < 0.01,
            "保持時間内は動かない"
        );

        for _ in 0..2 {
            meter.update(0.5); // 2.5秒 > HOLD_SECONDS
        }
        assert!(meter.hold_dbfs(0) < -6.02, "保持時間を過ぎたら落ちる");
    }

    /// 最大値とクリップは update では消えず、restart で消えること
    #[test]
    fn max_and_clip_latch_until_restarted() {
        let mut meter = PeakMeter::new();
        meter.push(&[1.2, 0.0]);
        assert!(meter.clipped());

        for _ in 0..100 {
            meter.update(0.5);
        }
        assert!(meter.clipped(), "時間では消えない");
        assert!((meter.max_dbfs() - to_dbfs(1.2)).abs() < 0.01);

        // バーは触らない (鳴っている音は鳴ったまま)
        meter.push(&[0.5, 0.5]);
        meter.restart();
        assert!(!meter.clipped());
        assert_eq!(meter.max_dbfs(), SILENCE_DBFS);
        assert!(meter.bar_dbfs(0) > SILENCE_DBFS);
    }

    /// 左右が混ざらないこと (取り込みのずれの検出)
    #[test]
    fn channels_stay_separate() {
        let mut meter = PeakMeter::new();
        meter.push(&[0.5, 0.0, 0.5, 0.0]);
        assert!(meter.bar_dbfs(0) > -7.0);
        assert_eq!(meter.bar_dbfs(1), SILENCE_DBFS, "右は無音のまま");
    }

    /// 無音が続くと 0 まで潰れて下限で止まること
    #[test]
    fn silence_settles_at_the_floor() {
        let mut meter = PeakMeter::new();
        meter.push(&[0.001, 0.001]);
        for _ in 0..100 {
            meter.update(0.5);
        }
        assert_eq!(meter.bar_dbfs(0), SILENCE_DBFS);
        assert_eq!(meter.hold_dbfs(0), SILENCE_DBFS);
    }
}
