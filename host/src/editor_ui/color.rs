//! ノートの塗り色。音高から Oklch で色を決め、sRGB へ落とす。
//!
//! 描画には触らない純関数だけを置く。

use crate::sequencer::{Note, ScaleMode};
use eframe::egui::Color32;

/// ノートの色相 (Oklch, 度)。低い音から高い音へ、
/// ティール → 青 → 藤 → 蘭 → 紅紫 → 珊瑚 → 琥珀。
///
/// **Oklch の色相角**なので、HSV の角度とは対応しない。おおよそ
/// ティール 195°・青 265°・紅紫 325°・赤 389° (= 29°)・琥珀 430° (= 70°)。
/// Oklch は色相を**知覚的に等間隔**に並べるので、HSV でやったときのように
/// 青〜紫が「どれも青っぽい」と潰れることがない。
///
/// 幅を 275° と広めに取っているのは色域の都合。**sRGB では水色〜藤色の帯だけ
/// 彩度が出せない** (明度 0.68 でクロマ上限 0.13 前後、紅紫は 0.29)。
/// 幅が狭いと、いちばん使うオクターブ4〜5 がその帯に居座って淡くなる。
/// 広げると4〜5 が紅紫側へ抜けるので、上限が 0.13 → 0.24 に上がる。
/// 引き換えに、上端は赤を通り越して琥珀色になる。
const NOTE_HUE_LOW: f32 = 195.0;
const NOTE_HUE_HIGH: f32 = 470.0;

/// ノートの明度 (Oklch)。音高によらず一定。
///
/// 一度は音高で動かしてみたが、**明るくするほど彩度が犠牲になる**。
/// 高い明度を要求すると色域が狭まり、クロマを削る処理が強く働くため、
/// 高音側が淡くなってしまった。色相だけで音高を表し、明度は
/// 彩度がいちばん稼げるところに固定するほうが見分けやすい。
///
/// 0.68 という値は上下から挟まれている。**下げるほど青と藤色の彩度が上がる**が、
/// ノート名のラベルが読めなくなる (塗りが届いた部分は [`palette::BG`] で
/// 抜いているため)。この値で全音域のコントラスト比が 5:1 以上ある。
///
/// [`palette::BG`]: crate::theme::palette::BG
const NOTE_LIGHTNESS: f32 = 0.68;

/// ノートの彩度 (Oklch のクロマ)。
///
/// sRGB に収まらない組み合わせは色相と明度を保ったままクロマを削るので、
/// ここは「出せるだけ出す」ための要求値でよい。色相ごとに出せる上限が
/// 違う (水色は低く、紅紫は高い) ため、一律の値は取れない。
const NOTE_CHROMA: f32 = 0.17;

/// 色を割り当てる音域。この外は端の色で頭打ちにする。
///
/// 指定できる範囲 ([`MIN_OCTAVE`]〜[`MAX_OCTAVE`]) 全体に広げると、実際に
/// よく使う音域が色相のごく一部に収まって隣同士の差が出ない。常用域に寄せて、
/// そこで色が大きく動くようにしてある。
/// 代償として、極端に高い音どうし・低い音どうしは同じ色になる。
const NOTE_COLOR_MIN_OCTAVE: i32 = 1;
const NOTE_COLOR_MAX_OCTAVE: i32 = 7;

