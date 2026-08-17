//! プラグインのアクティベートと CPAL ストリームへの接続。
//! GUI からのノート/パラメータイベントはリングバッファ経由でオーディオスレッドに渡す。
//!
//! プラグイン形式に依存する部分は [`clap`] へ切り出してある。ここから上は
//! 中立の [`events::BlockEvents`] だけを扱う。バックエンドの分岐は trait ではなく
//! enum にしている (停止・解放の経路が形式ごとに違うため、型で追える形にしたい)。

use crate::host::MiniHost;
use clack_host::prelude::*;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{
    BuildStreamError, Device, FromSample, OutputCallbackInfo, SampleFormat, Stream, StreamConfig,
};
use rtrb::Consumer;
use std::error::Error;

pub mod buffers;
pub mod clap;
pub mod config;
pub mod events;
pub mod offline;
pub mod transport;
pub mod vst3;

use clap::ClapProcessor;
use config::*;
use events::{BlockEvent, BlockEvents};
use transport::{Transport, TransportMsg, TransportShared};
use vst3::{SharedPlugin, Vst3Processor};

/// 1ブロックの処理が失敗した理由 (形式に依らない形)。
///
/// オーディオスレッドで作るので、確保しない形にしてある
/// (どちらのバックエンドのエラーも、処理経路のものは確保せずに作られる)。
#[derive(Debug)]
pub enum ProcessError {
    Clap(PluginInstanceError),
    Vst3(vst3_host::error::Error),
    /// 音源をメインスレッドが使っていて、このブロックを飛ばした (VST3 のみ)。
    /// イベントは持ち越されるので、音が消えるだけで崩れはしない。
    Busy,
}

impl std::fmt::Display for ProcessError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Clap(e) => write!(f, "{e}"),
            Self::Vst3(e) => write!(f, "{e}"),
            Self::Busy => write!(f, "音源が別の処理に使われていました"),
        }
    }
}

impl std::error::Error for ProcessError {}

impl From<PluginInstanceError> for ProcessError {
    fn from(error: PluginInstanceError) -> Self {
        Self::Clap(error)
    }
}

impl From<vst3_host::error::Error> for ProcessError {
    fn from(error: vst3_host::error::Error) -> Self {
        Self::Vst3(error)
    }
}

/// GUI スレッドからオーディオスレッドへ送るメッセージ
pub enum GuiMsg {
    NoteOn {
        track: usize,
        key: u16,
        velocity: f64,
    },
    NoteOff {
        track: usize,
        key: u16,
    },
    ParamValue {
        track: usize,
        /// チェーンの何段目に宛てたものか (0 が先頭)
        node: usize,
        id: u32,
        value: f64,
    },
    Transport(TransportMsg),
    /// トラックに音源を載せる (差し替え時は古い方を retired へ返す)
    SetTrack {
        track: usize,
        processor: Box<TrackProcessor>,
    },
    /// トラックの音源を外す
    ClearTrack {
        track: usize,
    },
}

/// 音源の形式ごとの処理器。
///
/// trait オブジェクトにしないのは、停止して解放する経路が形式ごとに違うため。
/// `dyn` にするとダウンキャストが要り、リングバッファ越しの受け渡しが
/// 型で守れなくなる。
pub enum Backend {
    Clap(ClapProcessor),
    Vst3(Vst3Processor),
}

/// チェーンの1段。音源とエフェクトを区別しない。
///
/// **違いは入力ポートを持つかどうかだけ**で、それはプラグインが自分で申告する。
/// 入力ポートを持たないノード (音源) は入ってきた音を読まずに上書きするので、
/// チェーンの途中に置けばそこまでの音が消える。
pub struct Node {
    backend: Backend,
}

impl Node {
    pub fn new(backend: Backend) -> Self {
        Self { backend }
    }

