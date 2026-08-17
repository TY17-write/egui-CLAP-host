//! オフラインレンダリング (ファイル書き出し用)。
//!
//! 再生用とは別のトランスポートを立てて、シーケンスをその場で最後まで回し、
//! 結果を f32 バッファへ溜める。実時間を待たないので長い曲でも短時間で済む。
//!
//! 処理器はオーディオスレッドから借りてくる。別インスタンスを立てる手もあるが、
//! state 拡張が未対応なのでパラメータが初期値に戻ってしまい、ユーザーが
//! 音作りした結果が書き出しに乗らなくなる。

use super::graph::{self, Graph};
use super::transport::{self, Transport, TransportMsg, TransportShared};
use crate::sequencer::SeqEvent;

/// 終端のあと、残響やリリースを録り切るために余分に回す長さ (秒)。
/// clap.tail 拡張が未対応なので、実際の減衰時間は分からない。長めに回して
/// あとから無音を切り落とす方針を取る。
pub const TAIL_SECONDS: f64 = 3.0;

/// 末尾で無音とみなす振幅 (およそ -90dBFS)。16bit に落とすと 0 になる領域。
const SILENCE: f32 = 3.2e-5;

/// レンダリングの指示
pub struct RenderSetup {
    /// **打ち込みトラック**番号順のイベント列。鳴らさないトラック (ミュート等) は
    /// 空にする。どのオーディオトラックが受け取るかはグラフ側が持っている。
    pub sequences: Vec<Box<[SeqEvent]>>,
    /// シーケンスの終端 (サンプル)
    pub end_sample: u64,
    /// 終端後に余分に回すサンプル数
    pub tail_samples: u64,
    /// 1ブロックのフレーム数。activate 時に宣言した上限を超えてはいけない。
    pub block_frames: usize,
    pub sample_rate: u32,
}

/// レンダリング結果
pub struct Rendered {
    /// インターリーブ済みの出力。-1.0..=1.0 に収めてある。
    pub samples: Vec<f32>,
    pub channels: usize,
    pub sample_rate: u32,
    /// 縮小する前のピーク。1.0 を超えていた場合は全体を縮めてある。
    pub peak: f32,
    /// 処理に失敗したトラック。空でなければ、そのトラックは無音のまま
    /// 混ざっているので、出来上がりは中身が欠けている。
    pub failures: Vec<TrackFailure>,
}

/// 書き出し中に処理が失敗したトラック
pub struct TrackFailure {
    /// **オーディオトラック**番号 (0 始まり。0 はマスター)
    pub track: usize,
    /// 失敗したブロック数
    pub blocks: usize,
    /// 最初のエラー内容。以降は同じ内容が延々と続くので1件だけ残す。
    pub message: String,
}

/// 失敗の記録。トラックごとに1件へまとめる。
///
/// 1ブロックでも失敗する音源はたいてい最後まで失敗し続けるので、
/// そのまま並べると数千件になって読めない。
#[derive(Default)]
struct FailureLog {
    entries: Vec<TrackFailure>,
}

impl FailureLog {
    fn record(&mut self, track: usize, error: impl std::fmt::Display) {
        match self.entries.iter_mut().find(|entry| entry.track == track) {
            Some(entry) => entry.blocks += 1,
            // オフライン処理なので、ここで確保しても問題ない
            None => self.entries.push(TrackFailure {
                track,
                blocks: 1,
                message: error.to_string(),
            }),
        }
    }
}

impl Rendered {
    /// 出力の長さ (秒)
    pub fn seconds(&self) -> f64 {
        let frames = self.samples.len() / self.channels.max(1);
        frames as f64 / self.sample_rate.max(1) as f64
    }
}

/// シーケンスを最後まで回して1本のバッファにまとめる。
///
/// `graph` は呼び出し側が組み立てたもの。オーディオスレッドから引き上げてきた
/// 処理器を載せて渡し、**終わったら必ず戻すこと**。
///
/// **再生と同じ [`Graph`] を通る。** 音の組み立てを2箇所に持つと、
/// 鳴らした音と書き出した音が食い違う。
///
/// 出力は常に [`graph::BUS_CHANNELS`] (2ch)。デバイスのチャンネル数には依らない。
pub fn render(graph: &mut Graph, setup: RenderSetup) -> Rendered {
    let channels = graph::BUS_CHANNELS;
    let block_frames = setup.block_frames.max(1);
    let total_frames = setup.end_sample + setup.tail_samples;
    graph.reserve(block_frames);

    // 再生用の位置表示を巻き込まないよう、共有状態は使い捨てのものを渡す
    let mut transport = Transport::new(TransportShared::new());
    for (track, events) in setup.sequences.into_iter().enumerate() {
        let _ = transport.handle_msg(TransportMsg::SetSequence {
            track,
            events,
            end_sample: setup.end_sample,
        });
    }
    let _ = transport.handle_msg(TransportMsg::Play);

    let mut samples = Vec::with_capacity((total_frames as usize + block_frames) * channels);
    let mut failures = FailureLog::default();

    // 直前まで鳴っていた音を消してから録り始める (無音から始めるため)。
    // このブロックの出力は捨てる。
    graph.clear_events();
    for (_, processor) in graph.processors_mut() {
        transport::push_choke(processor.events_mut(), 0);
    }
    graph.process(0, block_frames, &mut |track, e| failures.record(track, e));

    // 端数ブロックを作らず、常に同じ長さで回して最後に切る。
    // min_frames_count を下回るブロックをプラグインへ渡さずに済む。
    let mut steady = 0u64;
    let mut rendered_frames = 0u64;
    while rendered_frames < total_frames {
        let plan = transport.plan_block(block_frames as u64);
        graph.clear_events();
        graph.emit_from(&mut transport, &plan);

        graph.process(steady, block_frames, &mut |track, e| {
            failures.record(track, e)
        });
        samples.extend_from_slice(graph.master(block_frames));

        steady += block_frames as u64;
        rendered_frames += block_frames as u64;
    }

    samples.truncate(total_frames as usize * channels);
    let peak = finish(&mut samples, channels, setup.end_sample);

    Rendered {
        samples,
        channels,
        sample_rate: setup.sample_rate,
        peak,
        failures: failures.entries,
    }
}

