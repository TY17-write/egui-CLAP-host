//! 走査したプラグインの一覧 (`config\plugins.ron` の読み書き)。
//!
//! **置き場は実行ファイルの隣。** 設定が実行ファイルと一緒に動くので、
//! 持ち運びが素直になる。引き換えに `cargo run` で動かすと
//! `target\debug\config\` に落ちるため、**`cargo clean` すると消える**
//! (走査し直しには時間がかかるので、消したくないときは退避すること)。
//!
//! ここが持つのは**データと入出力だけ**。走査そのものは別 (フェーズ2)。

use crate::project::PluginKind;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// この形式のバージョン。**上げるのは、古いビルドで読めなくなるときだけ**
/// (項目を足すだけなら `#[serde(default)]` で読めるので上げない)。
const VERSION: u32 = 1;

/// 設定を置くディレクトリの名前 (実行ファイルの隣に作る)
const CONFIG_DIR: &str = "config";
/// 一覧の記録
const LIBRARY_FILE: &str = "plugins.ron";
/// 走査中の目印。**開く直前に書き、開き終えたら消す。**
/// 起動時に残っていれば、前回そこで落ちたということ
const SCANNING_FILE: &str = "scanning.txt";

/// プラグインの種別。**宣言を読んだだけで決まる** (インスタンス化しない)。
///
/// | 形式 | 見るところ |
/// |---|---|
/// | CLAP | `PluginDescriptor::features()` の `instrument` / `audio-effect` |
/// | VST3 | `ModuleClassInfo::sub_categories` の `Instrument` / `Fx` |
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, Serialize, Deserialize)]
pub enum Role {
    /// 音を作る側
    Instrument,
    /// 音を加工する側
    Effect,
    /// **どちらも宣言していない。** 隠さず「未分類」として出す
    #[default]
    Unknown,
}

impl Role {
    /// 画面に出す名前
    pub fn label(self) -> &'static str {
        match self {
            Role::Instrument => "音源",
            Role::Effect => "エフェクト",
            Role::Unknown => "未分類",
        }
    }
}

/// 一覧の1件。
///
/// **載せるのに要るのは `kind` / `path` / `id` の3つだけ**で、残りは表示用。
/// この3つは `App::load_plugin` がそのまま受け取れる形にしてある。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Entry {
    pub kind: PluginKind,
    pub path: PathBuf,
    /// CLAP のプラグイン ID、または VST3 のクラス UID。
    /// 1つのファイルに複数入りうるので、パスだけでは足りない
    pub id: String,
    pub name: String,
    pub vendor: String,
    pub version: String,
    pub role: Role,
    pub favorite: bool,
}

impl Default for Entry {
    fn default() -> Self {
        Self {
            kind: PluginKind::Clap,
            path: PathBuf::new(),
            id: String::new(),
            name: String::new(),
            vendor: String::new(),
            version: String::new(),
            role: Role::Unknown,
            favorite: false,
        }
    }
}

impl Entry {
    /// 一覧に出す名前。名前が空なら ID で代用する
    pub fn label(&self) -> &str {
        if self.name.is_empty() {
            &self.id
        } else {
            &self.name
        }
    }

    /// 同じプラグインを指しているか (パスと ID の組で決まる)
    pub fn is(&self, path: &Path, id: &str) -> bool {
        self.path == path && self.id == id
    }
}

/// 走査した結果ぜんぶ
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Library {
    /// 走査するフォルダ
    pub folders: Vec<PathBuf>,
    /// 見つかったプラグイン
    pub plugins: Vec<Entry>,
    /// 走査中に落ちたファイル。**次からは飛ばす** (画面から外せる)
    pub blocked: Vec<PathBuf>,
}

/// ファイルに書く形。`Library` に版を添えたもの
#[derive(Serialize, Deserialize)]
struct Stored {
    version: u32,
    #[serde(default)]
    folders: Vec<PathBuf>,
    #[serde(default)]
    plugins: Vec<Entry>,
    #[serde(default)]
    blocked: Vec<PathBuf>,
}

/// 版だけ先に見るための形 (project.rs と同じ手順)
#[derive(Deserialize)]
struct VersionProbe {
    version: u32,
}