/// ノートの塗り色。
///
/// **音高が上がるほど寒色から暖色へ変わる連続的なグラデーション**にしてある。
/// [`NOTE_COLOR_MIN_OCTAVE`]〜[`NOTE_COLOR_MAX_OCTAVE`] を色相の幅に割り当て、
/// その外は端の色で頭打ちにする。割り当ては音高そのものに紐づくので、
/// 同じ音なら常に同じ色になる (画面に出ている範囲で正規化すると、
/// スクロールしただけで色が変わってしまう)。
///
/// オクターブ内も半音で連続的に変える。オクターブ境界で色が飛ばないので、
/// 隣り合う音の高低が色の変化として読める。
pub(super) fn note_fill(note: &Note, scale: ScaleMode) -> Color32 {
    let steps = scale.steps_per_octave().max(1);
    // オクターブ + オクターブ内の位置。BP 音律なら13ステップで割る
    let position = note.octave as f32 + note.semitone.clamp(0, steps) as f32 / steps as f32;

    let span = (NOTE_COLOR_MAX_OCTAVE - NOTE_COLOR_MIN_OCTAVE + 1) as f32;
    let t = ((position - NOTE_COLOR_MIN_OCTAVE as f32) / span).clamp(0.0, 1.0);

    let hue = NOTE_HUE_LOW + (NOTE_HUE_HIGH - NOTE_HUE_LOW) * t;
    oklch_to_color(NOTE_LIGHTNESS, NOTE_CHROMA, hue)
}

/// Oklch (明度・クロマ・色相) から sRGB へ。`hue` は度。
///
/// Oklch を使うのは、**色相を知覚的に等間隔に並べたい**のと、`lightness` が
/// 見た目の明るさそのものになるため。HSV の `value` は明るさではなく
/// (シアンの V=0.62 はピンクの V=0.95 より明るく見える)、色相の間隔も
/// 知覚と合わない。
///
/// 指定した色が sRGB に収まらないときは、**色相と明度を保ったままクロマを削る**。
/// RGB を切り詰めると色相と明度がずれるので、そちらは採らない。
fn oklch_to_color(lightness: f32, chroma: f32, hue: f32) -> Color32 {
    let radians = hue.to_radians();
    let (sin, cos) = radians.sin_cos();

    // sRGB に収まる最大のクロマを二分探索で詰める。
    // 収まるなら1回で終わり、外れていても十数回で十分な精度になる。
    let mut low = 0.0f32;
    let mut high = chroma.max(0.0);
    if !fits_in_srgb(lightness, high, cos, sin) {
        for _ in 0..16 {
            let middle = (low + high) * 0.5;
            if fits_in_srgb(lightness, middle, cos, sin) {
                low = middle;
            } else {
                high = middle;
            }
        }
    } else {
        low = high;
    }

    let (r, g, b) = oklab_to_linear_srgb(lightness, low * cos, low * sin);
    Color32::from_rgb(linear_to_byte(r), linear_to_byte(g), linear_to_byte(b))
}

/// この明度・クロマ・色相が sRGB の範囲に収まるか
fn fits_in_srgb(lightness: f32, chroma: f32, cos: f32, sin: f32) -> bool {
    let (r, g, b) = oklab_to_linear_srgb(lightness, chroma * cos, chroma * sin);
    // 丸めで境界を割る分の余裕を見る
    let inside = |x: f32| (-1e-4..=1.0 + 1e-4).contains(&x);
    inside(r) && inside(g) && inside(b)
}

/// Oklab から線形 sRGB へ (Björn Ottosson の定義そのまま)
fn oklab_to_linear_srgb(lightness: f32, a: f32, b: f32) -> (f32, f32, f32) {
    let l_ = lightness + 0.396_337_78 * a + 0.215_803_76 * b;
    let m_ = lightness - 0.105_561_346 * a - 0.063_854_17 * b;
    let s_ = lightness - 0.089_484_18 * a - 1.291_485_5 * b;

    let l = l_ * l_ * l_;
    let m = m_ * m_ * m_;
    let s = s_ * s_ * s_;

    (
        4.076_741_7 * l - 3.307_711_6 * m + 0.230_969_94 * s,
        -1.268_438 * l + 2.609_757_4 * m - 0.341_319_38 * s,
        -0.004_196_086_3 * l - 0.703_418_6 * m + 1.707_614_7 * s,
    )
}

/// 線形値に sRGB のガンマをかけてバイトにする
fn linear_to_byte(value: f32) -> u8 {
    let value = value.clamp(0.0, 1.0);
    let encoded = if value <= 0.003_130_8 {
        value * 12.92
    } else {
        1.055 * value.powf(1.0 / 2.4) - 0.055
    };
    (encoded * 255.0).round().clamp(0.0, 255.0) as u8
}

