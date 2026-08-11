//! 音源ファイルのロードとプラグイン列挙 (.clap / .vst3)。
//!
//! どちらの形式も「1つのファイルに複数のプラグインが入りうる」ので、
//! 載せる前に中身を数え上げる必要がある。列挙の結果は [`FoundPlugin`] に
//! 揃えてあり、呼び出し側 (選択 UI) は形式を意識しない。

#![allow(unsafe_code)]

use clack_host::prelude::*;
use std::error::Error;
use std::path::Path;

/// ファイル内で見つかったプラグインの情報
pub struct FoundPlugin {
    /// CLAP のプラグイン ID、または VST3 のクラス UID (32桁の16進)
    pub id: String,
    pub name: String,
}

/// .clap ファイルをロードし、含まれる全プラグインの一覧を返す。
///
/// # Safety
/// CLAP のロードは任意の DLL の実行を意味するため、信頼できるファイルのみを開くこと。
pub fn load_clap_file(path: &Path) -> Result<(PluginEntry, Vec<FoundPlugin>), Box<dyn Error>> {
    // SAFETY: ユーザーが明示的に選択したファイルをロードする
    let entry = unsafe { PluginEntry::load(path)? };

    let Some(factory) = entry.get_plugin_factory() else {
        return Err("このファイルにはプラグインファクトリがありません".into());
    };

    let plugins: Vec<FoundPlugin> = factory
        .plugin_descriptors()
        .filter_map(|descriptor| {
            let id = descriptor.id()?.to_str().ok()?.to_string();
            let name = descriptor
                .name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| id.clone());
            Some(FoundPlugin { id, name })
        })
        .collect();

    if plugins.is_empty() {
        return Err("このファイルには CLAP プラグインが含まれていません".into());
    }

    Ok((entry, plugins))
}

/// VST3 のカテゴリ文字列。音源クラスはこれを含む
/// (同じファクトリに入っている他の種類のクラスと区別するため)。
const VST3_AUDIO_CLASS: &str = "Audio Module Class";

/// .vst3 を読み、含まれる音源クラスの一覧を返す。
///
/// Windows では `Foo.vst3` がバンドル**ディレクトリ**のことも素の DLL のこともある。
/// どちらを渡してもよい (`vst3-host` 側が判別する)。
///
/// CLAP と違って `PluginEntry` に当たるものは返さない。`vst3-host` の `Plugin` が
/// モジュールを自分で抱えるので、載せるときに開き直せばよい。
///
/// # Safety
/// VST3 のロードは任意の DLL の実行を意味するため、信頼できるファイルのみを開くこと。
pub fn load_vst3_file(path: &Path) -> Result<Vec<FoundPlugin>, Box<dyn Error>> {
    let info = vst3_host::discovery::get_detailed_plugin_info(path)?;

    let plugins: Vec<FoundPlugin> = info
        .classes
        .iter()
        .filter(|class| class.category.contains(VST3_AUDIO_CLASS))
        .map(|class| FoundPlugin {
            id: class.class_id.clone(),
            name: if class.name.is_empty() {
                class.class_id.clone()
            } else {
                class.name.clone()
            },
        })
        .collect();

    if plugins.is_empty() {
        return Err("このファイルには VST3 の音源が含まれていません".into());
    }

    Ok(plugins)
}
