//! プラグインを**別プロセスで開く**走査。
//!
//! 走査はプラグインの DLL を実行することと同じで、**行儀の悪いものは
//! ホストごと落とす**。ライセンス確認に失敗して `abort()` を呼ぶもの、
//! 純粋仮想関数を叩くもの、初期化で死ぬもの。Rust の `catch_unwind` では
//! 助からない (`abort()` は巻き戻さずにプロセスを終わらせる)。
//!
//! **落ちてよい相手を用意する**しかない。子プロセスに開かせて、結果を
//! ファイル越しに受け取る。子が落ちても親は生きているので、
//! そのファイルを飛ばして次へ進める。Ardour が `ardour-vst3-scanner` を、
//! Zrythm が `carla-discovery` を別に持っているのと同じ形。
//!
//! ## 子はこの実行ファイル自身
//!
//! 専用の実行ファイルを増やさず、`--scan-one <プラグイン> <書き出し先>` で
//! 自分を起動する。GUI を立ち上げる前に [`child_main`] が処理して終わる。
//!
//! ## 結果はファイルで渡す。標準出力は使わない
//!
//! **プラグインは平気で標準出力へ書く。** ロゴを出すもの、デバッグログを
//! 垂れ流すもの。同じ流れに結果を混ぜると壊れる。子は書き出し先を引数で
//! もらい、そこへ `.ron` を1つ書く。
//!
//! ## 落ちたら記録する
//!
//! 子が異常終了したら [`Failure::Crashed`]、時間切れなら
//! [`Failure::TimedOut`]。呼び出し側 ([`crate::library::Scan`]) が
//! それを `blocked` へ入れるので、次の走査では開きに行かない。

use crate::discovery::FoundPlugin;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

/// 子として起動するときの合図
pub const SCAN_ARG: &str = "--scan-one";

/// 1ファイルに与える時間。**超えたら殺す**。
///
/// 重い音源でも数秒で終わる。ここまで待って返らないものは、ライセンス確認で
/// ネットワークを待っているか、窓を出そうとして固まっている
pub const TIMEOUT: Duration = Duration::from_secs(30);

/// 子の様子を見る間隔
const POLL: Duration = Duration::from_millis(20);

/// 別プロセスでの走査が失敗した理由
#[derive(Debug)]
pub enum Failure {
    /// 子が落ちた。**このファイルは開いてはいけない**
    Crashed(String),
    /// 時間内に返らなかった。**同上**
    TimedOut,
    /// 子は無事に終わったが、開けないと言っている (中身の問題)
    Refused(String),
    /// 子を起動できなかった。**プラグインのせいではない**ので、
    /// 呼び出し側は自分で開き直してよい
    CannotSpawn(String),
}

impl Failure {
    /// このファイルを次から飛ばすべきか。
    ///
    /// **開けないと言われただけなら飛ばさない。** 環境変数を足せば読める
    /// といった直せる理由がある (clap-wrapper の VST3 など)
    pub fn should_block(&self) -> bool {
        matches!(self, Failure::Crashed(_) | Failure::TimedOut)
    }
}

impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Failure::Crashed(detail) => write!(f, "走査したプロセスが落ちました ({detail})"),
            Failure::TimedOut => write!(f, "{} 秒待っても返りませんでした", TIMEOUT.as_secs()),
            Failure::Refused(detail) => write!(f, "{detail}"),
            Failure::CannotSpawn(detail) => {
                write!(f, "走査するプロセスを起動できません ({detail})")
            }
        }
    }
}

/// 子として起てる実行ファイルの名前 (拡張子は付けない)
const HOST_EXE: &str = "egui-clap-host";

/// 子が書く結果の置き場。**1件ずつ順に走査する**ので1つで足りる
fn result_path() -> PathBuf {
    crate::library::config_dir().join("scan_result.ron")
}