impl Library {
    /// そのプラグインが記録済みか
    pub fn contains(&self, path: &Path, id: &str) -> bool {
        self.plugins.iter().any(|entry| entry.is(path, id))
    }

    /// その種別のものだけを並べる
    pub fn by_role(&self, role: Role) -> impl Iterator<Item = &Entry> {
        self.plugins.iter().filter(move |entry| entry.role == role)
    }

    /// 星を付けたものを並べる
    pub fn favorites(&self) -> impl Iterator<Item = &Entry> {
        self.plugins.iter().filter(|entry| entry.favorite)
    }

    /// 星を切り替える。**見つからなければ何もしない**
    pub fn toggle_favorite(&mut self, path: &Path, id: &str) {
        if let Some(entry) = self.plugins.iter_mut().find(|entry| entry.is(path, id)) {
            entry.favorite = !entry.favorite;
        }
    }

    /// そのフォルダから見つかった記録を捨てる (再走査の前に呼ぶ)。
    ///
    /// **星は引き継ぐ。** 捨てた中で星が付いていたものを返すので、
    /// 走査し直したあとに付け直せる。
    pub fn forget_under(&mut self, folder: &Path) -> Vec<(PathBuf, String)> {
        let mut kept_favorites = Vec::new();
        self.plugins.retain(|entry| {
            if !entry.path.starts_with(folder) {
                return true;
            }
            if entry.favorite {
                kept_favorites.push((entry.path.clone(), entry.id.clone()));
            }
            false
        });
        kept_favorites
    }

    /// 走査で見つけた1件を入れる。**同じものが居れば置き換える**
    /// (名前や種別が変わっていることがあるため)。星は引き継ぐ。
    pub fn insert(&mut self, entry: Entry) {
        match self
            .plugins
            .iter_mut()
            .find(|found| found.is(&entry.path, &entry.id))
        {
            Some(found) => {
                let favorite = found.favorite;
                *found = entry;
                found.favorite = favorite;
            }
            None => self.plugins.push(entry),
        }
    }

    /// 名前で並べ替える (走査した順のままだと探しにくい)
    pub fn sort(&mut self) {
        self.plugins
            .sort_by(|a, b| a.label().to_lowercase().cmp(&b.label().to_lowercase()));
    }
}

/// `.ron` のテキストにする
pub fn to_string(library: &Library) -> Result<String, String> {
    let stored = Stored {
        version: VERSION,
        folders: library.folders.clone(),
        plugins: library.plugins.clone(),
        blocked: library.blocked.clone(),
    };

    // 1件を1行に収める (数百件になると読めない)
    let config = ron::ser::PrettyConfig::new()
        .indentor("    ".to_string())
        .struct_names(false)
        .depth_limit(2);
    ron::ser::to_string_pretty(&stored, config).map_err(|e| format!("組み立てられません: {e}"))
}

/// `.ron` のテキストを読む
pub fn from_str(text: &str) -> Result<Library, String> {
    let probe: VersionProbe =
        ron::from_str(text).map_err(|e| format!("プラグイン一覧として読めません:\n{e}"))?;
    if probe.version > VERSION {
        return Err(format!(
            "この一覧はバージョン {} で保存されています。\n\
             このビルドが読めるのはバージョン {VERSION} までです。",
            probe.version
        ));
    }

    let stored: Stored =
        ron::from_str(text).map_err(|e| format!("プラグイン一覧として読めません:\n{e}"))?;

    Ok(Library {
        folders: stored.folders,
        // パスも ID も無いものは指し示す先が無い。読み飛ばす
        plugins: stored
            .plugins
            .into_iter()
            .filter(|entry| !entry.path.as_os_str().is_empty() && !entry.id.is_empty())
            .collect(),
        blocked: stored.blocked,
    })
}

/// 設定を置くディレクトリ (実行ファイルの隣の `config`)。
///
/// 実行ファイルの場所が取れない環境では、作業ディレクトリの下へ落とす。
pub fn config_dir() -> PathBuf {
    let beside_exe = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(PathBuf::from));
    match beside_exe {
        Some(dir) => dir.join(CONFIG_DIR),
        None => PathBuf::from(CONFIG_DIR),
    }
}

