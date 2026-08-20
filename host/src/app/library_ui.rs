//! プラグイン一覧の窓 (フォルダの管理・走査・タブ・お気に入り)。
//!
//! **走査は1フレームに1ファイルずつ進める。** 別スレッドにしないのは、
//! CLAP も VST3 もモジュールの読み込みがメインスレッドの規約に縛られているため。
//! VST3 は1本に1秒近くかかることがあるので、**今どれを開いているかを出して**
//! 固まったように見せない。

use super::notice::Notice;
use super::App;
use crate::library::{self, Entry, Role, Scan};
use crate::theme::palette;
use eframe::egui;
use std::path::PathBuf;

/// 一覧のどれを見ているか
#[derive(Clone, Copy, PartialEq, Eq, Default)]
pub(super) enum Tab {
    #[default]
    Instrument,
    Effect,
    Unknown,
    Favorite,
}

impl Tab {
    fn label(self) -> &'static str {
        match self {
            Tab::Instrument => Role::Instrument.label(),
            Tab::Effect => Role::Effect.label(),
            Tab::Unknown => Role::Unknown.label(),
            Tab::Favorite => "お気に入り",
        }
    }
}

/// 名前欄の幅。**固定にする** — 名前の長さで後ろのベンダー欄が動くと読みにくい
const NAME_W: f32 = 220.0;
const VENDOR_W: f32 = 150.0;

/// その1件をこのタブ・この絞り込みで出すか。
///
/// **絞り込みは名前とベンダーの両方に当てる。** ベンダー名しか覚えていない
/// ことがあるため。大文字小文字は区別しない (`filter` は小文字で渡すこと)。
fn shows(entry: &Entry, tab: Tab, filter: &str) -> bool {
    let in_tab = match tab {
        Tab::Favorite => entry.favorite,
        Tab::Instrument => entry.role == Role::Instrument,
        Tab::Effect => entry.role == Role::Effect,
        Tab::Unknown => entry.role == Role::Unknown,
    };
    if !in_tab {
        return false;
    }
    filter.is_empty()
        || entry.label().to_lowercase().contains(filter)
        || entry.vendor.to_lowercase().contains(filter)
}

impl App {
    /// 起動時に一覧を読み込む。**前回落ちていたらここで拾う。**
    ///
    /// 記録がまだ無ければ、標準の置き場のうち実在するものを入れておく
    /// (空の窓を見せても、何をすればいいか分からない)。
    pub(super) fn load_library(&mut self) {
        let first_run = !library::config_path().exists();
        let (loaded, problem) = library::load();
        self.library = loaded;

        if let Some(problem) = problem {
            self.notice = Some(Notice::error("プラグイン一覧を読めません", problem));
        }
        if first_run {
            self.library.folders = library::existing_standard_folders();
        }

        // 走査の途中で落ちていれば、目印が残っている
        if let Some(crashed) = library::take_crashed() {
            let message = format!(
                "前回このファイルの読み込み中に終了しました。\n\n{}\n\n\
                 今後の走査では飛ばします (プラグインの窓から外せます)。",
                crashed.display()
            );
            if !self.library.blocked.contains(&crashed) {
                self.library.blocked.push(crashed);
            }
            let _ = library::save(&self.library);
            self.notice = Some(Notice::error("走査中に落ちたファイルがあります", message));
        }
    }