/// 子として起てる実行ファイルを決める。
///
/// **自分自身とは限らない。** [`child_main`] を持っているのはホストの
/// 実行ファイルだけで、検証用のバイナリ (`scan_smoke` など) やテストの
/// ハーネスから呼ぶと自分を起てても走査してくれない。
///
/// 自分がホストならそれを使い、違えば**隣に居るホスト**を探す。
/// 見つからなければ `None` を返し、呼び出し側は同じプロセスで開く。
fn scanner_exe() -> Option<PathBuf> {
    let me = std::env::current_exe().ok()?;
    if me.file_stem()?.to_str()? == HOST_EXE {
        return Some(me);
    }

    let beside = me.with_file_name(format!("{HOST_EXE}{}", std::env::consts::EXE_SUFFIX));
    beside.is_file().then_some(beside)
}

/// 見つけたものを `.ron` のテキストにする
pub fn to_string(found: &[FoundPlugin]) -> Result<String, String> {
    let config = ron::ser::PrettyConfig::new()
        .indentor("    ".to_string())
        .struct_names(false)
        .depth_limit(2);
    ron::ser::to_string_pretty(&found, config).map_err(|e| format!("組み立てられません: {e}"))
}

/// `.ron` のテキストを読む
pub fn from_str(text: &str) -> Result<Vec<FoundPlugin>, String> {
    ron::from_str(text).map_err(|e| format!("走査の結果として読めません:\n{e}"))
}

/// **子プロセス側。** 1ファイルだけ開いて結果を書き、終わる。
///
/// 引数がこの形でなければ `None` を返す (親として動く合図)。
/// 戻り値は終了コードで、**0 なら結果のファイルが書けている**。
///
/// 開けなかったときも 0 で終わってその理由を書く。**落ちたことと
/// 開けなかったことを終了コードで区別する**ため
/// (異常終了は親から見て「このファイルは危険」を意味する)。
pub fn child_main(args: &[String]) -> Option<i32> {
    let at = args.iter().position(|arg| arg == SCAN_ARG)?;
    let plugin = args.get(at + 1)?;
    let out = args.get(at + 2)?;

    let text = match crate::discovery::scan_file(Path::new(plugin)) {
        Ok(found) => match to_string(&found) {
            Ok(text) => text,
            Err(e) => format!("Err({:?})", e),
        },
        // 開けなかった理由をそのまま渡す。`Err(..)` を頭に付けて見分ける
        Err(e) => format!("Err({:?})", e.to_string()),
    };

    match std::fs::write(out, text) {
        Ok(()) => Some(0),
        // 書けなければ親には「落ちた」と同じに見えるが、それでよい
        // (結果が無いことに変わりはない)
        Err(_) => Some(2),
    }
}

/// **親プロセス側。** 子を起てて1ファイル走査させる。
///
/// 時間切れなら殺す。子が落ちれば [`Failure::Crashed`] で返るので、
/// **こちらは生き残る**。
pub fn scan_file(path: &Path) -> Result<Vec<FoundPlugin>, Failure> {
    let exe = scanner_exe()
        .ok_or_else(|| Failure::CannotSpawn(format!("{HOST_EXE} が隣に見つかりません")))?;
    let out = result_path();

    // 前回の残りを消しておく。**残っていると古い結果を読む**
    let _ = std::fs::remove_file(&out);
    if let Some(dir) = out.parent() {
        let _ = std::fs::create_dir_all(dir);
    }

    let mut child = Command::new(&exe)
        .arg(SCAN_ARG)
        .arg(path)
        .arg(&out)
        // 子の標準出力・標準エラーは捨てる。**プラグインが何を書くか
        // 分からない**ので、親の出力に混ぜない
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .stdin(std::process::Stdio::null())
        .spawn()
        .map_err(|e| Failure::CannotSpawn(format!("{}: {e}", exe.display())))?;

    let deadline = Instant::now() + TIMEOUT;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {}
            Err(e) => return Err(Failure::CannotSpawn(format!("様子を見られません: {e}"))),
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            let _ = std::fs::remove_file(&out);
            return Err(Failure::TimedOut);
        }
        std::thread::sleep(POLL);
    };

    if !status.success() {
        let _ = std::fs::remove_file(&out);
        return Err(Failure::Crashed(describe(&status)));
    }

    let text = std::fs::read_to_string(&out)
        .map_err(|e| Failure::Crashed(format!("結果を残さずに終わりました: {e}")))?;
    let _ = std::fs::remove_file(&out);

    // 子が「開けなかった」と書いた場合
    if let Some(reason) = text.strip_prefix("Err(") {
        let reason = reason.trim_end().trim_end_matches(')');
        return Err(Failure::Refused(unquote(reason)));
    }

    from_str(&text).map_err(Failure::Refused)
}

