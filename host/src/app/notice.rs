//! 処理結果の通知ウィンドウ。

use crate::theme;
use eframe::egui;
use std::path::{Path, PathBuf};

/// 処理結果の通知。
///
/// ヘッダーの小さな文字だと、押したボタンから遠いうえに消えないので見落とす。
/// 閉じるまで画面中央に残す。
pub(super) struct Notice {
    title: String,
    body: String,
    is_error: bool,
    /// 書き出したファイル。**あれば「フォルダを開く」を出す。**
    ///
    /// 書き出した直後は、そのファイルを他のアプリで開いたり移したりすることが
    /// 多い。パスを読んで自分で辿るのは手間なので、その場から開けるようにする。
    reveal: Option<PathBuf>,
}

impl Notice {
    pub(super) fn ok(title: impl Into<String>, body: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            body: body.into(),
            is_error: false,
            reveal: None,
        }
    }

    pub(super) fn error(title: impl Into<String>, body: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            body: body.into(),
            is_error: true,
            reveal: None,
        }
    }

    /// 書き出したファイルを添える (「フォルダを開く」が出る)
    pub(super) fn with_path(mut self, path: impl Into<PathBuf>) -> Self {
        self.reveal = Some(path.into());
        self
    }
}

/// 結果通知のウィンドウ。OK / Enter / Esc で閉じる。
pub(super) fn notice_window(ctx: &egui::Context, notice: &mut Option<Notice>) {
    let Some(current) = notice.as_ref() else {
        return;
    };

    let mut dismiss = false;
    let mut reveal = None;
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
                // **書き出したファイルがあれば、その場から開ける。**
                // 出したものを他のアプリへ渡す流れが続くことが多い
                if let Some(path) = &current.reveal {
                    if ui
                        .button("フォルダを開く")
                        .on_hover_text(path.display().to_string())
                        .clicked()
                    {
                        reveal = Some(path.clone());
                    }
                }
                ui.weak("(Enter / Esc でも閉じます)");
            });
        });

    if let Some(path) = reveal {
        reveal_in_file_manager(&path);
    }
    if dismiss || ctx.input(|i| i.key_pressed(egui::Key::Enter) || i.key_pressed(egui::Key::Escape))
    {
        *notice = None;
    }
}

/// ファイルマネージャでそのファイルを見せる。
///
/// **失敗しても黙って諦める。** 開けなかったからといって、書き出し自体は
/// 成功している。ここでもう1枚エラーを出すほうが邪魔になる。
#[cfg(windows)]
fn reveal_in_file_manager(path: &Path) {
    use std::os::windows::process::CommandExt;

    // **`raw_arg` で渡す。** explorer は独自に命令行を解釈するので、
    // 空白を含むパスは `/select,"..."` の形でそのまま渡す必要がある
    // (`arg` だと Rust が引用符を付け直して届かない)。
    //
    // なお explorer は成功しても 0 以外を返すことがあるので、結果は見ない。
    let _ = std::process::Command::new("explorer.exe")
        .raw_arg(format!("/select,\"{}\"", path.display()))
        .spawn();
}

/// Windows 以外での「フォルダを開く」。
///
/// **ファイルを選択状態にはできない。** 入っているフォルダを開くだけ
/// (`xdg-open` にファイルを渡すと、そのファイルを関連付けで開いてしまう)。
///
/// このクレートは今のところ Windows でしかビルドが通らない
/// (クリップボードの判定に `windows-sys` を直接使っている) が、
/// 動かす当てが出たときに探し回らずに済むよう置いてある。
#[cfg(not(windows))]
fn reveal_in_file_manager(path: &Path) {
    let Some(folder) = path.parent() else {
        return;
    };
    let _ = std::process::Command::new("xdg-open").arg(folder).spawn();
}
