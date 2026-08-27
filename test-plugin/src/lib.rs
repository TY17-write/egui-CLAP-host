//! テスト用の最小 CLAP プラグイン。**1つの `.clap` に3つ入っている。**
//!
//! | 索引 | ID | 中身 |
//! |---|---|---|
//! | 0 | `com.example.test-sine` | 16ボイスのサイン波音源 ([`sine`]) |
//! | 1 | `com.example.test-gain` | 係数と直流を掛け足すエフェクト ([`gain`]) |
//! | 2 | `com.example.test-monitor` | **出力を持たない**モニタ ([`monitor`]) |
//!
//! **音源だけでは足りなくなったので2つにした。** 音声ルーティング
//! (`docs/archive/routing_plan.md`) はエフェクトをチェーンに刺す話なのに、
//! 手元にあるのが入力ポートを持たない音源だけでは**経路を検証できない**。
//! 実物のエフェクトで代用すると、落ちたときに**こちらの問題か相手の問題か
//! 切り分けられない**。
//!
//! 3つ目は**出力ポートを持たない**モニタ。モニタリング系を刺した段から後ろが
//! 無音になる不具合があり、その形を手元で再現するために足した (詳細は
//! [`monitor`])。
//!
//! 1ファイルに複数入れているのは、**ホスト側の「1ファイルに複数」の経路も
//! 一緒に通せる**ため (`discovery` と読み込みダイアログがそこを扱っている)。
//! 索引0 が音源のままなので、`plugins[0]` を取る既存の検証バイナリは影響を受けない。
//!
//! # ホストとは何も共有しない
//!
//! **このクレートは `egui-clap-host` に依存しない。** 依存は clack と、
//! 輪 (`rtrb`) と Win32 の口 (`windows-sys`) だけで、`.clap` 単体で
//! **他の DAW に読ませても同じように動く**。
//!
//! モニタが出すスペクトルとラウドネスも、ホストの `meter` を借りずに
//! [`monitor_meter`] に別実装で持っている。借りてしまうと、二つの表示を
//! 見比べても「同じコードが同じ値を出している」ことしか言えない。
//! **検証治具が測る側と同じ実装を使ってはいけない。**

use clack_plugin::entry::prelude::*;
use clack_plugin::entry::DefaultPluginFactory;
use clack_plugin::prelude::*;
use std::ffi::CStr;
use std::sync::atomic::{AtomicU32, Ordering};

pub mod gain;
pub mod monitor;
#[cfg(windows)]
pub mod monitor_gui;
pub mod monitor_meter;
pub mod sine;

use gain::TestGainPlugin;
use monitor::TestMonitorPlugin;
use sine::TestSinePlugin;

/// この `.clap` の入口。
///
/// [`SinglePluginEntry`] ではなく自前の実装にしているのは、
/// **公開するプラグインが2つある**ため。
pub struct TestPluginEntry {
    factory: PluginFactoryWrapper<TestPluginFactory>,
}

impl Entry for TestPluginEntry {
    fn new(_bundle_path: Option<&CStr>) -> Result<Self, EntryLoadError> {
        Ok(Self {
            factory: PluginFactoryWrapper::new(TestPluginFactory::new()),
        })
    }

    fn declare_factories<'a>(&'a self, builder: &mut EntryFactories<'a>) {
        builder.register_factory(&self.factory);
    }
}

/// 3つのプラグインの説明書きを持つファクトリ
struct TestPluginFactory {
    sine: PluginDescriptor,
    gain: PluginDescriptor,
    monitor: PluginDescriptor,
}

impl TestPluginFactory {
    fn new() -> Self {
        Self {
            sine: TestSinePlugin::get_descriptor(),
            gain: TestGainPlugin::get_descriptor(),
            monitor: TestMonitorPlugin::get_descriptor(),
        }
    }
}

impl PluginFactoryImpl for TestPluginFactory {
    fn plugin_count(&self) -> u32 {
        3
    }

    /// **音源を索引0 に固定してある。** 検証バイナリの多くが最初の1つを取るため。
    fn plugin_descriptor(&self, index: u32) -> Option<&PluginDescriptor> {
        match index {
            0 => Some(&self.sine),
            1 => Some(&self.gain),
            2 => Some(&self.monitor),
            _ => None,
        }
    }

    fn create_plugin<'a>(
        &'a self,
        host_info: HostInfo<'a>,
        plugin_id: &CStr,
    ) -> Option<PluginInstance<'a>> {
        if plugin_id == self.sine.id().unwrap_or_default() {
            Some(PluginInstance::new::<TestSinePlugin>(
                host_info,
                &self.sine,
                TestSinePlugin::new_shared,
                TestSinePlugin::new_main_thread,
            ))
        } else if plugin_id == self.gain.id().unwrap_or_default() {
            Some(PluginInstance::new::<TestGainPlugin>(
                host_info,
                &self.gain,
                TestGainPlugin::new_shared,
                TestGainPlugin::new_main_thread,
            ))
        } else if plugin_id == self.monitor.id().unwrap_or_default() {
            Some(PluginInstance::new::<TestMonitorPlugin>(
                host_info,
                &self.monitor,
                TestMonitorPlugin::new_shared,
                TestMonitorPlugin::new_main_thread,
            ))
        } else {
            None
        }
    }
}

/// f32 をアトミックに読み書きするためのヘルパー (両方のプラグインが使う)
pub(crate) struct AtomicF32(AtomicU32);

impl AtomicF32 {
    pub(crate) fn new(value: f32) -> Self {
        Self(AtomicU32::new(value.to_bits()))
    }

    pub(crate) fn store(&self, value: f32, order: Ordering) {
        self.0.store(value.to_bits(), order)
    }

    pub(crate) fn load(&self, order: Ordering) -> f32 {
        f32::from_bits(self.0.load(order))
    }
}

clack_export_entry!(TestPluginEntry);
