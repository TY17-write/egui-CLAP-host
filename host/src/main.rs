//! CLAP ミニホスト: .clap ファイルをロードして egui から鳴らすテスト用ホスト。

// GUI アプリなので、起動時に黒いコンソールを出さない (Windows のみ効く)。
// 引き換えに標準出力・標準エラーの行き先が無くなるため、cargo run で
// パニックのメッセージを見たいときは一時的に外すこと。
// smoke 系のバイナリはコンソールで使うものなので、こちらには付けない。
//#![windows_subsystem = "windows"]

use clap_host_test::{
    audio, ccs, discovery, editor_ui, gui, host, midi, opus, params, project, sequencer, theme, wav,
};

use audio::config::StreamAudioConfig;
use audio::graph;
use audio::offline::{RenderSetup, TAIL_SECONDS};
use audio::transport::{TransportMsg, TransportShared};
use audio::vst3::SharedPlugin;
use audio::GuiMsg;
use clack_host::prelude::*;
use crossbeam_channel::{Receiver, Sender};
use editor_ui::{EditorCommand, EditorState};
use eframe::egui;
use gui::{PluginGuiManager, Vst3GuiManager};
use host::{MainThreadMessage, MiniHost, MiniHostMainThread, MiniHostShared};
use params::ParamUi;
use project::PluginSnapshot;
use sequencer::SeqEvent;
use std::collections::HashSet;
use std::error::Error;
use std::ffi::CString;
use std::path::PathBuf;
use std::sync::atomic::Ordering;
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
    /// この候補から選んだプラグインを載せる段
    target_track: audio::NodeAddr,
}

/// オーディオエンジン (出力ストリーム1本と、それを操作する口)。
/// トラックより長生きし、音源はメッセージで出し入れする。
struct Engine {
    _stream: cpal::Stream,
    producer: rtrb::Producer<GuiMsg>,
    /// オーディオスレッドから返ってきた音源 (メインスレッドで deactivate する)。
    /// どのインスタンスへ返すか分かるようトラック番号が付く。
    retired: rtrb::Consumer<(audio::NodeAddr, audio::Node)>,
    /// 再生位置・再生中フラグの共有
    transport_shared: TransportShared,
    /// 全プラグイン共通のストリーム構成
    config: StreamAudioConfig,
}

/// チェーンの1段にロードされた音源。
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
    /// 処理を飛ばして素通しするか
    bypassed: bool,
    /// 入力・出力のチャンネル数。**どこでステレオが潰れるかを画面に出す**ために持つ
    /// (載せた時点でしか分からないので控えておく)。
    channels: (u16, u16),
    plugin: TrackPlugin,
}

/// オーディオトラック1本ぶんの、メインスレッド側の持ち物。
///
/// オーディオスレッド側の [`graph::AudioTrack`](audio::graph::AudioTrack) と
/// 対になる。**こちらが正**で、変更は必ずメッセージで向こうへ伝える。
#[derive(Default)]
struct AudioTrackUi {
    /// 上から順に通す。空なら何も鳴らない
    nodes: Vec<TrackAudio>,
    /// MIDI をどの打ち込みトラックから取るか。**`None` が未割り当て**
    /// (起動直後は全部これ)
    midi_track: Option<usize>,
    /// 送り先のオーディオトラック番号
    sends: Vec<usize>,
    gain: f32,
    /// `-1.0` (左) 〜 `1.0` (右)
    pan: f32,
    muted: bool,
    soloed: bool,
}

impl AudioTrackUi {
    /// 空のトラック。マスター以外はマスターへ送る
    fn new(index: usize) -> Self {
        Self {
            sends: if index == graph::MASTER {
                Vec::new()
            } else {
                vec![graph::MASTER]
            },
            gain: 1.0,
            ..Default::default()
        }
    }

    /// 画面に出す名前 (先頭の段の音源名)
    fn label(&self) -> &str {
        match self.nodes.first() {
            Some(node) => &node.name,
            None => "—",
        }
    }
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

/// オーディオトラック一覧で、名前と送り先に取る幅。
///
/// **固定にしてある。** 中身の文字数で幅が変わると、行ごとに音量や M/S の
/// 位置がずれて押しにくくなる。入りきらない名前は端を詰めて、
/// ホバーで全体を出す。
const TRACK_NAME_W: f32 = 118.0;
const TRACK_SENDS_W: f32 = 74.0;

/// 詳細ウィンドウで、段の名前とチャンネル表記に取る幅 (理由は上と同じ)
const NODE_NAME_W: f32 = 150.0;
const NODE_CHANNELS_W: f32 = 56.0;

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
    /// 「+」を押したあと、音源の形式を選んでもらっている最中の段。
    /// 形式ごとにダイアログの出し方が違うので、先に [`LoadChoice`] を決める必要がある。
    pending_load: Option<audio::NodeAddr>,
    candidates: Option<Candidates>,
    // 宣言順にドロップされるため、ストリームをインスタンスより先に止める
    engine: Option<Engine>,
    /// オーディオトラック ([`graph::AUDIO_TRACKS`] 本)。空のときは
    /// [`ensure_audio_tracks`](App::ensure_audio_tracks) が用意する
    audio_tracks: Vec<AudioTrackUi>,
    /// 詳細ウィンドウを出しているオーディオトラック
    detail_track: Option<usize>,
    /// 外したが、まだノードが返ってきていないインスタンス。
    /// 返却時に正しいインスタンスへ deactivate するため、ここで生かしておく。
    /// 添字は**オーディオトラック番号**で、同じトラック内では入れた順に返る。
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
    /// 画面中央に出す結果通知 (閉じるまで残る)。
    ///
    /// **結果の知らせはここに一本化してある。** 以前は上部に status 行を出して
    /// いたが、同じ内容を通知にも出していたうえ、上部パネルごと畳んだため。
    notice: Option<Notice>,
}

