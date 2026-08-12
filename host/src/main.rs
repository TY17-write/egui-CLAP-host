//! CLAP ミニホスト: .clap ファイルをロードして egui から鳴らすテスト用ホスト。

// GUI アプリなので、起動時に黒いコンソールを出さない (Windows のみ効く)。
// 引き換えに標準出力・標準エラーの行き先が無くなるため、cargo run で
// パニックのメッセージを見たいときは一時的に外すこと。
// smoke 系のバイナリはコンソールで使うものなので、こちらには付けない。
//#![windows_subsystem = "windows"]

use clap_host_test::{
    audio, ccs, discovery, editor_ui, gui, host, midi, opus, params, project, sequencer, theme,
    wav,
};

use audio::config::StreamAudioConfig;
use audio::offline::{RenderSetup, TAIL_SECONDS};
use audio::transport::{TransportMsg, TransportShared};
use audio::vst3::SharedPlugin;
use audio::GuiMsg;
use clack_host::prelude::*;
use crossbeam_channel::{Receiver, Sender};
use eframe::egui;
use editor_ui::{EditorCommand, EditorState};
use gui::{PluginGuiManager, Vst3GuiManager};
use host::{MainThreadMessage, MiniHost, MiniHostMainThread, MiniHostShared};
use params::ParamUi;
use project::PluginSnapshot;
use sequencer::SeqEvent;
use std::sync::atomic::Ordering;
use std::collections::HashSet;
use std::error::Error;
use std::ffi::CString;
use std::path::PathBuf;
use std::time::{Duration, Instant};

/// 鍵盤に表示する1オクターブ+1音 (C4〜C5) ※鍵盤 UI 無効化中のため未使用
#[allow(dead_code)]
const KEYS: [(u16, &str, bool); 13] = [
    (60, "C4", false),
    (61, "C#", true),
    (62, "D", false),
    (63, "D#", true),
    (64, "E", false),
    (65, "F", false),
    (66, "F#", true),
    (67, "G", false),
    (68, "G#", true),
    (69, "A", false),
    (70, "A#", true),
    (71, "B", false),
    (72, "C5", false),
];

fn main() -> eframe::Result {
    // 検証用 CLI: clap-host-test.exe [plugin.clap] [--open-gui]
    let args: Vec<String> = std::env::args().skip(1).collect();
    let auto_open_gui = args.iter().any(|a| a == "--open-gui");
    let autoload_path = args
        .iter()
        .find(|a| !a.starts_with("--"))
        .map(PathBuf::from);

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_inner_size([1100.0, 580.0]),
        ..Default::default()
    };
    eframe::run_native(
        "CLAP ミニホスト",
        options,
        Box::new(move |cc| {
            setup_japanese_fonts(&cc.egui_ctx);
            theme::apply(&cc.egui_ctx);
            Ok(Box::new(App {
                autoload: autoload_path.map(|p| (p, auto_open_gui)),
                ..Default::default()
            }))
        }),
    )
}

/// Windows のシステムフォントから日本語フォントを読み込む
fn setup_japanese_fonts(ctx: &egui::Context) {
    let candidates = [
        r"C:\Windows\Fonts\meiryo.ttc",
        r"C:\Windows\Fonts\YuGothM.ttc",
        r"C:\Windows\Fonts\msgothic.ttc",
    ];

    let mut fonts = egui::FontDefinitions::default();
    for path in candidates {
        let Ok(data) = std::fs::read(path) else {
            continue;
        };
        fonts
            .font_data
            .insert("japanese".into(), egui::FontData::from_owned(data).into());
        for family in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
            fonts
                .families
                .get_mut(&family)
                .unwrap()
                .push("japanese".into());
        }
        break;
    }
    ctx.set_fonts(fonts);
}

/// 中身を数え上げた音源ファイル (プラグイン選択待ち)。
/// .clap でも .vst3 でも同じ形になる。
struct Candidates {
    kind: project::PluginKind,
    path: PathBuf,
    plugins: Vec<discovery::FoundPlugin>,
    /// この候補から選んだプラグインを載せるトラック
    target_track: usize,
}

/// オーディオエンジン (出力ストリーム1本と、それを操作する口)。
/// トラックより長生きし、音源はメッセージで出し入れする。
struct Engine {
    _stream: cpal::Stream,
    producer: rtrb::Producer<GuiMsg>,
    /// オーディオスレッドから返ってきた音源 (メインスレッドで deactivate する)。
    /// どのインスタンスへ返すか分かるようトラック番号が付く。
    retired: rtrb::Consumer<(usize, Box<audio::TrackProcessor>)>,
    /// 再生位置・再生中フラグの共有
    transport_shared: TransportShared,
    /// 全プラグイン共通のストリーム構成
    config: StreamAudioConfig,
}

/// トラック1本にロードされた音源。
/// 形式に依らない情報だけをここに持ち、中身は [`TrackPlugin`] が形式ごとに抱える。
struct TrackAudio {
    name: String,
    /// この音源のファイルのパス。プロジェクトに保存して読み直すために持つ。
    path: PathBuf,
    /// CLAP のプラグイン ID / VST3 のクラス UID。
    /// 1つのファイルに複数入りうるので、パスだけでは足りない。
    id: String,
    #[allow(dead_code)] // 鍵盤 UI 無効化中
    pressed_keys: HashSet<u16>,
    plugin: TrackPlugin,
}

/// 音源のうち、メインスレッドに残る側。
///
/// CLAP と VST3 でここの形が大きく違う。CLAP はインスタンスがメインスレッドに
/// 残り、切り離した処理器だけがオーディオスレッドへ行く。VST3 は音源そのものを
/// 共有する (`audio::vst3::SharedPlugin` の説明を参照)。
enum TrackPlugin {
    Clap(ClapTrack),
    Vst3(Vst3Track),
}

struct ClapTrack {
    instance: PluginInstance<MiniHost>,
    receiver: Receiver<MainThreadMessage>,
    sender: Sender<MainThreadMessage>,
    #[allow(dead_code)] // パラメータ UI 無効化中
    params: Vec<ParamUi>,
    /// プラグイン独自 GUI の管理 (gui 拡張がない場合は None)
    gui: Option<PluginGuiManager>,
}

struct Vst3Track {
    /// オーディオスレッドの処理器と同じ音源を指す。
    /// 触るときは必ず `lock()` する (相手は1ブロックしか握らない)。
    plugin: SharedPlugin,
    /// ネイティブウィンドウからの通知 (閉じる・リサイズ)。
    /// CLAP と違い、プラグイン発の要求はこちらから取りに行く。
    receiver: Receiver<MainThreadMessage>,
    sender: Sender<MainThreadMessage>,
    gui: Vst3GuiManager,
}

impl TrackAudio {
    fn kind(&self) -> project::PluginKind {
        match self.plugin {
            TrackPlugin::Clap(_) => project::PluginKind::Clap,
            TrackPlugin::Vst3(_) => project::PluginKind::Vst3,
        }
    }

    /// 音源の今の状態を取り出す。取れない・失敗したときは空。
    ///
    /// 空でもパスと ID は保存するので、次に開いたとき音源自体は載る。
    fn capture_state(&mut self) -> Vec<u8> {
        match &mut self.plugin {
            TrackPlugin::Clap(clap) => {
                let Some(extension) = clap.instance.access_handler(|mt| mt.state.get()) else {
                    return Vec::new(); // state 拡張を持たない音源
                };
                let mut buffer = Vec::new();
                if extension
                    .save(&mut clap.instance.plugin_handle(), &mut buffer)
                    .is_err()
                {
                    buffer.clear();
                }
                buffer
            }
            TrackPlugin::Vst3(vst3) => vst3.plugin.lock().save_state().unwrap_or_default(),
        }
    }

    /// 保存しておいた状態を音源へ戻す。戻せたら true。
    ///
    /// 失敗しても音源自体は使えるので、呼び出し側は続行してよい
    /// (音作りだけ初期値になる)。
    fn restore_state(&mut self, state: &[u8]) -> bool {
        if state.is_empty() {
            return true;
        }
        match &mut self.plugin {
            TrackPlugin::Clap(clap) => {
                let Some(extension) = clap.instance.access_handler(|mt| mt.state.get()) else {
                    return false;
                };
                let mut reader = std::io::Cursor::new(state);
                extension
                    .load(&mut clap.instance.plugin_handle(), &mut reader)
                    .is_ok()
            }
            TrackPlugin::Vst3(vst3) => vst3.plugin.lock().load_state(state).is_ok(),
        }
    }
}

impl Drop for TrackAudio {
    fn drop(&mut self) {
        // 音源の破棄前にプラグイン GUI を確実に閉じる
        // (窓だけ残ると、貼り付いていた view の後始末が宙に浮く)
        match &mut self.plugin {
            TrackPlugin::Clap(clap) => {
                if let Some(gui) = &mut clap.gui {
                    gui.close(&mut clap.instance.plugin_handle());
                }
            }
            TrackPlugin::Vst3(vst3) => {
                let mut plugin = vst3.plugin.lock();
                vst3.gui.close(&mut plugin);
            }
        }
    }
}

