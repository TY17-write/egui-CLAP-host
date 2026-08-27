//! プロジェクト (.ron) の保存と読み込み、MIDI の入出力。

use super::notice::Notice;
use super::plugins::file_label;
use super::routing::{db_to_linear, linear_to_db};
use super::App;
use crate::audio::graph;
use crate::project::PluginSnapshot;
use crate::{audio, midi, project};
use std::path::PathBuf;

/// 描画ループの中でファイルダイアログを開けないので、種類だけ持ち帰る
pub(super) enum FileAction {
    ImportMidi,
    ExportMidi,
    OpenProject,
    SaveProject,
    SaveProjectAs,
    ExportWav,
    ExportOpus,
    ExportCcs,
}

impl App {
    /// MIDI ファイルを選んで読み込む (今のシーケンスは置き換わる)
    pub(super) fn import_midi(&mut self) {
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
    ///
    /// **画面上は「出力」の仲間**だが、コードは MIDI の読み込みと隣に置いてある
    /// (同じ形式の入り口と出口を離すほうが探しにくい)。
    pub(super) fn export_midi(&mut self) {
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
        let size = bytes.len();

        match std::fs::write(&path, bytes) {
            Ok(()) => {
                self.last_directory = path.parent().map(PathBuf::from);
                self.error = None;

                let editor = &self.editor.editor;
                let mut body = format!(
                    "{}\n\n{} トラック / {} ノート ({size} バイト)",
                    path.display(),
                    editor.track_count(),
                    editor.notes.len(),
                );
                // **揺らぎが乗ったことは伏せない。** 書いた位置と鳴る位置が
                // 違うファイルになるので、編集の保存に使うと記譜が崩れる
                let swinging = (0..editor.track_count()).any(|track| editor.track_swings(track));
                let waltzing = (0..editor.track_count()).any(|track| editor.track_waltzes(track));
                if swinging || waltzing {
                    let what = match (swinging, waltzing) {
                        (true, true) => "スウィングと拍の偏り",
                        (true, false) => "スウィング",
                        _ => "拍の偏り",
                    };
                    body.push_str(&format!(
                        "\n\n※ {what}が乗っています。\
                         記譜どおりに残したいときはプロジェクト (.ron) で保存してください。"
                    ));
                }
                self.notice = Some(Notice::ok("MIDI を書き出しました", body).with_path(&path));
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
    pub(super) fn save_project(&mut self, ask: bool) {
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
    pub(super) fn open_project(&mut self) {
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
                // 音源を載せる過程でエンジンが起きるので、そのあとに丸ごと送り直す
                self.push_engine_state();
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
    pub(super) fn set_project_path(&mut self, path: PathBuf) {
        self.last_directory = path.parent().map(PathBuf::from);
        self.editor.project_path = Some(file_label(&path));
        self.project_path = Some(path);
    }

    /// 保存先を選ばせる。拡張子を省略されたら補う。
    pub(super) fn ask_save_path(
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

    /// 保存用に、各オーディオトラックの中身を集める
    pub(super) fn audio_track_snapshots(&mut self) -> Vec<project::AudioTrackSnapshot> {
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
                midi_tracks: track.midi.iter().collect(),
                // **繋ぎ方と音量は音源の有無と無関係。** 空のトラックのぶんも残す
                // (繋ぎ替えてから音源を外しても、繋ぎ方は保たれる)
                sends: track.sends.clone(),
                // ファイルは線形で持つ (画面と単位を分ける)
                gain: db_to_linear(track.gain_db),
                pan: track.pan,
                muted: track.muted,
                soloed: track.soloed,
            })
            .collect()
    }

    /// プロジェクトに書かれていた音源とルーティングを読み直す。
    /// 失敗した段の説明を返す (その段は音源なしのままになる)。
    pub(super) fn restore_audio_tracks(
        &mut self,
        tracks: Vec<project::AudioTrackSnapshot>,
    ) -> Vec<String> {
        self.ensure_audio_tracks();
        self.unload_all_plugins();

        // **繋ぎ方と音量はファイルのものを使う。** 検証は読み込みで済んでいる
        for (index, track) in tracks.iter().enumerate().take(graph::AUDIO_TRACKS) {
            let slot = &mut self.audio_tracks[index];
            // 本数は読み込みの検証で見てあるので、ここへ入りきらないものは来ない
            slot.midi = graph::MidiSources::from_slice(&track.midi_tracks).0;
            slot.sends = track.sends.clone();
            slot.gain_db = linear_to_db(track.gain);
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
            let midi = self.audio_tracks[index].midi;
            self.set_midi_sources(index, midi);
        }
        failures
    }
}