#[cfg(test)]
mod tests {
    use super::super::metrics::{MAX_OCTAVE, MIN_OCTAVE};
    use super::*;
    use crate::theme::palette;

    fn pitched(semitone: i32, octave: i32) -> Note {
        Note {
            start_tick: 0.0,
            duration: 1.0,
            semitone,
            octave,
            velocity: 100,
            velocity_to: 100,
            track: 0,
            lane: 0,
        }
    }

    /// いちばん低い音は寒色、いちばん高い音は赤い
    #[test]
    fn note_fill_runs_from_blue_to_red() {
        let scale = ScaleMode::Equal12;
        let lowest = note_fill(&pitched(0, MIN_OCTAVE), scale);
        let highest = note_fill(&pitched(11, MAX_OCTAVE), scale);

        assert!(
            lowest.b() > lowest.r(),
            "最低音は赤より青が強いこと: {lowest:?}"
        );
        assert!(
            highest.r() > highest.b(),
            "最高音は青より赤が強いこと: {highest:?}"
        );
    }

    /// 色を割り当てた音域の外は、端の色で頭打ちになること。
    ///
    /// 上端に届くのは「最上位オクターブの次の頭」= `NOTE_COLOR_MAX_OCTAVE + 1` の
    /// 半音0。最上位オクターブの最上位半音はまだ1半音ぶん手前にある。
    #[test]
    fn note_fill_clamps_outside_the_colored_range() {
        let scale = ScaleMode::Equal12;
        let bottom = note_fill(&pitched(0, NOTE_COLOR_MIN_OCTAVE), scale);
        let top = note_fill(&pitched(0, NOTE_COLOR_MAX_OCTAVE + 1), scale);

        for octave in MIN_OCTAVE..NOTE_COLOR_MIN_OCTAVE {
            for semitone in [0, 11] {
                assert_eq!(
                    note_fill(&pitched(semitone, octave), scale),
                    bottom,
                    "オクターブ {octave} 半音 {semitone} が下端の色で止まっていない"
                );
            }
        }
        for octave in (NOTE_COLOR_MAX_OCTAVE + 1)..=MAX_OCTAVE {
            for semitone in [0, 11] {
                assert_eq!(
                    note_fill(&pitched(semitone, octave), scale),
                    top,
                    "オクターブ {octave} 半音 {semitone} が上端の色で止まっていない"
                );
            }
        }
    }

    /// 音高が上がるほど暖色へ進み、途中で寒色へ戻らないこと。
    ///
    /// 色を割り当てた音域では、**どの2つのオクターブも**別の色になること。
    ///
    /// 隣同士だけ見ていると、色相環を一周して離れたオクターブが同じ色になる
    /// 事故を見逃す。総当たりで確かめる。
    ///
    /// なお「赤 − 青」のような一方向の指標では単調性を見られない。
    /// ティールから青へ向かう区間は**赤から遠ざかる**ためで、これは経路として
    /// 正しい (寒色→暖色は端どうしの関係であって、途中まで単調ではない)。
    #[test]
    fn every_octave_gets_its_own_color() {
        let scale = ScaleMode::Equal12;
        let color = |octave: i32| note_fill(&pitched(0, octave), scale);
        let distance = |a: Color32, b: Color32| {
            (a.r() as i32 - b.r() as i32).abs()
                + (a.g() as i32 - b.g() as i32).abs()
                + (a.b() as i32 - b.b() as i32).abs()
        };

        for low in NOTE_COLOR_MIN_OCTAVE..=NOTE_COLOR_MAX_OCTAVE {
            for high in (low + 1)..=NOTE_COLOR_MAX_OCTAVE {
                let gap = distance(color(low), color(high));
                assert!(
                    gap > 30,
                    "オクターブ {low} と {high} の色差が小さすぎる (差 {gap}): \
                     {:?} / {:?}",
                    color(low),
                    color(high)
                );
            }
        }
    }

