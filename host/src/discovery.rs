//! 音源ファイルのロードとプラグイン列挙 (.clap / .vst3)。
//!
//! どちらの形式も「1つのファイルに複数のプラグインが入りうる」ので、
//! 載せる前に中身を数え上げる必要がある。列挙の結果は [`FoundPlugin`] に
//! 揃えてあり、呼び出し側 (選択 UI) は形式を意識しない。
//!
//! **種別 (音源かエフェクトか) は宣言を読むだけで分かる。** 入力ポートを数える
//! ために起動する必要は無い。
//!
//! 走査の重さは形式で違う。
//!
//! | 形式 | やること |
//! |---|---|
//! | CLAP | モジュールを読んで `clap_entry.init` まで |
//! | VST3 (`moduleinfo.json` あり) | **JSON を1つ読むだけ** ([`load_vst3_file`]) |
//! | VST3 (JSON なし) | モジュールを読んでファクトリに聞く |
//!
//! **どの道もプラグインを作らない。** 音源は生成時にウェーブテーブルや
//! プリセットを読み、1本で数秒かかる。`vst3-host` の
//! `get_detailed_plugin_info` はバス構成のために生成するので使わない
//! (走査でバス構成は要らない)。

#![allow(unsafe_code)]

use crate::library::{Role, Stamp};
use crate::project::PluginKind;
use clack_host::plugin::PluginDescriptor;
use clack_host::prelude::*;
use serde::{Deserialize, Serialize};
use std::error::Error;
use std::path::{Path, PathBuf};

/// フォルダを掘る深さの上限。
/// ベンダーが1〜2階層掘る程度なので十分で、リンクの輪に落ちる保険も兼ねる
const MAX_DEPTH: usize = 8;

/// ファイル内で見つかったプラグインの情報。
///
/// **別プロセスから受け取ることがある**ので `.ron` にできる
/// ([`crate::subscan`])。
#[derive(Debug, Serialize, Deserialize)]
pub struct FoundPlugin {
    /// CLAP のプラグイン ID、または VST3 のクラス UID (32桁の16進)
    pub id: String,
    pub name: String,
    /// 音源かエフェクトか (宣言から決まる。無ければ [`Role::Unknown`])
    pub role: Role,
    pub vendor: String,
    pub version: String,
}

/// 拡張子から形式を見分ける。どちらでもなければ `None`
pub fn kind_of(path: &Path) -> Option<PluginKind> {
    let extension = path.extension()?.to_str()?;
    if extension.eq_ignore_ascii_case("clap") {
        Some(PluginKind::Clap)
    } else if extension.eq_ignore_ascii_case("vst3") {
        Some(PluginKind::Vst3)
    } else {
        None
    }
}

/// フォルダの中のプラグイン候補を集める。**ファイルは開かない。**
///
/// - 下の階層まで掘る (ベンダーが `Common Files\VST3\<Vendor>\Foo.vst3` と掘るため)
/// - **`.clap` / `.vst3` のディレクトリには入らない。** バンドルそのものが
///   1つのプラグインなので、中の `Contents\x86_64-win\Foo.vst3` を
///   別に数えてはいけない
pub fn plugin_files(folder: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    collect_files(folder, 0, &mut found);
    found.sort();
    found.dedup();
    found
}

fn collect_files(dir: &Path, depth: usize, out: &mut Vec<PathBuf>) {
    if depth > MAX_DEPTH {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        // 読めないフォルダは黙って飛ばす (権限が無いことがある)
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        // **拡張子が付いていればそれで1つ。** ファイルでもディレクトリでも同じ
        // (Windows の .vst3 はディレクトリ、.clap はファイルだが、規格上は
        //  どちらもバンドルになりうる)
        if kind_of(&path).is_some() {
            out.push(path);
            continue;
        }
        if entry.file_type().map(|kind| kind.is_dir()).unwrap_or(false) {
            collect_files(&path, depth + 1, out);
        }
    }
}