    /// メインスレッドへ返せる形にする (このメソッド自体もメインスレッドで呼ぶ)
    pub fn into_retired(self) -> RetiredProcessor {
        match self.backend {
            Backend::Clap(processor) => RetiredProcessor::Clap(processor.into_stopped()),
            Backend::Vst3(processor) => RetiredProcessor::Vst3(processor.into_shared()),
        }
    }

    /// 1ブロック処理して `buf` を置き換える。
    /// `index` はチェーンの何段目か (自分宛てのパラメータを拾うために使う)。
    fn process(
        &mut self,
        events: &BlockEvents,
        index: usize,
        steady: u64,
        buf: &mut [f32],
    ) -> Result<(), ProcessError> {
        match &mut self.backend {
            Backend::Clap(processor) => processor.process(events, index, steady, buf),
            Backend::Vst3(processor) => processor.process(events, index, buf),
        }
    }
}

/// オーディオスレッドが持つトラック1本ぶんの処理器一式。
///
/// **ノードを上から順に通す。** 各ノードは渡されたバッファを読んで上書きするので、
/// 前段の出力がそのまま次段の入力になる。
pub struct TrackProcessor {
    /// このブロックでこのトラックに届いたイベント (バックエンドに依らない形)。
    ///
    /// **全ノードへ配る。** 受け取れないノードは自分で捨てる。ただし
    /// パラメータだけは宛先を持ち、その段でしか効かない
    /// ([`BlockEvent::Param`](events::BlockEvent::Param))。
    events: BlockEvents,
    nodes: Vec<Node>,
}

/// オーディオスレッドから外した処理器。メインスレッドで始末するために形式を保つ。
pub enum RetiredProcessor {
    /// 停止済み。あとはインスタンスへ返すだけ。
    Clap(StoppedPluginAudioProcessor<MiniHost>),
    /// **まだ止まっていない。** `setProcessing(false)` はリアルタイム安全でないので、
    /// 受け取った側 (メインスレッド) が止める。
    Vst3(SharedPlugin),
}

impl TrackProcessor {
    /// 1段だけのトラックを作る
    pub fn new(backend: Backend) -> Self {
        Self::from_node(Node::new(backend))
    }

    /// 1段だけのトラックを作る
    pub fn from_node(node: Node) -> Self {
        Self {
            events: BlockEvents::with_capacity(128),
            nodes: vec![node],
        }
    }

    /// チェーンの末尾に足す。**メインスレッドで組み立てること** (確保が起きる)
    pub fn push_node(&mut self, node: Node) {
        self.nodes.push(node);
    }

    pub fn node_count(&self) -> usize {
        self.nodes.len()
    }

    /// チェーンの全ノードを上から順に、メインスレッドへ返せる形にする
    pub fn into_retired(self) -> Vec<RetiredProcessor> {
        self.nodes.into_iter().map(Node::into_retired).collect()
    }

    /// 1段だけのトラックから、その1つを取り出す。**段数が1でなければ `None`。**
    ///
    /// 画面から作れるトラックはまだ1段だけ (チェーンを組めるのは `chain_smoke`
    /// のような検証経路だけ) なので、呼び出し側の多くはこれで足りる。
    /// **想定外の段数を黙って捨てない**ようにするために `Option` にしてある。
    pub fn into_single_retired(self) -> Option<RetiredProcessor> {
        if self.nodes.len() != 1 {
            return None;
        }
        self.nodes.into_iter().next().map(Node::into_retired)
    }

    /// このブロックで送るイベント (呼び出し側が積む)
    pub fn events_mut(&mut self) -> &mut BlockEvents {
        &mut self.events
    }

