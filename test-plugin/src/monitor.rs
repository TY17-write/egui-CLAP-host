//! テスト用の最小 CLAP モニタ: **入力を見るだけで、音を返さない。**
//!
//! アナライザ・チューナー・ラウドネスメーターといった、実物のモニタリング系
//! プラグインと同じ形をしている。**出力ポートを1つも持たない。**
//!
//! # なぜこの治具が要るのか
//!
//! 出力ポートを持たない段をチェーンに刺すと、**そこから後ろが無音になる**
//! 不具合があった。ホストは器としてステレオのバッファを用意するが、
//! プラグインはそこへ何も書かない。その空のバッファをそのまま採ると、
//! 前段までの音が消える。
//!
//! 手元の実物のアナライザで確かめると、落ちたときに**こちらの問題か相手の
//! 問題か切り分けられない**。ここでは「入力を見た」ことを数値で言える形に
//! してある。
//!
//! # 見たことをどう伝えるか
//!
//! **観測したピークを読み取り専用のパラメータで返す。** 素通しになっていれば
//! 出音は変わらないので、出音だけでは「音を見せたうえで素通しした」のか
//! 「段ごと飛ばした」のかを区別できない。飛ばす実装でも出音は同じになるため、
//! **入力が届いたことは中から言うしかない**。
//!
//! 値は活性化してからの最大値で、`Reset` パラメータに 1 以上を書くと 0 に戻る。

use crate::monitor_gui::{Handoff, MonitorView, MonitorWindow, HEIGHT, REDRAW_MS, WIDTH};
use crate::AtomicF32;
use clack_extensions::timer::{HostTimer, PluginTimer, PluginTimerImpl, TimerId};
use clack_extensions::{audio_ports::*, gui::*, params::*};
use clack_plugin::events::event_types::ParamValueEvent;
use clack_plugin::events::spaces::CoreEventSpace;
use clack_plugin::prelude::*;
use std::cell::{Cell, RefCell};
use std::ffi::CStr;
use std::fmt::Write as _;
use std::sync::atomic::Ordering;
use std::sync::Arc;

/// 観測したピーク (読み取り専用)
const PARAM_PEAK_ID: ClapId = ClapId::new(1);
/// 1 以上を書くとピークを 0 に戻す
const PARAM_RESET_ID: ClapId = ClapId::new(2);

/// ピークの上限。これを超える入力は張り付く (治具なので範囲は広めで足りる)
const MAX_PEAK: f32 = 4.0;

/// GUI へ渡す輪の大きさ (サンプル数)。48kHz ステレオで 0.7 秒ぶん
const RING_SAMPLES: usize = 1 << 16;

pub struct TestMonitorPlugin;

impl Plugin for TestMonitorPlugin {
    type AudioProcessor<'a> = TestMonitorAudioProcessor<'a>;
    type Shared<'a> = TestMonitorShared;
    type MainThread<'a> = TestMonitorMainThread<'a>;

    fn declare_extensions(
        builder: &mut PluginExtensions<Self>,
        _shared: Option<&TestMonitorShared>,
    ) {
        builder
            // **これが本体。** 出力ポートを持たないことをホストへ申告する
            .register::<PluginAudioPorts>()
            .register::<PluginParams>()
            // 入ってきた音をその場で見せる ([`crate::monitor_gui`])
            .register::<PluginGui>()
            // **描き直しはホストに叩いてもらう。** 自前の WM_TIMER では
            // ホストの描画に負けて間隔が乱れる (詳細は `set_parent`)
            .register::<PluginTimer>();
    }
}

impl DefaultPluginFactory for TestMonitorPlugin {
    fn get_descriptor() -> PluginDescriptor {
        use clack_plugin::plugin::features::*;

        PluginDescriptor::new("com.example.test-monitor", "Test Monitor").with_features([
            AUDIO_EFFECT,
            ANALYZER,
            MONO,
            UTILITY,
        ])
    }

    fn new_shared(_host: HostSharedHandle) -> Result<TestMonitorShared, PluginError> {
        Ok(TestMonitorShared {
            peak: AtomicF32::new(0.0),
            handoff: Arc::new(Handoff::default()),
        })
    }

    fn new_main_thread<'a>(
        host: HostMainThreadHandle<'a>,
        shared: &'a TestMonitorShared,
    ) -> Result<TestMonitorMainThread<'a>, PluginError> {
        // ホストがタイマーを持っているかは、ここで一度だけ聞く
        let host_timer = host.shared().get_extension::<HostTimer>();
        Ok(TestMonitorMainThread {
            shared,
            host,
            host_timer,
            timer: Cell::new(None),
            // **窓より先に作り、窓より後に捨てる。** `Box` の中身は動かないので、
            // 窓へ渡した生ポインタは開け閉めをまたいで有効なまま
            view: Box::new(RefCell::new(MonitorView::new(shared.handoff.clone()))),
            window: RefCell::new(None),
        })
    }
}

