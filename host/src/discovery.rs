//! .clap ファイルのロードとプラグイン列挙。

#![allow(unsafe_code)]

use clack_host::prelude::*;
use std::error::Error;
use std::path::Path;

/// ファイル内で見つかったプラグインの情報
pub struct FoundPlugin {
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