    /// 走査を1ファイル進める。**毎フレーム呼ぶ。**
    ///
    /// 終わったら記録を書いて、開けなかったものがあれば知らせる。
    pub(super) fn step_scan(&mut self) {
        let Some(scan) = self.scan.as_mut() else {
            return;
        };
        if let Some(path) = scan.step(&mut self.library) {
            self.scanning_now = Some(path);
        }
        if !scan.is_done() {
            return;
        }

        // ここまで来たら走査は終わり
        let problems = scan.problems().to_vec();
        self.scan = None;
        self.scanning_now = None;
        self.library.sort();

        let mut body = format!(
            "{} 個のプラグインが見つかりました。",
            self.library.plugins.len()
        );
        if let Err(e) = library::save(&self.library) {
            body.push_str(&format!("\n\n※ 記録を保存できませんでした:\n{e}"));
        }
        if problems.is_empty() {
            self.notice = Some(Notice::ok("走査が終わりました", body));
        } else {
            // **開けなかったものは必ず出す。** 黙って一覧から消えるのが困る
            body.push_str(&format!(
                "\n\n次の {} 件は開けませんでした。\n",
                problems.len()
            ));
            body.push_str(&problems.join("\n"));
            self.notice = Some(Notice::error("走査が終わりました (一部読めず)", body));
        }
    }

    /// プラグイン一覧の窓
    pub(super) fn library_window(&mut self, ctx: &egui::Context) {
        let mut open = self.show_library;
        // **初期位置は左上。** オーディオトラックの窓 (右上) と重ならない場所
        egui::Window::new("プラグイン")
            .default_width(620.0)
            .default_pos(egui::pos2(24.0, 60.0))
            .open(&mut open)
            .show(ctx, |ui| {
                self.library_picking(ui);
                self.library_folders(ui);
                ui.separator();
                self.library_progress(ui);
                self.library_tabs(ui);
                ui.separator();
                self.library_list(ui);
            });

        // **閉じたら段の指定も解く。** 載せる先を握ったまま窓だけ消えると、
        // 次に開いたときに身に覚えのない「載せます」が出る
        if !open {
            self.pending_load = None;
        }
        self.show_library = open;
    }

    /// 「この段に載せる」状態のときの見出し。
    ///
    /// **一覧に無いものはファイルから直接読める。** 走査で落ちるプラグインや、
    /// 登録していないフォルダのものを試したいことがあるので、この道は残す。
    fn library_picking(&mut self, ui: &mut egui::Ui) {
        let Some(addr) = self.pending_load else {
            return;
        };
        ui.horizontal(|ui| {
            ui.label(
                egui::RichText::new(format!(
                    "オーディオトラック {} の {} 段目に載せます",
                    addr.track,
                    addr.at + 1
                ))
                .color(palette::GREEN),
            );
            if ui
                .button("ファイルから直接…")
                .on_hover_text("一覧に無いプラグインを、ファイルを選んで読み込みます")
                .clicked()
            {
                self.direct_dialog = true;
            }
            if ui.button("やめる").clicked() {
                self.pending_load = None;
            }
        });
        ui.weak("下の一覧から選ぶと、その段に載ってこの窓は閉じます");
        ui.separator();
    }

    /// 走査するフォルダの管理
    fn library_folders(&mut self, ui: &mut egui::Ui) {
        let scanning = self.scan.is_some();
        let mut remove = None;

        ui.horizontal(|ui| {
            ui.label(
                egui::RichText::new("フォルダ")
                    .size(11.0)
                    .color(palette::FG_DIM),
            );
            if ui
                .add_enabled(!scanning, egui::Button::new("＋ 追加…"))
                .clicked()
            {
                if let Some(folder) = rfd::FileDialog::new().pick_folder() {
                    if !self.library.folders.contains(&folder) {
                        self.library.folders.push(folder);
                        let _ = library::save(&self.library);
                    }
                }
            }
            if ui
                .add_enabled(
                    !scanning && !self.library.folders.is_empty(),
                    egui::Button::new("すべて走査"),
                )
                .on_hover_text(
                    "登録したフォルダを全部走査し直します。\
                     プラグインの数によっては数分かかります",
                )
                .clicked()
            {
                self.scan = Some(Scan::start(&mut self.library));
            }
        });

        if self.library.folders.is_empty() {
            ui.weak("フォルダが登録されていません。「＋ 追加…」から足してください");
        }
        for (index, folder) in self.library.folders.iter().enumerate() {
            ui.horizontal(|ui| {
                // 無くなったフォルダは印を出す (外付けを抜いた等)
                let missing = !folder.is_dir();
                let text = egui::RichText::new(folder.display().to_string()).size(11.0);
                ui.label(if missing {
                    text.color(palette::RED)
                } else {
                    text
                })
                .on_hover_text(if missing {
                    "このフォルダは見つかりません"
                } else {
                    "走査するフォルダ"
                });
                if ui
                    .add_enabled(!scanning, egui::Button::new("外す").small())
                    .clicked()
                {
                    remove = Some(index);
                }
            });
        }

        if let Some(index) = remove {
            let folder = self.library.folders.remove(index);
            // そのフォルダから見つけた記録も一緒に片付ける
            self.library.forget_under(&folder);
            let _ = library::save(&self.library);
        }

        self.library_blocked(ui);
    }