    /// チェーンを上から順に通して `buf` を置き換える。
    ///
    /// `buf` は呼び出し側が 0 で埋めてから渡すこと。先頭が音源なら中身は
    /// 読まれずに上書きされ、先頭がエフェクトなら無音を加工することになる。
    ///
    /// `steady` は再生開始からの通し時間 (サンプル)。CLAP はこれを受け取るが、
    /// VST3 は `ProcessContext` を自前で組み立てる作りなので使わない。
    ///
    /// **どこか1段でも失敗したら、そのブロックは無音にして返す。** 途中まで
    /// 処理した音を出すと、加工されていない原音が混ざって出てしまう
    /// (呼び出し側は「失敗したトラックは無音のまま混ざる」前提で記録している)。
    pub fn process(&mut self, steady: u64, buf: &mut [f32]) -> Result<(), ProcessError> {
        for (index, node) in self.nodes.iter_mut().enumerate() {
            if let Err(e) = node.process(&self.events, index, steady, buf) {
                buf.fill(0.0);
                return Err(e);
            }
        }
        Ok(())
    }
}

/// 出力ストリームを1本だけ作って再生を開始する。
///
/// トラックの音源は後から `GuiMsg::SetTrack` で載せる。ストリームを作り直さずに
/// 差し替えられるので、トランスポート (再生位置) は1つのまま保たれる。
/// 外した音源は `retired` 経由でメインスレッドへ返す (オーディオスレッドで
/// 解放しないため)。
pub fn start_engine(
    gui_events: Consumer<GuiMsg>,
    retired: rtrb::Producer<(usize, Box<TrackProcessor>)>,
    transport_shared: TransportShared,
) -> Result<(Stream, StreamAudioConfig), Box<dyn Error>> {
    let cpal_host = cpal::default_host();

    let output_device = cpal_host
        .default_output_device()
        .ok_or("オーディオ出力デバイスが見つかりません")?;

    let stream_config = StreamAudioConfig::find_best(&output_device)?;
    println!("オーディオ構成: {stream_config}");

    let cpal_config = stream_config.as_cpal_stream_config();
    let sample_format = stream_config.sample_format;
    let audio_processor = StreamAudioProcessor::new(
        gui_events,
        retired,
        transport_shared,
        stream_config.output_channel_count,
    );

    let stream = build_output_stream_for_sample_format(
        &output_device,
        audio_processor,
        &cpal_config,
        sample_format,
    )?;
    stream.play()?;

    Ok((stream, stream_config))
}

/// CLAP プラグインを指定のストリーム構成でアクティベートし、チェーンの1段にする。
///
/// 音源かエフェクトかは見ない (プラグインが申告する入力ポートで決まる)。
pub fn activate_node(
    instance: &mut PluginInstance<MiniHost>,
    stream_config: &StreamAudioConfig,
) -> Result<Node, Box<dyn Error>> {
    let audio_config = FullAudioConfig::for_plugin(stream_config, instance);

    let note_port = clap::find_main_note_port(instance);
    if note_port.is_none() {
        println!("このプラグインにはノート入力ポートがありません。");
    }

    let processor = instance
        .activate(|_, _| (), stream_config.as_clack_plugin_config())?
        .start_processing()?;

    Ok(Node::new(Backend::Clap(ClapProcessor::new(
        processor,
        audio_config,
        note_port,
    ))))
}

/// CLAP プラグイン1つだけを載せたトラック処理器を作る
pub fn activate_track(
    instance: &mut PluginInstance<MiniHost>,
    stream_config: &StreamAudioConfig,
) -> Result<Box<TrackProcessor>, Box<dyn Error>> {
    Ok(Box::new(TrackProcessor::from_node(activate_node(
        instance,
        stream_config,
    )?)))
}

/// VST3 プラグインを指定のストリーム構成で読み込み、トラック処理器にする。
///
/// CLAP の `activate_track` は「既にあるインスタンスを起こす」形だが、こちらは
/// 読み込みも含める。`vst3-host` では音源の生成と処理器が同じ型なので、
/// 分ける意味がないため。
///
/// 戻り値の [`SharedPlugin`] はメインスレッド側 (エディタ・状態の保存) が持つ。
/// 処理器と同じ音源を指しているので、片方を捨てても音源は生き残る。
pub fn activate_vst3_track(
    path: &std::path::Path,
    class_id: &str,
    stream_config: &StreamAudioConfig,
) -> Result<(SharedPlugin, Box<TrackProcessor>), Box<dyn Error>> {
    let (shared, node) = activate_vst3_node(path, class_id, stream_config)?;
    Ok((shared, Box::new(TrackProcessor::from_node(node))))
}