impl App {
    /// .clap を選ばせて候補を読み込む。
    /// 戻り値は「候補を新しく読み込めたか」。キャンセルや失敗では false を返し、
    /// 前回の候補には触れない (キャンセルで前のプラグインが載らないようにするため)。
    fn open_clap_dialog(&mut self, target_track: audio::NodeAddr) -> bool {
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
    fn open_vst3_dialog(&mut self, target_track: audio::NodeAddr, bundle: bool) -> bool {
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
        self.audio_tracks
            .iter()
            .flat_map(|track| track.nodes.iter())
            .any(|node| match &node.plugin {
                TrackPlugin::Clap(clap) => clap.gui.as_ref().is_some_and(|gui| gui.is_open),
                TrackPlugin::Vst3(vst3) => vst3.gui.is_open,
            })
    }

    /// 数え上げた結果を候補として受け取る (形式によらず共通)
    fn accept_candidates(
        &mut self,
        kind: project::PluginKind,
        path: PathBuf,
        target_track: audio::NodeAddr,
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
        let Some(path) = dialog.pick_file() else {
            return;
        };
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
                self.notice = Some(Notice::ok(
                    "MIDI を読み込みました",
                    if cc_lanes > 0 {
                        format!(
                            "{name}\n\n{count} 個のノートと {cc_lanes} 本の CC 段を読み込みました"
                        )
                    } else {
                        format!("{name}\n\n{count} 個のノートを読み込みました")
                    },
                ));
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
        let snapshots = self.audio_track_snapshots();
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
        let Some(path) = dialog.pick_file() else {
            return;
        };

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
                let wanted = loaded
                    .audio_tracks
                    .iter()
                    .filter(|track| !track.nodes.is_empty())
                    .count();

                self.editor.replace_project(loaded.editor);
                // 音源はシーケンスを入れ替えたあとに載せる
                // (トラック数が揃ってからでないと行き先が決まらない)
                let mut failures = self.restore_audio_tracks(loaded.audio_tracks);
                // 音源を載せる過程でエンジンが起きるので、そのあとに送る
                self.push_routing();
                // 16本に収まらず載せられなかったぶんも同じ扱いで知らせる
                failures.extend(loaded.overflow);

                self.set_project_path(path.clone());
                self.error = None;

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
                    body.push_str(
                        "\nそのトラックは音源なしになっています (ノートは残っています)。\n",
                    );
                    body.push_str(&failures.join("\n"));
                    self.notice = Some(Notice::error("プロジェクトを一部だけ開きました", body));
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

    /// 今の繋ぎ方を組み立てる。
    ///
    /// **組めなければ既定 (全部マスターへ直結) に戻す。** 読み込みの時点で
    /// 検証してあるので普通は起きないが、鳴らなくなるより既定で鳴るほうがよい。
    fn routing(&self) -> graph::Routing {
        if self.audio_tracks.is_empty() {
            return graph::Routing::default();
        }
        let lists: Vec<Vec<usize>> = self
            .audio_tracks
            .iter()
            .map(|track| track.sends.clone())
            .collect();
        graph::Routing::from_lists(&lists).unwrap_or_else(|problems| {
            eprintln!(
                "繋ぎ方を組めないので既定に戻します:\n{}",
                problems.join("\n")
            );
            graph::Routing::default()
        })
    }

    /// オーディオトラックの器を用意する (`Default` では空なので)
    fn ensure_audio_tracks(&mut self) {
        if self.audio_tracks.len() == graph::AUDIO_TRACKS {
            return;
        }
        self.audio_tracks = (0..graph::AUDIO_TRACKS).map(AudioTrackUi::new).collect();
    }

    /// 繋ぎ方・音量・ミュート/ソロをまとめたもの。
    ///
    /// **これらを触る画面はまだ無い**ので、今は保存されている値
    /// (既定は等倍・中央・全部鳴る) をそのまま組み立てている。
    fn mixer(&self) -> graph::Mixer {
        let mut gain = [1.0f32; graph::AUDIO_TRACKS];
        let mut pan = [0.0f32; graph::AUDIO_TRACKS];
        let mut muted = 0u16;
        let mut soloed = 0u16;

        for (index, track) in self
            .audio_tracks
            .iter()
            .take(graph::AUDIO_TRACKS)
            .enumerate()
        {
            gain[index] = track.gain;
            pan[index] = track.pan;
            if track.muted {
                muted |= 1 << index;
            }
            if track.soloed {
                soloed |= 1 << index;
            }
        }
        graph::Mixer::build(self.routing(), &gain, &pan, muted, soloed)
    }

    /// 送り先を1本つけ外しする。**輪になるなら断って理由を返す。**
    ///
    /// 繋ぐ操作を断るのがいちばん安い (実行時に見つける形にすると、
    /// オーディオスレッドで探索することになる)。
    fn toggle_send(&mut self, from: usize, to: usize) -> Result<(), String> {
        self.ensure_audio_tracks();
        let Some(track) = self.audio_tracks.get_mut(from) else {
            return Err("そのトラックはありません".into());
        };
        let had = track.sends.contains(&to);
        if had {
            track.sends.retain(|target| *target != to);
        } else {
            track.sends.push(to);
            track.sends.sort_unstable();
        }

        // 組めるか試す。駄目なら元へ戻す
        let lists: Vec<Vec<usize>> = self
            .audio_tracks
            .iter()
            .map(|track| track.sends.clone())
            .collect();
        if let Err(problems) = graph::Routing::from_lists(&lists) {
            let track = &mut self.audio_tracks[from];
            if had {
                track.sends.push(to);
                track.sends.sort_unstable();
            } else {
                track.sends.retain(|target| *target != to);
            }
            return Err(problems.join("\n"));
        }

        self.push_routing();
        Ok(())
    }

    /// 繋ぎ方と音量をオーディオスレッドへ送る。
    ///
    /// **音源を載せ替えても繋ぎ方は変わらない**ので、送るのは繋ぎ方が
    /// 変わったときとエンジンを起こしたときだけでよい。
    fn push_routing(&mut self) {
        let mixer = self.mixer();
        if let Some(engine) = self.engine.as_mut() {
            let _ = engine.producer.push(GuiMsg::SetMixer(mixer));
        }
    }

    /// 借りたノードをオーディオスレッドへ返す。
    ///
    /// **返さないと音が出なくなる。** 途中で諦めるときも必ず通ること。
    fn return_processors(&mut self, nodes: Vec<(audio::NodeAddr, audio::Node)>) {
        let Some(engine) = self.engine.as_mut() else {
            return;
        };
        for (addr, node) in nodes {
            let _ = engine.producer.push(GuiMsg::SetMidiTrack {
                track: addr.track,
                midi_track: graph::midi_track_for(addr.track),
            });
            let _ = engine.producer.push(GuiMsg::SetNode {
                addr,
                node: Box::new(node),
            });
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

        // 借りるのは (オーディオトラック, 段) の組。番号はオーディオトラックのもの
        let loaded: Vec<audio::NodeAddr> = self
            .audio_tracks
            .iter()
            .enumerate()
            .flat_map(|(track, slot)| {
                (0..slot.nodes.len()).map(move |at| audio::NodeAddr { track, at })
            })
            .collect();

        // ---- 借りる ----
        // engine の借用はここで終える (このあと self.audio_tracks を触るため)
        let mut processors = {
            let Some(engine) = self.engine.as_mut() else {
                return Err("音源が未ロードです。".into());
            };
            let _ = engine.producer.push(GuiMsg::Transport(TransportMsg::Stop));
            // **後ろの段から外す。** 前から外すと後ろが繰り上がって番号がずれる
            for addr in loaded.iter().rev() {
                let _ = engine.producer.push(GuiMsg::RemoveNode { addr: *addr });
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

        // 借りた処理器をグラフへ載せて回す。**再生と同じ経路を通すため**
        let setup = self.render_setup(&export_config);
        let mut graph = audio::graph::Graph::new();
        // **繋ぎ方も音量も再生と同じにする。** ここを既定のままにすると、
        // 繋ぎ替えたときに鳴っている音と書き出した音が食い違う
        graph.set_mixer(self.mixer());
        // **段の順に組み直す。** 借りるときに後ろから外したので、並びは逆になっている
        processors.sort_by_key(|(addr, _)| (addr.track, addr.at));
        let mut chains: Vec<Vec<audio::Node>> =
            (0..graph::AUDIO_TRACKS).map(|_| Vec::new()).collect();
        for (addr, node) in processors {
            if let Some(chain) = chains.get_mut(addr.track) {
                chain.push(node);
            }
        }
        for (track, chain) in chains.into_iter().enumerate() {
            let midi_track = self
                .audio_tracks
                .get(track)
                .and_then(|slot| slot.midi_track);
            graph.place_chain(track, midi_track, chain);
        }
        let rendered = audio::offline::render(&mut graph, setup);
        let mut processors = graph.take_nodes();

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
            if let Some(slot) = self.audio_tracks.get_mut(*track) {
                slot.nodes.clear();
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
            .audio_tracks
            .iter()
            .enumerate()
            .filter_map(|(track, slot)| (!slot.nodes.is_empty()).then_some(track))
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
                    self.notice = Some(Notice::error("WAV の一部を書き出せませんでした", body));
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
        if self.audio_tracks.iter().all(|track| track.nodes.is_empty()) {
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
                body.push_str(&format!("\n・オーディオトラック {track}"));
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
                self.notice = Some(Notice::ok("CCS を書き出しました", body));
            }
            Err(e) => {
                self.notice = Some(Notice::error(
                    "CCS を書き出せません",
                    format!("保存できません:\n{e}"),
                ));
            }
        }
    }

    /// 保存先を選ばせる。拡張子を省略されたら補う。
    fn ask_save_path(
        &mut self,
        filter: &str,
        extension: &str,
        default_name: &str,
    ) -> Option<PathBuf> {
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
    /// 戻り値は (動かし直せた処理器, 失敗した**打ち込み**トラック番号)。
    /// 処理器の番号はオーディオトラックのものだが、失敗の報告は画面に出すので
    /// 打ち込み側の番号へ直して返す。
    ///
    /// **失敗したトラックの処理器は失われる。** CLAP は deactivate まで進んだあとの
    /// activate で落ちうるためで、そのトラックは呼び出し側が音源ごと外すこと
    /// (黙って鳴らないままにしない)。
    fn switch_processors_rate(
        &mut self,
        nodes: Vec<(audio::NodeAddr, audio::Node)>,
        config: &StreamAudioConfig,
    ) -> (Vec<(audio::NodeAddr, audio::Node)>, Vec<usize>) {
        let mut switched = Vec::with_capacity(nodes.len());
        let mut failed = Vec::new();

        for (addr, node) in nodes {
            match self.switch_one_rate(addr, node, config) {
                Ok(node) => switched.push((addr, node)),
                Err(e) => {
                    eprintln!(
                        "オーディオトラック {} のレート切り替えに失敗: {e}",
                        addr.track
                    );
                    failed.push(addr.track);
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
        addr: audio::NodeAddr,
        node: audio::Node,
        config: &StreamAudioConfig,
    ) -> Result<audio::Node, Box<dyn Error>> {
        let Some(audio) = self
            .audio_tracks
            .get_mut(addr.track)
            .and_then(|slot| slot.nodes.get_mut(addr.at))
        else {
            return Err("その段に音源がありません".into());
        };

        match (node.into_retired(), &mut audio.plugin) {
            (audio::RetiredProcessor::Clap(stopped), TrackPlugin::Clap(clap)) => {
                clap.instance.deactivate(stopped);
                audio::activate_node(&mut clap.instance, config)
            }
            (audio::RetiredProcessor::Vst3(shared), TrackPlugin::Vst3(_)) => {
                audio::reconfigure_vst3_node(shared, config)
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
        while let Ok((addr, node)) = engine.retired.pop() {
            let track = addr.track;

            // 外したインスタンスが待っていればそちらへ返す。
            // 待っていない場合 (書き出しの借り出しが時間切れになったときなど) は
            // 今その段に載っているものが当人なので、欄から降ろして始末する
            // (止めた音源を載ったままにすると、鳴らないトラックが残る)。
            //
            // **同じトラック内では入れた順に返る**ので、いちばん古いものを取る
            let waiting = self.retiring.iter().position(|(index, _)| *index == track);
            let mut owner = match waiting {
                Some(at) => self.retiring.remove(at).map(|(_, old)| old),
                None => self
                    .audio_tracks
                    .get_mut(track)
                    .filter(|slot| addr.at < slot.nodes.len())
                    .map(|slot| slot.nodes.remove(addr.at)),
            };

            // 形式ごとに始末の仕方が違う。**返ってくるのは1段ぶん**
            {
                match node.into_retired() {
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

    /// 選んだプラグインを指定の段に載せる (詳細ウィンドウからの操作)
    fn instantiate(&mut self, plugin_index: usize, track: audio::NodeAddr) {
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

    /// 指定の段に音源を載せる。`state` があれば復元してから鳴らせる状態にする。
    ///
    /// 呼び出し元は2つ。詳細ウィンドウからの選択と、プロジェクトを開いたとき。
    /// 戻り値は「状態を戻せたか」で、`state` が無いときは常に true。
    ///
    /// **`track.at` が段数と同じなら末尾へ足す。** それより後ろは受け付けない。
    fn load_plugin(
        &mut self,
        track: audio::NodeAddr,
        kind: project::PluginKind,
        path: &std::path::Path,
        id: &str,
        state: Option<&[u8]>,
    ) -> Result<bool, String> {
        self.ensure_audio_tracks();
        if track.track >= graph::AUDIO_TRACKS {
            return Err(format!(
                "オーディオトラック {} はありません ({} 本まで)",
                track.track,
                graph::AUDIO_TRACKS
            ));
        }
        if track.at >= graph::MAX_NODES {
            return Err(format!(
                "1本のトラックに刺せるのは {} 段までです",
                graph::MAX_NODES
            ));
        }

        // 最初のロード時にストリームを用意する
        if self.engine.is_none() {
            let engine = start_engine().map_err(|e| format!("オーディオを開始できません: {e}"))?;
            self.engine = Some(engine);
            // 起こしたばかりのエンジンは既定の繋ぎ方なので、今のものを送る
            self.push_routing();
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

        let (mut audio_track, node) = match kind {
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

        let _ = engine.producer.push(GuiMsg::SetNode {
            addr: track,
            node: Box::new(node),
        });

        // 未ロード中に動かした再生ヘッドの位置を引き継ぐ
        let spq = self
            .editor
            .editor
            .samples_per_quarter(engine.config.sample_rate as f64);
        let sample = (self.pos_quarters * spq).max(0.0) as u64;
        let _ = engine
            .producer
            .push(GuiMsg::Transport(TransportMsg::Seek { sample }));

        // 前の音源は、処理器が返ってくるまで生かしておく
        let nodes = &mut self.audio_tracks[track.track].nodes;
        if track.at < nodes.len() {
            let previous = std::mem::replace(&mut nodes[track.at], audio_track);
            self.retiring.push_back((track.track, previous));
        } else {
            nodes.push(audio_track);
        }
        // 新しいプラグインにシーケンスを送り直す
        self.editor.dirty = true;

        Ok(restored)
    }

    /// 段を外す
    fn remove_node(&mut self, addr: audio::NodeAddr) {
        let Some(slot) = self.audio_tracks.get_mut(addr.track) else {
            return;
        };
        if addr.at >= slot.nodes.len() {
            return;
        }
        let previous = slot.nodes.remove(addr.at);
        self.retiring.push_back((addr.track, previous));
        if let Some(engine) = self.engine.as_mut() {
            let _ = engine.producer.push(GuiMsg::RemoveNode { addr });
        }
    }

    /// 段を並べ替える。**パラメータの宛先は段の番号**なので向こうにも伝える
    fn move_node(&mut self, track: usize, from: usize, to: usize) {
        let Some(slot) = self.audio_tracks.get_mut(track) else {
            return;
        };
        if from >= slot.nodes.len() || to >= slot.nodes.len() || from == to {
            return;
        }
        let node = slot.nodes.remove(from);
        slot.nodes.insert(to, node);
        if let Some(engine) = self.engine.as_mut() {
            let _ = engine.producer.push(GuiMsg::MoveNode { track, from, to });
        }
    }

    /// 段を素通しにするか
    fn set_bypassed(&mut self, addr: audio::NodeAddr, bypassed: bool) {
        let Some(node) = self
            .audio_tracks
            .get_mut(addr.track)
            .and_then(|slot| slot.nodes.get_mut(addr.at))
        else {
            return;
        };
        node.bypassed = bypassed;
        if let Some(engine) = self.engine.as_mut() {
            let _ = engine.producer.push(GuiMsg::SetBypassed { addr, bypassed });
        }
    }

    /// どの打ち込みトラックから MIDI を取るかを決める
    fn set_midi_track(&mut self, track: usize, midi_track: Option<usize>) {
        let Some(slot) = self.audio_tracks.get_mut(track) else {
            return;
        };
        slot.midi_track = midi_track;
        if let Some(engine) = self.engine.as_mut() {
            let _ = engine
                .producer
                .push(GuiMsg::SetMidiTrack { track, midi_track });
        }
        // 割り当てが変わったらシーケンスを送り直す
        self.editor.dirty = true;
    }

    /// 音源の形式を選ばせるポップアップ。
    ///
    /// ダイアログの出し方が形式で違うので先に決めてもらう
    /// (.clap はファイル、.vst3 はバンドルディレクトリと単体ファイルの2通り)。
    ///
    /// **画面中央に出す。** 上部に行として出していたときは、押す場所が
    /// 画面の隅にあって遠かった。
    fn load_choice_popup(&mut self, ctx: &egui::Context) -> Option<LoadChoice> {
        let track = self.pending_load?;
        let mut chosen = None;

        egui::Window::new("音源を読み込む")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
            .show(ctx, |ui| {
                ui.set_max_width(360.0);
                ui.label(format!(
                    "オーディオトラック {} の {} 段目に読み込みます。",
                    track.track,
                    track.at + 1
                ));
                ui.add_space(6.0);
                ui.weak("形式によってダイアログの出し方が違うので、先に選んでください");
                ui.add_space(8.0);

                if ui
                    .add_sized([340.0, 26.0], egui::Button::new("CLAP (.clap ファイル)"))
                    .clicked()
                {
                    chosen = Some(LoadChoice::Clap);
                }
                if ui
                    .add_sized([340.0, 26.0], egui::Button::new("VST3 (.vst3 フォルダ)"))
                    .on_hover_text("現行の標準。フォルダ選択が開きます")
                    .clicked()
                {
                    chosen = Some(LoadChoice::Vst3Bundle);
                }
                if ui
                    .add_sized(
                        [340.0, 26.0],
                        egui::Button::new("VST3 (.vst3 単体ファイル)"),
                    )
                    .on_hover_text(
                        "フォルダになっていない古い形式。\
                         バンドルの中の DLL を直接指すのにも使えます",
                    )
                    .clicked()
                {
                    chosen = Some(LoadChoice::Vst3File);
                }

                ui.add_space(6.0);
                ui.separator();
                if ui.button("やめる").clicked() {
                    chosen = Some(LoadChoice::Cancel);
                }
                ui.weak("(Esc でも閉じます)");
            });

        if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            chosen = Some(LoadChoice::Cancel);
        }
        chosen
    }

    /// 1つのファイルに複数入っていたときの選択ポップアップ。
    /// 1つだけのファイルは選ばせずにそのまま載るので出さない。
    fn plugin_choice_popup(&mut self, ctx: &egui::Context) {
        let Some(candidates) = self
            .candidates
            .as_ref()
            .filter(|candidates| candidates.plugins.len() > 1)
        else {
            return;
        };
        let target = candidates.target_track;
        let file = file_label(&candidates.path);
        let plugins: Vec<(String, String)> = candidates
            .plugins
            .iter()
            .map(|plugin| (plugin.name.clone(), plugin.id.clone()))
            .collect();

        let mut chosen = None;
        let mut cancel = false;

        egui::Window::new("プラグインを選ぶ")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
            .show(ctx, |ui| {
                ui.set_max_width(420.0);
                ui.label(format!(
                    "オーディオトラック {} の {} 段目 — {file}",
                    target.track,
                    target.at + 1
                ));
                ui.weak("このファイルには複数のプラグインが入っています");
                ui.add_space(8.0);

                for (index, (name, id)) in plugins.iter().enumerate() {
                    if ui
                        .add_sized([400.0, 26.0], egui::Button::new(format!("▶ {name}")))
                        .on_hover_text(id)
                        .clicked()
                    {
                        chosen = Some(index);
                    }
                }

                ui.add_space(6.0);
                ui.separator();
                if ui.button("やめる").clicked() {
                    cancel = true;
                }
                ui.weak("(Esc でも閉じます)");
            });

        if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            cancel = true;
        }
        if let Some(index) = chosen {
            self.instantiate(index, target);
            self.candidates = None;
        } else if cancel {
            self.candidates = None;
        }
    }

    /// オーディオトラックの窓を出す。
    ///
    /// 一覧は常設で**ルーティング・音量・パン・ミュート/ソロ**だけを扱い、
    /// **プラグインの管理は詳細ウィンドウ**が受け持つ。
    /// 戻り値は「音源を載せたい段」(押されたときだけ)。
    fn audio_track_windows(&mut self, ctx: &egui::Context) -> Option<audio::NodeAddr> {
        self.ensure_audio_tracks();
        let mixer = self.mixer();
        let midi_tracks = self.editor.editor.track_count();

        let mut open_detail = None;
        let mut toggle_send = None;
        let mut mixer_changed = false;

        // **初期位置は右上。** 既定 (左上) だとエディタのツールバーに重なる。
        // `default_pos` なので、動かしたあとはその場所を覚える
        const LIST_W: f32 = 430.0;
        let right_top = {
            let screen = ctx.screen_rect();
            egui::pos2(screen.right() - LIST_W - 12.0, screen.top() + 12.0)
        };

        egui::Window::new("オーディオトラック")
            .default_width(LIST_W)
            .default_pos(right_top)
            .show(ctx, |ui| {
                ui.weak("0 はマスター。ここの出力がそのまま最終出力になる");
                ui.add_space(4.0);

                egui::ScrollArea::vertical().show(ui, |ui| {
                    for index in 0..graph::AUDIO_TRACKS {
                        // **マスターへ辿り着けないトラックは鳴らない。**
                        // 「繋がっていなければ鳴らない」を仕様にしたぶん、
                        // 画面で分かるようにする必要がある
                        let silent = !mixer.routing.reaches_master(index);
                        let frame = if silent {
                            egui::Frame::NONE
                                .stroke(egui::Stroke::new(1.0_f32, theme::palette::RED))
                                .inner_margin(2.0)
                        } else {
                            egui::Frame::NONE.inner_margin(2.0)
                        };

                        frame.show(ui, |ui| {
                            ui.horizontal(|ui| {
                                let track = &mut self.audio_tracks[index];
                                ui.monospace(format!("{index:>2}"));

                                // **幅を固定する。** 名前の長さで後ろのつまみが
                                // 動くと、行ごとに押す位置が変わって使いづらい
                                let name = track.label().to_string();
                                if ui
                                    .add_sized(
                                        [TRACK_NAME_W, 20.0],
                                        egui::Button::new(egui::RichText::new(&name).size(12.0))
                                            .truncate(),
                                    )
                                    .on_hover_text(format!("{name} (クリックで詳細)"))
                                    .clicked()
                                {
                                    open_detail = Some(index);
                                }

                                // 送り先。マスターは送り側にならないので出さないが、
                                // **場所は空けておく** (列がずれないように)
                                if index == graph::MASTER {
                                    ui.add_space(TRACK_SENDS_W + ui.spacing().item_spacing.x);
                                } else {
                                    let label = if track.sends.is_empty() {
                                        "→ なし".to_string()
                                    } else {
                                        let list: Vec<String> =
                                            track.sends.iter().map(|to| to.to_string()).collect();
                                        format!("→ {}", list.join(","))
                                    };
                                    let sends = &track.sends;
                                    ui.allocate_ui(egui::vec2(TRACK_SENDS_W, 20.0), |ui| {
                                        ui.menu_button(label, |ui| {
                                            for target in 0..graph::AUDIO_TRACKS {
                                                if target == index {
                                                    continue;
                                                }
                                                let mut on = sends.contains(&target);
                                                if ui
                                                    .checkbox(&mut on, format!("{target}"))
                                                    .changed()
                                                {
                                                    toggle_send = Some((index, target));
                                                }
                                            }
                                        })
                                        .response
                                        .on_hover_text("送り先 (複数選ぶと足し合わさる)");
                                    });
                                }

                                // 音量とパン
                                if ui
                                    .add(
                                        egui::DragValue::new(&mut track.gain)
                                            .speed(0.01)
                                            .range(0.0..=2.0)
                                            .prefix("×"),
                                    )
                                    .changed()
                                {
                                    mixer_changed = true;
                                }
                                if ui
                                    .add(
                                        egui::DragValue::new(&mut track.pan)
                                            .speed(0.01)
                                            .range(-1.0..=1.0)
                                            .prefix("P"),
                                    )
                                    .on_hover_text("パン (-1 左 〜 1 右)")
                                    .changed()
                                {
                                    mixer_changed = true;
                                }

                                if ui.toggle_value(&mut track.muted, "M").changed() {
                                    mixer_changed = true;
                                }
                                if ui
                                    .toggle_value(&mut track.soloed, "S")
                                    .on_hover_text("ソロ。マスターへ至る経路は残る")
                                    .changed()
                                {
                                    mixer_changed = true;
                                }

                                if silent {
                                    ui.colored_label(theme::palette::RED, "鳴りません")
                                        .on_hover_text("マスターへ辿り着けません");
                                }
                            });
                        });
                    }
                });
            });

        if let Some((from, to)) = toggle_send {
            if let Err(problems) = self.toggle_send(from, to) {
                self.notice = Some(Notice::error("その繋ぎ方はできません", problems));
            }
        }
        if mixer_changed {
            self.push_routing();
        }
        if let Some(index) = open_detail {
            self.detail_track = Some(index);
        }

        self.audio_track_detail(ctx, midi_tracks)
    }

    /// 選んでいる1本の詳細 (チェーンの管理)
    fn audio_track_detail(
        &mut self,
        ctx: &egui::Context,
        midi_tracks: usize,
    ) -> Option<audio::NodeAddr> {
        let index = self.detail_track?;
        if index >= self.audio_tracks.len() {
            self.detail_track = None;
            return None;
        }

        let mut add_node = None;
        let mut remove = None;
        let mut moved = None;
        let mut bypass = None;
        let mut midi_choice = None;
        let mut gui_error = None;
        let mut open = true;

        // 一覧の左隣に出す (一覧は右上なので、重ならない位置)
        const DETAIL_W: f32 = 420.0;
        let beside_list = {
            let screen = ctx.screen_rect();
            egui::pos2(
                (screen.right() - 430.0 - DETAIL_W - 24.0).max(screen.left() + 12.0),
                screen.top() + 12.0,
            )
        };

        egui::Window::new(format!("オーディオトラック {index}"))
            .default_width(DETAIL_W)
            .default_pos(beside_list)
            .open(&mut open)
            .show(ctx, |ui| {
                // ---- MIDI の割り当て ----
                // マスターは音を受けるだけなので、打ち込みを割り当てても意味がない
                if index != graph::MASTER {
                    ui.horizontal(|ui| {
                        ui.label("MIDI:");
                        let current = self.audio_tracks[index].midi_track;
                        let label = match current {
                            Some(track) => format!("トラック {}", track + 1),
                            None => "未割り当て".to_string(),
                        };
                        egui::ComboBox::from_id_salt(("midi", index))
                            .selected_text(label)
                            .show_ui(ui, |ui| {
                                if ui
                                    .selectable_label(current.is_none(), "未割り当て")
                                    .clicked()
                                {
                                    midi_choice = Some(None);
                                }
                                for track in 0..midi_tracks {
                                    let selected = current == Some(track);
                                    if ui
                                        .selectable_label(
                                            selected,
                                            format!("トラック {}", track + 1),
                                        )
                                        .clicked()
                                    {
                                        midi_choice = Some(Some(track));
                                    }
                                }
                            });
                        if self.audio_tracks[index].midi_track.is_none() {
                            ui.weak("(音源を鳴らすには割り当てが要ります)");
                        }
                    });
                    ui.separator();
                }

                // ---- チェーン ----
                let count = self.audio_tracks[index].nodes.len();
                if count == 0 {
                    ui.weak("何も刺さっていません");
                }
                for at in 0..count {
                    let node = &mut self.audio_tracks[index].nodes[at];
                    ui.horizontal(|ui| {
                        ui.monospace(format!("{}.", at + 1));
                        // 幅を固定して、名前の長さでボタンの位置が動かないようにする
                        let name = node.name.clone();
                        ui.add_sized(
                            [NODE_NAME_W, 20.0],
                            egui::Label::new(&name).truncate().halign(egui::Align::LEFT),
                        )
                        .on_hover_text(&name);
                        // **どこでステレオが潰れるかを見えるように**
                        ui.add_sized(
                            [NODE_CHANNELS_W, 20.0],
                            egui::Label::new(
                                egui::RichText::new(format!(
                                    "{}→{}ch",
                                    node.channels.0, node.channels.1
                                ))
                                .weak(),
                            ),
                        );

                        let mut bypassed = node.bypassed;
                        if ui
                            .toggle_value(&mut bypassed, "B")
                            .on_hover_text("バイパス (処理を飛ばして素通し)")
                            .changed()
                        {
                            bypass = Some((at, bypassed));
                        }
                        if ui.add_enabled(at > 0, egui::Button::new("▲")).clicked() {
                            moved = Some((at, at - 1));
                        }
                        if ui
                            .add_enabled(at + 1 < count, egui::Button::new("▼"))
                            .clicked()
                        {
                            moved = Some((at, at + 1));
                        }
                        if ui.button("✕").on_hover_text("この段を外す").clicked() {
                            remove = Some(at);
                        }

                        // プラグイン独自 GUI
                        let name = node.name.clone();
                        match &mut node.plugin {
                            TrackPlugin::Clap(clap) => {
                                if let Some(gui) = &mut clap.gui {
                                    if gui.supports_gui() {
                                        if !gui.is_open {
                                            if ui.button("エディタ").clicked() {
                                                if let Err(e) = gui.open(
                                                    &mut clap.instance.plugin_handle(),
                                                    &name,
                                                    clap.sender.clone(),
                                                ) {
                                                    gui_error =
                                                        Some(format!("GUI を開けません: {e}"));
                                                }
                                            }
                                        } else if ui.button("閉じる").clicked() {
                                            gui.close(&mut clap.instance.plugin_handle());
                                        }
                                    }
                                }
                            }
                            TrackPlugin::Vst3(vst3) => {
                                ui.weak("VST3");
                                if vst3.gui.supports_gui() {
                                    if !vst3.gui.is_open {
                                        if ui.button("エディタ").clicked() {
                                            let mut plugin = vst3.plugin.lock();
                                            if let Err(e) = vst3.gui.open(
                                                &mut plugin,
                                                &name,
                                                vst3.sender.clone(),
                                            ) {
                                                gui_error = Some(format!("GUI を開けません: {e}"));
                                            }
                                        }
                                    } else if ui.button("閉じる").clicked() {
                                        let mut plugin = vst3.plugin.lock();
                                        vst3.gui.close(&mut plugin);
                                    }
                                }
                            }
                        }
                    });
                }

                ui.add_space(4.0);
                if ui
                    .add_enabled(
                        count < graph::MAX_NODES,
                        egui::Button::new("＋ 音源 / エフェクトを足す"),
                    )
                    .clicked()
                {
                    add_node = Some(audio::NodeAddr {
                        track: index,
                        at: count,
                    });
                }
                ui.weak("上から順に通ります。入力を持たない段 (音源) は、そこまでの音を捨てます");
            });

        if !open {
            self.detail_track = None;
        }
        if let Some(choice) = midi_choice {
            self.set_midi_track(index, choice);
        }
        if let Some((at, bypassed)) = bypass {
            self.set_bypassed(audio::NodeAddr { track: index, at }, bypassed);
        }
        if let Some((from, to)) = moved {
            self.move_node(index, from, to);
        }
        if let Some(at) = remove {
            self.remove_node(audio::NodeAddr { track: index, at });
        }
        if gui_error.is_some() {
            self.error = gui_error;
        }
        add_node
    }

    /// 保存用に、各オーディオトラックの中身を集める
    fn audio_track_snapshots(&mut self) -> Vec<project::AudioTrackSnapshot> {
        self.ensure_audio_tracks();
        self.audio_tracks
            .iter_mut()
            .map(|track| project::AudioTrackSnapshot {
                name: String::new(),
                nodes: track
                    .nodes
                    .iter_mut()
                    .map(|node| PluginSnapshot {
                        kind: node.kind(),
                        path: node.path.clone(),
                        id: node.id.clone(),
                        state: node.capture_state(),
                        bypassed: node.bypassed,
                    })
                    .collect(),
                midi_track: track.midi_track,
                // **繋ぎ方と音量は音源の有無と無関係。** 空のトラックのぶんも残す
                // (繋ぎ替えてから音源を外しても、繋ぎ方は保たれる)
                sends: track.sends.clone(),
                gain: track.gain,
                pan: track.pan,
                muted: track.muted,
                soloed: track.soloed,
            })
            .collect()
    }

    /// 今載っている音源を全部降ろす (プロジェクトを開く前の片付け)
    fn unload_all_plugins(&mut self) {
        for track in 0..self.audio_tracks.len() {
            // **後ろの段から外す。** 前から外すと後ろが繰り上がって番号がずれる
            for at in (0..self.audio_tracks[track].nodes.len()).rev() {
                let previous = self.audio_tracks[track].nodes.remove(at);
                self.retiring.push_back((track, previous));
                if let Some(engine) = self.engine.as_mut() {
                    let _ = engine.producer.push(GuiMsg::RemoveNode {
                        addr: audio::NodeAddr { track, at },
                    });
                }
            }
        }
    }

    /// プロジェクトに書かれていた音源とルーティングを読み直す。
    /// 失敗した段の説明を返す (その段は音源なしのままになる)。
    fn restore_audio_tracks(&mut self, tracks: Vec<project::AudioTrackSnapshot>) -> Vec<String> {
        self.ensure_audio_tracks();
        self.unload_all_plugins();

        // **繋ぎ方と音量はファイルのものを使う。** 検証は読み込みで済んでいる
        for (index, track) in tracks.iter().enumerate().take(graph::AUDIO_TRACKS) {
            let slot = &mut self.audio_tracks[index];
            slot.midi_track = track.midi_track;
            slot.sends = track.sends.clone();
            slot.gain = track.gain;
            slot.pan = track.pan;
            slot.muted = track.muted;
            slot.soloed = track.soloed;
        }

        let mut failures = Vec::new();
        for (index, track) in tracks.into_iter().enumerate().take(graph::AUDIO_TRACKS) {
            for (at, plugin) in track.nodes.into_iter().enumerate() {
                let label = file_label(&plugin.path);
                let addr = audio::NodeAddr { track: index, at };
                match self.load_plugin(
                    addr,
                    plugin.kind,
                    &plugin.path,
                    &plugin.id,
                    Some(&plugin.state),
                ) {
                    Ok(true) => {}
                    Ok(false) => failures.push(format!(
                        "・オーディオトラック {index} の {} 段目: {label} は読み込めましたが、\
                         音作りの復元に失敗しました",
                        at + 1
                    )),
                    Err(e) => failures.push(format!(
                        "・オーディオトラック {index} の {} 段目: {label} — {e}",
                        at + 1
                    )),
                }
                // バイパスは載せたあとに伝える
                if plugin.bypassed {
                    self.set_bypassed(addr, true);
                }
            }
            // MIDI の割り当てをオーディオスレッドへ伝える
            let midi_track = self.audio_tracks[index].midi_track;
            self.set_midi_track(index, midi_track);
        }
        failures
    }
}

/// `ClearTrack` で外した処理器が返ってくるのを待つ。
///
/// オーディオコールバックが回っている前提なので、普通は1〜2ブロック分で揃う。
/// 揃わないまま時間切れになったら、集まったぶんだけ返す (呼び出し側が戻す)。
fn collect_processors(engine: &mut Engine, expected: usize) -> Vec<(audio::NodeAddr, audio::Node)> {
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
) -> Result<(TrackAudio, audio::Node), Box<dyn Error>> {
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

    // **activate の前に数える。** 画面に「どこでステレオが潰れるか」を出すため
    let channels = {
        let inputs = audio::config::get_config_from_ports(&mut instance.plugin_handle(), true);
        let outputs = audio::config::get_config_from_ports(&mut instance.plugin_handle(), false);
        let main = |config: &audio::config::PluginAudioPortsConfig| {
            config
                .ports
                .get(config.main_port_index as usize)
                .map_or(0, |port| port.port_layout.channel_count())
        };
        (main(&inputs), main(&outputs))
    };

    let node = audio::activate_node(&mut instance, stream_config)?;
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
            bypassed: false,
            channels,
            plugin: TrackPlugin::Clap(ClapTrack {
                instance,
                receiver,
                sender,
                params,
                gui,
            }),
        },
        node,
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
) -> Result<(TrackAudio, audio::Node), Box<dyn Error>> {
    let (plugin, node) = audio::activate_vst3_node(path, class_id, stream_config)?;

    // 入力側は `vst3-host` に問い合わせる術が無く、こちらが要求した数になる
    // (`audio::activate_vst3_node` の説明を参照)
    let channels = (
        graph::BUS_CHANNELS as u16,
        plugin.lock().output_channel_count() as u16,
    );

    let gui = Vst3GuiManager::new(&plugin.lock());
    let (sender, receiver) = crossbeam_channel::unbounded();

    Ok((
        TrackAudio {
            name: plugin_name.to_string(),
            path: path.to_path_buf(),
            id: class_id.to_string(),
            pressed_keys: HashSet::new(),
            bypassed: false,
            channels,
            plugin: TrackPlugin::Vst3(Vst3Track {
                plugin,
                receiver,
                sender,
                gui,
            }),
        },
        node,
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
                    // 検証用 CLI なので、オーディオトラック1 の先頭に載せて
                    // 打ち込みトラック1 を鳴らす形に決め打ちする
                    let addr = audio::NodeAddr { track: 1, at: 0 };
                    self.candidates = Some(Candidates {
                        kind,
                        path,
                        plugins,
                        target_track: addr,
                    });
                    self.instantiate(0, addr);
                    self.set_midi_track(1, Some(0));
                    if open_gui {
                        if let Some(track) = self
                            .audio_tracks
                            .get_mut(1)
                            .and_then(|slot| slot.nodes.first_mut())
                        {
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
        for track in self
            .audio_tracks
            .iter_mut()
            .flat_map(|slot| slot.nodes.iter_mut())
        {
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

        // エラーは通知へ寄せる (上部の状態行は畳んだ)
        if let Some(error) = self.error.take() {
            self.notice = Some(Notice::error("エラー", error));
        }

        // 音源の形式とプラグインの選択。**画面中央のポップアップで出す。**
        // 上部に行として出していたときは、押す場所が画面の隅で遠かった
        let chosen_kind = self.load_choice_popup(ctx);
        self.plugin_choice_popup(ctx);

        egui::CentralPanel::default().show(ctx, |ui| {
            // 音源の操作はオーディオトラックの窓へ移した (下の `audio_track_windows`)
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
                                        // チェーンの何段目か。この UI を戻すときは
                                        // 段を選べるようにすること
                                        node: 0,
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

            if self.audio_tracks.iter().all(|track| track.nodes.is_empty()) {
                ui.label(
                    "音源が未ロードです (「オーディオトラック」の窓から載せてください)。\
                     ロードしなくてもシーケンスの編集はできます。",
                );
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

            // 打ち込みトラック欄に出す情報。**どのオーディオトラックが
            // その打ち込みを見ているか**を集める。誰も見ていなければ鳴らないので、
            // 欄に赤枠を出す材料になる
            self.editor.track_plugins = (0..self.editor.editor.track_count())
                .map(|midi_track| {
                    let users: Vec<String> = self
                        .audio_tracks
                        .iter()
                        .enumerate()
                        .filter(|(_, slot)| slot.midi_track == Some(midi_track))
                        .map(|(index, slot)| format!("{index}: {}", slot.label()))
                        .collect();
                    (!users.is_empty()).then(|| users.join(" / "))
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
                    EditorCommand::SaveProjectAs => file_action = Some(FileAction::SaveProjectAs),
                    EditorCommand::ExportWav => file_action = Some(FileAction::ExportWav),
                    EditorCommand::ExportOpus => file_action = Some(FileAction::ExportOpus),
                    EditorCommand::ExportCcs => file_action = Some(FileAction::ExportCcs),
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
                                let end_sample = (self.editor.editor.length_quarters_bar_aligned()
                                    as f64
                                    * spq) as u64;
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
                            | EditorCommand::ExportCcs => continue,
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

            // オーディオトラックの窓 (一覧は常設、詳細は選んだ1本だけ)
            if let Some(addr) = self.audio_track_windows(ui.ctx()) {
                load_plugin_track = Some(addr);
            }

            // 詳細ウィンドウの「+」。まず形式を聞く行を出す
            if let Some(track) = load_plugin_track {
                self.pending_load = Some(track);
                self.candidates = None;
            }

            // 形式が決まったらダイアログを開く。
            // 新しく読み込めたときだけ装填する (キャンセルでは何もしない)
            if let Some(chosen) = chosen_kind {
                let Some(track) = self.pending_load.take() else {
                    return;
                };
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
