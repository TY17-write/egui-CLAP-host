//! オーディオトラックとそこに載る音源の、メインスレッド側の持ち物。
//!
//! オーディオエンジンの起動と、CLAP / VST3 のインスタンス化もここに置く。

use crate::audio::config::StreamAudioConfig;
use crate::audio::graph;
use crate::audio::transport::TransportShared;
use crate::audio::vst3::SharedPlugin;
use crate::audio::GuiMsg;
use crate::gui::{PluginGuiManager, Vst3GuiManager};
use crate::host::{MainThreadMessage, MiniHost, MiniHostMainThread, MiniHostShared};
use crate::{audio, discovery, project};
use clack_host::prelude::*;
use crossbeam_channel::{Receiver, Sender};
use std::error::Error;
use std::ffi::CString;
use std::path::PathBuf;
use std::time::{Duration, Instant};

/// メーターへ渡すリングバッファの大きさ (サンプル数)。
/// 96kHz のステレオでも 0.34秒ぶんあり、画面の1フレームには十分な余裕がある
const MONITOR_RING_SAMPLES: usize = 1 << 16;

/// 中身を数え上げた音源ファイル (プラグイン選択待ち)。
/// .clap でも .vst3 でも同じ形になる。
pub(super) struct Candidates {
    pub(super) kind: project::PluginKind,
    pub(super) path: PathBuf,
    pub(super) plugins: Vec<discovery::FoundPlugin>,
    /// この候補から選んだプラグインを載せる段
    pub(super) target_track: audio::NodeAddr,
}

/// オーディオエンジン (出力ストリーム1本と、それを操作する口)。
/// トラックより長生きし、音源はメッセージで出し入れする。
pub(super) struct Engine {
    pub(super) _stream: cpal::Stream,
    pub(super) producer: rtrb::Producer<GuiMsg>,
    /// オーディオスレッドから返ってきた音源 (メインスレッドで deactivate する)。
    /// どのインスタンスへ返すか分かるようトラック番号が付く。
    pub(super) retired: rtrb::Consumer<(audio::NodeAddr, audio::Node)>,
    /// 再生位置・再生中フラグの共有
    pub(super) transport_shared: TransportShared,
    /// マスターの出力 (L/R 交互)。上部のメーターが読む
    pub(super) monitor: rtrb::Consumer<f32>,
    /// 全プラグイン共通のストリーム構成
    pub(super) config: StreamAudioConfig,
}

/// チェーンの1段にロードされた音源。
/// 形式に依らない情報だけをここに持ち、中身は [`TrackPlugin`] が形式ごとに抱える。
pub(super) struct TrackAudio {
    pub(super) name: String,
    /// この音源のファイルのパス。プロジェクトに保存して読み直すために持つ。
    pub(super) path: PathBuf,
    /// CLAP のプラグイン ID / VST3 のクラス UID。
    /// 1つのファイルに複数入りうるので、パスだけでは足りない。
    pub(super) id: String,
    /// 処理を飛ばして素通しするか
    pub(super) bypassed: bool,
    /// 入力・出力のチャンネル数。**どこでステレオが潰れるかを画面に出す**ために持つ
    /// (載せた時点でしか分からないので控えておく)。
    pub(super) channels: (u16, u16),
    pub(super) plugin: TrackPlugin,
}

/// オーディオトラック1本ぶんの、メインスレッド側の持ち物。
///
/// オーディオスレッド側の [`graph::AudioTrack`](audio::graph::AudioTrack) と
/// 対になる。**こちらが正**で、変更は必ずメッセージで向こうへ伝える。
#[derive(Default)]
pub(super) struct AudioTrackUi {
    /// 上から順に通す。空なら何も鳴らない
    pub(super) nodes: Vec<TrackAudio>,
    /// MIDI をどの打ち込みトラックから取るか。**`None` が未割り当て**
    /// (起動直後は全部これ)
    pub(super) midi_track: Option<usize>,
    /// 送り先のオーディオトラック番号
    pub(super) sends: Vec<usize>,
    /// 音量 (dB)。**画面と同じ単位で持つ。**
    ///
    /// エンジンとファイルは線形なので、境目で
    /// [`db_to_linear`](super::routing::db_to_linear) /
    /// [`linear_to_db`](super::routing::linear_to_db) を通す。
    /// dB で持つと **`Default` の 0 がそのまま「0 dB = 等倍」**になり、
    /// 「既定値のつもりが無音」という取り違えが起きない。
    pub(super) gain_db: f32,
    /// `-1.0` (左) 〜 `1.0` (右)
    pub(super) pan: f32,
    pub(super) muted: bool,
    pub(super) soloed: bool,
}

