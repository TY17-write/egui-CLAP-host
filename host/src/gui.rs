//! プラグイン独自 GUI の管理。
//!
//! CLAP ([`PluginGuiManager`]) と VST3 ([`Vst3GuiManager`]) で別の型になる。
//! 拡張の呼び方が違うだけで、**ネイティブウィンドウ (`plugin_window.rs`) は共通**。
//! ホストが Win32 の窓を作り、プラグインにはその HWND を渡して中身を描かせる、
//! という組み立ては両形式で同じ。
//!
//! CLAP は embedded (Win32 埋め込み) を優先し、非対応なら floating に落とす。
//! VST3 に floating に当たるものは無いので、常に埋め込みになる。

#![allow(unsafe_code)]

use crate::host::MainThreadMessage;
use crate::plugin_window::PluginWindow;
use clack_extensions::gui::{
    GuiApiType, GuiConfiguration, GuiSize, PluginGui, Window as ClapWindow,
};
use clack_host::prelude::*;
use crossbeam_channel::Sender;
use std::error::Error;
use std::ffi::CString;
use vst3_host::plugin::{Plugin as Vst3Plugin, WindowHandle};

/// プラグイン GUI の状態を管理する (メインスレッド専用)
pub struct PluginGuiManager {
    plugin_gui: PluginGui,
    /// ネゴシエート済みの構成。None なら GUI 非対応。
    configuration: Option<GuiConfiguration<'static>>,
    /// embedded モードで使うネイティブウィンドウ
    window: Option<PluginWindow>,
    pub is_open: bool,
    can_resize: bool,
}

impl PluginGuiManager {
    pub fn new(plugin_gui: PluginGui, plugin: &mut PluginMainThreadHandle) -> Self {
        Self {
            plugin_gui,
            configuration: Self::negotiate_configuration(&plugin_gui, plugin),
            window: None,
            is_open: false,
            can_resize: false,
        }
    }

    /// プラットフォーム標準 API (Windows では Win32) で embedded → floating の順に交渉する
    fn negotiate_configuration(
        gui: &PluginGui,
        plugin: &mut PluginMainThreadHandle,
    ) -> Option<GuiConfiguration<'static>> {
        let api_type = GuiApiType::default_for_current_platform()?;
        let mut config = GuiConfiguration {
            api_type,
            is_floating: false,
        };