/// 差分走査のための印を取る。**中身は読まない** (メタデータだけ)。
///
/// **`.vst3` は Windows ではバンドルディレクトリのことがある。** ディレクトリの
/// 更新時刻は中のファイルを書き換えても動かないので、**中の実体を見る**。
/// 実体が見つからなければ諦めて `None` を返し、開いて確かめる側へ倒す。
pub fn stamp(path: &Path) -> Option<Stamp> {
    let target = match kind_of(path) {
        Some(PluginKind::Vst3) if path.is_dir() => {
            vst3_host::discovery::get_vst3_binary_path(path).ok()?
        }
        _ => path.to_path_buf(),
    };
    Stamp::of(&target)
}

/// 開けなかった `.vst3` に添える手がかり。
///
/// **clap-wrapper で作った VST3 は、同名の `.clap` を CLAP の探索パスから
/// 探して読み込む。** 見つからないとファクトリを返せず、
/// `GetPluginFactory returned null` とだけ言って終わる。
///
/// 隣に同名の `.clap` があるなら、まずこれを疑う。そのままでは
/// 「読めない理由が分からないプラグイン」として一覧から消えてしまう。
fn clap_wrapper_hint(path: &Path) -> Option<String> {
    let beside = path.with_extension("clap");
    if !beside.exists() {
        return None;
    }
    Some(format!(
        "隣に {} があります。clap-wrapper で作った VST3 は同名の .clap を \
         CLAP の探索パスから探すので、環境変数 CLAP_PATH にこのフォルダを足すか、\
         .clap を CLAP の標準の置き場へ入れてください",
        beside
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default()
    ))
}

/// 1ファイルを開いて中のプラグインを数え上げる (形式は拡張子で決まる)。
///
/// # Safety
/// 任意の DLL を実行することと同じ。信頼できる置き場だけを走査すること。
pub fn scan_file(path: &Path) -> Result<Vec<FoundPlugin>, Box<dyn Error>> {
    match kind_of(path) {
        // CLAP は列挙のために開いた `PluginEntry` をここで捨てる
        // (載せるときに開き直す。走査中は抱えておく理由が無い)
        Some(PluginKind::Clap) => load_clap_file(path).map(|(_entry, plugins)| plugins),
        Some(PluginKind::Vst3) => load_vst3_file(path).map_err(|e| match clap_wrapper_hint(path) {
            Some(hint) => format!("{e}\n  → {hint}").into(),
            None => e,
        }),
        None => Err("拡張子が .clap でも .vst3 でもありません".into()),
    }
}

/// CLAP の宣言から種別を決める。
///
/// **両方宣言していれば音源を採る。** 音を作れるものは音源として置きたいし、
/// 入力も取る音源 (サンプラー等) が `audio-effect` を併記することがある。
fn clap_role(descriptor: &PluginDescriptor) -> Role {
    use clack_host::plugin::features::{AUDIO_EFFECT, INSTRUMENT};

    let mut effect = false;
    for feature in descriptor.features() {
        if feature == INSTRUMENT {
            return Role::Instrument;
        }
        if feature == AUDIO_EFFECT {
            effect = true;
        }
    }
    if effect {
        Role::Effect
    } else {
        Role::Unknown
    }
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

    let text = |value: Option<&std::ffi::CStr>| {
        value
            .map(|v| v.to_string_lossy().to_string())
            .unwrap_or_default()
    };

    let plugins: Vec<FoundPlugin> = factory
        .plugin_descriptors()
        .filter_map(|descriptor| {
            let id = descriptor.id()?.to_str().ok()?.to_string();
            let name = descriptor
                .name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| id.clone());
            Some(FoundPlugin {
                id,
                name,
                role: clap_role(&descriptor),
                vendor: text(descriptor.vendor()),
                version: text(descriptor.version()),
            })
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

/// VST3 の副カテゴリから種別を決める。
///
/// 副カテゴリは `Instrument|Synth` のように `|` で連なる。分解済みで渡って
/// くる経路と、連なったまま渡ってくる経路の両方があるので、ここで割る。
/// CLAP と同じく**音源を優先する**。
fn vst3_role(sub_categories: &[String]) -> Role {
    let has = |needle: &str| {
        sub_categories
            .iter()
            .flat_map(|category| category.split('|'))
            .any(|part| part.trim().eq_ignore_ascii_case(needle))
    };
    if has("Instrument") {
        Role::Instrument
    } else if has("Fx") {
        Role::Effect
    } else {
        Role::Unknown
    }
}

/// `moduleinfo.json` の宣言だけで一覧を作る。**モジュールを読み込まない。**
///
/// SDK 3.7.5 以降のバンドルは、クラスの ID・名前・作者・版・副カテゴリを
/// すべて JSON に書き出している。**走査で要るのはこれで全部**なので、
/// これが読めたならモジュールを開く理由が無い。
///
/// クラス ID の綴りは両方の道で揃う。JSON 側は読み込み時に大文字の
/// 32桁へ正規化され、ファクトリ側も同じ正準形へ直してから返ってくる
/// (Windows の COM 順の入れ替えは `vst3-host` が吸収する)。
fn vst3_from_module_info(module: &vst3_host::discovery::ModuleInfo) -> Vec<FoundPlugin> {
    module
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
            role: vst3_role(&class.sub_categories),
            vendor: if class.vendor.is_empty() {
                module.factory.vendor.clone()
            } else {
                class.vendor.clone()
            },
            version: if class.version.is_empty() {
                module.version.clone()
            } else {
                class.version.clone()
            },
        })
        .collect()
}

