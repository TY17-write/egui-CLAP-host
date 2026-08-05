//! プラグイン独自 GUI (clap.gui 拡張) の管理。
//! embedded (Win32 埋め込み) を優先し、非対応なら floating にフォールバックする。

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