    /// 走査中に落ちたファイル (あれば)
    fn library_blocked(&mut self, ui: &mut egui::Ui) {
        if self.library.blocked.is_empty() {
            return;
        }
        let mut unblock = None;
        ui.collapsing(
            format!("走査で飛ばすファイル ({} 件)", self.library.blocked.len()),
            |ui| {
                ui.weak("前回ここで終了したため、走査から外してあります");
                for (index, path) in self.library.blocked.iter().enumerate() {
                    ui.horizontal(|ui| {
                        ui.label(egui::RichText::new(path.display().to_string()).size(11.0));
                        if ui
                            .button("試す")
                            .on_hover_text("次の走査で開き直します")
                            .clicked()
                        {
                            unblock = Some(index);
                        }
                    });
                }
            },
        );
        if let Some(index) = unblock {
            self.library.blocked.remove(index);
            let _ = library::save(&self.library);
        }
    }

    /// 走査中の進捗
    fn library_progress(&mut self, ui: &mut egui::Ui) {
        let Some(scan) = self.scan.as_ref() else {
            return;
        };
        let (done, total) = scan.progress();
        let ratio = if total == 0 {
            0.0
        } else {
            done as f32 / total as f32
        };

        ui.horizontal(|ui| {
            ui.add(
                egui::ProgressBar::new(ratio)
                    .desired_width(320.0)
                    .text(format!("{done} / {total}")),
            );
            if ui.button("やめる").clicked() {
                self.scan = None;
                self.scanning_now = None;
                let _ = library::save(&self.library);
            }
        });
        // **今どれを開いているかを出す。** VST3 は1本に1秒近くかかるので、
        // 出さないと固まったように見える
        if let Some(path) = &self.scanning_now {
            ui.label(
                egui::RichText::new(path.display().to_string())
                    .size(10.0)
                    .color(palette::FG_DIM),
            );
        }
        ui.separator();
    }