/// 終了の仕方を人が読める形にする。
///
/// **Windows では終了コードがそのまま例外コードになる。** `0xC0000005`
/// (アクセス違反) や `0xC0000409` (スタック破壊) が出れば、プラグインが
/// 落ちたということ
fn describe(status: &std::process::ExitStatus) -> String {
    match status.code() {
        Some(code) if code < 0 || code as u32 >= 0xC000_0000 => {
            format!("終了コード {:#010X}", code as u32)
        }
        Some(code) => format!("終了コード {code}"),
        None => "強制終了".to_string(),
    }
}

/// `ron` が書いた文字列リテラルを剥がす (`"..."` と `\"` を戻す)
fn unquote(text: &str) -> String {
    let inner = text
        .strip_prefix('"')
        .and_then(|rest| rest.strip_suffix('"'))
        .unwrap_or(text);
    inner.replace("\\\"", "\"").replace("\\n", "\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::library::Role;

    fn found(id: &str, role: Role) -> FoundPlugin {
        FoundPlugin {
            id: id.to_string(),
            name: format!("名前 {id}"),
            role,
            vendor: "作者".to_string(),
            version: "1.0".to_string(),
        }
    }

    /// **書いて読んで元に戻ること。** ここが崩れると走査の結果が届かない
    #[test]
    fn a_result_survives_a_round_trip() {
        let before = vec![
            found("com.example.one", Role::Instrument),
            found("com.example.two", Role::Effect),
        ];
        let text = to_string(&before).expect("書けること");
        let after = from_str(&text).expect("読めること");
        assert_eq!(after.len(), 2);
        assert_eq!(after[0].id, "com.example.one");
        assert_eq!(after[0].role, Role::Instrument);
        assert_eq!(after[1].role, Role::Effect);
    }

    /// 空の結果も往復すること (中に1つも入っていないファイル)
    #[test]
    fn an_empty_result_survives_too() {
        let text = to_string(&[]).expect("書けること");
        assert!(from_str(&text).expect("読めること").is_empty());
    }

    /// **落ちたときだけ次から飛ばすこと。**
    /// 開けないだけのものを飛ばすと、直しても戻ってこない
    #[test]
    fn only_a_crash_blocks_the_file() {
        assert!(Failure::Crashed("x".into()).should_block());
        assert!(Failure::TimedOut.should_block());
        assert!(!Failure::Refused("x".into()).should_block());
        assert!(!Failure::CannotSpawn("x".into()).should_block());
    }

    /// 子が書いた「開けなかった理由」が、そのまま読めること
    #[test]
    fn a_refusal_keeps_its_text() {
        let written = format!("Err({:?})", "隣に Wrapped.clap があります");
        let stripped = written.strip_prefix("Err(").unwrap();
        let reason = unquote(stripped.trim_end_matches(')'));
        assert_eq!(reason, "隣に Wrapped.clap があります");
    }

    /// 改行を含む理由も戻ること (手がかりを添えた文は2行になる)
    #[test]
    fn a_multi_line_refusal_comes_back_whole() {
        let written = format!("Err({:?})", "開けません\n  → 手がかり");
        let stripped = written.strip_prefix("Err(").unwrap();
        let reason = unquote(stripped.trim_end_matches(')'));
        assert_eq!(reason, "開けません\n  → 手がかり");
    }

    /// 引数がその形でなければ、子として動かないこと
    #[test]
    fn without_the_flag_it_is_not_a_child() {
        assert_eq!(child_main(&[]), None);
        assert_eq!(child_main(&["--open-gui".to_string()]), None);
        // 合図はあっても引数が足りなければ動かない
        assert_eq!(child_main(&[SCAN_ARG.to_string()]), None);
        assert_eq!(
            child_main(&[SCAN_ARG.to_string(), "a.clap".to_string()]),
            None
        );
    }
}