/// 一覧の記録の場所
pub fn config_path() -> PathBuf {
    config_dir().join(LIBRARY_FILE)
}

/// 走査中の目印の場所
fn scanning_path() -> PathBuf {
    config_dir().join(SCANNING_FILE)
}

/// 一覧を読む。
///
/// 戻り値の2つ目は**画面に出したい知らせ**。ファイルが無いのは normal なので
/// `None`、読めたが壊れていたときだけ中身が入る (空の一覧で始める)。
pub fn load() -> (Library, Option<String>) {
    let path = config_path();
    let Ok(text) = std::fs::read_to_string(&path) else {
        // まだ無い。空から始める
        return (Library::default(), None);
    };
    match from_str(&text) {
        Ok(library) => (library, None),
        Err(e) => (
            Library::default(),
            Some(format!("{}\n\n{e}", path.display())),
        ),
    }
}

/// 一覧を書く。ディレクトリが無ければ作る
pub fn save(library: &Library) -> Result<(), String> {
    let text = to_string(library)?;
    let dir = config_dir();
    std::fs::create_dir_all(&dir).map_err(|e| format!("{} を作れません: {e}", dir.display()))?;
    let path = config_path();
    std::fs::write(&path, text).map_err(|e| format!("{} に書けません: {e}", path.display()))
}

/// **これから開くファイル**を目印に書く。開き終えたら [`clear_scanning`] を呼ぶ。
///
/// 書けなくても走査は続ける (落ちたときに分からなくなるだけで、実害は無い)。
pub fn mark_scanning(path: &Path) {
    let dir = config_dir();
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    let _ = std::fs::write(scanning_path(), path.to_string_lossy().as_bytes());
}

/// 目印を消す (無事に開き終えた)
pub fn clear_scanning() {
    let _ = std::fs::remove_file(scanning_path());
}

/// 前回落ちたファイルを取り出す (**目印は消す**)。
///
/// 起動時に1回だけ呼ぶ。返ってきたら `blocked` へ入れて、画面で知らせること。
pub fn take_crashed() -> Option<PathBuf> {
    let path = scanning_path();
    let text = std::fs::read_to_string(&path).ok()?;
    let _ = std::fs::remove_file(&path);
    let trimmed = text.trim();
    (!trimmed.is_empty()).then(|| PathBuf::from(trimmed))
}

/// 規格が決めているプラグインの置き場。
///
/// **実在するかは見ない。** 呼び出し側が絞る ([`existing_standard_folders`])。
/// Linux のぶんも並べてあるのは、動かす当てが出たときに探し回らずに済むから
/// (ただのデータなので、持っていても害は無い)。
pub fn standard_folders() -> Vec<PathBuf> {
    let mut folders = Vec::new();
    let from_env = |name: &str, tail: &str| {
        std::env::var(name)
            .ok()
            .filter(|value| !value.is_empty())
            .map(|value| PathBuf::from(value).join(tail))
    };

    if cfg!(target_os = "windows") {
        // 「Common Files」の場所は環境変数で決まる (日本語 Windows でも同じ)
        for format in ["CLAP", "VST3"] {
            folders.extend(from_env("COMMONPROGRAMFILES", format));
            folders.extend(from_env(
                "LOCALAPPDATA",
                &format!("Programs\\Common\\{format}"),
            ));
        }
    } else {
        for (system, home) in [("clap", ".clap"), ("vst3", ".vst3")] {
            folders.push(PathBuf::from("/usr/lib").join(system));
            folders.push(PathBuf::from("/usr/local/lib").join(system));
            folders.extend(from_env("HOME", home));
        }
    }
    folders
}