/// スレッド間で共有されるデータ (観測したピークと、GUI への受け渡し口)
pub struct TestMonitorShared {
    peak: AtomicF32,
    handoff: Arc<Handoff>,
}

impl TestMonitorShared {
    /// 活性化してからの最大値。**ブロックをまたいで残す**ので、
    /// 処理を回し終えてから1回読めばよい
    fn observe(&self, peak: f32) {
        let seen = self.peak.load(Ordering::SeqCst);
        if peak > seen {
            self.peak.store(peak.min(MAX_PEAK), Ordering::SeqCst);
        }
    }

    fn peak(&self) -> f32 {
        self.peak.load(Ordering::SeqCst)
    }

    fn handle_param_event(&self, event: &ParamValueEvent) {
        // ピークは読み取り専用。書き込みを受けるのはリセットだけ
        if event.param_id() == Some(PARAM_RESET_ID) && event.value() >= 1.0 {
            self.peak.store(0.0, Ordering::SeqCst);
        }
    }
}

impl PluginShared<'_> for TestMonitorShared {}

pub struct TestMonitorMainThread<'a> {
    shared: &'a TestMonitorShared,
    /// ホストの口。**タイマーの登録・解除に要る**
    host: HostMainThreadHandle<'a>,
    /// ホストが駆動するタイマー。持たないホストでは `None` (自前の窓タイマーへ落ちる)
    host_timer: Option<HostTimer>,
    /// 登録できたタイマーの ID
    timer: Cell<Option<TimerId>>,
    /// 窓が指す表示状態。**窓を閉じても捨てない** (掛け直しても続きから出す)
    view: Box<RefCell<MonitorView>>,
    /// 開いている窓。閉じると `None`。
    ///
    /// **内部可変にしてある。** `clap.gui` の口はどれも `&self` で来るので、
    /// ここを普通のフィールドにすると窓を持てない
    window: RefCell<Option<MonitorWindow>>,
}

impl<'a> PluginMainThread<'a, TestMonitorShared> for TestMonitorMainThread<'a> {}

impl TestMonitorMainThread<'_> {
    /// ホストのタイマーを登録する。**登録できたら `true`**。
    ///
    /// 持っていない・断られたホストでは `false` を返し、呼び出し側が自前の
    /// 窓タイマーへ落とす。
    fn ensure_host_timer(&self) -> bool {
        if self.timer.get().is_some() {
            return true;
        }
        let Some(extension) = &self.host_timer else {
            return false;
        };
        match extension.register_timer(&self.host, REDRAW_MS) {
            Ok(id) => {
                self.timer.set(Some(id));
                true
            }
            Err(_) => false,
        }
    }
}

/// ホストが叩いてくれる描き直し。
///
/// **これが本命の経路。** 取り込んでその場で描く
/// ([`MonitorWindow::redraw`](crate::monitor_gui::MonitorWindow::redraw))。
impl PluginTimerImpl for TestMonitorMainThread<'_> {
    fn on_timer(&self, _timer_id: TimerId) {
        if let Ok(mut view) = self.view.try_borrow_mut() {
            view.tick();
        }
        if let Some(window) = self.window.borrow().as_ref() {
            window.redraw();
        }
    }
}

/// オーディオスレッドで動くプロセッサ
pub struct TestMonitorAudioProcessor<'a> {
    shared: &'a TestMonitorShared,
    /// GUI へサンプルを渡す輪。**入らなければ捨てる** (見せる相手がいないだけ)
    to_gui: rtrb::Producer<f32>,
}

