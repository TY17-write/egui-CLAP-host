//! 繋ぎ方・音量・MIDI の割り当てを組み立てて、オーディオスレッドへ送る。
//!
//! 音量の単位はここが境目になる。**画面は dB、エンジンとファイルは線形**で、
//! 変換は [`db_to_linear`] / [`linear_to_db`] を必ず通す。

use super::track::AudioTrackUi;
use super::App;
use crate::audio::graph;
use crate::audio::GuiMsg;
use crate::audio::{self};

/// 音量つまみの下限 (dB)。**ここまで下げたら無音として扱う。**
///
/// 対数なので本当の無音は -∞ になる。下限を決めて、そこを無音に割り当てる。
pub(super) const MIN_GAIN_DB: f32 = -60.0;
/// 音量つまみの上限 (dB)
pub(super) const MAX_GAIN_DB: f32 = 12.0;

/// dB を線形の係数へ。**下限まで下げたら 0 (無音)**
pub(super) fn db_to_linear(db: f32) -> f32 {
    if !db.is_finite() || db <= MIN_GAIN_DB {
        return 0.0;
    }
    10f32.powf(db / 20.0)
}

/// 線形の係数を dB へ ([`db_to_linear`] の逆)
pub(super) fn linear_to_db(linear: f32) -> f32 {
    if !linear.is_finite() || linear <= 0.0 {
        return MIN_GAIN_DB;
    }
    (20.0 * linear.log10()).clamp(MIN_GAIN_DB, MAX_GAIN_DB)
}

impl App {
    /// 今の繋ぎ方を組み立てる。
    ///
    /// **組めなければ既定 (全部マスターへ直結) に戻す。** 読み込みの時点で
    /// 検証してあるので普通は起きないが、鳴らなくなるより既定で鳴るほうがよい。
    pub(super) fn routing(&self) -> graph::Routing {
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
    pub(super) fn ensure_audio_tracks(&mut self) {
        if self.audio_tracks.len() == graph::AUDIO_TRACKS {
            return;
        }
        self.audio_tracks = (0..graph::AUDIO_TRACKS).map(AudioTrackUi::new).collect();
        // 起動直後は開いておく (ここが音源を載せる唯一の入口なので)
        self.show_audio_tracks = true;
    }

    /// 繋ぎ方・音量・ミュート/ソロをまとめたもの。
    ///
    /// **これらを触る画面はまだ無い**ので、今は保存されている値
    /// (既定は等倍・中央・全部鳴る) をそのまま組み立てている。
    pub(super) fn mixer(&self) -> graph::Mixer {
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
            // エンジンは線形で受け取る
            gain[index] = db_to_linear(track.gain_db);
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
    pub(super) fn toggle_send(&mut self, from: usize, to: usize) -> Result<(), String> {
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
    /// 変わったときだけでよい。
    pub(super) fn push_routing(&mut self) {
        let mixer = self.mixer();
        if let Some(engine) = self.engine.as_mut() {
            let _ = engine.producer.push(GuiMsg::SetMixer(mixer));
        }
    }

    /// 今の設定を丸ごとオーディオスレッドへ送り直す。
    ///
    /// **エンジンを起こした直後に必ず呼ぶこと。** 設定を触るメソッドは
    /// 「エンジンが無ければ送らない」で済ませているので、**エンジンが立つ前に
    /// 決めたことは向こうに届いていない**。
    ///
    /// 実際にこれで音が出なくなった: 音源を載せる前に MIDI の割り当てだけ
    /// 済ませると、エンジンが起きたときに割り当てが再送されず、画面には
    /// 「トラック1」と出ているのにオーディオスレッド側は未割り当てのままだった。
    pub(super) fn push_engine_state(&mut self) {
        self.ensure_audio_tracks();
        let mixer = self.mixer();
        let assignments: Vec<(usize, Option<usize>)> = self
            .audio_tracks
            .iter()
            .enumerate()
            .map(|(track, slot)| (track, slot.midi_track))
            .collect();

        let Some(engine) = self.engine.as_mut() else {
            return;
        };
        let _ = engine.producer.push(GuiMsg::SetMixer(mixer));
        for (track, midi_track) in assignments {
            let _ = engine
                .producer
                .push(GuiMsg::SetMidiTrack { track, midi_track });
        }
    }

    /// 借りたノードをオーディオスレッドへ返す。
    ///
    /// **返さないと音が出なくなる。** 途中で諦めるときも必ず通ること。
    pub(super) fn return_processors(&mut self, nodes: Vec<(audio::NodeAddr, audio::Node)>) {
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

    /// オーディオスレッドから返ってきた音源をここで停止・解放する
    /// (オーディオスレッドで解放してはいけないため)。
    /// 差し替えで外したインスタンスが待っていればそちらへ、
    /// 無ければ今そのトラックに載っているインスタンスへ返す。
    pub(super) fn drain_retired(&mut self) {
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
                        if let Some(super::track::TrackPlugin::Clap(clap)) =
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

    /// 段を外す
    pub(super) fn remove_node(&mut self, addr: audio::NodeAddr) {
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
    pub(super) fn move_node(&mut self, track: usize, from: usize, to: usize) {
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
    pub(super) fn set_bypassed(&mut self, addr: audio::NodeAddr, bypassed: bool) {
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
    pub(super) fn set_midi_track(&mut self, track: usize, midi_track: Option<usize>) {
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
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **既定 (0 dB) が等倍であること。**
    ///
    /// ここがずれると、何も触っていないトラックの音量が変わる。
    /// 以前パンの既定で -3dB 落ちる不具合を入れているので、必ず縛っておく。
    #[test]
    fn zero_db_is_unity() {
        assert!((db_to_linear(0.0) - 1.0).abs() < 1e-6);
        assert!(linear_to_db(1.0).abs() < 1e-4);
    }

    /// `AudioTrackUi` の既定がそのまま 0 dB になること。
    /// (線形で持っていた頃は `Default` の 0.0 が無音を意味していた)
    #[test]
    fn default_track_is_unity() {
        let track = AudioTrackUi::new(1);
        assert_eq!(track.gain_db, 0.0);
        assert!((db_to_linear(track.gain_db) - 1.0).abs() < 1e-6);
    }

    /// dB と線形が往復すること (画面・エンジン・ファイルで単位が違うため)
    #[test]
    fn db_and_linear_round_trip() {
        for db in [-48.0, -24.0, -6.0, 0.0, 6.0, 12.0] {
            let back = linear_to_db(db_to_linear(db));
            assert!((back - db).abs() < 1e-3, "{db} dB が {back} dB に化けた");
        }
    }

    /// -6 dB がおよそ半分、+6 dB がおよそ倍になること
    #[test]
    fn six_db_halves_and_doubles() {
        assert!((db_to_linear(-6.0) - 0.501).abs() < 0.01);
        assert!((db_to_linear(6.0) - 1.995).abs() < 0.01);
    }

    /// 下限まで下げたら無音になること (対数なので本当の 0 は表せない)
    #[test]
    fn the_bottom_of_the_range_is_silence() {
        assert_eq!(db_to_linear(MIN_GAIN_DB), 0.0);
        assert_eq!(db_to_linear(MIN_GAIN_DB - 10.0), 0.0);
        assert_eq!(linear_to_db(0.0), MIN_GAIN_DB);
    }

    /// NaN を通さないこと (以降の計算すべてに波及する)
    #[test]
    fn broken_values_fall_back_to_silence() {
        assert_eq!(db_to_linear(f32::NAN), 0.0);
        assert_eq!(linear_to_db(f32::NAN), MIN_GAIN_DB);
    }
}
