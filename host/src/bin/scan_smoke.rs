//! フォルダ走査の検証。**渡したフォルダの中身を実際に開く。**
//!
//! `target` を渡すと、検証用の `test_plugin.clap` (と `.vst3`) から
//! **音源1つ・エフェクト1つ**が出るはず。これが揃えば
//! 「1ファイルに複数」「種別の見分け」の両方が通っていることになる。
//!
//! 本物のプラグインフォルダを渡せば、そこで何が読めて何が読めないかが分かる。
//! **読めなかったものも必ず出す** (黙って一覧から消えるのがいちばん困る)。
//!
//! **走査は2回まわす。**
//!
//! 1. 記録が無い状態から全部開く → ファイルごとの所要時間が出る
//! 2. そのまま走査し直す → **1件も開かないはず** (差分走査)
//!
//! 2回目で開いたものが出たら、印の取り方がそのファイルに効いていない
//! (ディレクトリを見てしまう等)。時間のかかるプラグインが分かるので、
//! 遅い環境ではまずこれを流すとよい。
//!
//! 使い方: cargo run -p egui-clap-host --bin scan_smoke -- <フォルダ> [フォルダ...]

use egui_clap_host::discovery;
use egui_clap_host::library::{self, Library, Role, Scan};
use std::error::Error;
use std::path::PathBuf;
use std::time::{Duration, Instant};

/// いちばん遅かったファイルを何件出すか
const SLOWEST: usize = 5;

/// 走査を1回まわす。戻り値は (かかった時間, ファイルごとの ms, 走査の結果)
fn run(library: &mut Library, full: bool) -> (Duration, Vec<(u128, PathBuf)>, Scan) {
    let started = Instant::now();
    let mut scan = if full {
        Scan::start_full(library)
    } else {
        Scan::start(library)
    };
    let mut timings = Vec::new();
    while !scan.is_done() {
        let at = Instant::now();
        if let Some(path) = scan.step(library) {
            timings.push((at.elapsed().as_millis(), path));
        }
    }
    let elapsed = started.elapsed();
    library.sort();
    // 遅かったものから並べる
    timings.sort_by_key(|(took, _)| std::cmp::Reverse(*took));
    (elapsed, timings, scan)
}

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

    // **フォルダを渡すもの。** プラグインそのものを渡されたときは、
    // 「0件」と出して終わるより先に理由を言う
    if let Some(not_a_folder) = folders.iter().find(|path| !path.is_dir()) {
        return Err(format!(
            "{} はフォルダではありません。\
             プラグインの入っているフォルダを渡してください",
            not_a_folder.display()
        )
        .into());
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
    let (elapsed, timings, scan) = run(&mut library, false);

    let (done, total) = scan.progress();
    println!(
        "1回目: {done}/{total} ファイルを開いて {} 個のプラグインを見つけた ({:.1} 秒)",
        library.plugins.len(),
        elapsed.as_secs_f64()
    );
    for (took, path) in timings.iter().take(SLOWEST) {
        println!("  {took:>6} ms  {}", path.display());
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

    // **2回目。** 何も変えていないので、1件も開かないはず
    let before = library.clone();
    let (again, reopened, second) = run(&mut library, false);
    let (opened, _) = second.progress();
    println!();
    println!(
        "2回目: {opened} 件を開き、{} 件は前の記録を使った ({:.1} 秒)",
        second.reused(),
        again.as_secs_f64()
    );
    for (took, path) in reopened.iter().take(SLOWEST) {
        println!("  開き直した: {took:>6} ms  {}", path.display());
    }
    if opened > 0 {
        return Err(format!(
            "2回目で {opened} 件を開き直した。\
             印がそのファイルに効いていない (上の一覧を参照)"
        )
        .into());
    }
    if library != before {
        return Err("使い回した記録が1回目と食い違う".into());
    }

    println!("✅ 走査できた (差分走査も効いている)");
    Ok(())
}