impl<'a> PluginAudioProcessor<'a, TestMonitorShared, TestMonitorMainThread<'a>>
    for TestMonitorAudioProcessor<'a>
{
    fn activate(
        _host: HostAudioProcessorHandle<'a>,
        _main_thread: &TestMonitorMainThread,
        shared: &'a TestMonitorShared,
        audio_config: PluginAudioConfiguration,
    ) -> Result<Self, PluginError> {
        // **輪はここで張る。** 掛け直すたびに新しくすることで、
        // 前回の残りが次の表示に混ざらない。0.7秒ぶんあれば画面が
        // 少し止まっても溢れない
        let to_gui = shared
            .handoff
            .open(audio_config.sample_rate as u32, RING_SAMPLES);
        Ok(Self { shared, to_gui })
    }

    /// 入力を見るだけ。**出力ポートが無いので何も書かない。**
    fn process(
        &mut self,
        _process: Process,
        audio: Audio,
        events: Events,
    ) -> Result<ProcessStatus, PluginError> {
        for event in events.input {
            if let Some(CoreEventSpace::ParamValue(event)) = event.as_core_event() {
                self.shared.handle_param_event(event);
            }
        }

        let Some(port) = audio.input_port(0) else {
            // 入力が繋がっていない。見るものが無いだけで、異常ではない
            return Ok(ProcessStatus::Continue);
        };
        let channels = port
            .channels()?
            .into_f32()
            .ok_or(PluginError::Message("Expected f32 input"))?;

        let mut peak = 0.0f32;
        for channel in channels.iter() {
            for sample in channel {
                peak = peak.max(sample.abs());
            }
        }
        self.shared.observe(peak);

        // 見せる相手 (GUI) がいれば流す。**入らなければ捨てる**
        let left = channels.channel(0).unwrap_or(&[]);
        let right = channels.channel(1).unwrap_or(left);
        feed_gui(&mut self.to_gui, left, right);

        // **Sleep を返さない。** 音は出さないが、次のブロックも見たい
        Ok(ProcessStatus::Continue)
    }
}

/// 見ている音を GUI へ流す。**待たない・確保しない**ので、
/// オーディオスレッドの規約から外れない。
///
/// フレームの途中で切ると、次の塊で L と R が入れ替わったまま届く。
/// **必ず偶数個**にしてから渡す。
fn feed_gui(to_gui: &mut rtrb::Producer<f32>, left: &[f32], right: &[f32]) {
    let frames = left.len().min(right.len());
    let want = (frames * 2).min(to_gui.slots()) & !1;
    if want == 0 {
        return;
    }
    if let Ok(chunk) = to_gui.write_chunk_uninit(want) {
        chunk.fill_from_iter((0..want / 2).flat_map(|frame| [left[frame], right[frame]]));
    }
}

impl PluginAudioPortsImpl for TestMonitorMainThread<'_> {
    /// **入力だけを持ち、出力は 0 本。** ここがこの治具の全て。
    ///
    /// ホスト側はこれを見て「音を返さない段」と判断し、チェーンでは
    /// 入ってきた音をそのまま後ろへ流す。
    fn count(&self, is_input: bool) -> u32 {
        if is_input {
            1
        } else {
            0
        }
    }

    fn get(&self, index: u32, is_input: bool, writer: &mut AudioPortInfoWriter) {
        if !is_input || index != 0 {
            return;
        }
        writer.set(&AudioPortInfo {
            id: ClapId::new(1),
            name: b"main",
            // **ここだけステレオ。** 他の治具は計算を単純にするためモノラルだが、
            // モニタはホストのマスターメーターと見比べるものなので、
            // ホストと同じステレオのまま測りたい (落として測ると値がずれる)
            channel_count: 2,
            flags: AudioPortFlags::IS_MAIN,
            port_type: Some(AudioPortType::STEREO),
            in_place_pair: None,
        });
    }
}

impl PluginMainThreadParams for TestMonitorMainThread<'_> {
    fn count(&self) -> u32 {
        2
    }

    fn get_info(&self, param_index: u32, info: &mut ParamInfoWriter) {
        let (id, name, flags, max) = match param_index {
            0 => (
                PARAM_PEAK_ID,
                b"Observed Peak".as_slice(),
                ParamInfoFlags::IS_READONLY,
                MAX_PEAK as f64,
            ),
            1 => (
                PARAM_RESET_ID,
                b"Reset".as_slice(),
                ParamInfoFlags::IS_STEPPED,
                1.0,
            ),
            _ => return,
        };

        info.set(&ParamInfo {
            id,
            flags,
            cookie: Default::default(),
            name,
            module: b"",
            min_value: 0.0,
            max_value: max,
            default_value: 0.0,
        });
    }

    fn get_value(&self, param_id: ClapId) -> Option<f64> {
        match param_id {
            PARAM_PEAK_ID => Some(self.shared.peak() as f64),
            // 押しっぱなしにはならない
            PARAM_RESET_ID => Some(0.0),
            _ => None,
        }
    }

    fn value_to_text(
        &self,
        param_id: ClapId,
        value: f64,
        writer: &mut ParamDisplayWriter,
    ) -> std::fmt::Result {
        match param_id {
            PARAM_PEAK_ID => write!(writer, "{value:.4}"),
            PARAM_RESET_ID => write!(writer, "{}", if value >= 1.0 { "reset" } else { "-" }),
            _ => Err(std::fmt::Error),
        }
    }

    fn text_to_value(&self, param_id: ClapId, text: &CStr) -> Option<f64> {
        let text = text.to_str().ok()?.trim();
        match param_id {
            PARAM_RESET_ID => text.parse().ok(),
            // 読み取り専用
            _ => None,
        }
    }

    fn flush(
        &self,
        input_parameter_changes: &InputEvents,
        _output_parameter_changes: &mut OutputEvents,
    ) {
        for event in input_parameter_changes {
            if let Some(CoreEventSpace::ParamValue(event)) = event.as_core_event() {
                self.shared.handle_param_event(event);
            }
        }
    }
}

