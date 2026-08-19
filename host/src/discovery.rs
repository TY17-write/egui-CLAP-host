//! 音源ファイルのロードとプラグイン列挙 (.clap / .vst3)。
//!
//! どちらの形式も「1つのファイルに複数のプラグインが入りうる」ので、
//! 載せる前に中身を数え上げる必要がある。列挙の結果は [`FoundPlugin`] に
//! 揃えてあり、呼び出し側 (選択 UI) は形式を意識しない。
//!
//! **種別 (音源かエフェクトか) は宣言を読むだけで分かる。** インスタンス化して
//! 入力ポートを数える必要は無いので、フォルダ走査で全部を起動せずに済む。

#![allow(unsafe_code)]

use crate::library::Role;
use crate::project::PluginKind;
use clack_host::plugin::PluginDescriptor;
use clack_host::prelude::*;
use std::error::Error;
use std::path::{Path, PathBuf};

/// フォルダを掘る深さの上限。
/// ベンダーが1〜2階層掘る程度なので十分で、リンクの輪に落ちる保険も兼ねる
const MAX_DEPTH: usize = 8;

/// ファイル内で見つかったプラグインの情報
#[derive(Debug)]
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

/// 1ファイルを開いて中のプラグインを数え上げる (形式は拡張子で決まる)。
///
/// # Safety
/// 任意の DLL を実行することと同じ。信頼できる置き場だけを走査すること。
pub fn scan_file(path: &Path) -> Result<Vec<FoundPlugin>, Box<dyn Error>> {
    match kind_of(path) {
        // CLAP は列挙のために開いた `PluginEntry` をここで捨てる
        // (載せるときに開き直す。走査中は抱えておく理由が無い)
        Some(PluginKind::Clap) => load_clap_file(path).map(|(_entry, plugins)| plugins),
        Some(PluginKind::Vst3) => load_vst3_file(path),
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

/// ファイル全体のカテゴリを、その中のクラスに当ててよいか決める。
///
/// **当てられるのは、音のクラスが1つだけのとき。** 複数入っていると、
/// ファイル全体の値は代表の1つのものでしかない。他のクラスに当てると
/// 間違える (検証用の VST3 で、エフェクトが音源として出た)。
fn file_wide_role(audio_class_count: usize, category: &str) -> Role {
    if audio_class_count == 1 {
        vst3_role(std::slice::from_ref(&category.to_string()))
    } else {
        Role::Unknown
    }
}

/// .vst3 を読み、含まれる音源クラスの一覧を返す。
///
/// Windows では `Foo.vst3` がバンドル**ディレクトリ**のことも素の DLL のこともある。
/// どちらを渡してもよい (`vst3-host` 側が判別する)。
///
/// CLAP と違って `PluginEntry` に当たるものは返さない。`vst3-host` の `Plugin` が
/// モジュールを自分で抱えるので、載せるときに開き直せばよい。
///
/// **種別の情報源は2つある。**
///
/// | 出どころ | 粒度 | いつ取れるか |
/// |---|---|---|
/// | `moduleinfo.json` の `sub_categories` | クラスごと | SDK 3.7.5 以降のバンドルだけ |
/// | ファクトリの `category` | **ファイルに1つ** | いつでも |
///
/// **前者を優先する。** 無いときは後者で代用するが、それは
/// [`file_wide_role`] のとおりクラスが1つのファイルに限る。
/// 当てられないものは [`Role::Unknown`] のまま「未分類」へ置く
/// (**間違った分類より、分からないと言うほうがよい**)。
///
/// # Safety
/// VST3 のロードは任意の DLL の実行を意味するため、信頼できるファイルのみを開くこと。
pub fn load_vst3_file(path: &Path) -> Result<Vec<FoundPlugin>, Box<dyn Error>> {
    let info = vst3_host::discovery::get_detailed_plugin_info(path)?;

    let audio_classes = info
        .classes
        .iter()
        .filter(|class| class.category.contains(VST3_AUDIO_CLASS));
    let declared = info.module_info.as_ref().map(|module| &module.classes);

    let file_role = file_wide_role(audio_classes.clone().count(), &info.info.category);

    let plugins: Vec<FoundPlugin> = audio_classes
        .map(|class| {
            let from_module_info = declared.and_then(|classes| {
                classes
                    .iter()
                    .find(|entry| entry.class_id.eq_ignore_ascii_case(&class.class_id))
            });
            FoundPlugin {
                id: class.class_id.clone(),
                name: if class.name.is_empty() {
                    class.class_id.clone()
                } else {
                    class.name.clone()
                },
                role: match from_module_info {
                    Some(entry) => vst3_role(&entry.sub_categories),
                    None => file_role,
                },
                vendor: match from_module_info {
                    Some(entry) if !entry.vendor.is_empty() => entry.vendor.clone(),
                    _ => info.info.vendor.clone(),
                },
                version: if class.version.is_empty() {
                    info.info.version.clone()
                } else {
                    class.version.clone()
                },
            }
        })
        .collect();

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

    /// **クラスが複数あるファイルには、全体のカテゴリを当てないこと。**
    ///
    /// 当てると間違える。検証用の VST3 (1ファイルに音源とエフェクト、
    /// `moduleinfo.json` 無し) で、エフェクトが音源として出た。
    /// 「未分類」に置くほうが正しい。
    #[test]
    fn a_file_wide_category_only_applies_to_a_single_class() {
        assert_eq!(file_wide_role(1, "Instrument|Synth"), Role::Instrument);
        assert_eq!(file_wide_role(1, "Fx|Reverb"), Role::Effect);
        // 2つ以上なら、どちらのものか分からない
        assert_eq!(file_wide_role(2, "Instrument|Synth"), Role::Unknown);
        assert_eq!(file_wide_role(5, "Fx"), Role::Unknown);
        // クラスが1つでもカテゴリが空なら分からない
        assert_eq!(file_wide_role(1, ""), Role::Unknown);
    }

    /// 拡張子が違うファイルは開かずに断ること
    #[test]
    fn scanning_a_foreign_file_is_refused() {
        let error = scan_file(Path::new("a/b.dll")).expect_err("断ること");
        assert!(error.to_string().contains("拡張子"), "{error}");
    }
}