        if gui.is_api_supported(plugin, config) {
            Some(config)
        } else {
            config.is_floating = true;
            if gui.is_api_supported(plugin, config) {
                Some(config)
            } else {
                None
            }
        }
    }

    /// このプラグインで GUI を開けるか
    pub fn supports_gui(&self) -> bool {
        self.configuration.is_some()
    }

    /// 現在の構成が floating モードか
    pub fn is_floating(&self) -> bool {
        matches!(
            self.configuration,
            Some(GuiConfiguration {
                is_floating: true,
                ..
            })
        )
    }

    /// プラグイン GUI を開く
    pub fn open(
        &mut self,
        plugin: &mut PluginMainThreadHandle,
        title: &str,
        sender: Sender<MainThreadMessage>,
    ) -> Result<(), Box<dyn Error>> {
        if self.is_open {
            return Ok(());
        }
        let Some(configuration) = self.configuration else {
            return Err("このプラグインは対応する GUI API を持っていません".into());
        };

        self.plugin_gui.create(plugin, configuration)?;

        if configuration.is_floating {
            // プラグイン自身がウィンドウを作る
            if let Ok(title) = CString::new(title) {
                self.plugin_gui.suggest_title(plugin, &title);
            }
            self.plugin_gui.show(plugin)?;
        } else {
            // ホストが作った Win32 ウィンドウに埋め込む
            let initial_size = self.plugin_gui.get_size(plugin).unwrap_or(GuiSize {
                width: 640,
                height: 480,
            });
            self.can_resize = self.plugin_gui.can_resize(plugin);

            let window = PluginWindow::create(
                title,
                initial_size.width,
                initial_size.height,
                self.can_resize,
                sender,
            )
            .ok_or("ネイティブウィンドウの作成に失敗しました")?;

            // SAFETY: window はプラグイン GUI の破棄 (close) まで生存する
            unsafe {
                self.plugin_gui
                    .set_parent(plugin, ClapWindow::from_win32_hwnd(window.hwnd()))?
            };
            // show を呼ばないと表示されないプラグインもあれば、エラーを返すものもある
            let _ = self.plugin_gui.show(plugin);

            self.window = Some(window);
        }

        self.is_open = true;
        Ok(())
    }

    /// プラグインからのリサイズ要求 (request_resize) を反映する
    pub fn on_plugin_request_resize(&mut self, new_size: GuiSize) {
        if let Some(window) = &self.window {
            window.resize_client(new_size.width, new_size.height);
        }
    }

    /// ユーザーがネイティブウィンドウをリサイズしたときの処理
    pub fn on_user_resized(
        &mut self,
        plugin: &mut PluginMainThreadHandle,
        width: u32,
        height: u32,
    ) {
        if !self.is_open {
            return;
        }
        let Some(window) = &self.window else {
            return;
        };

        if !self.can_resize {
            // 固定サイズのプラグインなら、プラグインの言うサイズに戻す
            if let Some(size) = self.plugin_gui.get_size(plugin) {
                if size.width != width || size.height != height {
                    window.resize_client(size.width, size.height);
                }
            }
            return;
        }

        let requested = GuiSize { width, height };
        let adjusted = self
            .plugin_gui
            .adjust_size(plugin, requested)
            .unwrap_or(requested);

        let _ = self.plugin_gui.set_size(plugin, adjusted);

        if adjusted.width != width || adjusted.height != height {
            window.resize_client(adjusted.width, adjusted.height);
        }
    }

    /// GUI を閉じてリソースを解放する
    pub fn close(&mut self, plugin: &mut PluginMainThreadHandle) {
        if self.is_open {
            self.plugin_gui.destroy(plugin);
            self.is_open = false;
        }
        // Drop で DestroyWindow が呼ばれる
        self.window = None;
    }
}

/// エディタを貼り付ける前の仮のウィンドウサイズ。
///
/// 本来のサイズもリサイズ可否も、**貼り付けたあとに**聞いて合わせる。開く前に
/// 聞くこともできるが、エディタが無い状態での問い合わせは**そのためだけに
/// 使い捨ての view を作る**。Surge XT ではエディタの生成そのものが数秒かかり、
/// その間ずっと音源を握ることになる (= そのトラックが無音になる) ので、
/// 問い合わせは生きている view に対してだけ行う。
const VST3_PROVISIONAL_SIZE: (u32, u32) = (640, 480);

/// VST3 エディタの管理 (メインスレッド専用)。
///
/// `IPlugView` を直接叩くのではなく `vst3-host` の `open_editor` / `close_editor` を
/// 使う。中身は `createView` → `setFrame` → `attached(hwnd, kPlatformTypeHWND)` で、
/// **窓を作るのはこちら**。`vst3-host` 側の `PluginWindow` / `EmbeddedEditor` には
/// 頼っていない (どちらも Windows での実行時検証がされていないため)。
pub struct Vst3GuiManager {
    /// エディタを埋め込むネイティブウィンドウ (CLAP と同じ型)
    window: Option<PluginWindow>,
    pub is_open: bool,
    /// このプラグインがエディタを持つか
    has_editor: bool,
}

impl Vst3GuiManager {
    pub fn new(plugin: &Vst3Plugin) -> Self {
        Self {
            window: None,
            is_open: false,
            has_editor: plugin.has_editor(),
        }
    }

    /// このプラグインでエディタを開けるか
    pub fn supports_gui(&self) -> bool {
        self.has_editor
    }