impl AudioTrackUi {
    /// 空のトラック。マスター以外はマスターへ送る
    pub(super) fn new(index: usize) -> Self {
        Self {
            sends: if index == graph::MASTER {
                Vec::new()
            } else {
                vec![graph::MASTER]
            },
            ..Default::default()
        }
    }

    /// 画面に出す名前 (先頭の段の音源名)
    pub(super) fn label(&self) -> &str {
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
pub(super) enum TrackPlugin {
    Clap(ClapTrack),
    Vst3(Vst3Track),
}

pub(super) struct ClapTrack {
    pub(super) instance: PluginInstance<MiniHost>,
    pub(super) receiver: Receiver<MainThreadMessage>,
    pub(super) sender: Sender<MainThreadMessage>,
    /// プラグイン独自 GUI の管理 (gui 拡張がない場合は None)
    pub(super) gui: Option<PluginGuiManager>,
}

pub(super) struct Vst3Track {
    /// オーディオスレッドの処理器と同じ音源を指す。
    /// 触るときは必ず `lock()` する (相手は1ブロックしか握らない)。
    pub(super) plugin: SharedPlugin,
    /// ネイティブウィンドウからの通知 (閉じる・リサイズ)。
    /// CLAP と違い、プラグイン発の要求はこちらから取りに行く。
    pub(super) receiver: Receiver<MainThreadMessage>,
    pub(super) sender: Sender<MainThreadMessage>,
    pub(super) gui: Vst3GuiManager,
}

impl TrackAudio {
    pub(super) fn kind(&self) -> project::PluginKind {
        match self.plugin {
            TrackPlugin::Clap(_) => project::PluginKind::Clap,
            TrackPlugin::Vst3(_) => project::PluginKind::Vst3,
        }
    }

    /// 音源の今の状態を取り出す。取れない・失敗したときは空。
    ///
    /// 空でもパスと ID は保存するので、次に開いたとき音源自体は載る。
    pub(super) fn capture_state(&mut self) -> Vec<u8> {
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
    pub(super) fn restore_state(&mut self, state: &[u8]) -> bool {
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

/// `ClearTrack` で外した処理器が返ってくるのを待つ。
///
/// オーディオコールバックが回っている前提なので、普通は1〜2ブロック分で揃う。
/// 揃わないまま時間切れになったら、集まったぶんだけ返す (呼び出し側が戻す)。
pub(super) fn collect_processors(
    engine: &mut Engine,
    expected: usize,
) -> Vec<(audio::NodeAddr, audio::Node)> {
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
pub(super) fn start_engine() -> Result<Engine, Box<dyn Error>> {
    let (producer, consumer) = rtrb::RingBuffer::new(256);
    // 外した音源をメインスレッドへ返す口 (オーディオスレッドで解放しないため)
    let (retired_producer, retired) = rtrb::RingBuffer::new(8);
    // マスターの出力をメーターへ渡す口。**0.5秒ぶん**あれば、画面が少し
    // 止まっても取りこぼさない。溢れたぶんはオーディオ側が捨てる
    let (monitor_producer, monitor) = rtrb::RingBuffer::new(MONITOR_RING_SAMPLES);
    let transport_shared = TransportShared::new();

    let (stream, config) = audio::start_engine(
        consumer,
        retired_producer,
        transport_shared.clone(),
        monitor_producer,
    )?;

    Ok(Engine {
        _stream: stream,
        producer,
        retired,
        transport_shared,
        monitor,
        config,
    })
}

/// CLAP プラグインをインスタンス化して、指定のストリーム構成で鳴らせる状態にする。
/// 戻り値の処理器は呼び出し側がエンジンへ送る。
pub(super) fn instantiate_clap(
    path: &std::path::Path,
    plugin_id: &str,
    plugin_name: &str,
    stream_config: &StreamAudioConfig,
) -> Result<(TrackAudio, audio::Node), Box<dyn Error>> {
    // clack は PluginInstance の中で DLL を生かすので、ここで開き直してよい
    let (entry, _) = discovery::load_clap_file(path)?;

    let host_info = HostInfo::new(
        "egui-CLAP-host",
        "egui-clap-host",
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

    // gui 拡張があれば GUI マネージャを用意する
    let gui_ext = instance.access_handler(|mt| mt.gui.get());
    let gui = gui_ext.map(|ext| PluginGuiManager::new(ext, &mut instance.plugin_handle()));

    Ok((
        TrackAudio {
            name: plugin_name.to_string(),
            path: path.to_path_buf(),
            id: plugin_id.to_string(),
            bypassed: false,
            channels,
            plugin: TrackPlugin::Clap(ClapTrack {
                instance,
                receiver,
                sender,
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
pub(super) fn instantiate_vst3(
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
