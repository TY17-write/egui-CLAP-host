//! `Plugin` / `RealtimePluginRunner` が `Send` かどうかをコンパイルで確定させる。
//!
//! 本体は処理器をリングバッファでオーディオスレッドへ渡す設計なので、
//! ここが `Send` でないとフェーズ2 の作りを変える必要がある。

use vst3_host::plugin::Plugin;
use vst3_host::realtime::{RealtimePluginRunner, RtControl};

/// 通ればその型は `Send`。通らなければコンパイルエラーになる。
fn assert_send<T: Send>(label: &str) {
    println!("{label}: Send");
}

fn main() {
    assert_send::<Plugin>("Plugin");
    assert_send::<RealtimePluginRunner>("RealtimePluginRunner");
    assert_send::<RtControl>("RtControl");
    println!("→ 処理器をオーディオスレッドへ渡す設計がそのまま使える");
}