    /// エディタを開く
    pub fn open(
        &mut self,
        plugin: &mut Vst3Plugin,
        title: &str,
        sender: Sender<MainThreadMessage>,
    ) -> Result<(), Box<dyn Error>> {
        if self.is_open {
            return Ok(());
        }
        if !self.has_editor {
            return Err("このプラグインはエディタを持っていません".into());
        }

        // まず仮の大きさ・リサイズ可で窓を作って貼り付ける。
        // 大きさも可否も、生きている view に聞き直して後から直す。
        let (width, height) = VST3_PROVISIONAL_SIZE;
        let window = PluginWindow::create(title, width, height, true, sender)
            .ok_or("ネイティブウィンドウの作成に失敗しました")?;

        // SAFETY: window はエディタを閉じる (close) まで生存する
        unsafe { plugin.open_editor(WindowHandle::from_hwnd(window.hwnd()))? };

        window.set_resizable(plugin.editor_can_resize());
        let reported = plugin.get_editor_size();
        if let Ok((width, height)) = reported {
            if width > 0 && height > 0 {
                window.resize_client(width as u32, height as u32);
            }
        }

        // 開いたときの寸法を1行だけ出す。
        //
        // 「窓は開くが触れない」を追うのに要る。**音源が言う大きさ・こちらの窓・
        // 実際に貼り付いた中身**が食い違っていれば、窓の一部に中身が無いことになり、
        // その範囲をクリックしても何も起きない。
        println!(
            "[vst3 gui] {title}: 音源={} 窓={:?} 中身={:?}",
            match reported {
                Ok((width, height)) => format!("{width}x{height}"),
                Err(e) => format!("不明 ({e})"),
            },
            window.client_size(),
            window.embedded_child_size(),
        );
        for child in window.describe_embedded_children() {
            println!("[vst3 gui]   子: {child}");
        }

        self.window = Some(window);
        self.is_open = true;
        Ok(())
    }

    /// プラグイン発のリサイズ要求 (`IPlugFrame::resizeView`) を拾う。
    ///
    /// CLAP は要求がコールバックで届くが、VST3 は溜まった要求を取りに行く形なので、
    /// エディタを開いている間は毎フレーム呼ぶこと。
    ///
    /// view 側の `onSize` は `vst3-host` が要求を受けた時点で済ませているので、
    /// ここでやることはこちらの窓を合わせるだけ。
    pub fn poll_resize_request(&mut self, plugin: &Vst3Plugin) {
        if !self.is_open {
            return;
        }
        let Some((width, height)) = plugin.take_editor_resize_request() else {
            return;
        };
        if let Some(window) = &self.window {
            if width > 0 && height > 0 {
                window.resize_client(width as u32, height as u32);
            }
        }
    }

    /// ユーザーがネイティブウィンドウをリサイズしたときの処理。
    ///
    /// 固定サイズのエディタでも `resize_editor` が今のサイズを返してくるので、
    /// CLAP 側のように分岐を書き分ける必要はない。
    pub fn on_user_resized(&mut self, plugin: &mut Vst3Plugin, width: u32, height: u32) {
        if !self.is_open {
            return;
        }
        let Some(window) = &self.window else {
            return;
        };

        let Ok((accepted_width, accepted_height)) =
            plugin.resize_editor(width.max(1) as i32, height.max(1) as i32)
        else {
            return;
        };

        if accepted_width as u32 != width || accepted_height as u32 != height {
            window.resize_client(accepted_width.max(1) as u32, accepted_height.max(1) as u32);
        }
    }

    /// エディタを閉じてリソースを解放する
    pub fn close(&mut self, plugin: &mut Vst3Plugin) {
        if self.is_open {
            let _ = plugin.close_editor();
            self.is_open = false;
        }
        // Drop で DestroyWindow が呼ばれる
        self.window = None;
    }
}