/// ファクトリの宣言から一覧を作る。**モジュールは読むが、プラグインは作らない。**
///
/// `moduleinfo.json` が無いバンドル向け。`IPluginFactory2` 以降が
/// クラスごとの副カテゴリを持っているので、JSON があるときと同じ粒度で分かる。
fn vst3_from_factory(listed: &vst3_host::discovery::FactoryClasses) -> Vec<FoundPlugin> {
    listed
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
            role: vst3_role(&class.sub_categories),
            vendor: if class.vendor.is_empty() {
                listed.factory.vendor.clone()
            } else {
                class.vendor.clone()
            },
            version: class.version.clone(),
        })
        .collect()
}

/// .vst3 を読み、含まれる音源クラスの一覧を返す。
///
/// Windows では `Foo.vst3` がバンドル**ディレクトリ**のことも素の DLL のこともある。
/// どちらを渡してもよい (`vst3-host` 側が判別する)。
///
/// CLAP と違って `PluginEntry` に当たるものは返さない。`vst3-host` の `Plugin` が
/// モジュールを自分で抱えるので、載せるときに開き直せばよい。
///
/// **種別の情報源は2つあり、どちらもクラス単位で分かる。**
///
/// | 出どころ | いつ取れるか | 代償 |
/// |---|---|---|
/// | `moduleinfo.json` の `sub_categories` | SDK 3.7.5 以降のバンドルだけ | JSON を1つ読むだけ |
/// | ファクトリの `sub_categories` | `IPluginFactory2` 以降 (ほぼ全部) | モジュールを読む |
///
/// **前者で足りるならそこで返す。** 桁違いに安い。
///
/// **どちらもプラグインを作らない。** `get_detailed_plugin_info` は使いもしない
/// バス構成を調べるために `createInstance` と `initialize` を呼ぶので使わない。
/// 音源はそこでウェーブテーブルやプリセットを読み、1本で数秒かかる。
///
/// 副カテゴリを宣言していないものは [`Role::Unknown`] のまま「未分類」へ置く
/// (**間違った分類より、分からないと言うほうがよい**)。
///
/// # Safety
/// VST3 のロードは任意の DLL の実行を意味するため、信頼できるファイルのみを開くこと。
pub fn load_vst3_file(path: &Path) -> Result<Vec<FoundPlugin>, Box<dyn Error>> {
    // **JSON で足りるならモジュールを開かない。**
    // 読めない・壊れている・音のクラスが1つも無い、のいずれでも下へ落ちる
    // (「入っていません」と断るのは、モジュールに直接聞いてからにする)
    if let Ok(Some(module)) = vst3_host::discovery::read_module_info(path) {
        let plugins = vst3_from_module_info(&module);
        if !plugins.is_empty() {
            return Ok(plugins);
        }
    }

    let listed = vst3_host::discovery::list_plugin_classes(path)?;
    let plugins = vst3_from_factory(&listed);

    if plugins.is_empty() {
        return Err("このファイルには VST3 の音源が含まれていません".into());
    }

    Ok(plugins)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 使い捨てのフォルダを作る (テストごとに別の名前)
    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("egui-clap-host-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("一時フォルダを作れること");
        dir
    }

    fn touch(path: &Path) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, b"").unwrap();
    }

    #[test]
    fn the_extension_decides_the_format() {
        assert_eq!(kind_of(Path::new("a/b.clap")), Some(PluginKind::Clap));
        assert_eq!(kind_of(Path::new("a/b.vst3")), Some(PluginKind::Vst3));
        // 大文字でも通ること (Windows は大小を区別しない)
        assert_eq!(kind_of(Path::new("a/B.CLAP")), Some(PluginKind::Clap));
        assert_eq!(kind_of(Path::new("a/b.dll")), None);
        assert_eq!(kind_of(Path::new("a/b")), None);
    }

    /// **`.vst3` のディレクトリの中へ入らないこと。**
    ///
    /// バンドルそのものが1つのプラグインなので、中の
    /// `Contents\x86_64-win\Foo.vst3` を別に数えると二重になる。
    #[test]
    fn a_bundle_counts_once() {
        let root = temp_dir("bundle");
        touch(&root.join("Foo.vst3/Contents/x86_64-win/Foo.vst3"));

        let found = plugin_files(&root);
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0], root.join("Foo.vst3"));

        let _ = std::fs::remove_dir_all(&root);
    }

    /// ベンダーのフォルダを掘って見つけること
    #[test]
    fn it_digs_through_vendor_folders() {
        let root = temp_dir("vendor");
        touch(&root.join("Direct.clap"));
        touch(&root.join("VendorA/Nested.clap"));
        touch(&root.join("VendorB/Deeper/Deep.vst3"));

        let found = plugin_files(&root);
        let names: Vec<String> = found
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(found.len(), 3, "{names:?}");
        assert!(names.contains(&"Direct.clap".to_string()));
        assert!(names.contains(&"Nested.clap".to_string()));
        assert!(names.contains(&"Deep.vst3".to_string()));

        let _ = std::fs::remove_dir_all(&root);
    }

    /// 関係ないファイルは拾わないこと
    #[test]
    fn it_ignores_everything_else() {
        let root = temp_dir("noise");
        touch(&root.join("readme.txt"));
        touch(&root.join("plugin.dll"));
        touch(&root.join("Real.clap"));

        let found = plugin_files(&root);
        assert_eq!(found.len(), 1, "{found:?}");
        assert!(found[0].ends_with("Real.clap"));

        let _ = std::fs::remove_dir_all(&root);
    }

    /// 無いフォルダを渡しても落ちないこと (登録したフォルダが消えることがある)
    #[test]
    fn a_missing_folder_is_empty_not_an_error() {
        let missing = std::env::temp_dir().join("egui-clap-host-does-not-exist");
        assert!(plugin_files(&missing).is_empty());
    }

    /// VST3 の副カテゴリから種別が決まること。**音源を優先する**
    #[test]
    fn vst3_sub_categories_decide_the_role() {
        let of = |list: &[&str]| vst3_role(&list.iter().map(|s| s.to_string()).collect::<Vec<_>>());
        assert_eq!(of(&["Instrument", "Synth"]), Role::Instrument);
        assert_eq!(of(&["Fx", "Reverb"]), Role::Effect);
        assert_eq!(of(&["Fx", "Instrument"]), Role::Instrument, "音源が優先");
        assert_eq!(of(&["Analyzer"]), Role::Unknown);
        assert_eq!(of(&[]), Role::Unknown);
        // 大小の違いで落とさない
        assert_eq!(of(&["instrument"]), Role::Instrument);
    }

    /// `|` で連なった副カテゴリも割れること。
    /// クラスごとの情報が無いときは、ファクトリの `Fx|Reverb` 形式が渡る
    #[test]
    fn piped_sub_categories_are_split() {
        let of = |text: &str| vst3_role(std::slice::from_ref(&text.to_string()));
        assert_eq!(of("Instrument|Synth"), Role::Instrument);
        assert_eq!(of("Fx|Reverb"), Role::Effect);
        assert_eq!(of("Fx"), Role::Effect);
        assert_eq!(of(""), Role::Unknown);
        assert_eq!(of("Instrument | Synth"), Role::Instrument, "空白があっても");
    }

    /// **クラスごとに種別が分かれること** (ファクトリから読んだ場合)。
    ///
    /// ここが以前の弱点だった。ファイル全体のカテゴリしか取れなかったころは、
    /// 音のクラスが2つあるファイルを全部「未分類」に置くしかなかった
    /// (当てると間違える。検証用の VST3 でエフェクトが音源として出た)。
    /// `list_plugin_classes` がクラスごとの副カテゴリを返すので、その妥協は消えた。
    #[test]
    fn each_factory_class_gets_its_own_role() {
        use vst3_host::discovery::{ClassInfo, FactoryClasses, FactoryInfo};

        let class = |name: &str, sub: &[&str]| ClassInfo {
            name: name.to_string(),
            category: "Audio Module Class".to_string(),
            class_id: format!("{name}-id"),
            sub_categories: sub.iter().map(|s| s.to_string()).collect(),
            ..Default::default()
        };
        let listed = FactoryClasses {
            factory: FactoryInfo {
                vendor: "Example Audio".to_string(),
                ..Default::default()
            },
            classes: vec![
                class("Synth", &["Instrument", "Synth"]),
                class("Reverb", &["Fx", "Reverb"]),
                // 宣言が無いものは未分類のまま
                class("Mystery", &[]),
                // 音のクラスでないものは拾わない
                ClassInfo {
                    name: "Controller".to_string(),
                    category: "Component Controller Class".to_string(),
                    class_id: "controller-id".to_string(),
                    ..Default::default()
                },
            ],
        };

        let found = vst3_from_factory(&listed);
        assert_eq!(found.len(), 3, "音のクラスだけ拾うこと: {found:?}");
        assert_eq!(found[0].role, Role::Instrument);
        assert_eq!(found[1].role, Role::Effect);
        assert_eq!(found[2].role, Role::Unknown);
        assert_eq!(found[0].vendor, "Example Audio", "無ければ作者を借りること");
    }

    /// 名前が空なら ID で代用すること (一覧が空欄にならない)
    #[test]
    fn a_nameless_factory_class_shows_its_id() {
        use vst3_host::discovery::{ClassInfo, FactoryClasses};

        let listed = FactoryClasses {
            classes: vec![ClassInfo {
                category: "Audio Module Class".to_string(),
                class_id: "AABBCC".to_string(),
                ..Default::default()
            }],
            ..Default::default()
        };
        assert_eq!(vst3_from_factory(&listed)[0].name, "AABBCC");
    }

    /// **隣に同名の .clap があるときだけ手がかりを添えること。**
    ///
    /// clap-wrapper で作った VST3 は `GetPluginFactory returned null` としか
    /// 言わないので、そのままでは理由が分からない。
    #[test]
    fn a_clap_wrapper_vst3_gets_a_hint() {
        let root = temp_dir("wrapper");
        touch(&root.join("Wrapped.clap"));
        touch(&root.join("Wrapped.vst3"));
        touch(&root.join("Plain.vst3"));

        let hint = clap_wrapper_hint(&root.join("Wrapped.vst3")).expect("手がかりが付くこと");
        assert!(hint.contains("Wrapped.clap"), "{hint}");
        assert!(hint.contains("CLAP_PATH"), "直し方まで言うこと: {hint}");

        assert!(
            clap_wrapper_hint(&root.join("Plain.vst3")).is_none(),
            "隣に .clap が無ければ添えないこと"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// 拡張子が違うファイルは開かずに断ること
    #[test]
    fn scanning_a_foreign_file_is_refused() {
        let error = scan_file(Path::new("a/b.dll")).expect_err("断ること");
        assert!(error.to_string().contains("拡張子"), "{error}");
    }

    /// **`moduleinfo.json` だけのバンドルを読めること。**
    ///
    /// 中に実体 (DLL) を一切置かない。それでも一覧が返るなら、
    /// **モジュールを開く道を通っていない**と言い切れる。
    ///
    /// ついでに、**クラスごとに種別が分かれること**も見ている。
    #[test]
    fn a_bundle_with_module_info_is_read_without_loading_it() {
        let root = temp_dir("module-info").join("Pack.vst3");
        let json = root.join("Contents").join("Resources");
        std::fs::create_dir_all(&json).unwrap();

        // CID は32桁の16進。読み込み時に大文字へ正規化される
        std::fs::write(
            json.join("moduleinfo.json"),
            br#"{
                "Name": "Pack",
                "Version": "2.5.0",
                "Factory Info": { "Vendor": "Example Audio" },
                "Classes": [
                    {
                        "CID": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                        "Category": "Audio Module Class",
                        "Name": "Pack Synth",
                        "Sub Categories": ["Instrument", "Synth"]
                    },
                    {
                        "CID": "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
                        "Category": "Audio Module Class",
                        "Name": "Pack Reverb",
                        "Vendor": "Other Vendor",
                        "Version": "1.2",
                        "Sub Categories": ["Fx", "Reverb"]
                    },
                    {
                        "CID": "cccccccccccccccccccccccccccccccc",
                        "Category": "Component Controller Class",
                        "Name": "Pack Controller"
                    }
                ]
            }"#,
        )
        .unwrap();

        let found = load_vst3_file(&root).expect("JSON だけで読めること");
        assert_eq!(found.len(), 2, "音のクラスだけ拾うこと: {found:?}");

        let synth = &found[0];
        assert_eq!(synth.id, "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA", "大文字の32桁");
        assert_eq!(synth.name, "Pack Synth");
        assert_eq!(synth.role, Role::Instrument);
        assert_eq!(synth.vendor, "Example Audio", "無ければ作者を借りること");
        assert_eq!(synth.version, "2.5.0", "無ければ版を借りること");

        let reverb = &found[1];
        assert_eq!(reverb.role, Role::Effect, "クラスごとに分かれること");
        assert_eq!(reverb.vendor, "Other Vendor", "あれば自分のものを使うこと");
        assert_eq!(reverb.version, "1.2");

        let _ = std::fs::remove_dir_all(root.parent().unwrap());
    }

    /// **JSON が音のクラスを1つも宣言していなければ、下の道へ落ちること。**
    ///
    /// 実体が無いので開けずに終わる。**「入っていません」と断ってしまわない**
    /// のが要点で、断るのはモジュールに直接聞いてからにする
    #[test]
    fn a_module_info_without_audio_classes_falls_through() {
        let root = temp_dir("module-info-empty").join("Empty.vst3");
        let json = root.join("Contents").join("Resources");
        std::fs::create_dir_all(&json).unwrap();
        std::fs::write(
            json.join("moduleinfo.json"),
            br#"{ "Name": "Empty", "Classes": [] }"#,
        )
        .unwrap();

        load_vst3_file(&root).expect_err("実体が無いので開けないこと");

        let _ = std::fs::remove_dir_all(root.parent().unwrap());
    }

    /// バンドルの印は**中の実体**から取ること。
    ///
    /// ディレクトリの更新時刻は中を書き換えても動かないので、そこを見ていると
    /// 差分走査が更新に気付けない
    #[test]
    fn a_bundle_is_stamped_by_its_binary() {
        let root = temp_dir("bundle-stamp").join("Stamped.vst3");
        let binary = root
            .join("Contents")
            .join("x86_64-win")
            .join("Stamped.vst3");
        std::fs::create_dir_all(binary.parent().unwrap()).unwrap();
        std::fs::write(&binary, b"0123456789").unwrap();

        let taken = stamp(&root).expect("印が取れること");
        assert_eq!(taken.size, 10, "中の実体の大きさになること");
        assert_eq!(taken, Stamp::of(&binary).unwrap());

        let _ = std::fs::remove_dir_all(root.parent().unwrap());
    }

    /// 実体の無いバンドルからは印を取らないこと (**開いて確かめる側へ倒す**)
    #[test]
    fn a_bundle_without_a_binary_has_no_stamp() {
        let root = temp_dir("bundle-no-binary").join("Hollow.vst3");
        std::fs::create_dir_all(root.join("Contents")).unwrap();

        assert!(stamp(&root).is_none());

        let _ = std::fs::remove_dir_all(root.parent().unwrap());
    }

    /// 素のファイルはそのまま印を取ること (`.clap` はこちら)
    #[test]
    fn a_plain_file_is_stamped_directly() {
        let root = temp_dir("file-stamp");
        let path = root.join("Plain.clap");
        std::fs::write(&path, b"01234").unwrap();

        let taken = stamp(&path).expect("印が取れること");
        assert_eq!(taken.size, 5);

        let _ = std::fs::remove_dir_all(&root);
    }
}
