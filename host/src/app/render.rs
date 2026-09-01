//! 音声としての書き出し (WAV / Opus / CeVIO)。
//!
//! **再生と同じ経路を通す。** 音源をオーディオスレッドから借りてきて、
//! 同じ `Graph` に同じミキサ設定で載せて回すので、鳴っている音と書き出した音が
//! 食い違うことがない。

use super::notice::Notice;
use super::track::collect_processors;
use super::App;
use crate::audio::config::StreamAudioConfig;
use crate::audio::graph;
use crate::audio::offline::{RenderSetup, TAIL_SECONDS};
use crate::audio::transport::TransportMsg;
use crate::audio::GuiMsg;
use crate::sequencer::SeqEvent;
use crate::{audio, ccs, opus, wav};
use std::path::PathBuf;

impl App {
    /// 書き出し用のレンダリング設定を、指定のストリーム構成で組み立てる
    pub(super) fn render_setup(&self, config: &StreamAudioConfig) -> RenderSetup {
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
            // プラグインへ見せる拍の情報 (再生時と同じ値)
            tempo: self.editor.editor.tempo.max(1) as f64,
            beats: self.editor.editor.beats.max(1) as u16,
            beat_type: self.editor.editor.beat_type.max(1) as u16,
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
    pub(super) fn render_for_export(
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
            let midi = self
                .audio_tracks
                .get(track)
                .map(|slot| slot.midi)
                .unwrap_or_default();
            graph.place_chain(track, midi, chain);
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
    pub(super) fn export_wav(&mut self) {
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
        let Some(path) = self.ask_save_path("WAV ファイル", "wav", "mix") else {
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
                    self.notice = Some(Notice::ok("WAV を書き出しました", body).with_path(&path));
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
                    // **失敗した側にも付ける。** ファイル自体はできているので、
                    // 中身を聞いて確かめたくなる
                    self.notice = Some(
                        Notice::error("WAV の一部を書き出せませんでした", body).with_path(&path),
                    );
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
    pub(super) fn export_opus(&mut self) {
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
        // **デバイスのチャンネル数では断らない。** 書き出しはグラフの出力
        // (常にステレオ) をそのまま符号化するので、デバイスが何チャンネルでも
        // Opus に載る。以前はここでデバイスを見ており、3ch 以上のデバイスでは
        // ステレオしか書かないのに断っていた。
        const { assert!(graph::BUS_CHANNELS <= 2, "Opus はステレオまで") };

        // 時間のかかる処理に入る前に保存先を聞く
        let Some(path) = self.ask_save_path("Opus ファイル", "opus", "mix") else {
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

        // 一部に問題があってもファイル自体はできているので、どちらにも付ける
        if failed {
            Notice::error("Opus を書き出しました (一部に問題あり)", body).with_path(path)
        } else {
            Notice::ok("Opus を書き出しました", body).with_path(path)
        }
    }

    fn fail_opus(&mut self, message: impl Into<String>) {
        self.notice = Some(Notice::error("Opus を書き出せません", message));
    }

    /// 1トラック目を CeVIO のプロジェクトファイル (.ccs) に書き出す。
    /// 音を鳴らさないデータ変換なので、音源が未ロードでも使える。
    pub(super) fn export_ccs(&mut self) {
        // 変換に失敗するならダイアログを出す前に知らせる
        let exported = match ccs::export(&self.editor.editor) {
            Ok(exported) => exported,
            Err(e) => {
                self.notice = Some(Notice::error("CCS を書き出せません", e));
                return;
            }
        };

        let Some(path) = self.ask_save_path("CeVIO プロジェクト", "ccs", "song") else {
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
                self.notice = Some(Notice::ok("CCS を書き出しました", body).with_path(&path));
            }
            Err(e) => {
                self.notice = Some(Notice::error(
                    "CCS を書き出せません",
                    format!("保存できません:\n{e}"),
                ));
            }
        }
    }
}
