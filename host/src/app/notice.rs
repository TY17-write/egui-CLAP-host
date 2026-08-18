//! 処理結果の通知ウィンドウ。

use crate::theme;
use eframe::egui;

/// 処理結果の通知。
///
/// ヘッダーの小さな文字だと、押したボタンから遠いうえに消えないので見落とす。
/// 閉じるまで画面中央に残す。
pub(super) struct Notice {
    title: String,
    body: String,
    is_error: bool,
}

impl Notice {
    pub(super) fn ok(title: impl Into<String>, body: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            body: body.into(),
            is_error: false,
        }
    }

    pub(super) fn error(title: impl Into<String>, body: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            body: body.into(),
            is_error: true,
        }
    }
}

/// 結果通知のウィンドウ。OK / Enter / Esc で閉じる。
pub(super) fn notice_window(ctx: &egui::Context, notice: &mut Option<Notice>) {
    let Some(current) = notice.as_ref() else {
        return;
    };

    let mut dismiss = false;
    egui::Window::new(&current.title)
        // タイトルが変わっても位置がリセットされないよう ID は固定する
        .id(egui::Id::new("notice"))
        .collapsible(false)
        .resizable(false)
        .order(egui::Order::Foreground)
        .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
        .show(ctx, |ui| {
            ui.set_max_width(440.0);
            if current.is_error {
                ui.colored_label(theme::palette::RED, &current.body);
            } else {
                ui.label(&current.body);
            }
            ui.add_space(10.0);
            ui.separator();
            ui.horizontal(|ui| {
                if ui.button("OK").clicked() {
                    dismiss = true;
                }
                ui.weak("(Enter / Esc でも閉じます)");
            });
        });

    if dismiss || ctx.input(|i| i.key_pressed(egui::Key::Enter) || i.key_pressed(egui::Key::Escape))
    {
        *notice = None;
    }
}