/// VST3 プラグインを読み込んでチェーンの1段にする (CLAP の `activate_node` に対応)
pub fn activate_vst3_node(
    path: &std::path::Path,
    class_id: &str,
    stream_config: &StreamAudioConfig,
) -> Result<(SharedPlugin, Node), Box<dyn Error>> {
    // ホストはインスタンス化のための入口でしかないので、ここで作って捨ててよい
    // (`Plugin` がモジュールを自分で抱える)。
    let mut vst3_host = vst3_host::host::Vst3Host::builder()
        .sample_rate(stream_config.sample_rate as f64)
        // CPAL が一度に渡してくる最大長。これを超えるブロックは
        // `vst3-host` 側が分割して処理する。
        .block_size(stream_config.max_likely_buffer_size as usize)
        // **エフェクトを載せるので入力バスが要る。** 0 にすると、繋いでも
        // 音が入らない音源専用の構成になる。音源側は入力を読まないだけなので、
        // 常に用意しておいて損はない
        .input_channels(stream_config.output_channel_count)
        .output_channels(stream_config.output_channel_count)
        .build()?;

    let mut plugin = vst3_host.load_plugin_class(path, class_id)?;
    let plugin_channels = plugin.output_channel_count();
    plugin.start_processing()?;

    let shared = SharedPlugin::new(plugin);
    let processor = Vst3Processor::new(
        shared.clone(),
        plugin_channels,
        stream_config.output_channel_count,
        stream_config,
    );

    Ok((shared, Node::new(Backend::Vst3(processor))))
}

/// 借りている VST3 の処理器を、別のサンプルレートで動かし直す。
///
/// **読み込み直さない。** `activate_vst3_track` はファイルから開き直すので
/// 音作りが飛ぶ。`reconfigure` は「デバイスのレートが変わったときに再ロードの
/// 代わりに使う」ためのもので、状態を保ったまま `setupProcessing` をやり直す。
///
/// 処理器のバッファは生成時のレート・ブロック長で作ってあるので、作り直す。
///
/// **失敗しても音源は生きている。** 呼び出し側は元のレートで呼び直して戻すこと
/// (戻せないとそのトラックが鳴らなくなる)。
pub fn reconfigure_vst3_track(
    shared: SharedPlugin,
    stream_config: &StreamAudioConfig,
) -> Result<Box<TrackProcessor>, Box<dyn Error>> {
    let plugin_channels = {
        let mut plugin = shared.lock();
        // reconfigure は処理中には呼べない
        plugin.stop_processing()?;
        plugin.reconfigure(
            stream_config.sample_rate as f64,
            stream_config.max_likely_buffer_size as usize,
        )?;
        plugin.start_processing()?;
        plugin.output_channel_count()
    };

    Ok(Box::new(TrackProcessor::new(Backend::Vst3(
        Vst3Processor::new(
            shared,
            plugin_channels,
            stream_config.output_channel_count,
            stream_config,
        ),
    ))))
}