    /// オクターブ境界で色が飛ばないこと。
    ///
    /// 半音でも連続的に変えているので、あるオクターブの最上位半音と
    /// 次のオクターブの半音0 は隣り合う色になる。
    #[test]
    fn note_fill_is_continuous_across_octaves() {
        let scale = ScaleMode::Equal12;
        let top = note_fill(&pitched(11, 4), scale);
        let next_bottom = note_fill(&pitched(0, 5), scale);

        let gap = (top.r() as i32 - next_bottom.r() as i32).abs()
            + (top.g() as i32 - next_bottom.g() as i32).abs()
            + (top.b() as i32 - next_bottom.b() as i32).abs();

        // 1半音ぶんの差しかないので、ごくわずかしか離れない
        assert!(gap < 24, "オクターブ境界で色が飛んでいる (差 {gap})");
        assert_ne!(top, next_bottom, "半音が変われば色も変わること");
    }

    /// sRGB の相対輝度 (WCAG の定義)。見た目の明るさの目安に使う。
    fn relative_luminance(color: Color32) -> f32 {
        let channel = |byte: u8| {
            let value = byte as f32 / 255.0;
            if value <= 0.040_45 {
                value / 12.92
            } else {
                ((value + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * channel(color.r()) + 0.7152 * channel(color.g()) + 0.0722 * channel(color.b())
    }

    /// どの音高でも、塗りの上に抜くラベルが読めること。
    ///
    /// 塗りが届いた部分のラベルは [`palette::BG`] で抜いている。
    /// [`NOTE_LIGHTNESS`] の下限はこの条件から決めた。Oklch の明度が一定でも
    /// 見た目の輝度は色相で変わるので、全音域を見る。
    #[test]
    fn every_note_keeps_its_label_readable() {
        let scale = ScaleMode::Equal12;
        let background = relative_luminance(palette::BG);

        for octave in MIN_OCTAVE..=MAX_OCTAVE {
            for semitone in 0..12 {
                let fill = note_fill(&pitched(semitone, octave), scale);
                let contrast = (relative_luminance(fill) + 0.05) / (background + 0.05);
                assert!(
                    contrast >= 4.5,
                    "オクターブ {octave} 半音 {semitone} でラベルのコントラストが不足: \
                     {contrast:.2}:1 ({fill:?})"
                );
            }
        }
    }

    /// どの音高でも sRGB からはみ出さないこと。
    ///
    /// はみ出したまま切り詰めると、色相と明度が指定からずれる。
    /// クロマを削って収める処理が効いているかを見る。
    #[test]
    fn note_colors_stay_inside_srgb() {
        let scale = ScaleMode::Equal12;
        for octave in MIN_OCTAVE..=MAX_OCTAVE {
            for semitone in 0..12 {
                let color = note_fill(&pitched(semitone, octave), scale);
                // 3チャンネルとも振り切れていたら、削り切れずに潰れている
                let clipped = [color.r(), color.g(), color.b()]
                    .iter()
                    .filter(|byte| **byte == 0 || **byte == 255)
                    .count();
                assert!(
                    clipped < 3,
                    "オクターブ {octave} 半音 {semitone} で色が潰れている: {color:?}"
                );
            }
        }
    }

    /// 音階モードでステップ数が変わっても、オクターブ内の相対位置で色が決まること
    #[test]
    fn note_fill_uses_scale_steps() {
        // 差が丸めに埋もれないよう、オクターブ内で高い位置を見る
        let eq = note_fill(&pitched(11, 4), ScaleMode::Equal12); // 11/12 ≈ 0.92
        let bp = note_fill(&pitched(11, 4), ScaleMode::BohlenPierce13); // 11/13 ≈ 0.85
        assert_ne!(eq, bp, "ステップ数の違いが色に反映されること");
    }
}
