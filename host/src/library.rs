//! 走査したプラグインの一覧 (`config\plugins.ron` の読み書き)。
//!
//! **置き場は実行ファイルの隣。** 設定が実行ファイルと一緒に動くので、
//! 持ち運びが素直になる。引き換えに `cargo run` で動かすと
//! `target\debug\config\` に落ちるため、**`cargo clean` すると消える**
//! (走査し直しには時間がかかるので、消したくないときは退避すること)。
//!
//! 一覧そのもの ([`Library`]) と、走査の進行 ([`Scan`]) を持つ。
//! ファイルを実際に開くのは [`crate::discovery`] の側。

use crate::project::PluginKind;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// この形式のバージョン。**上げるのは、古いビルドで読めなくなるときだけ**
/// (項目を足すだけなら `#[serde(default)]` で読めるので上げない)。
const VERSION: u32 = 1;

/// 走査器のバージョン。**読み取りの中身を変えたら上げる。**
///
/// 差分走査はファイルの印しか見ないので、**こちらの読み取りを直しても
/// 記録は古いまま居座る**。ここが記録と食い違っていれば印を信用せず、
/// 次の走査で全部開き直す (Ardour の `ARDOUR_VST3_CACHE_FILE_VERSION` と
/// 同じ役目)。
///
/// | 版 | 何が変わったか |
/// |---|---|
/// | 1 | 最初 (印を持たない記録もここに入る) |
/// | 2 | VST3 は `moduleinfo.json` があればそれを読む (種別がクラス単位になった) |
/// | 3 | JSON が無い VST3 もファクトリから読む。**「未分類」が減る** |
pub const SCANNER_VERSION: u32 = 3;

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

/// ファイルの印。**一致すれば開き直さない** ([`Scan::start`] の差分走査)。
///
/// 更新時刻とバイト数の組。中身のハッシュではないので、**同じ大きさのまま
/// 時刻を保って書き換えられると気付けない**。プラグインの入れ替えは
/// インストーラがやることなので、実用上はこれで足りる。
///
/// **取れなかったら `None`。** 迷ったら開いて確かめる側へ倒す
/// (見落として古い記録を出すより、無駄に開くほうがましなので)。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Stamp {
    /// 最終更新時刻 (UNIX 元期からの秒)
    pub modified: u64,
    /// バイト数
    pub size: u64,
}