/// サンプル形式に応じた CPAL 出力ストリームを構築する
fn build_output_stream_for_sample_format(
    device: &Device,
    processor: StreamAudioProcessor,
    config: &StreamConfig,
    sample_format: SampleFormat,
) -> Result<Stream, BuildStreamError> {
    let err = |e| eprintln!("{e}");

    match sample_format {
        SampleFormat::I8 => {
            device.build_output_stream(config, make_stream_runner::<i8>(processor), err, None)
        }
        SampleFormat::I16 => {
            device.build_output_stream(config, make_stream_runner::<i16>(processor), err, None)
        }
        SampleFormat::I32 => {
            device.build_output_stream(config, make_stream_runner::<i32>(processor), err, None)
        }
        SampleFormat::I64 => {
            device.build_output_stream(config, make_stream_runner::<i64>(processor), err, None)
        }
        SampleFormat::U8 => {
            device.build_output_stream(config, make_stream_runner::<u8>(processor), err, None)
        }
        SampleFormat::U16 => {
            device.build_output_stream(config, make_stream_runner::<u16>(processor), err, None)
        }
        SampleFormat::U32 => {
            device.build_output_stream(config, make_stream_runner::<u32>(processor), err, None)
        }
        SampleFormat::U64 => {
            device.build_output_stream(config, make_stream_runner::<u64>(processor), err, None)
        }
        SampleFormat::F32 => {
            device.build_output_stream(config, make_stream_runner::<f32>(processor), err, None)
        }
        SampleFormat::F64 => {
            device.build_output_stream(config, make_stream_runner::<f64>(processor), err, None)
        }
        f => unimplemented!("未知のサンプル形式: {f:?}"),
    }
}

fn make_stream_runner<S: FromSample<f32>>(
    mut audio_processor: StreamAudioProcessor,
) -> impl FnMut(&mut [S], &OutputCallbackInfo) {
    move |data, _info| audio_processor.process(data)
}

/// オーディオスレッド上で動くデータ一式
struct StreamAudioProcessor {
    /// トラックごとの音源。None は音源未ロードのトラック。
    tracks: Vec<Option<Box<TrackProcessor>>>,
    /// 外した音源を (トラック番号, 音源) でメインスレッドへ返す口。
    /// どのインスタンスへ返すか分かるようトラック番号を添える。
    retired: rtrb::Producer<(usize, Box<TrackProcessor>)>,
    gui_events: Consumer<GuiMsg>,
    /// 全トラックの出力を足し込むバッファ (インターリーブ済み)
    mix: Vec<f32>,
    /// **1トラック分の作業バッファ。** チェーンはここを通る。
    ///
    /// 各ノードが読んで上書きする形なので、トラックごとに独立した場所が要る
    /// (`mix` へ直接書くと、前のトラックの音をエフェクトが拾ってしまう)。
    scratch: Vec<f32>,
    output_channel_count: usize,
    transport: Transport,
    steady_counter: u64,
}

impl StreamAudioProcessor {
    fn new(
        gui_events: Consumer<GuiMsg>,
        retired: rtrb::Producer<(usize, Box<TrackProcessor>)>,
        transport_shared: TransportShared,
        output_channel_count: usize,
    ) -> Self {
        Self {
            tracks: Vec::new(),
            retired,
            gui_events,
            mix: Vec::new(),
            scratch: Vec::new(),
            output_channel_count,
            transport: Transport::new(transport_shared),
            steady_counter: 0,
        }
    }

    /// トラック数を確保する (足りなければ空きで埋める)
    fn ensure_track(&mut self, track: usize) {
        while self.tracks.len() <= track {
            self.tracks.push(None);
        }
    }

    /// 外した音源をメインスレッドへ返す。返せなければやむなくここで解放する
    /// (リングバッファが詰まるのは異常時だけ)。
    fn retire(&mut self, track: usize, processor: Option<Box<TrackProcessor>>) {
        if let Some(processor) = processor {
            let _ = self.retired.push((track, processor));
        }
    }

