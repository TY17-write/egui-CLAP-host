//! clack のホストハンドラ実装。
//! clack リポジトリの cpal サンプルをベースに簡略化したもの。

use crate::timers::Timers;
use clack_extensions::gui::{GuiSize, HostGui, HostGuiImpl, PluginGui};
use clack_extensions::log::{HostLog, HostLogImpl, LogSeverity};
use clack_extensions::params::{
    HostParams, HostParamsImplMainThread, HostParamsImplShared, ParamClearFlags, ParamRescanFlags,
};
use clack_extensions::timer::{HostTimer, HostTimerImpl, PluginTimer, TimerId};
use clack_host::prelude::*;
use crossbeam_channel::Sender;
use std::rc::Rc;
use std::time::Duration;

/// プラグインのどのスレッドからでもメインスレッドへ送られるメッセージ
pub enum MainThreadMessage {
    /// プラグインが on_main_thread コールバックの実行を要求した
    RunOnMainThread,
    /// プラグインが GUI ウィンドウのリサイズを要求した
    GuiRequestResized { new_size: GuiSize },
    /// プラグイン側で GUI が閉じられた (floating モード)
    GuiClosed,
    /// ホスト側ネイティブウィンドウの閉じるボタンが押された (embedded モード)
    PluginWindowClosed,
    /// ユーザーがネイティブウィンドウをリサイズした (クライアント領域の物理ピクセル)
    PluginWindowResized { width: u32, height: u32 },
}

/// このホストの実装本体
pub struct MiniHost;

impl HostHandlers for MiniHost {
    type Shared<'a> = MiniHostShared;
    type MainThread<'a> = MiniHostMainThread<'a>;
    type AudioProcessor<'a> = ();

    fn declare_extensions(builder: &mut HostExtensions<Self>, _shared: &Self::Shared<'_>) {
        builder
            .register::<HostLog>()
            .register::<HostParams>()
            .register::<HostGui>()
            .register::<HostTimer>();
    }
}

/// 全スレッドからアクセスされる共有データ
pub struct MiniHostShared {
    sender: Sender<MainThreadMessage>,
}

impl MiniHostShared {
    pub fn new(sender: Sender<MainThreadMessage>) -> Self {
        Self { sender }
    }
}

impl<'a> SharedHandler<'a> for MiniHostShared {
    fn initializing(&self, _instance: InitializingPluginHandle<'a>) {}

    fn request_restart(&self) {
        // 再起動はサポートしない
    }

    fn request_process(&self) {
        // ストリームは常時 process を呼び続けるので何もしない
    }

    fn request_callback(&self) {
        let _ = self.sender.send(MainThreadMessage::RunOnMainThread);
    }
}

impl HostGuiImpl for MiniHostShared {
    fn resize_hints_changed(&self) {
        // リサイズヒントは未対応
    }

    fn request_resize(&self, new_size: GuiSize) -> Result<(), HostError> {
        Ok(self
            .sender
            .send(MainThreadMessage::GuiRequestResized { new_size })?)
    }

    fn request_show(&self) -> Result<(), HostError> {
        // ウィンドウは常に表示済み
        Ok(())
    }

    fn request_hide(&self) -> Result<(), HostError> {
        Ok(())
    }

    fn closed(&self, _was_destroyed: bool) {
        let _ = self.sender.send(MainThreadMessage::GuiClosed);
    }
}

/// メインスレッド専用データ
pub struct MiniHostMainThread<'a> {
    _shared: &'a MiniHostShared,
    plugin: Option<InitializedPluginHandle<'a>>,
    /// プラグインの GUI 拡張 (あれば)
    pub gui: Option<PluginGui>,
    /// プラグインのタイマー拡張 (あれば)
    pub timer_support: Option<PluginTimer>,
    /// ホスト側のタイマー管理
    pub timers: Rc<Timers>,
}

impl<'a> MiniHostMainThread<'a> {
    pub fn new(shared: &'a MiniHostShared) -> Self {
        Self {
            _shared: shared,
            plugin: None,
            gui: None,
            timer_support: None,
            timers: Rc::new(Timers::new()),
        }
    }
}

impl<'a> MainThreadHandler<'a> for MiniHostMainThread<'a> {
    fn initialized(&mut self, instance: InitializedPluginHandle<'a>) {
        self.gui = instance.get_extension();
        self.timer_support = instance.get_extension();
        self.plugin = Some(instance);
    }
}

impl HostTimerImpl for MiniHostMainThread<'_> {
    fn register_timer(&mut self, period_ms: u32) -> Result<TimerId, HostError> {
        Ok(self
            .timers
            .register_new(Duration::from_millis(period_ms as u64)))
    }

    fn unregister_timer(&mut self, timer_id: TimerId) -> Result<(), HostError> {
        if self.timers.unregister(timer_id) {
            Ok(())
        } else {
            Err(HostError::Message("Unknown timer ID"))
        }
    }
}

impl HostLogImpl for MiniHostShared {
    fn log(&self, severity: LogSeverity, message: &str) {
        if severity <= LogSeverity::Debug {
            return;
        }
        eprintln!("[{severity}] {message}");
    }
}

impl HostParamsImplMainThread for MiniHostMainThread<'_> {
    fn rescan(&mut self, _flags: ParamRescanFlags) {
        // パラメータの動的変更は追跡しない
    }

    fn clear(&mut self, _param_id: ClapId, _flags: ParamClearFlags) {}
}

impl HostParamsImplShared for MiniHostShared {
    fn request_flush(&self) {
        // 常に process が回っているため flush は不要
    }
}