    /// タブと絞り込み
    fn library_tabs(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            for tab in [Tab::Instrument, Tab::Effect, Tab::Unknown, Tab::Favorite] {
                let count = self.count_of(tab);
                let selected = self.library_tab == tab;
                if ui
                    .selectable_label(selected, format!("{} ({count})", tab.label()))
                    .clicked()
                {
                    self.library_tab = tab;
                }
            }
            ui.separator();
            ui.label(
                egui::RichText::new("絞り込み")
                    .size(11.0)
                    .color(palette::FG_DIM),
            );
            ui.add(
                egui::TextEdit::singleline(&mut self.library_filter)
                    .desired_width(140.0)
                    .hint_text("名前 / ベンダー"),
            );
            if !self.library_filter.is_empty() && ui.small_button("×").clicked() {
                self.library_filter.clear();
            }
        });
    }

    fn count_of(&self, tab: Tab) -> usize {
        match tab {
            Tab::Favorite => self.library.favorites().count(),
            Tab::Instrument => self.library.by_role(Role::Instrument).count(),
            Tab::Effect => self.library.by_role(Role::Effect).count(),
            Tab::Unknown => self.library.by_role(Role::Unknown).count(),
        }
    }

    /// 選んでいるタブの中身
    fn library_list(&mut self, ui: &mut egui::Ui) {
        let tab = self.library_tab;
        let filter = self.library_filter.to_lowercase();

        let shown: Vec<(PathBuf, String)> = self
            .library
            .plugins
            .iter()
            .filter(|entry| shows(entry, tab, &filter))
            .map(|entry| (entry.path.clone(), entry.id.clone()))
            .collect();

        if shown.is_empty() {
            ui.weak(if self.library.plugins.is_empty() {
                "まだ走査していません。フォルダを足して「すべて走査」を押してください"
            } else {
                "この条件に合うプラグインはありません"
            });
            return;
        }

        let picking = self.pending_load.is_some();
        let mut toggle = None;
        let mut load = None;
        egui::ScrollArea::vertical()
            .max_height(320.0)
            .show(ui, |ui| {
                for (path, id) in &shown {
                    let Some(entry) = self.library.plugins.iter().find(|entry| entry.is(path, id))
                    else {
                        continue;
                    };
                    ui.horizontal(|ui| {
                        // 星。**押しやすいよう先頭に置く**
                        let star = if entry.favorite { "★" } else { "☆" };
                        let color = if entry.favorite {
                            palette::YELLOW
                        } else {
                            palette::FG_DIM
                        };
                        if ui
                            .add(egui::Button::new(egui::RichText::new(star).color(color)))
                            .on_hover_text("お気に入り")
                            .clicked()
                        {
                            toggle = Some((path.clone(), id.clone()));
                        }

                        // **段を選んでいる間だけ押せる。** そうでないときに
                        // ボタンに見えると、押しても何も起きなくて戸惑う
                        let where_it_is = format!("{}\n{}", entry.path.display(), entry.id);
                        if picking {
                            if ui
                                .add_sized(
                                    [NAME_W, 20.0],
                                    egui::Button::new(entry.label()).truncate(),
                                )
                                .on_hover_text(format!("この段に載せる\n\n{where_it_is}"))
                                .clicked()
                            {
                                load = Some((path.clone(), id.clone()));
                            }
                        } else {
                            ui.add_sized(
                                [NAME_W, 20.0],
                                egui::Label::new(entry.label())
                                    .truncate()
                                    .halign(egui::Align::LEFT),
                            )
                            .on_hover_text(where_it_is);
                        }

                        ui.add_sized(
                            [VENDOR_W, 20.0],
                            egui::Label::new(egui::RichText::new(&entry.vendor).size(11.0).weak())
                                .truncate()
                                .halign(egui::Align::LEFT),
                        );

                        ui.label(
                            egui::RichText::new(format!("{:?}", entry.kind))
                                .size(10.0)
                                .color(palette::FG_DIM),
                        );
                        if !entry.version.is_empty() {
                            ui.label(
                                egui::RichText::new(&entry.version)
                                    .size(10.0)
                                    .color(palette::FG_DIM),
                            );
                        }
                    });
                }
            });

        if let Some((path, id)) = toggle {
            self.library.toggle_favorite(&path, &id);
            let _ = library::save(&self.library);
        }
        if let Some((path, id)) = load {
            self.load_from_library(&path, &id);
        }
    }

    /// 一覧の1件を、待っている段へ載せる。
    ///
    /// 一覧が持っているのは (形式, パス, ID) の3つで、これは
    /// [`load_plugin`](App::load_plugin) がそのまま受け取れる形。
    /// **載せる経路自体は今までと同じ**で、選び方だけが変わっている。
    fn load_from_library(&mut self, path: &std::path::Path, id: &str) {
        let Some(addr) = self.pending_load else {
            return;
        };
        let Some(entry) = self.library.plugins.iter().find(|entry| entry.is(path, id)) else {
            return;
        };
        let (kind, path, name) = (entry.kind, entry.path.clone(), entry.label().to_string());

        match self.load_plugin(addr, kind, &path, id, None) {
            Ok(_) => {
                self.pending_load = None;
                self.error = None;
                // **載せたら閉じる。** 開いたままだと、載ったのかどうかが
                // 画面から読み取れない。次の段は「＋」から入り直す
                self.show_library = false;
            }
            // **窓は開けたままにする。** 別のものを選び直せるように
            Err(e) => {
                self.notice = Some(Notice::error(
                    "音源を読み込めません",
                    format!("{name}\n{}\n\n{e}", path.display()),
                ))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project::PluginKind;

    fn entry(name: &str, vendor: &str, role: Role, favorite: bool) -> Entry {
        Entry {
            kind: PluginKind::Clap,
            path: PathBuf::from(format!("C:\\plugins\\{name}.clap")),
            id: format!("com.example.{name}"),
            name: name.to_string(),
            vendor: vendor.to_string(),
            version: "1.0".to_string(),
            role,
            favorite,
        }
    }

    /// タブごとに出るものが分かれること。
    /// **未分類は音源にもエフェクトにも混ざらない**
    #[test]
    fn each_tab_shows_only_its_own() {
        let instrument = entry("Synth", "Vendor", Role::Instrument, false);
        let effect = entry("Reverb", "Vendor", Role::Effect, false);
        let unknown = entry("Mystery", "Vendor", Role::Unknown, false);

        assert!(shows(&instrument, Tab::Instrument, ""));
        assert!(!shows(&instrument, Tab::Effect, ""));
        assert!(!shows(&instrument, Tab::Unknown, ""));

        assert!(shows(&effect, Tab::Effect, ""));
        assert!(!shows(&effect, Tab::Instrument, ""));

        assert!(shows(&unknown, Tab::Unknown, ""));
        assert!(!shows(&unknown, Tab::Instrument, ""));
        assert!(!shows(&unknown, Tab::Effect, ""));
    }

    /// お気に入りは種別を問わずに出ること
    #[test]
    fn the_favorite_tab_ignores_the_role() {
        for role in [Role::Instrument, Role::Effect, Role::Unknown] {
            let starred = entry("Starred", "Vendor", role, true);
            let plain = entry("Plain", "Vendor", role, false);
            assert!(shows(&starred, Tab::Favorite, ""), "{role:?}");
            assert!(!shows(&plain, Tab::Favorite, ""), "{role:?}");
        }
    }

    /// **絞り込みは名前とベンダーの両方に当たること。**
    /// ベンダー名しか覚えていないことがある
    #[test]
    fn the_filter_matches_the_name_or_the_vendor() {
        let found = entry("Surge XT", "Surge Synth Team", Role::Instrument, false);

        assert!(shows(&found, Tab::Instrument, "surge"), "名前に当たる");
        assert!(
            shows(&found, Tab::Instrument, "synth team"),
            "ベンダーに当たる"
        );
        assert!(shows(&found, Tab::Instrument, "xt"), "途中でも当たる");
        assert!(!shows(&found, Tab::Instrument, "reverb"));
    }

    /// 絞り込みが空なら全部出ること
    #[test]
    fn an_empty_filter_shows_everything_in_the_tab() {
        let found = entry("Synth", "Vendor", Role::Instrument, false);
        assert!(shows(&found, Tab::Instrument, ""));
    }

    /// **絞り込みはタブより後に効くこと。**
    /// 名前が合っていても、別のタブなら出さない
    #[test]
    fn the_filter_never_pulls_in_another_tab() {
        let effect = entry("Surge XT", "Surge Synth Team", Role::Effect, false);
        assert!(!shows(&effect, Tab::Instrument, "surge"));
        assert!(shows(&effect, Tab::Effect, "surge"));
    }

    /// 名前が空でも ID で当たること (一覧に出る文字列と同じもので絞る)
    #[test]
    fn a_nameless_plugin_is_filtered_by_its_id() {
        let mut nameless = entry("x", "Vendor", Role::Instrument, false);
        nameless.name = String::new();
        // label() は ID (com.example.x) を返す
        assert!(shows(&nameless, Tab::Instrument, "com.example"));
    }
}