    /// GUI からのメッセージをすべて取り出し、トラックごとのイベントに変換する。
    /// その後、再生中ならシーケンスのイベントを sample_count 分発行する。
    fn collect_gui_events(&mut self, sample_count: u64) {
        for track in self.tracks.iter_mut().flatten() {
            track.events.clear();
        }

        while let Ok(msg) = self.gui_events.pop() {
            match msg {
                GuiMsg::NoteOn {
                    track,
                    key,
                    velocity,
                } => {
                    if let Some(Some(target)) = self.tracks.get_mut(track) {
                        target.events.push(BlockEvent::NoteOn {
                            offset: 0,
                            key: key.min(127) as u8,
                            velocity,
                        });
                    }
                }
                GuiMsg::NoteOff { track, key } => {
                    if let Some(Some(target)) = self.tracks.get_mut(track) {
                        target.events.push(BlockEvent::NoteOff {
                            offset: 0,
                            key: key.min(127) as u8,
                        });
                    }
                }
                GuiMsg::ParamValue {
                    track,
                    node,
                    id,
                    value,
                } => {
                    if let Some(Some(target)) = self.tracks.get_mut(track) {
                        target.events.push(BlockEvent::Param {
                            offset: 0,
                            node,
                            id,
                            value,
                        });
                    }
                }
                GuiMsg::Transport(msg) => {
                    // 状態の更新は1回。消音が要るときは全トラックへ配る
                    if self.transport.handle_msg(msg) {
                        for track in self.tracks.iter_mut().flatten() {
                            transport::push_choke(&mut track.events, 0);
                        }
                    }
                }
                GuiMsg::SetTrack { track, processor } => {
                    self.ensure_track(track);
                    let previous = self.tracks[track].replace(processor);
                    self.retire(track, previous);
                }
                GuiMsg::ClearTrack { track } => {
                    if let Some(slot) = self.tracks.get_mut(track) {
                        let previous = slot.take();
                        self.retire(track, previous);
                    }
                }
            }
        }

        // 再生位置を1回だけ進めてから、トラックごとにイベントを発行する。
        // (オーディオスレッドで確保しないよう、区間の計画は固定長で持ち回る)
        let plan = self.transport.plan_block(sample_count);
        if !plan.is_empty() {
            for (index, slot) in self.tracks.iter_mut().enumerate() {
                if let Some(track) = slot {
                    self.transport.emit_track(index, &plan, &mut track.events);
                }
            }
        }
    }

    /// CPAL の出力バッファ1回分を処理する。
    ///
    /// **トラックごとに作業バッファを通してから足し合わせる。** チェーンの各ノードが
    /// バッファを読んで上書きする形なので、共有のミックスへ直接書かせると、
    /// 前のトラックの音を次のトラックのエフェクトが拾ってしまう。
    fn process<S: FromSample<f32>>(&mut self, data: &mut [S]) {
        let sample_count = data.len() / self.output_channel_count.max(1);

        self.collect_gui_events(sample_count as u64);

        // ミックス用と作業用のバッファを用意する。
        // 伸びるのは最初と、想定より長いブロックが来たときだけ
        if self.mix.len() < data.len() {
            self.mix.resize(data.len(), 0.0);
        }
        if self.scratch.len() < data.len() {
            self.scratch.resize(data.len(), 0.0);
        }

        let steady = self.steady_counter;
        // フィールドを個別に借りる (トラックと2つのバッファを同時に触るため)
        let Self {
            tracks,
            mix,
            scratch,
            ..
        } = self;
        let mix = &mut mix[..data.len()];
        let scratch = &mut scratch[..data.len()];
        mix.fill(0.0);

        for track in tracks.iter_mut().flatten() {
            // 先頭が音源なら読まれずに上書きされる。エフェクトなら無音から始まる
            scratch.fill(0.0);
            match track.process(steady, scratch) {
                Ok(()) => {}
                // 想定内で、放っておけば直る。VST3 のエディタを開くときに
                // 数秒ぶん出ることがあり (音源の生成が重い)、そのたびに
                // オーディオスレッドから書き出すほうが害が大きい
                Err(ProcessError::Busy) => continue,
                Err(e) => {
                    eprintln!("{e}");
                    continue;
                }
            }
            for (mixed, sample) in mix.iter_mut().zip(scratch.iter()) {
                *mixed += *sample;
            }
        }

        for (out, mixed) in data.iter_mut().zip(self.mix.iter()) {
            *out = FromSample::from_sample_(*mixed);
        }

        self.steady_counter += sample_count as u64;
    }
}