/// 標準の置き場のうち、実在するものだけ (初回起動時に入れる)
pub fn existing_standard_folders() -> Vec<PathBuf> {
    standard_folders()
        .into_iter()
        .filter(|folder| folder.is_dir())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(name: &str, id: &str, role: Role) -> Entry {
        Entry {
            kind: PluginKind::Clap,
            path: PathBuf::from(format!("C:\\plugins\\{name}.clap")),
            id: id.to_string(),
            name: name.to_string(),
            vendor: "Example".to_string(),
            version: "1.0".to_string(),
            role,
            favorite: false,
        }
    }

    fn sample() -> Library {
        Library {
            folders: vec![PathBuf::from("C:\\plugins")],
            plugins: vec![
                entry("Synth", "com.example.synth", Role::Instrument),
                entry("Reverb", "com.example.reverb", Role::Effect),
                entry("Mystery", "com.example.mystery", Role::Unknown),
            ],
            blocked: vec![PathBuf::from("C:\\plugins\\Bad.clap")],
        }
    }

    /// **書いて読んで元に戻ること。** ここが崩れると走査し直しになる
    #[test]
    fn it_survives_a_round_trip() {
        let before = sample();
        let text = to_string(&before).expect("書けること");
        let after = from_str(&text).expect("読めること");
        assert_eq!(before, after);
    }

    /// 新しい版のファイルは断ること (黙って一部だけ読まない)
    #[test]
    fn a_newer_version_is_refused() {
        let text = to_string(&sample()).unwrap();
        let bumped = text.replacen("version: 1", "version: 99", 1);
        let error = from_str(&bumped).expect_err("断ること");
        assert!(error.contains("99"), "版が伝わること: {error}");
    }

    /// **項目が欠けていても読めること。**
    /// あとから項目を足したときに、古いファイルが読めなくなると困る
    #[test]
    fn an_older_file_without_new_fields_still_loads() {
        let text = r#"(
            version: 1,
            plugins: [
                (kind: Clap, path: "C:\\a.clap", id: "com.a"),
            ],
        )"#;
        let library = from_str(text).expect("読めること");
        assert_eq!(library.plugins.len(), 1);
        assert_eq!(library.plugins[0].role, Role::Unknown, "既定は未分類");
        assert!(!library.plugins[0].favorite);
        assert!(library.folders.is_empty());
        assert!(library.blocked.is_empty());
    }

    /// パスか ID が空の記録は読み飛ばすこと (指し示す先が無い)
    #[test]
    fn entries_without_a_target_are_dropped() {
        let text = r#"(
            version: 1,
            plugins: [
                (kind: Clap, path: "", id: "com.a"),
                (kind: Clap, path: "C:\\b.clap", id: ""),
                (kind: Clap, path: "C:\\c.clap", id: "com.c"),
            ],
        )"#;
        let library = from_str(text).expect("読めること");
        assert_eq!(library.plugins.len(), 1);
        assert_eq!(library.plugins[0].id, "com.c");
    }

    /// 種別ごとに引けること。**未分類は音源にもエフェクトにも混ざらない**
    #[test]
    fn each_role_stands_on_its_own() {
        let library = sample();
        assert_eq!(library.by_role(Role::Instrument).count(), 1);
        assert_eq!(library.by_role(Role::Effect).count(), 1);
        assert_eq!(library.by_role(Role::Unknown).count(), 1);
    }

    /// 星の付け外しと、星だけを引くこと
    #[test]
    fn favorites_can_be_toggled_and_listed() {
        let mut library = sample();
        assert_eq!(library.favorites().count(), 0);

        let path = PathBuf::from("C:\\plugins\\Synth.clap");
        library.toggle_favorite(&path, "com.example.synth");
        assert_eq!(library.favorites().count(), 1);
        assert_eq!(library.favorites().next().unwrap().name, "Synth");

        library.toggle_favorite(&path, "com.example.synth");
        assert_eq!(library.favorites().count(), 0);
    }

    /// 同じ ID でもパスが違えば別物として扱うこと。
    /// (同じプラグインを2箇所に置いている環境がある)
    #[test]
    fn the_path_is_part_of_the_identity() {
        let mut library = Library::default();
        let mut here = entry("Synth", "com.example.synth", Role::Instrument);
        let mut there = here.clone();
        there.path = PathBuf::from("D:\\other\\Synth.clap");
        here.favorite = true;

        library.insert(here);
        library.insert(there);
        assert_eq!(library.plugins.len(), 2);
        assert_eq!(library.favorites().count(), 1, "星は片方だけ");
    }

    /// **走査し直しても星が消えないこと。**
    /// 名前や種別は新しいもので上書きする
    #[test]
    fn rescanning_keeps_the_star_but_refreshes_the_rest() {
        // **同じファイル・同じ ID** で名前と種別だけが変わった状況を作る
        // (走査し直しで名前が変わっていることがある)
        let mut library = Library::default();
        let mut old = entry("Synth", "com.example.synth", Role::Unknown);
        old.name = "古い名前".to_string();
        old.favorite = true;
        library.insert(old);

        let mut fresh = entry("Synth", "com.example.synth", Role::Instrument);
        fresh.name = "新しい名前".to_string();
        library.insert(fresh);

        assert_eq!(library.plugins.len(), 1, "増えないこと");
        assert_eq!(library.plugins[0].name, "新しい名前");
        assert_eq!(library.plugins[0].role, Role::Instrument);
        assert!(library.plugins[0].favorite, "星は残ること");
    }

    /// フォルダごと忘れられること。星の付いていたものは呼び出し側へ返す
    #[test]
    fn a_folder_can_be_forgotten() {
        let mut library = sample();
        library.toggle_favorite(
            &PathBuf::from("C:\\plugins\\Synth.clap"),
            "com.example.synth",
        );

        let kept = library.forget_under(Path::new("C:\\plugins"));
        assert!(library.plugins.is_empty(), "全部消えること");
        assert_eq!(kept.len(), 1, "星の付いていたものが返ること");
        assert_eq!(kept[0].1, "com.example.synth");
    }

    /// 別のフォルダの記録は巻き込まないこと
    #[test]
    fn forgetting_one_folder_leaves_the_others() {
        let mut library = Library::default();
        library.insert(entry("A", "com.a", Role::Instrument));
        let mut other = entry("B", "com.b", Role::Instrument);
        other.path = PathBuf::from("D:\\other\\B.clap");
        library.insert(other);

        library.forget_under(Path::new("C:\\plugins"));
        assert_eq!(library.plugins.len(), 1);
        assert_eq!(library.plugins[0].id, "com.b");
    }

    /// 名前で並ぶこと (大文字小文字を区別しない)
    #[test]
    fn sorting_ignores_case() {
        let mut library = Library::default();
        for name in ["zeta", "Alpha", "beta"] {
            library.insert(entry(name, &format!("com.{name}"), Role::Instrument));
        }
        library.sort();
        let names: Vec<&str> = library.plugins.iter().map(|e| e.label()).collect();
        assert_eq!(names, vec!["Alpha", "beta", "zeta"]);
    }

    /// 名前が空なら ID で代用すること (一覧が空欄にならない)
    #[test]
    fn a_nameless_plugin_shows_its_id() {
        let mut nameless = entry("", "com.example.nameless", Role::Instrument);
        nameless.name = String::new();
        assert_eq!(nameless.label(), "com.example.nameless");
    }

    /// 設定の置き場が実行ファイルの隣になること
    #[test]
    fn the_config_sits_next_to_the_executable() {
        let dir = config_dir();
        assert!(dir.ends_with(CONFIG_DIR), "{}", dir.display());

        let exe = std::env::current_exe().expect("テストの実行ファイルは取れる");
        assert_eq!(
            dir.parent(),
            exe.parent(),
            "実行ファイルと同じ場所に置くこと"
        );
        assert!(config_path().ends_with(LIBRARY_FILE));
    }

    /// 標準の置き場が、形式ごとに用意されていること。
    /// **実在は見ない** (走っている環境に無くても一覧は出る)
    #[test]
    fn the_standard_folders_cover_both_formats() {
        let folders = standard_folders();
        let has = |needle: &str| {
            folders
                .iter()
                .any(|f| f.to_string_lossy().to_lowercase().contains(needle))
        };
        assert!(has("clap"), "CLAP の置き場: {folders:?}");
        assert!(has("vst3"), "VST3 の置き場: {folders:?}");
        assert!(
            folders.iter().all(|f| f.is_absolute()),
            "絶対パスであること: {folders:?}"
        );
    }
}
