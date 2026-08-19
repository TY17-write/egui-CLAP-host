//! フォルダ走査の検証。**渡したフォルダの中身を実際に開く。**
//!
//! `target` を渡すと、検証用の `test_plugin.clap` (と `.vst3`) から
//! **音源1つ・エフェクト1つ**が出るはず。これが揃えば
//! 「1ファイルに複数」「種別の見分け」の両方が通っていることになる。
//!
//! 本物のプラグインフォルダを渡せば、そこで何が読めて何が読めないかが分かる。
//! **読めなかったものも必ず出す** (黙って一覧から消えるのがいちばん困る)。
//!
//! 使い方: cargo run -p egui-clap-host --bin scan_smoke -- <フォルダ> [フォルダ...]

use egui_clap_host::discovery;
use egui_clap_host::library::{self, Library, Role, Scan};
use std::error::Error;
use std::path::PathBuf;
use std::time::Instant;

fn main() -> Result<(), Box<dyn Error>> {
    let folders: Vec<PathBuf> = std::env::args().skip(1).map(PathBuf::from).collect();
    if folders.is_empty() {
        println!("使い方: scan_smoke <フォルダ> [フォルダ...]");
        println!();
        println!("この環境の標準の置き場:");
        for folder in library::standard_folders() {
            let mark = if folder.is_dir() { "あり" } else { "なし" };
            println!("  [{mark}] {}", folder.display());
        }
        return Err("フォルダを1つ以上渡してください".into());
    }

    // 候補を数えるところまでは開かない
    for folder in &folders {
        let files = discovery::plugin_files(folder);
        println!("{} → 候補 {} 件", folder.display(), files.len());
    }
    println!();

    let mut library = Library {
        folders: folders.clone(),
        ..Default::default()
    };

    // 前回落ちていたら、そのファイルは飛ばす
    if let Some(crashed) = library::take_crashed() {
        println!(
            "⚠ 前回 {} で落ちています。今回は飛ばします",
            crashed.display()
        );
        library.blocked.push(crashed);
        println!();
    }

    // **本体と同じく1ファイルずつ進める** (画面から毎フレーム呼ぶのと同じ経路)
    let started = Instant::now();
    let mut scan = Scan::start(&mut library);
    let mut slowest = (0u128, PathBuf::new());
    while !scan.is_done() {
        let at = Instant::now();
        if let Some(path) = scan.step(&mut library) {
            let took = at.elapsed().as_millis();
            if took > slowest.0 {
                slowest = (took, path);
            }
        }
    }
    let elapsed = started.elapsed();
    library.sort();

    let (done, total) = scan.progress();
    println!(
        "{done}/{total} ファイルを開いて {} 個のプラグインを見つけた ({:.1} 秒)",
        library.plugins.len(),
        elapsed.as_secs_f64()
    );
    if slowest.0 > 0 {
        println!(
            "  いちばん遅かったファイル: {} ({} ms)",
            slowest.1.display(),
            slowest.0
        );
    }
    println!();

    for role in [Role::Instrument, Role::Effect, Role::Unknown] {
        let entries: Vec<_> = library.by_role(role).collect();
        println!("-- {} ({} 件) --", role.label(), entries.len());
        for entry in entries {
            let vendor = if entry.vendor.is_empty() {
                String::new()
            } else {
                format!(" / {}", entry.vendor)
            };
            println!(
                "  {:?} {}{}  [{}]",
                entry.kind,
                entry.label(),
                vendor,
                entry.id
            );
        }
        println!();
    }

    if !scan.problems().is_empty() {
        println!("-- 開けなかったもの ({} 件) --", scan.problems().len());
        for problem in scan.problems() {
            println!("  {problem}");
        }
        println!();
    }

    // 記録を往復させて、書いたものがそのまま読めることも見る
    let text = library::to_string(&library)?;
    let read_back = library::from_str(&text)?;
    if read_back != library {
        return Err("記録を書いて読み直すと中身が変わる".into());
    }
    println!("記録の往復: OK ({} バイト)", text.len());

    if library.plugins.is_empty() {
        return Err("プラグインが1つも見つからなかった".into());
    }
    println!("✅ 走査できた");
    Ok(())
}