/// GUI の口。**embedded (ホストの窓に埋め込む) だけ**を受ける。
///
/// floating (自前で窓を出す) は要らない。ホスト側が embedded を優先するので
/// この治具では通らず、両方持つと確かめられない経路が増える。
impl PluginGuiImpl for TestMonitorMainThread<'_> {
    fn is_api_supported(&self, configuration: GuiConfiguration) -> bool {
        configuration.api_type == GuiApiType::WIN32 && !configuration.is_floating
    }

    fn get_preferred_api(&self) -> Option<GuiConfiguration<'_>> {
        Some(GuiConfiguration {
            api_type: GuiApiType::WIN32,
            is_floating: false,
        })
    }

    /// 窓はまだ作らない。**親が決まる [`set_parent`](Self::set_parent) で作る**
    fn create(&self, configuration: GuiConfiguration) -> Result<(), PluginError> {
        if !self.is_api_supported(configuration) {
            return Err(PluginError::Message("win32 の埋め込みだけに対応しています"));
        }
        Ok(())
    }

    /// 窓を閉じる。**表示状態 (`view`) は残す** ので、開き直すと続きから出る
    fn destroy(&self) {
        self.window.borrow_mut().take();
        // 窓が無い間まで叩かせない
        if let (Some(extension), Some(id)) = (&self.host_timer, self.timer.take()) {
            let _ = extension.unregister_timer(&self.host, id);
        }
    }

    fn set_scale(&self, _scale: f64) -> Result<(), PluginError> {
        // 拡大率は見ない (治具なので等倍で足りる)
        Ok(())
    }

    fn get_size(&self) -> Option<GuiSize> {
        Some(GuiSize {
            width: WIDTH,
            height: HEIGHT,
        })
    }

    fn can_resize(&self) -> bool {
        true
    }

    fn set_size(&self, size: GuiSize) -> Result<(), PluginError> {
        if let Some(window) = self.window.borrow().as_ref() {
            window.resize(size.width, size.height);
        }
        Ok(())
    }

    /// **ここで実際に窓を作る。** 親が決まらないと子ウィンドウは作れない
    fn set_parent(&self, window: Window) -> Result<(), PluginError> {
        let parent = window
            .as_win32_hwnd()
            .ok_or(PluginError::Message("win32 の窓ではありません"))?;

        // 掛け直しに備えて古い窓を先に捨てる
        self.window.borrow_mut().take();

        // **描き直しはホストのタイマーに任せる。** 自前の `SetTimer` だと
        // `WM_TIMER` がメッセージの中で最も優先度が低く、ホストが描画で回り
        // 続けている間は順番が回ってこない (実測で 300〜500ms まで開いた)。
        // `clap.timer-support` はホストが直接呼ぶので、要求どおりに届く。
        //
        // 持たないホストのために `SetTimer` も残す (どこでも動くのが前提の治具)。
        let host_driven = self.ensure_host_timer();

        // SAFETY: `parent` はホストが渡した有効な HWND。`view` は `Box` の中なので
        // 場所が動かず、`self` (と窓) より長生きする
        let created = unsafe {
            MonitorWindow::open(
                parent.cast(),
                &*self.view as *const RefCell<MonitorView>,
                WIDTH,
                HEIGHT,
                !host_driven,
            )
        };
        *self.window.borrow_mut() =
            Some(created.ok_or(PluginError::Message("ウィンドウを作れませんでした"))?);
        Ok(())
    }

    fn set_transient(&self, _window: Window) -> Result<(), PluginError> {
        Err(PluginError::Message("floating には対応していません"))
    }

    fn show(&self) -> Result<(), PluginError> {
        if let Some(window) = self.window.borrow().as_ref() {
            window.set_visible(true);
        }
        Ok(())
    }

    fn hide(&self) -> Result<(), PluginError> {
        if let Some(window) = self.window.borrow().as_ref() {
            window.set_visible(false);
        }
        Ok(())
    }
}

impl PluginAudioProcessorParams for TestMonitorAudioProcessor<'_> {
    fn flush(
        &mut self,
        input_parameter_changes: &InputEvents,
        _output_parameter_changes: &mut OutputEvents,
    ) {
        for event in input_parameter_changes {
            if let Some(CoreEventSpace::ParamValue(event)) = event.as_core_event() {
                self.shared.handle_param_event(event);
            }
        }
    }
}
