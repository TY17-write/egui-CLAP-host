//! vim-hybrid (w0ng/vim-hybrid) 風のダークテーマ。

use eframe::egui::{self, CornerRadius, Stroke};

/// vim-hybrid のカラーパレット
pub mod palette {
    use eframe::egui::Color32;

    /// 基本の背景 (#1d1f21)
    pub const BG: Color32 = Color32::from_rgb(0x1d, 0x1f, 0x21);
    /// 一段暗い背景 (#191b1d) — グリッドの溝など
    pub const BG_DARK: Color32 = Color32::from_rgb(0x19, 0x1b, 0x1d);
    /// パネルなど一段明るい背景 (#282a2e)
    pub const BG_LIGHT: Color32 = Color32::from_rgb(0x28, 0x2a, 0x2e);
    /// カーソル行・選択 (#373b41)
    pub const BG_SELECT: Color32 = Color32::from_rgb(0x37, 0x3b, 0x41);
    /// ホバー時 (#4a5057)
    pub const BG_HOVER: Color32 = Color32::from_rgb(0x4a, 0x50, 0x57);

    /// 標準の文字色 (#c5c8c6)
    pub const FG: Color32 = Color32::from_rgb(0xc5, 0xc8, 0xc6);
    /// コメント等の淡い文字 (#707880)
    pub const FG_DIM: Color32 = Color32::from_rgb(0x70, 0x78, 0x80);

    pub const RED: Color32 = Color32::from_rgb(0xcc, 0x66, 0x66);
    pub const GREEN: Color32 = Color32::from_rgb(0xb5, 0xbd, 0x68);
    pub const YELLOW: Color32 = Color32::from_rgb(0xf0, 0xc6, 0x74);
    pub const BLUE: Color32 = Color32::from_rgb(0x81, 0xa2, 0xbe);
    pub const PURPLE: Color32 = Color32::from_rgb(0xb2, 0x94, 0xbb);
    pub const CYAN: Color32 = Color32::from_rgb(0x8a, 0xbe, 0xb7);
}

use palette::*;

/// vim-hybrid 風テーマを適用する。
///
/// egui はライト/ダークそれぞれに Visuals を持ち、既定ではシステム設定に追従する。
/// `set_visuals` は「現在のテーマ」にしか効かないため、
/// ダークに固定したうえでダーク用の Visuals を差し替える。
pub fn apply(ctx: &egui::Context) {
    ctx.set_theme(egui::ThemePreference::Dark);

    let mut visuals = egui::Visuals::dark();

    visuals.panel_fill = BG;
    visuals.window_fill = BG;
    visuals.extreme_bg_color = BG_DARK;
    visuals.faint_bg_color = BG_LIGHT;
    visuals.code_bg_color = BG_LIGHT;

    visuals.override_text_color = Some(FG);
    visuals.hyperlink_color = BLUE;
    visuals.warn_fg_color = YELLOW;
    visuals.error_fg_color = RED;

    visuals.selection.bg_fill = BG_SELECT;
    visuals.selection.stroke = Stroke::new(1.0_f32, CYAN);

    visuals.window_stroke = Stroke::new(1.0_f32, BG_SELECT);
    visuals.window_corner_radius = CornerRadius::same(4);

    let widgets = &mut visuals.widgets;

    // 非対話 (ラベル・区切り線など)
    widgets.noninteractive.bg_fill = BG;
    widgets.noninteractive.weak_bg_fill = BG;
    widgets.noninteractive.bg_stroke = Stroke::new(1.0_f32, BG_SELECT);
    widgets.noninteractive.fg_stroke = Stroke::new(1.0_f32, FG_DIM);

    // 通常状態のボタン等
    widgets.inactive.bg_fill = BG_LIGHT;
    widgets.inactive.weak_bg_fill = BG_LIGHT;
    widgets.inactive.bg_stroke = Stroke::new(1.0_f32, BG_SELECT);
    widgets.inactive.fg_stroke = Stroke::new(1.0_f32, FG);

    // ホバー
    widgets.hovered.bg_fill = BG_SELECT;
    widgets.hovered.weak_bg_fill = BG_SELECT;
    widgets.hovered.bg_stroke = Stroke::new(1.0_f32, BG_HOVER);
    widgets.hovered.fg_stroke = Stroke::new(1.5_f32, CYAN);

    // 押下・選択中
    widgets.active.bg_fill = BG_HOVER;
    widgets.active.weak_bg_fill = BG_HOVER;
    widgets.active.bg_stroke = Stroke::new(1.0_f32, CYAN);
    widgets.active.fg_stroke = Stroke::new(2.0_f32, FG);

    // 開いているコンボボックス等
    widgets.open.bg_fill = BG_LIGHT;
    widgets.open.weak_bg_fill = BG_LIGHT;
    widgets.open.bg_stroke = Stroke::new(1.0_f32, BG_HOVER);
    widgets.open.fg_stroke = Stroke::new(1.0_f32, FG);

    for widget in [
        &mut widgets.noninteractive,
        &mut widgets.inactive,
        &mut widgets.hovered,
        &mut widgets.active,
        &mut widgets.open,
    ] {
        widget.corner_radius = CornerRadius::same(3);
    }

    ctx.set_visuals_of(egui::Theme::Dark, visuals);
}