/// 末尾の無音を切り落とし、ピークが 1.0 を超えていれば全体を縮める。
/// 戻り値は縮める前のピーク。
///
/// `keep_frames` より手前は切らない (シーケンス本体が無音でも長さを保つため)。
/// 縮小は単純な定数倍で、リミッタのように波形は変えない。
fn finish(samples: &mut Vec<f32>, channels: usize, keep_frames: u64) -> f32 {
    let channels = channels.max(1);

    // 末尾の無音を落とす。切る位置はフレーム境界に合わせる
    // (途中で切るとチャンネルがずれて以降が入れ替わる)。
    let floor = (keep_frames as usize * channels).min(samples.len());
    let mut end = samples.len();
    while end > floor && samples[end - 1].abs() < SILENCE {
        end -= 1;
    }
    let end = (end.div_ceil(channels) * channels).min(samples.len());
    samples.truncate(end);

    let peak = samples.iter().fold(0.0f32, |max, s| max.max(s.abs()));
    if peak > 1.0 {
        let scale = 1.0 / peak;
        for sample in samples.iter_mut() {
            *sample *= scale;
        }
    }
    peak
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 失敗はトラックごとに1件へまとまり、回数だけ増えること。
    /// (毎ブロック記録すると数千件になって読めなくなる)
    #[test]
    fn failures_are_grouped_per_track() {
        let mut log = FailureLog::default();
        log.record(1, "処理に失敗");
        log.record(1, "処理に失敗");
        log.record(0, "別のエラー");
        log.record(1, "あとから変わった内容");

        assert_eq!(log.entries.len(), 2, "トラックごとに1件");

        let first = &log.entries[0];
        assert_eq!(first.track, 1);
        assert_eq!(first.blocks, 3, "失敗した回数を数えること");
        assert_eq!(first.message, "処理に失敗", "最初の内容を残すこと");

        assert_eq!(log.entries[1].track, 0);
        assert_eq!(log.entries[1].blocks, 1);
    }

    /// ピークが 1.0 を超えたら 0dBFS に収まるまで全体が縮むこと。
    /// (クリップしたまま 16bit に落とすと歪みが焼き付いてしまう)
    #[test]
    fn peak_over_one_is_scaled_down() {
        let mut samples = vec![0.5, -2.0, 1.0, 0.25];
        let peak = finish(&mut samples, 2, 2);

        assert_eq!(peak, 2.0, "縮める前のピークを返すこと");
        assert!((samples[1] + 1.0).abs() < 1e-6, "最大点が -1.0 になること");
        assert!((samples[0] - 0.25).abs() < 1e-6, "他も同じ比率で縮むこと");
    }

    /// 1.0 に収まっていれば触らない (勝手に音量を上げない)
    #[test]
    fn quiet_output_is_left_alone() {
        let mut samples = vec![0.1, 0.2, 0.3, 0.4];
        let peak = finish(&mut samples, 2, 2);

        assert!((peak - 0.4).abs() < 1e-6);
        assert_eq!(samples, vec![0.1, 0.2, 0.3, 0.4]);
    }

    /// 末尾の無音は落とすが、シーケンス本体の長さは残ること
    #[test]
    fn trailing_silence_is_trimmed_down_to_the_sequence() {
        // ステレオ6フレーム。3フレーム目までが本体、以降は無音。
        let mut samples = vec![0.5, 0.5, 0.4, 0.4, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0];
        finish(&mut samples, 2, 3);

        assert_eq!(samples.len(), 6, "本体の3フレームまで縮むこと");
    }

    /// 残響が残っているうちは切らないこと
    #[test]
    fn audible_tail_is_kept() {
        let mut samples = vec![0.5, 0.5, 0.0, 0.0, 0.01, 0.01, 0.0, 0.0];
        finish(&mut samples, 2, 1);

        assert_eq!(samples.len(), 6, "鳴っている最後のフレームまで残ること");
    }

    /// 全部無音でも、シーケンス本体の長さは保たれること
    #[test]
    fn silent_render_keeps_sequence_length() {
        let mut samples = vec![0.0; 12];
        finish(&mut samples, 2, 4);

        assert_eq!(samples.len(), 8, "4フレーム分は残ること");
    }
}
