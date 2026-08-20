//! テスト用の最小 CLAP プラグイン。**1つの `.clap` に2つ入っている。**
//!
//! | 索引 | ID | 中身 |
//! |---|---|---|
//! | 0 | `com.example.test-sine` | 16ボイスのサイン波音源 ([`sine`]) |
//! | 1 | `com.example.test-gain` | 係数と直流を掛け足すエフェクト ([`gain`]) |
//!
//! **音源だけでは足りなくなったので2つにした。** 音声ルーティング
//! (`docs/archive/routing_plan.md`) はエフェクトをチェーンに刺す話なのに、
//! 手元にあるのが入力ポートを持たない音源だけでは**経路を検証できない**。
//! 実物のエフェクトで代用すると、落ちたときに**こちらの問題か相手の問題か
//! 切り分けられない**。
//!
//! 1ファイルに2つ入れているのは、**ホスト側の「1ファイルに複数」の経路も
//! 一緒に通せる**ため (`discovery` と読み込みダイアログがそこを扱っている)。
//! 索引0 が音源のままなので、`plugins[0]` を取る既存の検証バイナリは影響を受けない。

use clack_plugin::entry::prelude::*;
use clack_plugin::entry::DefaultPluginFactory;
use clack_plugin::prelude::*;
use std::ffi::CStr;
use std::sync::atomic::{AtomicU32, Ordering};

pub mod gain;
pub mod sine;

use gain::TestGainPlugin;
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

/// 2つのプラグインの説明書きを持つファクトリ
struct TestPluginFactory {
    sine: PluginDescriptor,
    gain: PluginDescriptor,
}

impl TestPluginFactory {
    fn new() -> Self {
        Self {
            sine: TestSinePlugin::get_descriptor(),
            gain: TestGainPlugin::get_descriptor(),
        }
    }
}

impl PluginFactoryImpl for TestPluginFactory {
    fn plugin_count(&self) -> u32 {
        2
    }

    /// **音源を索引0 に固定してある。** 検証バイナリの多くが最初の1つを取るため。
    fn plugin_descriptor(&self, index: u32) -> Option<&PluginDescriptor> {
        match index {
            0 => Some(&self.sine),
            1 => Some(&self.gain),
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
