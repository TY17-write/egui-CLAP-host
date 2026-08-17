//! 音源の状態 (clap.state) の保存・復元の検証。オーディオデバイス不要。
//!
//! プロジェクト保存で「開いたら音作りも戻る」を成り立たせている経路そのもの。
//! パラメータを既定値から変えて状態を取り出し、**別のインスタンス**へ流し込んで、
//! 値が移っていることを確かめる。
//!
//! 併せて .ron への往復も通す (base64 を挟んでバイト列が壊れないことの確認)。
//!
//! 使い方: cargo run -p clap-host-test --bin state_smoke -- <path\to\plugin.clap>

use clack_extensions::params::PluginParams;
use clack_extensions::state::PluginState;
use clack_host::events::event_types::ParamValueEvent;
use clack_host::events::Pckn;
use clack_host::prelude::*;
use clap_host_test::host::{MiniHost, MiniHostMainThread, MiniHostShared};
use clap_host_test::sequencer::MidiEditor;
use clap_host_test::{discovery, project};
use std::error::Error;
use std::ffi::CString;
use std::io::Cursor;
use std::path::{Path, PathBuf};

/// 検証に使うパラメータ ID (テストプラグインの Volume)
const PARAM_ID: ClapId = ClapId::new(1);
/// 既定値 (0.2) と十分離れた値にする
const TARGET_VALUE: f64 = 0.73;

fn main() -> Result<(), Box<dyn Error>> {
    let path = std::env::args()
        .nth(1)
        .ok_or("使い方: state_smoke <path\\to\\plugin.clap>")?;
    let path = PathBuf::from(path);

    let (_entry, plugins) = discovery::load_clap_file(&path)?;
    let id = plugins[0].id.clone();
    println!("プラグイン: {} ({id})", plugins[0].name);

    // ---- 1. パラメータを変えて状態を取り出す ----
    let mut source = instantiate(&path, &id)?;
    let params = params_of(&mut source).ok_or("params 拡張がありません")?;
    let state = state_of(&mut source).ok_or("state 拡張がありません")?;

    let before = params
        .get_value(&source.plugin_handle(), PARAM_ID)
        .ok_or("パラメータを読めません")?;

    set_param(&mut source, TARGET_VALUE)?;
    let changed = params
        .get_value(&source.plugin_handle(), PARAM_ID)
        .ok_or("パラメータを読めません")?;

    let mut saved = Vec::new();
    state.save(&source.plugin_handle(), &mut saved)?;
    println!(
        "既定値 {before} → 変更後 {changed} / 状態 {} バイト",
        saved.len()
    );

    // ---- 2. .ron へ載せて読み戻す (base64 を挟む) ----
    let snapshot = project::PluginSnapshot {
        kind: project::PluginKind::Clap,
        path: path.clone(),
        id: id.clone(),
        state: saved.clone(),
    };
    // オーディオトラック1 (0 はマスター) の1段目に載せる
    let mut tracks: Vec<project::AudioTrackSnapshot> = (0..project::AUDIO_TRACKS)
        .map(|_| project::AudioTrackSnapshot::default())
        .collect();
    tracks[1] = project::AudioTrackSnapshot {
        name: String::new(),
        nodes: vec![snapshot],
        midi_track: Some(0),
        sends: vec![project::MASTER],
    };

    let text = project::to_string(&MidiEditor::default(), &tracks)?;
    let loaded = project::from_str(&text)?;
    let restored_snapshot = loaded.audio_tracks[1]
        .nodes
        .first()
        .ok_or("音源が .ron から戻ってきません")?;

    // ---- 3. 別インスタンスへ流し込む ----
    let mut target = instantiate(&path, &id)?;
    let target_params = params_of(&mut target).ok_or("params 拡張がありません")?;
    let target_state = state_of(&mut target).ok_or("state 拡張がありません")?;

    let fresh = target_params
        .get_value(&target.plugin_handle(), PARAM_ID)
        .ok_or("パラメータを読めません")?;

    let mut reader = Cursor::new(&restored_snapshot.state);
    target_state.load(&target.plugin_handle(), &mut reader)?;

    let after = target_params
        .get_value(&target.plugin_handle(), PARAM_ID)
        .ok_or("パラメータを読めません")?;
    println!("新しいインスタンス: 初期値 {fresh} → 復元後 {after}");

    // ---- 判定 ----
    let mut failures = Vec::new();
    if (changed - TARGET_VALUE).abs() > 1e-6 {
        failures.push(format!("パラメータを変更できていない ({changed})"));
    }
    if saved.is_empty() {
        failures.push("状態が空".to_string());
    }
    if restored_snapshot.state != saved {
        failures.push("`.ron` を往復してバイト列が変わった".to_string());
    }
    if restored_snapshot.path != path || restored_snapshot.id != id {
        failures.push("パスかプラグイン ID が往復で変わった".to_string());
    }
    if (fresh - before).abs() > 1e-6 {
        failures.push(format!(
            "新しいインスタンスが既定値で始まっていない ({fresh})"
        ));
    }
    if (after - TARGET_VALUE).abs() > 1e-6 {
        failures.push(format!("状態を復元できていない ({after})"));
    }

    if failures.is_empty() {
        println!("✅ 状態の保存・復元テスト成功");
        Ok(())
    } else {
        Err(format!("❌ 失敗: {}", failures.join(", ")).into())
    }
}

fn params_of(instance: &mut PluginInstance<MiniHost>) -> Option<PluginParams> {
    instance.plugin_handle().get_extension()
}

fn state_of(instance: &mut PluginInstance<MiniHost>) -> Option<PluginState> {
    instance.plugin_handle().get_extension()
}

fn instantiate(path: &Path, id: &str) -> Result<PluginInstance<MiniHost>, Box<dyn Error>> {
    let (entry, _) = discovery::load_clap_file(path)?;
    let host_info = HostInfo::new(
        "State Smoke",
        "clap-host-test",
        "https://example.com",
        "0.1.0",
    )?;
    let plugin_id = CString::new(id)?;
    let (sender, _receiver) = crossbeam_channel::unbounded();

    Ok(PluginInstance::<MiniHost>::new(
        |_| MiniHostShared::new(sender.clone()),
        |shared| MiniHostMainThread::new(shared),
        &entry,
        &plugin_id,
        &host_info,
    )?)
}

/// パラメータを変更する (プラグインは flush でこれを受け取る)。
///
/// このバイナリはアクティベートしないので、非アクティブ用のハンドルを使う。
fn set_param(instance: &mut PluginInstance<MiniHost>, value: f64) -> Result<(), Box<dyn Error>> {
    let params = params_of(instance).ok_or("params 拡張がありません")?;
    let mut events = EventBuffer::with_capacity(1);
    events.push(&ParamValueEvent::new(0, PARAM_ID, Pckn::match_all(), value));

    let mut handle = instance
        .inactive_plugin_handle()
        .ok_or("非アクティブなハンドルを取れません")?;
    params.flush(&mut handle, &events.as_input(), &mut OutputEvents::void());
    Ok(())
}