impl Stamp {
    /// ファイルから取る。**中身は読まない** (メタデータだけ)。
    ///
    /// バンドルの中の実体を指すこと。ディレクトリを渡しても、
    /// 中のファイルを書き換えただけでは更新時刻が動かない
    /// (解決は [`crate::discovery::stamp`] がやる)
    pub fn of(path: &Path) -> Option<Self> {
        let meta = std::fs::metadata(path).ok()?;
        let modified = meta
            .modified()
            .ok()?
            .duration_since(std::time::UNIX_EPOCH)
            .ok()?
            .as_secs();
        Some(Self {
            modified,
            size: meta.len(),
        })
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
    /// 走査した時点のファイルの印。**次の走査で開き直すかを決める**。
    /// 印の無い記録 (古いファイルから読んだもの) は開き直す
    pub stamp: Option<Stamp>,
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
            stamp: None,
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
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Library {
    /// 走査するフォルダ
    pub folders: Vec<PathBuf>,
    /// 見つかったプラグイン
    pub plugins: Vec<Entry>,
    /// 走査中に落ちたファイル。**次からは飛ばす** (画面から外せる)
    pub blocked: Vec<PathBuf>,
    /// この記録を作った走査器の版 ([`SCANNER_VERSION`])。
    /// **食い違っていれば印を信用しない**
    pub scanner: u32,
    /// **プラグインを別プロセスで開くか。** 既定は開く
    /// ([`crate::subscan`])。切ると速いが、落ちたら道連れになる
    pub isolate: bool,
}

impl Default for Library {
    fn default() -> Self {
        Self {
            folders: Vec::new(),
            plugins: Vec::new(),
            blocked: Vec::new(),
            // **記録が無い状態は「版が古い」ではない。** 開くものが無いので
            // どちらでも同じだが、走査前に書き出したときに嘘が残らないほう
            scanner: SCANNER_VERSION,
            isolate: true,
        }
    }
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
    /// **既定は 1。** この項目を持たない記録は、印を入れる前のもの
    #[serde(default = "first_scanner_version")]
    scanner: u32,
    #[serde(default = "isolate_by_default")]
    isolate: bool,
}

fn first_scanner_version() -> u32 {
    1
}

fn isolate_by_default() -> bool {
    true
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

    /// そのフォルダから見つかった記録を**一覧から外して返す** (再走査の前に呼ぶ)。
    ///
    /// 捨てずに返すのは、走査のあとで2つのことに使うため。
    ///
    /// - **星を引き継ぐ** (開き直したものに付け直す)
    /// - **印が変わっていなければそのまま戻す** (開き直さない)
    ///
    /// 戻り値を捨てれば、そのフォルダの記録を消したことになる。
    pub fn take_under(&mut self, folder: &Path) -> Vec<Entry> {
        let mut taken = Vec::new();
        self.plugins.retain(|entry| {
            if !entry.path.starts_with(folder) {
                return true;
            }
            taken.push(entry.clone());
            false
        });
        taken
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
        scanner: library.scanner,
        isolate: library.isolate,
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
        scanner: stored.scanner,
        isolate: stored.isolate,
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

/// 走査の進行。
///
/// **1ファイルずつ進める。** 別スレッドにしないのは、CLAP も VST3 も
/// モジュールの読み込みがメインスレッドの規約に縛られているため。
/// 画面から毎フレーム [`step`](Self::step) を呼べば、進捗を出しながら
/// 反応を保てるし、途中でやめられる。
pub struct Scan {
    /// まだ開いていない候補 (**後ろから取る**ので逆順に積んである)
    queue: Vec<PathBuf>,
    /// 開くと決めた候補の数 (**使い回すぶんは入らない**)
    total: usize,
    /// 開かずに前の記録を戻したファイルの数
    reused: usize,
    /// 開けなかったものの説明
    problems: Vec<String>,
    /// 落ちたので次から飛ばすことにしたファイルの数
    crashed: usize,
    /// 走査前に星が付いていたもの。**走査し直しても消さない**
    favorites: Vec<(PathBuf, String)>,
    /// 別プロセスで開くか (走査の途中で切り替わらないよう控えておく)
    isolate: bool,
}

/// 前の記録をそのまま使えるか決める。**開き直すなら `None`。**
///
/// 使い回せるのは、そのファイルの記録が残っていて、**印が全て一致する**とき。
/// 1ファイルに複数のプラグインが入りうるので、**1件でも印が違えば全部開き直す**
/// (同じファイルから来た記録の印は揃っているはずで、揃っていないほうが異常)。
///
/// `stamp` が `None` (メタデータが取れない) なら開いて確かめる。
fn reusable(previous: &[Entry], path: &Path, stamp: Option<Stamp>) -> Option<Vec<Entry>> {
    let stamp = stamp?;
    let found: Vec<Entry> = previous
        .iter()
        .filter(|entry| entry.path == path)
        .cloned()
        .collect();
    if found.is_empty() {
        return None;
    }
    found
        .iter()
        .all(|entry| entry.stamp == Some(stamp))
        .then_some(found)
}

impl Scan {
    /// 登録されたフォルダから候補を集める。**まだ1つも開かない。**
    ///
    /// **変わっていないファイルは開き直さない** ([`reusable`])。プラグインを
    /// 開くのは1本で数秒かかることがあるので、2回目以降はここでほとんどが
    /// 落ちる。何件を使い回したかは [`reused`](Self::reused) で分かる。
    ///
    /// **走査器の版が変わっていれば印は信用しない** ([`SCANNER_VERSION`])。
    /// ファイルは同じでも、こちらが読み取る中身が変わっているため。
    pub fn start(library: &mut Library) -> Self {
        let current = library.scanner == SCANNER_VERSION;
        Self::begin(library, current)
    }

    /// 印を見ずに**全部開き直す**。
    ///
    /// 分類がおかしいときに使う。版を上げ忘れたときの逃げ道でもある。
    pub fn start_full(library: &mut Library) -> Self {
        Self::begin(library, false)
    }

    fn begin(library: &mut Library, reuse: bool) -> Self {
        let isolate = library.isolate;
        // **走査し終えたときではなく、始めるときに書く。** 途中でやめても
        // 「この版で開いたものが入っている」ことに変わりはない
        library.scanner = SCANNER_VERSION;

        let mut previous = Vec::new();
        let mut queue = Vec::new();

        for folder in library.folders.clone() {
            // 記録はいったん全部外す。**在るものだけを入れ直す**ので、
            // 消えたプラグインが居残らない
            previous.extend(library.take_under(&folder));
            for path in crate::discovery::plugin_files(&folder) {
                // 前に落ちたファイルは開きに行かない
                if library.blocked.contains(&path) {
                    continue;
                }
                queue.push(path);
            }
        }

        queue.sort();
        queue.dedup();

        let favorites = previous
            .iter()
            .filter(|entry| entry.favorite)
            .map(|entry| (entry.path.clone(), entry.id.clone()))
            .collect();

        // **開かずに済むものはここで戻す。** 残りが `queue` になる
        let mut reused = 0;
        let mut to_open = Vec::with_capacity(queue.len());
        for path in queue {
            let kept = reuse
                .then(|| reusable(&previous, &path, crate::discovery::stamp(&path)))
                .flatten();
            match kept {
                Some(entries) => {
                    reused += 1;
                    for entry in entries {
                        library.insert(entry);
                    }
                }
                None => to_open.push(path),
            }
        }

        let total = to_open.len();
        // pop() で前から取れるように積み直す
        to_open.reverse();

        Self {
            queue: to_open,
            total,
            reused,
            problems: Vec::new(),
            crashed: 0,
            favorites,
            isolate,
        }
    }

    /// 1ファイルだけ開いて一覧へ入れる。終わっていれば `None`。
    ///
    /// **別プロセスで開くのが既定** ([`crate::subscan`])。落ちたファイルは
    /// その場で `blocked` へ入るので、次の走査では開きに行かない。
    ///
    /// 同じプロセスで開く設定のときは、**開く直前に目印を書いて開き終えたら
    /// 消す**。途中で落ちても次の起動でどのファイルだったか分かる
    /// ([`take_crashed`])。別プロセスなら落ちても親が生きているので要らない。
    pub fn step(&mut self, library: &mut Library) -> Option<PathBuf> {
        let path = self.queue.pop()?;
        let Some(kind) = crate::discovery::kind_of(&path) else {
            return Some(path); // 候補の集め方が変わらない限り起きない
        };

        // **開く前に印を取る。** 途中で書き換えられても、記録した印は
        // 実際に読んだものと揃う (次の走査でずれに気付ける)
        let stamp = crate::discovery::stamp(&path);

        let found = if self.isolate {
            self.open_isolated(library, &path)
        } else {
            mark_scanning(&path);
            let opened =
                crate::discovery::scan_file(&path).map_err(|e| format!("{}: {e}", path.display()));
            clear_scanning();
            opened.map_err(|problem| {
                self.problems.push(problem);
            })
        };

        if let Ok(found) = found {
            for plugin in found {
                let favorite = self
                    .favorites
                    .iter()
                    .any(|(kept, id)| *kept == path && *id == plugin.id);
                library.insert(Entry {
                    kind,
                    path: path.clone(),
                    id: plugin.id,
                    name: plugin.name,
                    vendor: plugin.vendor,
                    version: plugin.version,
                    role: plugin.role,
                    favorite,
                    stamp,
                });
            }
        }

        Some(path)
    }

    /// 別プロセスで1ファイル開く。**落ちたらここで飛ばすことにする。**
    ///
    /// 子を起てられなかったときだけ、この場で自分で開く。プラグインのせいでは
    /// ないので、飛ばしてしまうと直しても戻ってこない。
    fn open_isolated(
        &mut self,
        library: &mut Library,
        path: &Path,
    ) -> Result<Vec<crate::discovery::FoundPlugin>, ()> {
        match crate::subscan::scan_file(path) {
            Ok(found) => Ok(found),
            Err(crate::subscan::Failure::CannotSpawn(detail)) => {
                // 一度だけ知らせる。**毎回出すと問題の一覧が埋まる**
                if self.crashed == 0 && self.problems.is_empty() {
                    self.problems.push(format!(
                        "別プロセスで開けないため同じプロセスで開きます: {detail}"
                    ));
                }
                mark_scanning(path);
                let opened = crate::discovery::scan_file(path);
                clear_scanning();
                opened.map_err(|e| {
                    self.problems.push(format!("{}: {e}", path.display()));
                })
            }
            Err(failure) => {
                self.problems.push(format!("{}: {failure}", path.display()));
                if failure.should_block() && !library.blocked.contains(&path.to_path_buf()) {
                    library.blocked.push(path.to_path_buf());
                    self.crashed += 1;
                }
                Err(())
            }
        }
    }

    pub fn is_done(&self) -> bool {
        self.queue.is_empty()
    }

    /// (開いた数, 開くと決めた数)。**使い回したぶんは入らない**
    pub fn progress(&self) -> (usize, usize) {
        (self.total - self.queue.len(), self.total)
    }

    /// 開かずに前の記録を戻したファイルの数
    pub fn reused(&self) -> usize {
        self.reused
    }

    /// 落ちたので次から飛ばすことにしたファイルの数
    pub fn crashed(&self) -> usize {
        self.crashed
    }

    /// 開けなかったものの説明。**空でも成功とは限らない**
    /// (中にプラグインが1つも無いファイルもここに来る)
    pub fn problems(&self) -> &[String] {
        &self.problems
    }
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
            stamp: None,
        }
    }

    /// 印を1つ作る (中身に意味は無い。**一致するかだけを見る**)
    fn stamp(modified: u64) -> Stamp {
        Stamp {
            modified,
            size: 1024,
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
            ..Default::default()
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

    /// フォルダごと取り出せること。**記録はそのまま返る** (星も付いたまま)
    #[test]
    fn a_folder_can_be_taken_out() {
        let mut library = sample();
        library.toggle_favorite(
            &PathBuf::from("C:\\plugins\\Synth.clap"),
            "com.example.synth",
        );

        let taken = library.take_under(Path::new("C:\\plugins"));
        assert!(library.plugins.is_empty(), "全部外れること");
        assert_eq!(taken.len(), 3, "外したものが全部返ること");
        assert_eq!(
            taken.iter().filter(|entry| entry.favorite).count(),
            1,
            "星も付いたまま返ること"
        );
    }

    /// 別のフォルダの記録は巻き込まないこと
    #[test]
    fn taking_one_folder_leaves_the_others() {
        let mut library = Library::default();
        library.insert(entry("A", "com.a", Role::Instrument));
        let mut other = entry("B", "com.b", Role::Instrument);
        other.path = PathBuf::from("D:\\other\\B.clap");
        library.insert(other);

        library.take_under(Path::new("C:\\plugins"));
        assert_eq!(library.plugins.len(), 1);
        assert_eq!(library.plugins[0].id, "com.b");
    }

    // ---- 差分走査 ----

    /// **印が同じなら開き直さない。** 記録がそのまま返ること
    #[test]
    fn an_unchanged_file_is_reused() {
        let mut kept = entry("Synth", "com.example.synth", Role::Instrument);
        kept.stamp = Some(stamp(100));
        let path = kept.path.clone();

        let reused = reusable(&[kept.clone()], &path, Some(stamp(100)));
        assert_eq!(reused, Some(vec![kept]));
    }

    /// 印が違えば開き直すこと (更新時刻でも大きさでも)
    #[test]
    fn a_changed_file_is_opened_again() {
        let mut kept = entry("Synth", "com.example.synth", Role::Instrument);
        kept.stamp = Some(stamp(100));
        let path = kept.path.clone();

        assert_eq!(reusable(&[kept.clone()], &path, Some(stamp(101))), None);

        let bigger = Stamp {
            modified: 100,
            size: 2048,
        };
        assert_eq!(reusable(&[kept], &path, Some(bigger)), None);
    }

    /// **印の無い記録は開き直すこと。**
    /// 差分走査を入れる前に書かれた一覧が、ずっと使い回されると困る
    #[test]
    fn a_record_without_a_stamp_is_opened_again() {
        let kept = entry("Synth", "com.example.synth", Role::Instrument);
        assert_eq!(kept.stamp, None);
        let path = kept.path.clone();

        assert_eq!(reusable(&[kept], &path, Some(stamp(100))), None);
    }

    /// **印が取れなければ開き直すこと** (消えた・権限が無い)
    #[test]
    fn a_file_we_cannot_stamp_is_opened_again() {
        let mut kept = entry("Synth", "com.example.synth", Role::Instrument);
        kept.stamp = Some(stamp(100));
        let path = kept.path.clone();

        assert_eq!(reusable(&[kept], &path, None), None);
    }

    /// 記録が無いファイルは当然開くこと (新しく置かれたもの)
    #[test]
    fn a_file_we_have_never_seen_is_opened() {
        assert_eq!(
            reusable(&[], Path::new("C:\\plugins\\New.clap"), Some(stamp(100))),
            None
        );
    }

    /// **1ファイルに複数入っていれば、まとめて返すこと。**
    /// 1件だけ戻すと、同じファイルの他のプラグインが一覧から消える
    #[test]
    fn every_plugin_in_the_file_comes_back_together() {
        let path = PathBuf::from("C:\\plugins\\Pack.clap");
        let make = |id: &str| {
            let mut entry = entry("Pack", id, Role::Instrument);
            entry.path = path.clone();
            entry.stamp = Some(stamp(100));
            entry
        };
        let previous = vec![make("com.pack.one"), make("com.pack.two")];

        let reused = reusable(&previous, &path, Some(stamp(100))).expect("使い回せること");
        assert_eq!(reused.len(), 2);
    }

    /// 同じファイルの記録で印が食い違っていたら、**全部開き直すこと**
    #[test]
    fn a_file_with_mismatched_stamps_is_opened_again() {
        let path = PathBuf::from("C:\\plugins\\Pack.clap");
        let make = |id: &str, at: u64| {
            let mut entry = entry("Pack", id, Role::Instrument);
            entry.path = path.clone();
            entry.stamp = Some(stamp(at));
            entry
        };
        let previous = vec![make("com.pack.one", 100), make("com.pack.two", 101)];

        assert_eq!(reusable(&previous, &path, Some(stamp(100))), None);
    }

    // ---- 走査器の版 ----

    /// 版を持たない記録は 1 として読むこと (印を入れる前のファイル)
    #[test]
    fn a_record_without_a_scanner_version_reads_as_one() {
        let text = r#"(
            version: 1,
            plugins: [
                (kind: Clap, path: "C:\\a.clap", id: "com.a"),
            ],
        )"#;
        let library = from_str(text).expect("読めること");
        assert_eq!(library.scanner, 1);
        assert!(library.isolate, "別プロセスが既定であること");
    }

    /// 版と設定が往復すること
    #[test]
    fn the_scanner_version_survives_a_round_trip() {
        let mut before = sample();
        before.scanner = 7;
        before.isolate = false;

        let after = from_str(&to_string(&before).unwrap()).unwrap();
        assert_eq!(after.scanner, 7);
        assert!(!after.isolate);
    }

    /// 走査するフォルダを1つ作り、その中に空のファイルを置く
    fn folder_with_one_file(name: &str) -> (PathBuf, PathBuf) {
        let dir = std::env::temp_dir().join(format!("egui-clap-host-lib-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("A.clap");
        std::fs::write(&file, b"not really a plugin").unwrap();
        (dir, file)
    }

    /// 印が合っていれば、開く候補が残らないこと (**前提の確認**)
    #[test]
    fn a_matching_stamp_leaves_nothing_to_open() {
        let (dir, file) = folder_with_one_file("stamp-current");
        let mut library = Library {
            folders: vec![dir.clone()],
            ..Default::default()
        };
        let mut kept = entry("A", "com.a", Role::Instrument);
        kept.path = file.clone();
        kept.stamp = crate::discovery::stamp(&file);
        library.insert(kept);

        let scan = Scan::start(&mut library);
        assert_eq!(scan.progress().1, 0, "開くものが無いこと");
        assert_eq!(scan.reused(), 1);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **走査器の版が古ければ、印が合っていても開き直すこと。**
    ///
    /// ファイルは同じでも、こちらが読み取る中身が変わっている。
    /// ここが効かないと、読み取りを直しても古い記録が居座る
    #[test]
    fn an_old_scanner_version_ignores_the_stamps() {
        let (dir, file) = folder_with_one_file("stamp-old-scanner");
        let mut library = Library {
            folders: vec![dir.clone()],
            ..Default::default()
        };
        let mut kept = entry("A", "com.a", Role::Instrument);
        kept.path = file.clone();
        kept.stamp = crate::discovery::stamp(&file);
        library.insert(kept);
        library.scanner = SCANNER_VERSION - 1;

        let scan = Scan::start(&mut library);
        assert_eq!(scan.progress().1, 1, "開き直すこと");
        assert_eq!(scan.reused(), 0);
        // **版はここで揃う。** 走査を始めた時点で新しい読み取りに入っている
        assert_eq!(library.scanner, SCANNER_VERSION);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 「すべて開き直す」は版が合っていても開き直すこと
    #[test]
    fn a_full_scan_ignores_the_stamps_too() {
        let (dir, file) = folder_with_one_file("stamp-full");
        let mut library = Library {
            folders: vec![dir.clone()],
            ..Default::default()
        };
        let mut kept = entry("A", "com.a", Role::Instrument);
        kept.path = file.clone();
        kept.stamp = crate::discovery::stamp(&file);
        library.insert(kept);

        let scan = Scan::start_full(&mut library);
        assert_eq!(scan.progress().1, 1);
        assert_eq!(scan.reused(), 0);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 別のファイルの記録を巻き込まないこと
    #[test]
    fn reuse_only_looks_at_the_same_path() {
        let mut other = entry("Other", "com.other", Role::Instrument);
        other.path = PathBuf::from("C:\\plugins\\Other.clap");
        other.stamp = Some(stamp(100));

        assert_eq!(
            reusable(
                &[other],
                Path::new("C:\\plugins\\Synth.clap"),
                Some(stamp(100))
            ),
            None
        );
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
