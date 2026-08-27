//! CLAP プラグインのパラメータ列挙。
//!
//! **ID を名前から引くために使う。** 検証バイナリ (`gain_smoke` / `chain_smoke`) が
//! プラグイン側の定数を写し取らずにパラメータを指定できるのはこれによる。

use crate::host::MiniHost;
use clack_extensions::params::{ParamInfoBuffer, ParamInfoFlags, PluginParams};
use clack_host::prelude::*;

/// 列挙したパラメータ1つ分
pub struct ParamEntry {
    pub id: ClapId,
    pub name: String,
    pub min: f64,
    pub max: f64,
    /// 列挙した時点の値 (取れなければ既定値)
    pub value: f64,
    /// 整数ステップのパラメータか
    pub is_stepped: bool,
}

/// プラグインの全パラメータを列挙する。params 拡張がなければ空を返す。
pub fn read_params(instance: &mut PluginInstance<MiniHost>) -> Vec<ParamEntry> {
    let mut handle = instance.plugin_handle();
    let Some(params_ext) = handle.get_extension::<PluginParams>() else {
        return vec![];
    };

    let mut buffer = ParamInfoBuffer::new();
    let count = params_ext.count(&mut handle);
    let mut result = Vec::with_capacity(count as usize);

    for index in 0..count {
        let Some(info) = params_ext.get_info(&mut handle, index, &mut buffer) else {
            continue;
        };

        // 不可視パラメータはスキップ
        if info.flags.contains(ParamInfoFlags::IS_HIDDEN) {
            continue;
        }

        let id = info.id;
        let name = String::from_utf8_lossy(info.name).into_owned();
        let min = info.min_value;
        let max = info.max_value;
        let default = info.default_value;
        let is_stepped = info.flags.contains(ParamInfoFlags::IS_STEPPED);

        let value = params_ext.get_value(&mut handle, id).unwrap_or(default);

        result.push(ParamEntry {
            id,
            name,
            min,
            max,
            value,
            is_stepped,
        });
    }

    result
}
