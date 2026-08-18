//! 音源の読み込みと取り外し、サンプルレートの切り替え。

use super::track::{instantiate_clap, instantiate_vst3, start_engine, Candidates, TrackPlugin};
use super::App;
use crate::audio::config::StreamAudioConfig;
use crate::audio::graph;
use crate::audio::transport::TransportMsg;
use crate::audio::GuiMsg;
use crate::{audio, discovery, project};
use std::error::Error;
use std::path::PathBuf;

/// Windows で VST3 が置かれる標準の場所 (ダイアログの初期位置に使う)
const VST3_STANDARD_DIRECTORY: &str = r"C:\Program Files\Common Files\VST3";

/// 「♪」で聞く音源の選び方。
///
/// ダイアログの出し方が形式ごとに違うので、開く前に決めてもらう必要がある。
/// VST3 は**入れ物が2通りある**ため3択になる (詳細は [`App::open_vst3_dialog`])。
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum LoadChoice {
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
pub(super) fn file_label(path: &std::path::Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

impl App {
    /// .clap を選ばせて候補を読み込む。
    /// 戻り値は「候補を新しく読み込めたか」。キャンセルや失敗では false を返し、
    /// 前回の候補には触れない (キャンセルで前のプラグインが載らないようにするため)。
    pub(super) fn open_clap_dialog(&mut self, target_track: audio::NodeAddr) -> bool {
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
    pub(super) fn open_vst3_dialog(&mut self, target_track: audio::NodeAddr, bundle: bool) -> bool {
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
    pub(super) fn any_editor_open(&self) -> bool {
        self.audio_tracks
            .iter()
            .flat_map(|track| track.nodes.iter())
            .any(|node| match &node.plugin {
                TrackPlugin::Clap(clap) => clap.gui.as_ref().is_some_and(|gui| gui.is_open),
                TrackPlugin::Vst3(vst3) => vst3.gui.is_open,
            })
    }

    /// 数え上げた結果を候補として受け取る (形式によらず共通)
    pub(super) fn accept_candidates(
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
    pub(super) fn switch_processors_rate(
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

    /// ファイルダイアログを開くフォルダ
    pub(super) fn dialog_directory(&self) -> Option<PathBuf> {
        self.project_path
            .as_ref()
            .and_then(|path| path.parent())
            .map(PathBuf::from)
            .or_else(|| self.last_directory.clone())
    }

    /// 選んだプラグインを指定の段に載せる (詳細ウィンドウからの操作)
    pub(super) fn instantiate(&mut self, plugin_index: usize, track: audio::NodeAddr) {
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
    pub(super) fn load_plugin(
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
            // **起こしたばかりのエンジンは何も知らない。**
            // 立つ前に決めた繋ぎ方と MIDI の割り当てを丸ごと送り直す
            self.push_engine_state();
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

    /// 今載っている音源を全部降ろす (プロジェクトを開く前の片付け)
    pub(super) fn unload_all_plugins(&mut self) {
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
}