/// 描画ループの中でファイルダイアログを開けないので、種類だけ持ち帰る
enum FileAction {
    ImportMidi,
    ExportMidi,
    OpenProject,
    SaveProject,
    SaveProjectAs,
    ExportWav,
    ExportOpus,
    ExportCcs,
}

/// 処理結果の通知。
///
/// ヘッダーの小さな文字だと、押したボタンから遠いうえに消えないので見落とす。
/// 閉じるまで画面中央に残す。
struct Notice {
    title: String,
    body: String,
    is_error: bool,
}

impl Notice {
    fn ok(title: impl Into<String>, body: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            body: body.into(),
            is_error: false,
        }
    }

    fn error(title: impl Into<String>, body: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            body: body.into(),
            is_error: true,
        }
    }
}

/// 結果通知のウィンドウ。OK / Enter / Esc で閉じる。
fn notice_window(ctx: &egui::Context, notice: &mut Option<Notice>) {
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

/// Windows で VST3 が置かれる標準の場所 (ダイアログの初期位置に使う)
const VST3_STANDARD_DIRECTORY: &str = r"C:\Program Files\Common Files\VST3";

/// 「♪」で聞く音源の選び方。
///
/// ダイアログの出し方が形式ごとに違うので、開く前に決めてもらう必要がある。
/// VST3 は**入れ物が2通りある**ため3択になる (詳細は [`App::open_vst3_dialog`])。
#[derive(Clone, Copy, PartialEq, Eq)]
enum LoadChoice {
    /// .clap ファイル
    Clap,
    /// バンドルディレクトリ形式の .vst3 (現行の標準)
    Vst3Bundle,
    /// 素の DLL 1ファイルの .vst3 (VST 3.6.10 以降は非推奨だが、まだ出回っている)
    Vst3File,
    /// 読み込みをやめる
    Cancel,
}

/// 表示用のファイル名 (取れなければパス全体)
fn file_label(path: &std::path::Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

#[derive(Default)]
struct App {
    /// 「♪」を押したあと、音源の形式を選んでもらっている最中のトラック。
    /// 形式ごとにダイアログの出し方が違うので、先に [`LoadChoice`] を決める必要がある。
    pending_load: Option<usize>,
    candidates: Option<Candidates>,
    // 宣言順にドロップされるため、ストリームをインスタンスより先に止める
    engine: Option<Engine>,
    /// トラックごとの音源 (エディタのトラック数に合わせて伸ばす)
    tracks: Vec<Option<TrackAudio>>,
    /// 差し替えで外したが、まだ処理器が返ってきていないインスタンス。
    /// 返却時に正しいインスタンスへ deactivate するため、ここで生かしておく。
    retiring: std::collections::VecDeque<(usize, TrackAudio)>,
    error: Option<String>,
    /// 起動時に自動ロードする .clap ファイル (パス, GUI も開くか)
    autoload: Option<(PathBuf, bool)>,
    /// シーケンスエディタの状態 (プラグインのロードをまたいで保持)
    editor: EditorState,
    /// 再生ヘッドの位置 (四分音符単位)。
    /// プラグイン未ロード時はここが本体で、ロード中はトランスポートの位置を写す。
    /// 再生できない状態でも、貼り付け位置などの編集操作に再生ヘッドを使うため。
    pos_quarters: f64,
    /// プロジェクトの保存先。Ctrl+S はここへ上書きする。
    /// MIDI のインポートでは設定しない (読み込んだファイルを上書きしないため)
    project_path: Option<PathBuf>,
    /// 最後に読み書きしたフォルダ (ダイアログの初期位置)
    last_directory: Option<PathBuf>,
    /// 保存・読み込みの結果メッセージ
    status: Option<String>,
    /// 画面中央に出す結果通知 (閉じるまで残る)
    notice: Option<Notice>,
}

impl App {
    /// .clap を選ばせて候補を読み込む。
    /// 戻り値は「候補を新しく読み込めたか」。キャンセルや失敗では false を返し、
    /// 前回の候補には触れない (キャンセルで前のプラグインが載らないようにするため)。
    fn open_clap_dialog(&mut self, target_track: usize) -> bool {
        let picked = rfd::FileDialog::new()
            .add_filter("CLAP プラグイン", &["clap"])
            .pick_file();

        // キャンセルされたら何もしない (前回の候補もそのまま残す)
        let Some(path) = picked else { return false };

        // entry はここでは持たない。clack が PluginInstance の中で
        // DLL を生かしておくので、載せるときに開き直せばよい。
        let found = discovery::load_clap_file(&path).map(|(_entry, plugins)| plugins);
        self.accept_candidates(project::PluginKind::Clap, path, target_track, found)
    }

    /// .vst3 を選ばせて候補を読み込む。
    ///
    /// **同じ拡張子で入れ物が2通りある**ので、どちらを選ぶかを呼び出し側が決める。
    ///
    /// | `bundle` | 対象 | ダイアログ |
    /// |---|---|---|
    /// | `true` | `Foo.vst3\Contents\x86_64-win\Foo.vst3` (現行の標準) | フォルダ選択 |
    /// | `false` | `Foo.vst3` 単体の DLL (VST 3.6.10 以降は非推奨) | ファイル選択 |
    ///
    /// Windows の共通ダイアログはフォルダとファイルを1つの選択で混ぜられないため、
    /// ここを1つにまとめることはできない。読み込む側 (`discovery::load_vst3_file` /
    /// `audio::activate_vst3_track`) はどちらのパスを渡しても通るので、
    /// **違いはダイアログの出し方だけ**に閉じている。
    ///
    /// なお、ファイル選択でもバンドルディレクトリには**入っていける**ので、
    /// 中の DLL を直接指してもよい (バンドルの場所を渡すのと同じ結果になる)。
    fn open_vst3_dialog(&mut self, target_track: usize, bundle: bool) -> bool {
        let mut dialog = rfd::FileDialog::new();
        // 標準の置き場から始める。無ければダイアログの既定に任せる
        let standard = std::path::Path::new(VST3_STANDARD_DIRECTORY);
        if standard.is_dir() {
            dialog = dialog.set_directory(standard);
        }

        let picked = if bundle {
            dialog.pick_folder()
        } else {
            dialog.add_filter("VST3 プラグイン", &["vst3"]).pick_file()
        };
        let Some(path) = picked else { return false };

        let found = discovery::load_vst3_file(&path);
        self.accept_candidates(project::PluginKind::Vst3, path, target_track, found)
    }

    /// どれか1つでも音源のエディタを開いているか。
    ///
    /// 開いている間は、その窓がこちらと同じスレッドのメッセージループに乗る。
    /// 再描画の間隔を決めるのに使う (`update` の末尾を参照)。
    fn any_editor_open(&self) -> bool {
        self.tracks
            .iter()
            .flatten()
            .any(|track| match &track.plugin {
                TrackPlugin::Clap(clap) => clap.gui.as_ref().is_some_and(|gui| gui.is_open),
                TrackPlugin::Vst3(vst3) => vst3.gui.is_open,
            })
    }

    /// 数え上げた結果を候補として受け取る (形式によらず共通)
    fn accept_candidates(
        &mut self,
        kind: project::PluginKind,
        path: PathBuf,
        target_track: usize,
        found: Result<Vec<discovery::FoundPlugin>, Box<dyn Error>>,
    ) -> bool {
        match found {
            Ok(plugins) => {
                self.error = None;
                self.candidates = Some(Candidates {
                    kind,
                    path,
                    plugins,
                    target_track,
                });
                true
            }
            Err(e) => {
                self.error = Some(format!("ロード失敗: {e}"));
                false
            }
        }
    }

    /// MIDI ファイルを選んで読み込む (今のシーケンスは置き換わる)
    fn import_midi(&mut self) {
        let mut dialog = rfd::FileDialog::new().add_filter("MIDI ファイル", &["mid", "midi"]);
        if let Some(directory) = self.dialog_directory() {
            dialog = dialog.set_directory(directory);
        }
        let Some(path) = dialog.pick_file() else { return };
        self.last_directory = path.parent().map(PathBuf::from);

        let bytes = match std::fs::read(&path) {
            Ok(bytes) => bytes,
            Err(e) => {
                self.notice = Some(Notice::error(
                    "MIDI を読み込めません",
                    format!("読み込めません:\n{e}"),
                ));
                return;
            }
        };

        match midi::from_bytes(&bytes, self.editor.editor.scale) {
            Ok(imported) => {
                let count = imported.notes.len();
                let cc_lanes = imported.lane_ccs.len();
                let name = file_label(&path);
                self.editor.replace_sequence(
                    imported.notes,
                    imported.tempo,
                    imported.time_signature,
                    &imported.lane_ccs,
                );
                // MIDI をプロジェクトの保存先にはしない
                // (Ctrl+S が .ron を書くので、読み込み元とは無関係にする)
                self.project_path = None;
                self.editor.project_path = None;
                self.error = None;
                self.status = Some(if cc_lanes > 0 {
                    format!("{name} から {count} 個のノートと {cc_lanes} 本の CC 段を読み込みました")
                } else {
                    format!("{name} から {count} 個のノートを読み込みました")
                });
            }
            Err(e) => {
                self.notice = Some(Notice::error(
                    "MIDI を読み込めません",
                    format!("{}\n\n{e}", file_label(&path)),
                ));
            }
        }
    }

    /// MIDI ファイルへ書き出す。スウィングが乗るので、編集の保存には使わない
    /// (それはプロジェクト形式の役目)。
    fn export_midi(&mut self) {
        let Some(path) = self.ask_save_path("MIDI ファイル", "mid", "sequence.mid") else {
            return;
        };

        let bytes = match midi::to_bytes(&self.editor.editor) {
            Ok(bytes) => bytes,
            Err(e) => {
                self.notice = Some(Notice::error("MIDI を書き出せません", e));
                return;
            }
        };

        match std::fs::write(&path, bytes) {
            Ok(()) => {
                self.last_directory = path.parent().map(PathBuf::from);
                self.error = None;
                self.status = Some(format!("書き出しました: {}", file_label(&path)));
            }
            Err(e) => {
                self.notice = Some(Notice::error(
                    "MIDI を書き出せません",
                    format!("保存できません:\n{e}"),
                ));
            }
        }
    }

    /// プロジェクトを保存する。
    /// `ask` が false なら、保存先が決まっていれば黙って上書きする (Ctrl+S)。
    fn save_project(&mut self, ask: bool) {
        let path = if ask || self.project_path.is_none() {
            let Some(path) = self.ask_save_path("プロジェクト", "ron", "song.ron") else {
                return;
            };
            path
        } else {
            self.project_path.clone().unwrap_or_default()
        };

        // 音源の状態を集めてから組み立てる (音源はエディタではなく App 側にある)
        let snapshots = self.plugin_snapshots();
        let text = match project::to_string(&self.editor.editor, &snapshots) {
            Ok(text) => text,
            Err(e) => {
                self.notice = Some(Notice::error("保存できません", e));
                return;
            }
        };

        match std::fs::write(&path, text) {
            Ok(()) => {
                self.set_project_path(path.clone());
                self.error = None;
                self.status = Some(format!("保存しました: {}", file_label(&path)));
            }
            Err(e) => {
                self.notice = Some(Notice::error(
                    "保存できません",
                    format!("書き込めません:\n{e}"),
                ));
            }
        }
    }

    /// プロジェクトを選んで読み込む。
    /// 検証に通らなければ**何も変更しない** (壊れたファイルで作業中の内容を失わないため)。
    fn open_project(&mut self) {
        let mut dialog = rfd::FileDialog::new().add_filter("プロジェクト", &["ron"]);
        if let Some(directory) = self.dialog_directory() {
            dialog = dialog.set_directory(directory);
        }
        let Some(path) = dialog.pick_file() else { return };

        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(e) => {
                self.notice = Some(Notice::error("開けません", format!("読み込めません:\n{e}")));
                return;
            }
        };

        match project::from_str(&text) {
            Ok(loaded) => {
                let notes = loaded.editor.notes.len();
                let tracks = loaded.editor.tracks.len();
                let wanted = loaded.plugins.iter().flatten().count();

                self.editor.replace_project(loaded.editor);
                // 音源はシーケンスを入れ替えたあとに載せる
                // (トラック数が揃ってからでないと行き先が決まらない)
                let failures = self.restore_plugins(loaded.plugins);

                self.set_project_path(path.clone());
                self.error = None;
                self.status = Some(format!("開きました: {}", file_label(&path)));

                let mut body = format!(
                    "{}\n\n{tracks} トラック / {notes} ノート / 音源 {} 個",
                    path.display(),
                    wanted - failures.len()
                );
                // 一部が読めなくてもシーケンスは開く。何が欠けたかは伝える。
                if failures.is_empty() {
                    self.notice = Some(Notice::ok("プロジェクトを開きました", body));
                } else {
                    body.push_str("\n\n次の音源は読み込めませんでした。");
                    body.push_str("\nそのトラックは音源なしになっています (ノートは残っています)。\n");
                    body.push_str(&failures.join("\n"));
                    self.notice =
                        Some(Notice::error("プロジェクトを一部だけ開きました", body));
                }
            }
            Err(e) => {
                self.notice = Some(Notice::error(
                    "開けません",
                    format!("{}\n\n{e}", file_label(&path)),
                ));
            }
        }
    }

    /// 保存先を覚える (エディタ側には表示用のファイル名だけ渡す)
    fn set_project_path(&mut self, path: PathBuf) {
        self.last_directory = path.parent().map(PathBuf::from);
        self.editor.project_path = Some(file_label(&path));
        self.project_path = Some(path);
    }

    /// 借りた処理器をオーディオスレッドへ返す。
    ///
    /// **返さないと音が出なくなる。** 途中で諦めるときも必ず通ること。
    fn return_processors(&mut self, processors: Vec<(usize, Box<audio::TrackProcessor>)>) {
        let Some(engine) = self.engine.as_mut() else {
            return;
        };
        for (track, processor) in processors {
            let _ = engine.producer.push(GuiMsg::SetTrack { track, processor });
        }
    }

    /// 書き出し用のレンダリング設定を、指定のストリーム構成で組み立てる
    fn render_setup(&self, config: &StreamAudioConfig) -> RenderSetup {
        let sample_rate = config.sample_rate as f64;
        let spq = self.editor.editor.samples_per_quarter(sample_rate);
        RenderSetup {
            // ミュート/ソロで鳴らさないトラックは空にする (再生時と同じ判定)
            sequences: (0..self.editor.editor.track_count())
                .map(|track| {
                    if self.editor.editor.is_audible(track) {
                        self.editor
                            .editor
                            .to_events_for_track(track, sample_rate)
                            .into_boxed_slice()
                    } else {
                        Vec::<SeqEvent>::new().into_boxed_slice()
                    }
                })
                .collect(),
            end_sample: (self.editor.editor.length_quarters_bar_aligned() as f64 * spq) as u64,
            tail_samples: (TAIL_SECONDS * sample_rate) as u64,
            // activate 時に宣言した上限。これを超えるブロックは渡せない。
            block_frames: config.max_likely_buffer_size as usize,
            channels: config.output_channel_count,
            sample_rate: config.sample_rate,
        }
    }

    /// 音源を借りて、指定のサンプルレートで最後まで回す。
    ///
    /// `target_rate` が今のストリームと違うなら、**音源自体をそのレートで
    /// 動かし直してから描画し、必ず元へ戻す**。リサンプリングではないので、
    /// 本当にそのレートで鳴らした音になる (Opus は 48kHz でしか鳴らせないが、
    /// ストリームはデバイスのレートで開いているため、この仕組みが要る)。
    ///
    /// 戻り値は (描画結果, レートの切り替えに失敗して**音源を外した**トラック)。
    fn render_for_export(
        &mut self,
        stream_config: StreamAudioConfig,
        target_rate: u32,
    ) -> Result<(audio::offline::Rendered, Vec<usize>), String> {
        let export_config = StreamAudioConfig {
            sample_rate: target_rate,
            ..stream_config
        };
        let switching = target_rate != stream_config.sample_rate;

        let loaded: Vec<usize> = self
            .tracks
            .iter()
            .enumerate()
            .filter_map(|(track, slot)| slot.as_ref().map(|_| track))
            .collect();

        // ---- 借りる ----
        // engine の借用はここで終える (このあと self.tracks を触るため)
        let mut processors = {
            let Some(engine) = self.engine.as_mut() else {
                return Err("音源が未ロードです。".into());
            };
            let _ = engine.producer.push(GuiMsg::Transport(TransportMsg::Stop));
            for track in &loaded {
                let _ = engine.producer.push(GuiMsg::ClearTrack { track: *track });
            }
            collect_processors(engine, loaded.len())
        };
        if processors.len() != loaded.len() {
            self.return_processors(processors);
            return Err("音源を取り出せませんでした。もう一度実行してください。".into());
        }

        // ---- 書き出し用のレートへ ----
        let mut dropped = Vec::new();
        if switching {
            let (switched, failed) = self.switch_processors_rate(processors, &export_config);
            processors = switched;
            dropped.extend(failed);
        }

        let setup = self.render_setup(&export_config);
        let rendered = audio::offline::render(&mut processors, setup);

        // ---- 元のレートへ戻す ----
        // **ここを飛ばすと、書き出し後に再生できなくなる。**
        if switching {
            let (switched, failed) = self.switch_processors_rate(processors, &stream_config);
            processors = switched;
            dropped.extend(failed);
        }

        self.return_processors(processors);

        // 戻せなかったトラックは音源ごと外す。
        // 載っているのに鳴らない状態で放置するより、消えている方が気付ける。
        for track in &dropped {
            if let Some(slot) = self.tracks.get_mut(*track) {
                *slot = None;
            }
        }
        Ok((rendered, dropped))
    }

    /// シーケンス全体を鳴らして WAV ファイルに書き出す。
    ///
    /// オーディオスレッドから処理器を一旦引き上げ、その場で最後まで回してから戻す。
    /// ユーザーが音作りしたパラメータをそのまま使うためで、別インスタンスを立てると
    /// state 拡張が未対応なぶん初期値に戻ってしまう。
    /// 処理の間はメインスレッドが止まるので、画面も一時的に固まる。
    fn export_wav(&mut self) {
        // 差し替え待ちの音源が残っていると、引き上げた処理器がどちらのものか
        // 見分けられなくなる。片付いてから始める。
        self.drain_retired();
        if !self.retiring.is_empty() {
            self.fail_export("音源の切り替え中です。少し待ってからもう一度実行してください");
            return;
        }

        let Some(config) = self.engine.as_ref().map(|engine| engine.config) else {
            self.fail_export(
                "音源が未ロードです。\n左のトラック欄の「♪」から .clap を読み込んでください。",
            );
            return;
        };
        if self.editor.editor.notes.is_empty() {
            self.fail_export("ノートが1つもありません。");
            return;
        }

        let loaded: Vec<usize> = self
            .tracks
            .iter()
            .enumerate()
            .filter_map(|(track, slot)| slot.as_ref().map(|_| track))
            .collect();
        if loaded.is_empty() {
            self.fail_export("音源が載っているトラックがありません。");
            return;
        }

        // 時間のかかる処理に入る前に保存先を聞く
        let Some(path) = self.ask_save_path("WAV ファイル", "wav", "mix.wav") else {
            return;
        };

        // WAV はデバイスのレートのまま書く (切り替えの必要がない)
        let rendered = match self.render_for_export(config, config.sample_rate) {
            Ok((rendered, _)) => rendered,
            Err(e) => {
                self.fail_export(e);
                return;
            }
        };

        let bytes = match wav::to_bytes_16bit(
            &rendered.samples,
            rendered.channels as u16,
            rendered.sample_rate,
        ) {
            Ok(bytes) => bytes,
            Err(e) => {
                self.fail_export(e);
                return;
            }
        };

        match std::fs::write(&path, bytes) {
            Ok(()) => {
                self.last_directory = path.parent().map(PathBuf::from);
                self.error = None;

                let channels = if rendered.channels == 1 {
                    "モノラル"
                } else {
                    "ステレオ"
                };
                let mut body = format!(
                    "{}\n\n{:.1} 秒 / {} Hz / {} / 16bit PCM",
                    path.display(),
                    rendered.seconds(),
                    rendered.sample_rate,
                    channels,
                );
                // 勝手に音量を変えた事実は伏せない
                if rendered.peak > 1.0 {
                    body.push_str(&format!(
                        "\n\n※ ピークが {:.2} (0dBFS 超) だったため、歪まないよう全体の音量を下げました。",
                        rendered.peak
                    ));
                }
                // 音源の処理に失敗したトラックは無音のまま混ざっている。
                // ファイルは残すが、成功として見せると欠けたまま気付けない。
                if rendered.failures.is_empty() {
                    self.status = Some(format!("書き出しました: {}", file_label(&path)));
                    self.notice = Some(Notice::ok("WAV を書き出しました", body));
                } else {
                    body.push_str(
                        "\n\n次のトラックは音源の処理に失敗したため、無音になっています。",
                    );
                    for failure in &rendered.failures {
                        body.push_str(&format!(
                            "\n・トラック {}: {} ({} ブロック)",
                            failure.track + 1,
                            failure.message,
                            failure.blocks
                        ));
                    }
                    self.status = Some(format!(
                        "書き出しました (一部失敗): {}",
                        file_label(&path)
                    ));
                    self.notice =
                        Some(Notice::error("WAV の一部を書き出せませんでした", body));
                }
            }
            Err(e) => self.fail_export(format!("保存できません:\n{e}")),
        }
    }

    /// 書き出しの失敗を通知する
    fn fail_export(&mut self, message: impl Into<String>) {
        self.notice = Some(Notice::error("WAV を書き出せません", message));
    }

    /// シーケンス全体を鳴らして Ogg/Opus に書き出す。
    ///
    /// **Opus は 48kHz でしか鳴らせない。** ストリームはデバイスのレート
    /// (多くは 44.1kHz) で開いているので、書き出しの間だけ**音源自体を 48kHz で
    /// 動かし直す**。リサンプリングではないので、本当に 48kHz で鳴らした音になる。
    fn export_opus(&mut self) {
        self.drain_retired();
        if !self.retiring.is_empty() {
            self.fail_opus("音源の切り替え中です。少し待ってからもう一度実行してください");
            return;
        }

        let Some(config) = self.engine.as_ref().map(|engine| engine.config) else {
            self.fail_opus(
                "音源が未ロードです。\n左のトラック欄の「♪」から音源を読み込んでください。",
            );
            return;
        };
        if self.editor.editor.notes.is_empty() {
            self.fail_opus("ノートが1つもありません。");
            return;
        }
        if self.tracks.iter().all(Option::is_none) {
            self.fail_opus("音源が載っているトラックがありません。");
            return;
        }
        // 符号化まで進んでから断ると、レートの切り替えを無駄に往復することになる
        if config.output_channel_count != 1 && config.output_channel_count != 2 {
            self.fail_opus(format!(
                "Opus で書き出せるのはモノラルかステレオだけです ({}ch)",
                config.output_channel_count
            ));
            return;
        }

        // 時間のかかる処理に入る前に保存先を聞く
        let Some(path) = self.ask_save_path("Opus ファイル", "opus", "mix.opus") else {
            return;
        };

        let bitrate = self.editor.opus_bitrate_kbps;
        let (rendered, dropped) = match self.render_for_export(config, opus::SAMPLE_RATE) {
            Ok(result) => result,
            Err(e) => {
                self.fail_opus(e);
                return;
            }
        };

        let bytes = match opus::to_bytes(
            &rendered.samples,
            rendered.channels as u16,
            rendered.sample_rate,
            bitrate,
        ) {
            Ok(bytes) => bytes,
            Err(e) => {
                self.fail_opus(e);
                return;
            }
        };

        match std::fs::write(&path, bytes) {
            Ok(()) => {
                self.last_directory = path.parent().map(PathBuf::from);
                self.error = None;
                self.notice = Some(self.opus_result_notice(&path, &rendered, bitrate, &dropped));
                self.status = Some(format!("書き出しました: {}", file_label(&path)));
            }
            Err(e) => self.fail_opus(format!("保存できません:\n{e}")),
        }
    }

    /// 書き出しの結果をまとめる。**欠けたものは伏せない。**
    fn opus_result_notice(
        &self,
        path: &std::path::Path,
        rendered: &audio::offline::Rendered,
        bitrate: u32,
        dropped: &[usize],
    ) -> Notice {
        let channels = if rendered.channels == 1 {
            "モノラル"
        } else {
            "ステレオ"
        };
        let mut body = format!(
            "{}\n\n{:.1} 秒 / {} Hz / {} / {} kbps",
            path.display(),
            rendered.seconds(),
            rendered.sample_rate,
            channels,
            bitrate,
        );
        if rendered.peak > 1.0 {
            body.push_str(&format!(
                "\n\n※ ピークが {:.2} (0dBFS 超) だったため、歪まないよう全体の音量を下げました。",
                rendered.peak
            ));
        }

        let mut failed = false;
        if !rendered.failures.is_empty() {
            failed = true;
            body.push_str("\n\n次のトラックは音源の処理に失敗したため、無音になっています。");
            for failure in &rendered.failures {
                body.push_str(&format!(
                    "\n・トラック {}: {} ({} ブロック)",
                    failure.track + 1,
                    failure.message,
                    failure.blocks
                ));
            }
        }
        // レートを戻せなかったトラックは音源ごと外してある。
        // 黙っていると「読み込んだはずの音源が消えている」ことになる。
        if !dropped.is_empty() {
            failed = true;
            body.push_str(
                "\n\n次のトラックは 48kHz への切り替えに失敗したため、音源を外しました。\
                 \n読み込み直してください。",
            );
            for track in dropped {
                body.push_str(&format!("\n・トラック {}", track + 1));
            }
        }

        if failed {
            Notice::error("Opus を書き出しました (一部に問題あり)", body)
        } else {
            Notice::ok("Opus を書き出しました", body)
        }
    }

    fn fail_opus(&mut self, message: impl Into<String>) {
        self.notice = Some(Notice::error("Opus を書き出せません", message));
    }

    /// 1トラック目を CeVIO のプロジェクトファイル (.ccs) に書き出す。
    /// 音を鳴らさないデータ変換なので、音源が未ロードでも使える。
    fn export_ccs(&mut self) {
        // 変換に失敗するならダイアログを出す前に知らせる
        let exported = match ccs::export(&self.editor.editor) {
            Ok(exported) => exported,
            Err(e) => {
                self.notice = Some(Notice::error("CCS を書き出せません", e));
                return;
            }
        };

        let Some(path) = self.ask_save_path("CeVIO プロジェクト", "ccs", "song.ccs") else {
            return;
        };

        match std::fs::write(&path, &exported.bytes) {
            Ok(()) => {
                self.last_directory = path.parent().map(PathBuf::from);
                self.error = None;

                let mut body = format!(
                    "{}\n\n{} パート / {} ノート\n音律: {}",
                    path.display(),
                    exported.parts,
                    exported.notes,
                    self.editor.editor.scale.label(),
                );
                if exported.skipped > 0 {
                    body.push_str(&format!(
                        "\n\n※ 音域外または音価0のノート {} 個は書き出していません。",
                        exported.skipped
                    ));
                }
                self.status = Some(format!("書き出しました: {}", file_label(&path)));
                self.notice = Some(Notice::ok("CCS を書き出しました", body));
            }
            Err(e) => {
                self.notice = Some(Notice::error("CCS を書き出せません", format!("保存できません:\n{e}")));
            }
        }
    }

    /// 保存先を選ばせる。拡張子を省略されたら補う。
    fn ask_save_path(&mut self, filter: &str, extension: &str, default_name: &str) -> Option<PathBuf> {
        let mut dialog = rfd::FileDialog::new()
            .add_filter(filter, &[extension])
            .set_file_name(default_name);
        if let Some(directory) = self.dialog_directory() {
            dialog = dialog.set_directory(directory);
        }
        let path = dialog.save_file()?;
        Some(if path.extension().is_none() {
            path.with_extension(extension)
        } else {
            path
        })
    }

    /// 借りている処理器を、指定のストリーム構成で動かし直す。
    ///
    /// **書き出しを別のサンプルレートで行うためのもの。** Opus は 48kHz でしか
    /// 鳴らせないが、ストリームはデバイスのレート (多くは 44.1kHz) で開いている。
    /// リサンプリングではなく**音源自身をそのレートで動かす**ので、本当にその
    /// レートで鳴らした音になる。
    ///
    /// 戻り値は (動かし直せた処理器, 失敗したトラック番号)。
    ///
    /// **失敗したトラックの処理器は失われる。** CLAP は deactivate まで進んだあとの
    /// activate で落ちうるためで、そのトラックは呼び出し側が音源ごと外すこと
    /// (黙って鳴らないままにしない)。
    fn switch_processors_rate(
        &mut self,
        processors: Vec<(usize, Box<audio::TrackProcessor>)>,
        config: &StreamAudioConfig,
    ) -> (Vec<(usize, Box<audio::TrackProcessor>)>, Vec<usize>) {
        let mut switched = Vec::with_capacity(processors.len());
        let mut failed = Vec::new();

        for (track, processor) in processors {
            match self.switch_one_rate(track, processor, config) {
                Ok(processor) => switched.push((track, processor)),
                Err(e) => {
                    eprintln!("トラック {} のレート切り替えに失敗: {e}", track + 1);
                    failed.push(track);
                }
            }
        }
        (switched, failed)
    }

    /// 1トラックぶんの動かし直し。
    ///
    /// **どちらの形式も読み込み直さない** (読み直すと音作りが飛ぶ)。CLAP は
    /// deactivate → activate、VST3 は `reconfigure` で、状態を保ったまま
    /// `setupProcessing` 相当をやり直す。
    fn switch_one_rate(
        &mut self,
        track: usize,
        processor: Box<audio::TrackProcessor>,
        config: &StreamAudioConfig,
    ) -> Result<Box<audio::TrackProcessor>, Box<dyn Error>> {
        let Some(audio) = self.tracks.get_mut(track).and_then(|slot| slot.as_mut()) else {
            return Err("このトラックに音源がありません".into());
        };

        match (processor.into_retired(), &mut audio.plugin) {
            (audio::RetiredProcessor::Clap(stopped), TrackPlugin::Clap(clap)) => {
                clap.instance.deactivate(stopped);
                audio::activate_track(&mut clap.instance, config)
            }
            (audio::RetiredProcessor::Vst3(shared), TrackPlugin::Vst3(_)) => {
                audio::reconfigure_vst3_track(shared, config)
            }
            // 形式が食い違うことは無いはずだが、ここで取り違えると
            // 処理器を失ったまま気付けないので明示的に落とす
            _ => Err("処理器と音源の形式が食い違っています".into()),
        }
    }

    /// オーディオスレッドから返ってきた音源をここで停止・解放する
    /// (オーディオスレッドで解放してはいけないため)。
    /// 差し替えで外したインスタンスが待っていればそちらへ、
    /// 無ければ今そのトラックに載っているインスタンスへ返す。
    fn drain_retired(&mut self) {
        let Some(engine) = &mut self.engine else {
            return;
        };
        while let Ok((track, processor)) = engine.retired.pop() {
            // 差し替えで外したインスタンスが待っていればそちらへ返す。
            // 待っていない場合 (書き出しの借り出しが時間切れになったときなど) は
            // 今そのトラックに載っているものが当人なので、欄から降ろして始末する
            // (止めた音源を載ったままにすると、鳴らないトラックが残る)。
            let waiting = self.retiring.iter().position(|(index, _)| *index == track);
            let mut owner = match waiting {
                Some(at) => self.retiring.remove(at).map(|(_, old)| old),
                None => self.tracks.get_mut(track).and_then(|slot| slot.take()),
            };

            // 形式ごとに始末の仕方が違う
            match processor.into_retired() {
                audio::RetiredProcessor::Clap(stopped) => {
                    // CLAP は処理器をインスタンスへ返して初めて解放できる
                    if let Some(TrackPlugin::Clap(clap)) =
                        owner.as_mut().map(|track| &mut track.plugin)
                    {
                        clap.instance.deactivate(stopped);
                    }
                }
                audio::RetiredProcessor::Vst3(shared) => {
                    // VST3 はオーディオスレッドで止められない (`setProcessing` が
                    // リアルタイム安全でない) ので、ここで止める
                    let _ = shared.lock().stop_processing();
                }
            }
            // owner はここで破棄される (CLAP は GUI も閉じられる)
        }
    }

    /// ファイルダイアログを開くフォルダ
    fn dialog_directory(&self) -> Option<PathBuf> {
        self.project_path
            .as_ref()
            .and_then(|path| path.parent())
            .map(PathBuf::from)
            .or_else(|| self.last_directory.clone())
    }

    /// トラック数をエディタに合わせる (足りなければ空きで埋める)
    fn ensure_track_slots(&mut self, track: usize) {
        let needed = (track + 1).max(self.editor.editor.track_count());
        while self.tracks.len() < needed {
            self.tracks.push(None);
        }
    }

    /// 選んだプラグインを指定トラックに載せる (トラック欄からの操作)
    fn instantiate(&mut self, plugin_index: usize, track: usize) {
        let Some(candidates) = &self.candidates else {
            return;
        };
        let kind = candidates.kind;
        let path = candidates.path.clone();
        let id = candidates.plugins[plugin_index].id.clone();

        match self.load_plugin(track, kind, &path, &id, None) {
            Ok(_) => self.error = None,
            Err(e) => self.error = Some(e),
        }
    }

    /// 指定トラックに音源を載せる。`state` があれば復元してから鳴らせる状態にする。
    ///
    /// 呼び出し元は2つ。トラック欄の「♪」からの選択と、プロジェクトを開いたとき。
    /// 戻り値は「状態を戻せたか」で、`state` が無いときは常に true。
    fn load_plugin(
        &mut self,
        track: usize,
        kind: project::PluginKind,
        path: &std::path::Path,
        id: &str,
        state: Option<&[u8]>,
    ) -> Result<bool, String> {
        // 最初のロード時にストリームを用意する
        if self.engine.is_none() {
            let engine =
                start_engine().map_err(|e| format!("オーディオを開始できません: {e}"))?;
            self.engine = Some(engine);
        }

        // 名前は選択 UI に出したものと同じにしたいので、ここでも数え上げる
        let found = match kind {
            project::PluginKind::Clap => {
                discovery::load_clap_file(path).map(|(_entry, plugins)| plugins)
            }
            project::PluginKind::Vst3 => discovery::load_vst3_file(path),
        }
        .map_err(|e| e.to_string())?;

        let Some(plugin) = found.iter().find(|plugin| plugin.id == id) else {
            return Err(format!("プラグイン {id} がこのファイルにありません"));
        };
        let name = plugin.name.clone();

        let Some(engine) = &mut self.engine else {
            return Err("オーディオを開始できません".into());
        };

        let (mut audio_track, processor) = match kind {
            project::PluginKind::Clap => instantiate_clap(path, id, &name, &engine.config),
            project::PluginKind::Vst3 => instantiate_vst3(path, id, &name, &engine.config),
        }
        .map_err(|e| format!("インスタンス化失敗: {e}"))?;

        // 状態の復元は、処理器をオーディオスレッドへ渡す前に済ませる
        // (初期値のまま鳴り始めるのを避けるため)
        let restored = match state {
            Some(bytes) => audio_track.restore_state(bytes),
            None => true,
        };

        let _ = engine.producer.push(GuiMsg::SetTrack { track, processor });

        // 未ロード中に動かした再生ヘッドの位置を引き継ぐ
        let spq = self
            .editor
            .editor
            .samples_per_quarter(engine.config.sample_rate as f64);
        let sample = (self.pos_quarters * spq).max(0.0) as u64;
        let _ = engine
            .producer
            .push(GuiMsg::Transport(TransportMsg::Seek { sample }));

        self.ensure_track_slots(track);
        // 前の音源は、処理器が返ってくるまで生かしておく
        if let Some(previous) = self.tracks[track].take() {
            self.retiring.push_back((track, previous));
        }
        self.tracks[track] = Some(audio_track);
        // 新しいプラグインにシーケンスを送り直す
        self.editor.dirty = true;

        Ok(restored)
    }

    /// 保存用に、各トラックの音源とその状態を集める
    fn plugin_snapshots(&mut self) -> Vec<Option<PluginSnapshot>> {
        let count = self.editor.editor.track_count();
        let mut snapshots = Vec::with_capacity(count);
        for track in 0..count {
            let slot = self.tracks.get_mut(track).and_then(|slot| slot.as_mut());
            snapshots.push(slot.map(|audio| PluginSnapshot {
                kind: audio.kind(),
                path: audio.path.clone(),
                id: audio.id.clone(),
                state: audio.capture_state(),
            }));
        }
        snapshots
    }

    /// 今載っている音源を全部降ろす (プロジェクトを開く前の片付け)
    fn unload_all_plugins(&mut self) {
        let Some(engine) = &mut self.engine else {
            return;
        };
        for track in 0..self.tracks.len() {
            if let Some(previous) = self.tracks[track].take() {
                let _ = engine.producer.push(GuiMsg::ClearTrack { track });
                // 処理器が返ってくるまで生かしておく (解放はメインスレッド)
                self.retiring.push_back((track, previous));
            }
        }
    }

    /// プロジェクトに書かれていた音源を読み直す。
    /// 失敗したトラックの説明を返す (そのトラックは音源なしのままになる)。
    fn restore_plugins(&mut self, plugins: Vec<Option<PluginSnapshot>>) -> Vec<String> {
        self.unload_all_plugins();

        let mut failures = Vec::new();
        for (track, slot) in plugins.into_iter().enumerate() {
            let Some(plugin) = slot else { continue };
            let label = file_label(&plugin.path);
            match self.load_plugin(
                track,
                plugin.kind,
                &plugin.path,
                &plugin.id,
                Some(&plugin.state),
            ) {
                Ok(true) => {}
                Ok(false) => failures.push(format!(
                    "・トラック {}: {label} は読み込めましたが、音作りの復元に失敗しました",
                    track + 1
                )),
                Err(e) => failures.push(format!("・トラック {}: {label} — {e}", track + 1)),
            }
        }
        failures
    }
}

/// `ClearTrack` で外した処理器が返ってくるのを待つ。
///
/// オーディオコールバックが回っている前提なので、普通は1〜2ブロック分で揃う。
/// 揃わないまま時間切れになったら、集まったぶんだけ返す (呼び出し側が戻す)。
fn collect_processors(
    engine: &mut Engine,
    expected: usize,
) -> Vec<(usize, Box<audio::TrackProcessor>)> {
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut collected = Vec::with_capacity(expected);

    while collected.len() < expected && Instant::now() < deadline {
        match engine.retired.pop() {
            Ok(item) => collected.push(item),
            Err(_) => std::thread::sleep(Duration::from_millis(1)),
        }
    }
    collected
}

/// 出力ストリームを1本だけ作る (最初に音源をロードするときに一度だけ呼ぶ)
fn start_engine() -> Result<Engine, Box<dyn Error>> {
    let (producer, consumer) = rtrb::RingBuffer::new(256);
    // 外した音源をメインスレッドへ返す口 (オーディオスレッドで解放しないため)
    let (retired_producer, retired) = rtrb::RingBuffer::new(8);
    let transport_shared = TransportShared::new();

    let (stream, config) =
        audio::start_engine(consumer, retired_producer, transport_shared.clone())?;

    Ok(Engine {
        _stream: stream,
        producer,
        retired,
        transport_shared,
        config,
    })
}

/// CLAP プラグインをインスタンス化して、指定のストリーム構成で鳴らせる状態にする。
/// 戻り値の処理器は呼び出し側がエンジンへ送る。
fn instantiate_clap(
    path: &std::path::Path,
    plugin_id: &str,
    plugin_name: &str,
    stream_config: &StreamAudioConfig,
) -> Result<(TrackAudio, Box<audio::TrackProcessor>), Box<dyn Error>> {
    // clack は PluginInstance の中で DLL を生かすので、ここで開き直してよい
    let (entry, _) = discovery::load_clap_file(path)?;

    let host_info = HostInfo::new(
        "CLAP Mini Host",
        "clap-host-test",
        "https://example.com",
        "0.1.0",
    )?;
    let plugin_id_cstr = CString::new(plugin_id)?;
    let (sender, receiver) = crossbeam_channel::unbounded();

    let mut instance = PluginInstance::<MiniHost>::new(
        |_| MiniHostShared::new(sender.clone()),
        |shared| MiniHostMainThread::new(shared),
        &entry,
        &plugin_id_cstr,
        &host_info,
    )?;

    let processor = audio::activate_track(&mut instance, stream_config)?;
    let params = params::read_params(&mut instance);

    // gui 拡張があれば GUI マネージャを用意する
    let gui_ext = instance.access_handler(|mt| mt.gui.get());
    let gui = gui_ext.map(|ext| PluginGuiManager::new(ext, &mut instance.plugin_handle()));

    Ok((
        TrackAudio {
            name: plugin_name.to_string(),
            path: path.to_path_buf(),
            id: plugin_id.to_string(),
            pressed_keys: HashSet::new(),
            plugin: TrackPlugin::Clap(ClapTrack {
                instance,
                receiver,
                sender,
                params,
                gui,
            }),
        },
        processor,
    ))
}

/// VST3 プラグインを読み込んで、指定のストリーム構成で鳴らせる状態にする。
///
/// CLAP と違って音源そのものを処理器と共有する。エディタ (フェーズ3) と
/// 状態の保存がメインスレッドから音源を要求するため。
fn instantiate_vst3(
    path: &std::path::Path,
    class_id: &str,
    plugin_name: &str,
    stream_config: &StreamAudioConfig,
) -> Result<(TrackAudio, Box<audio::TrackProcessor>), Box<dyn Error>> {
    let (plugin, processor) = audio::activate_vst3_track(path, class_id, stream_config)?;

    let gui = Vst3GuiManager::new(&plugin.lock());
    let (sender, receiver) = crossbeam_channel::unbounded();

    Ok((
        TrackAudio {
            name: plugin_name.to_string(),
            path: path.to_path_buf(),
            id: class_id.to_string(),
            pressed_keys: HashSet::new(),
            plugin: TrackPlugin::Vst3(Vst3Track {
                plugin,
                receiver,
                sender,
                gui,
            }),
        },
        processor,
    ))
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // 起動時の自動ロード (検証用 CLI)
        if let Some((path, open_gui)) = self.autoload.take() {
            // 拡張子で形式を見分ける (CLI なので聞き返す相手がいない)
            let kind = match path.extension().and_then(|ext| ext.to_str()) {
                Some(ext) if ext.eq_ignore_ascii_case("vst3") => project::PluginKind::Vst3,
                _ => project::PluginKind::Clap,
            };
            let found = match kind {
                project::PluginKind::Clap => {
                    discovery::load_clap_file(&path).map(|(_entry, plugins)| plugins)
                }
                project::PluginKind::Vst3 => discovery::load_vst3_file(&path),
            };

            match found {
                Ok(plugins) => {
                    self.candidates = Some(Candidates {
                        kind,
                        path,
                        plugins,
                        target_track: 0,
                    });
                    self.instantiate(0, 0);
                    if open_gui {
                        if let Some(Some(track)) = self.tracks.get_mut(0) {
                            let name = track.name.clone();
                            let opened = match &mut track.plugin {
                                TrackPlugin::Clap(clap) => clap.gui.as_mut().map(|gui| {
                                    gui.open(
                                        &mut clap.instance.plugin_handle(),
                                        &name,
                                        clap.sender.clone(),
                                    )
                                }),
                                TrackPlugin::Vst3(vst3) => {
                                    let mut plugin = vst3.plugin.lock();
                                    Some(vst3.gui.open(&mut plugin, &name, vst3.sender.clone()))
                                }
                            };
                            if let Some(Err(e)) = opened {
                                self.error = Some(format!("GUI を開けません: {e}"));
                            }
                        }
                    }
                }
                Err(e) => self.error = Some(format!("自動ロード失敗: {e}")),
            }
        }

        // 差し替えで外した音源をここで停止・解放する
        self.drain_retired();

        // プラグインからのメインスレッド要求 & GUI ウィンドウイベントを処理
        for track in self.tracks.iter_mut().flatten() {
            let clap = match &mut track.plugin {
                TrackPlugin::Clap(clap) => clap,
                TrackPlugin::Vst3(vst3) => {
                    // エディタを閉じているときは音源に触らない
                    // (触るぶんだけオーディオスレッドと取り合いになる)
                    if !vst3.gui.is_open && vst3.receiver.is_empty() {
                        continue;
                    }
                    // ここで待ってはいけない。毎フレーム待ちに行くと、オーディオ
                    // スレッドが手放すたびに横取りする形になり、次のブロックが
                    // 落ちるのが常態化する。取れなければ次のフレームでよい。
                    let Some(mut plugin) = vst3.plugin.try_lock() else {
                        continue;
                    };
                    while let Ok(msg) = vst3.receiver.try_recv() {
                        match msg {
                            MainThreadMessage::PluginWindowClosed => vst3.gui.close(&mut plugin),
                            MainThreadMessage::PluginWindowResized { width, height } => {
                                vst3.gui.on_user_resized(&mut plugin, width, height)
                            }
                            // CLAP 拡張から来るものなので VST3 では届かない
                            MainThreadMessage::RunOnMainThread
                            | MainThreadMessage::GuiRequestResized { .. }
                            | MainThreadMessage::GuiClosed => {}
                        }
                    }
                    // プラグイン発のリサイズ要求はこちらから取りに行く
                    vst3.gui.poll_resize_request(&plugin);
                    // VSTGUI のエディタは Linux でこれを回さないと描画されない
                    // (Windows では何もしない)
                    plugin.service_run_loop();
                    continue;
                }
            };
            while let Ok(msg) = clap.receiver.try_recv() {
                match msg {
                    MainThreadMessage::RunOnMainThread => {
                        clap.instance.call_on_main_thread_callback()
                    }
                    MainThreadMessage::GuiRequestResized { new_size } => {
                        if let Some(gui) = &mut clap.gui {
                            gui.on_plugin_request_resize(new_size);
                        }
                    }
                    MainThreadMessage::GuiClosed | MainThreadMessage::PluginWindowClosed => {
                        if let Some(gui) = &mut clap.gui {
                            gui.close(&mut clap.instance.plugin_handle());
                        }
                    }
                    MainThreadMessage::PluginWindowResized { width, height } => {
                        if let Some(gui) = &mut clap.gui {
                            gui.on_user_resized(&mut clap.instance.plugin_handle(), width, height);
                        }
                    }
                }
            }

            // プラグインが登録したタイマーを駆動する (GUI 描画などに必要)
            let timer = clap
                .instance
                .access_handler(|mt| mt.timer_support.get().map(|ext| (mt.timers.clone(), ext)));
            if let Some((timers, timer_ext)) = timer {
                timers.tick_timers(&timer_ext, &mut clap.instance.plugin_handle());
            }
        }

        egui::CentralPanel::default().show(ctx, |ui| {
            // ヘッダー行 (状態表示)
            ui.horizontal(|ui| {
                ui.heading("CLAP ミニホスト");
                ui.separator();
                // .clap のロードはトラック欄の「♪」から行うので、ここでは出さない
                /*
                if ui.button(".clap ファイルを開く…").clicked() {
                    // ヘッダーからのロードはトラック1へ
                    self.open_file_dialog(0);
                }
                */
                if let Some(error) = &self.error {
                    ui.colored_label(egui::Color32::RED, error);
                }
                if let Some(status) = &self.status {
                    ui.weak(status);
                }
            });

            // 音源の形式の選択。ダイアログの出し方が形式で違うので先に決めてもらう
            // (.clap はファイル、.vst3 はバンドルディレクトリと単体ファイルの2通り)。
            let mut chosen_kind = None;
            if let Some(track) = self.pending_load {
                ui.horizontal_wrapped(|ui| {
                    ui.label(format!("トラック {} に読み込む音源の形式:", track + 1));
                    if ui.button("CLAP (.clap ファイル)").clicked() {
                        chosen_kind = Some(LoadChoice::Clap);
                    }
                    if ui
                        .button("VST3 (.vst3 フォルダ)")
                        .on_hover_text("現行の標準。フォルダ選択が開きます")
                        .clicked()
                    {
                        chosen_kind = Some(LoadChoice::Vst3Bundle);
                    }
                    if ui
                        .button("VST3 (.vst3 単体ファイル)")
                        .on_hover_text(
                            "フォルダになっていない古い形式。\
                             バンドルの中の DLL を直接指すのにも使えます",
                        )
                        .clicked()
                    {
                        chosen_kind = Some(LoadChoice::Vst3File);
                    }
                    if ui.button("やめる").clicked() {
                        chosen_kind = Some(LoadChoice::Cancel);
                    }
                });
            }

            // プラグイン選択。1つの .clap に複数入っていて、まだ選んでいないときだけ出す
            // (1つだけのファイルはトラック欄の「♪」でそのまま載るので出さない)。
            let mut instantiate_index = None;
            if let Some(candidates) = self
                .candidates
                .as_ref()
                .filter(|candidates| candidates.plugins.len() > 1)
            {
                ui.horizontal_wrapped(|ui| {
                    ui.label(format!(
                        "トラック {} に読み込むプラグインを選択 ({}):",
                        candidates.target_track + 1,
                        file_label(&candidates.path)
                    ));
                    for (i, plugin) in candidates.plugins.iter().enumerate() {
                        if ui
                            .button(format!("▶ {} ({})", plugin.name, plugin.id))
                            .clicked()
                        {
                            instantiate_index = Some(i);
                        }
                    }
                });
            }
            if let Some(i) = instantiate_index {
                let track = self.candidates.as_ref().map_or(0, |c| c.target_track);
                self.instantiate(i, track);
                // 選び終わったら候補を片付ける (選択行を残さない)
                self.candidates = None;
            }

            // ロード済みプラグインの操作 UI (トラックごと)
            let mut gui_error = None;
            for (index, track) in self.tracks.iter_mut().enumerate() {
                let Some(track) = track else { continue };
                ui.horizontal(|ui| {
                    ui.label(format!("トラック {}: {}", index + 1, track.name));

                    match &mut track.plugin {
                        // プラグイン独自 GUI の開閉ボタン
                        TrackPlugin::Clap(clap) => {
                            if let Some(gui) = &mut clap.gui {
                                if gui.supports_gui() {
                                    if !gui.is_open {
                                        let label = if gui.is_floating() {
                                            "エディタを開く (floating)"
                                        } else {
                                            "エディタを開く"
                                        };
                                        if ui.button(label).clicked() {
                                            if let Err(e) = gui.open(
                                                &mut clap.instance.plugin_handle(),
                                                &track.name,
                                                clap.sender.clone(),
                                            ) {
                                                gui_error =
                                                    Some(format!("GUI を開けません: {e}"));
                                            }
                                        }
                                    } else if ui.button("エディタを閉じる").clicked() {
                                        gui.close(&mut clap.instance.plugin_handle());
                                    }
                                }
                            }
                        }
                        TrackPlugin::Vst3(vst3) => {
                            ui.weak("VST3");
                            if vst3.gui.supports_gui() {
                                if !vst3.gui.is_open {
                                    if ui.button("エディタを開く").clicked() {
                                        let mut plugin = vst3.plugin.lock();
                                        if let Err(e) = vst3.gui.open(
                                            &mut plugin,
                                            &track.name,
                                            vst3.sender.clone(),
                                        ) {
                                            gui_error = Some(format!("GUI を開けません: {e}"));
                                        }
                                    }
                                } else if ui.button("エディタを閉じる").clicked() {
                                    let mut plugin = vst3.plugin.lock();
                                    vst3.gui.close(&mut plugin);
                                }
                            }
                        }
                    }
                });
            }
            if gui_error.is_some() {
                self.error = gui_error;
            }
            if !self.tracks.iter().flatten().count() == 0 {
                ui.add_space(8.0);
            }
            {

                // パラメータ汎用エディタ (一時的に無効化中)
                /*
                if loaded.params.is_empty() {
                    ui.label("(このプラグインには表示可能なパラメータがありません)");
                } else {
                    egui::ScrollArea::vertical()
                        .max_height((ui.available_height() - 160.0).max(60.0))
                        .show(ui, |ui| {
                            for param in &mut loaded.params {
                                let mut slider =
                                    egui::Slider::new(&mut param.value, param.min..=param.max)
                                        .text(&param.name);
                                if param.is_stepped {
                                    slider = slider.step_by(1.0);
                                }
                                if ui.add(slider).changed() {
                                    let _ = loaded.producer.push(GuiMsg::ParamValue {
                                        track: 0,
                                        id: param.id,
                                        value: param.value,
                                    });
                                }
                            }
                        });
                }
                */

                // 鍵盤 (一時的に無効化中)
                /*
                ui.add_space(12.0);
                ui.separator();
                ui.label("鍵盤 (クリックで発音):");
                ui.horizontal(|ui| {
                    for (key, label, is_black) in KEYS {
                        let color = if is_black {
                            egui::Color32::from_gray(40)
                        } else {
                            egui::Color32::from_gray(230)
                        };
                        let text_color = if is_black {
                            egui::Color32::WHITE
                        } else {
                            egui::Color32::BLACK
                        };
                        let button = egui::Button::new(
                            egui::RichText::new(label).color(text_color).size(11.0),
                        )
                        .fill(color)
                        .min_size(egui::vec2(34.0, 90.0));

                        let response = ui.add(button);
                        let is_down = response.is_pointer_button_down_on();
                        let was_down = loaded.pressed_keys.contains(&key);

                        if is_down && !was_down {
                            loaded.pressed_keys.insert(key);
                            let _ = loaded.producer.push(GuiMsg::NoteOn {
                                track: 0,
                                key,
                                velocity: 0.8,
                            });
                        } else if !is_down && was_down {
                            loaded.pressed_keys.remove(&key);
                            let _ = loaded
                                .producer
                                .push(GuiMsg::NoteOff { track: 0, key });
                        }
                    }
                });
                */
            }

            if self.tracks.iter().flatten().count() == 0 {
                ui.label("音色となるプラグインが未ロードです (左のトラック欄の「♪」から .clap を選択)。ロードしなくてもシーケンスの編集はできます。");
                ui.add_space(8.0);
            }

            // シーケンスエディタ (残りの領域全体を使う)。
            // プラグインは音色として使うだけなので、未ロードでも編集できるようにする。
            ui.separator();
            let sample_rate = self
                .engine
                .as_ref()
                .map_or(48_000.0, |engine| engine.config.sample_rate as f64);
            let spq = self.editor.editor.samples_per_quarter(sample_rate);
            // エンジンがあればトランスポートの位置が本体。無いときは
            // 自前で覚えている位置を使う (シークだけは効くようにする)。
            let playing = match &self.engine {
                Some(engine) => {
                    self.pos_quarters =
                        engine.transport_shared.pos.load(Ordering::Relaxed) as f64 / spq;
                    engine.transport_shared.playing.load(Ordering::Relaxed)
                }
                None => false,
            };

            // トラック欄に出す音源名を渡す
            self.editor.track_plugins = (0..self.editor.editor.track_count())
                .map(|track| {
                    self.tracks
                        .get(track)
                        .and_then(|slot| slot.as_ref())
                        .map(|audio| audio.name.clone())
                })
                .collect();

            let commands =
                editor_ui::editor_panel(ui, &mut self.editor, self.pos_quarters, playing);

            // ファイルダイアログは self 全体を触るので、ループを抜けてから実行する
            let mut file_action = None;
            let mut load_plugin_track = None;

            for command in commands {
                match command {
                    EditorCommand::ImportMidi => file_action = Some(FileAction::ImportMidi),
                    EditorCommand::ExportMidi => file_action = Some(FileAction::ExportMidi),
                    EditorCommand::OpenProject => file_action = Some(FileAction::OpenProject),
                    EditorCommand::SaveProject => file_action = Some(FileAction::SaveProject),
                    EditorCommand::SaveProjectAs => {
                        file_action = Some(FileAction::SaveProjectAs)
                    }
                    EditorCommand::ExportWav => file_action = Some(FileAction::ExportWav),
                    EditorCommand::ExportOpus => file_action = Some(FileAction::ExportOpus),
                    EditorCommand::ExportCcs => file_action = Some(FileAction::ExportCcs),
                    EditorCommand::LoadPlugin { track } => load_plugin_track = Some(track),
                    // エンジンが無いときは送り先がないので、再生ヘッドの移動だけ
                    // 自前で処理する。ロード時に位置とシーケンスを送り直す。
                    EditorCommand::Seek { quarters } if self.engine.is_none() => {
                        self.pos_quarters = quarters.max(0.0);
                    }
                    command => {
                        let Some(engine) = &mut self.engine else {
                            continue;
                        };
                        let msg = match command {
                            EditorCommand::Commit => {
                                // トラックごとに分けて送る (音源が別々のため)
                                let end_sample =
                                    (self.editor.editor.length_quarters_bar_aligned() as f64 * spq)
                                        as u64;
                                for track in 0..self.editor.editor.track_count() {
                                    // ミュート/ソロで鳴らさないトラックは空にして送る
                                    // (再生中でも即座に止まる)
                                    let events = if self.editor.editor.is_audible(track) {
                                        self.editor
                                            .editor
                                            .to_events_for_track(track, sample_rate)
                                            .into_boxed_slice()
                                    } else {
                                        Vec::new().into_boxed_slice()
                                    };
                                    let _ = engine.producer.push(GuiMsg::Transport(
                                        TransportMsg::SetSequence {
                                            track,
                                            events,
                                            end_sample,
                                        },
                                    ));
                                }
                                continue;
                            }
                            EditorCommand::Play => GuiMsg::Transport(TransportMsg::Play),
                            EditorCommand::Stop => GuiMsg::Transport(TransportMsg::Stop),
                            EditorCommand::Seek { quarters } => {
                                GuiMsg::Transport(TransportMsg::Seek {
                                    sample: (quarters * spq).max(0.0) as u64,
                                })
                            }
                            EditorCommand::SetLoop(enabled) => {
                                GuiMsg::Transport(TransportMsg::SetLoop { enabled })
                            }
                            // ファイル操作はループの外で処理済み
                            EditorCommand::ImportMidi
                            | EditorCommand::ExportMidi
                            | EditorCommand::OpenProject
                            | EditorCommand::SaveProject
                            | EditorCommand::SaveProjectAs
                            | EditorCommand::ExportWav
                            | EditorCommand::ExportOpus
                            | EditorCommand::ExportCcs
                            | EditorCommand::LoadPlugin { .. } => continue,
                        };
                        let _ = engine.producer.push(msg);
                    }
                }
            }

            match file_action {
                Some(FileAction::ImportMidi) => self.import_midi(),
                Some(FileAction::ExportMidi) => self.export_midi(),
                Some(FileAction::OpenProject) => self.open_project(),
                Some(FileAction::SaveProject) => self.save_project(false),
                Some(FileAction::SaveProjectAs) => self.save_project(true),
                Some(FileAction::ExportWav) => self.export_wav(),
                Some(FileAction::ExportOpus) => self.export_opus(),
                Some(FileAction::ExportCcs) => self.export_ccs(),
                None => {}
            }

            // トラック欄の「♪」。まず形式を聞く行を出す
            if let Some(track) = load_plugin_track {
                self.pending_load = Some(track);
                self.candidates = None;
            }

            // 形式が決まったらダイアログを開く。
            // 新しく読み込めたときだけ装填する (キャンセルでは何もしない)
            if let Some(chosen) = chosen_kind {
                let track = self.pending_load.take().unwrap_or(0);
                let opened = match chosen {
                    LoadChoice::Clap => self.open_clap_dialog(track),
                    LoadChoice::Vst3Bundle => self.open_vst3_dialog(track, true),
                    LoadChoice::Vst3File => self.open_vst3_dialog(track, false),
                    LoadChoice::Cancel => false,
                };
                if opened {
                    // 候補が1つだけならそのまま載せる (選択の手間を省く)
                    let single = self
                        .candidates
                        .as_ref()
                        .is_some_and(|candidates| candidates.plugins.len() == 1);
                    if single {
                        self.instantiate(0, track);
                        self.candidates = None;
                    }
                }
            }
        });

        // 結果通知は最前面に出したいので、パネルを描いたあとに重ねる
        notice_window(ctx, &mut self.notice);

        // 鍵盤の離鍵検出などのために定期的に再描画する。
        //
        // **間隔は実際のフレーム時間より長くすること。** eframe は「期限を過ぎた
        // 再描画要求」を見つけると `ControlFlow::Poll` に落とす
        // (eframe `native/run.rs` の `check_redraw_requests`)。vsync 60Hz の
        // フレームは約 16.7ms なので、16ms を要求すると**毎回すでに期限切れ**になり、
        // ループが一度も待機状態に入らなくなる。
        //
        // そうなると winit がメッセージ配送を捌ききれない。winit は
        // `RedrawRequested` を配送するたびに配送ループを打ち切る作りのため
        // (`interrupt_msg_dispatch`)、待機に入らない限りキューが常に捌き残る。
        // 割を食うのは**同じスレッドに貼り付いたプラグインのエディタ**で、
        // 単発のクリックは通るのにホバーやドラッグが効かなくなる
        // (Native Instruments の音源で発覚。経緯は docs/vst3_host_plan.md のフェーズ7)。
        //
        // エディタを開いていないときは誰も割を食わないので、そのまま滑らかに保つ。
        let interval = if self.any_editor_open() { 33 } else { 16 };
        ctx.request_repaint_after(Duration::from_millis(interval));
    }
}
