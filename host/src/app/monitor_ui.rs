//! 上部パネルのマスターメーター (スペクトルとラウドネス)。
//!
//! **常に見えている場所に置く。** オーディオトラックの窓と違って閉じられない
//! ので、高さは1行に収まるところまでに抑えてある。

use super::App;
use crate::meter::{spectrum, REFERENCE_LUFS, SILENCE_LUFS};
use crate::theme::palette;
use eframe::egui::{self, vec2, CornerRadius, Pos2, Rect, Sense, Stroke};

/// スペクトルの表示領域
const SPECTRUM_W: f32 = 240.0;
const SPECTRUM_H: f32 = 34.0;

/// ラウドネスの目盛りの幅
const LUFS_BAR_W: f32 = 150.0;
const LUFS_BAR_H: f32 = 12.0;

/// 目盛りに出す範囲 (LUFS)。基準の -14 を挟んで上下に取る
const SCALE_TOP: f32 = -4.0;
const SCALE_BOTTOM: f32 = -34.0;

/// 基準からこれだけ離れるまでは「合っている」として緑にする
const TOLERANCE_LU: f32 = 1.0;

impl App {
    /// マスターのメーター一式。上部パネルの中から呼ぶ
    pub(super) fn master_meters(&mut self, ui: &mut egui::Ui) {
        // エンジンが無いときは何も測れない。**枠は出しておく**
        // (急に現れたり消えたりすると、上部パネルの高さが跳ねる)
        let running = self.engine.is_some();

        spectrum_view(ui, self.meters.spectrum_levels(), running);

        ui.separator();

        let momentary = self.meters.momentary_lufs();
        let short_term = self.meters.short_term_lufs();
        let integrated = self.meters.integrated_lufs();

        ui.vertical(|ui| {
            ui.spacing_mut().item_spacing.y = 2.0;
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new("M").size(10.0).color(palette::FG_DIM))
                    .on_hover_text("Momentary (直近 400ms)");
                ui.label(reading(momentary, running));
                ui.label(egui::RichText::new("S").size(10.0).color(palette::FG_DIM))
                    .on_hover_text("Short-term (直近3秒)");
                ui.label(reading(short_term, running));
                // **Integrated が最後に読む値。** 少し強めに出す
                ui.label(egui::RichText::new("I").size(10.0).color(palette::FG))
                    .on_hover_text(
                        "Integrated (頭からの積算)。\
                         再生を始めたときとループの折り返しで測り直します",
                    );
                ui.label(reading(integrated, running).strong());
                ui.label(
                    egui::RichText::new(format!("基準 {REFERENCE_LUFS:.0}"))
                        .size(10.0)
                        .color(palette::FG_DIM),
                )
                .on_hover_text("配信で事実上の基準になっている値");
            });
            // 棒は Short-term で振り、Integrated は印で重ねる。
            // Momentary は揺れが速すぎて目盛りとしては読めない
            lufs_bar(ui, short_term, integrated, running);
        });
    }
}

/// 読み値の文字。**基準からのずれで色を変える**
fn reading(lufs: f32, running: bool) -> egui::RichText {
    if !running || lufs <= SILENCE_LUFS {
        return egui::RichText::new("  -∞  ")
            .monospace()
            .color(palette::FG_DIM);
    }
    let text = egui::RichText::new(format!("{lufs:+6.1}")).monospace();
    let off = lufs - REFERENCE_LUFS;
    if off > TOLERANCE_LU {
        // 基準より大きい = 配信側で下げられる側。目立たせる
        text.color(palette::RED)
    } else if off < -TOLERANCE_LU {
        text.color(palette::CYAN)
    } else {
        text.color(palette::GREEN)
    }
}

/// ラウドネスの横棒。
///
/// 塗りが `lufs` (Short-term)、白い線が基準、上下に出た三角が `integrated`。
/// **積算だけ形を変える** — 揺れる塗りの上に重なるので、色だけだと見失う。
fn lufs_bar(ui: &mut egui::Ui, lufs: f32, integrated: f32, running: bool) {
    let (rect, _) = ui.allocate_exact_size(vec2(LUFS_BAR_W, LUFS_BAR_H), Sense::hover());
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, CornerRadius::same(2), palette::BG_DARK);

    let to_x = |value: f32| {
        let t = ((value - SCALE_BOTTOM) / (SCALE_TOP - SCALE_BOTTOM)).clamp(0.0, 1.0);
        rect.left() + rect.width() * t
    };

    if running && lufs > SILENCE_LUFS {
        let filled = Rect::from_min_max(rect.left_top(), Pos2::new(to_x(lufs), rect.bottom()));
        let color = if lufs - REFERENCE_LUFS > TOLERANCE_LU {
            palette::RED
        } else if REFERENCE_LUFS - lufs > TOLERANCE_LU {
            palette::CYAN
        } else {
            palette::GREEN
        };
        painter.rect_filled(filled, CornerRadius::same(2), color.gamma_multiply(0.8));
    }

    // 基準の線。ここに合わせるための目印なので、棒より上に描く
    let x = to_x(REFERENCE_LUFS);
    painter.line_segment(
        [Pos2::new(x, rect.top()), Pos2::new(x, rect.bottom())],
        Stroke::new(1.5_f32, palette::FG),
    );

    // Integrated の印 (太い縦棒)。塗りの色と重なっても分かるよう、
    // 基準の線とは違う色にする
    if running && integrated > SILENCE_LUFS {
        let x = to_x(integrated);
        painter.rect_filled(
            Rect::from_min_max(
                Pos2::new(x - 1.5, rect.top() - 2.0),
                Pos2::new(x + 1.5, rect.bottom() + 2.0),
            ),
            CornerRadius::same(1),
            palette::YELLOW,
        );
    }
}

/// スペクトル。左が低域で、縦は dB
fn spectrum_view(ui: &mut egui::Ui, levels: &[f32; spectrum::BANDS], running: bool) {
    let (rect, response) = ui.allocate_exact_size(vec2(SPECTRUM_W, SPECTRUM_H), Sense::hover());
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, CornerRadius::same(2), palette::BG_DARK);

    if running {
        let width = rect.width() / spectrum::BANDS as f32;
        for (band, level) in levels.iter().enumerate() {
            let t = ((level - spectrum::FLOOR_DB) / (spectrum::CEIL_DB - spectrum::FLOOR_DB))
                .clamp(0.0, 1.0);
            if t <= 0.0 {
                continue;
            }
            let x = rect.left() + width * band as f32;
            let bar = Rect::from_min_max(
                Pos2::new(x, rect.bottom() - rect.height() * t),
                Pos2::new(x + width - 1.0, rect.bottom()),
            );
            // 低域から高域へ、ノートの色と同じ寒色→暖色の並びにする
            let hue = band as f32 / spectrum::BANDS as f32;
            let color = palette::CYAN.lerp_to_gamma(palette::YELLOW, hue);
            painter.rect_filled(bar, CornerRadius::ZERO, color);
        }
    }

    // 1kHz の位置に目印を1本。**絵のどこが何 Hz かの手がかりが要る**
    let mark = (0..spectrum::BANDS)
        .find(|band| spectrum::Spectrum::center_hz(*band) >= 1000.0)
        .unwrap_or(0);
    let x = rect.left() + rect.width() * mark as f32 / spectrum::BANDS as f32;
    painter.line_segment(
        [Pos2::new(x, rect.top()), Pos2::new(x, rect.bottom())],
        Stroke::new(1.0_f32, palette::FG_DIM.gamma_multiply(0.5)),
    );

    response.on_hover_text("マスターのスペクトル (左が低域。縦線は 1kHz)");
}
