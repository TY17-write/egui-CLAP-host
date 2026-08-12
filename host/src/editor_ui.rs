//! シーケンスエディタの egui UI。
//! 段 (レーン) 方式: デフォルト16段の横レーンがあり、ノートは置かれた段に属する。
//! 各ノートは "(半音,オクターブ)" ラベルを表示し、縦ドラッグで段の移動、
//! 右端ドラッグで音価を変更する。ピッチは選択して数値で編集する。

use crate::sequencer::{MidiEditor, Note, ScaleMode, TrackInfo};
use crate::swing;
use crate::theme::palette;
use eframe::egui::{
    self, Align2, Color32, CornerRadius, CursorIcon, FontId, Pos2, Rect, Sense, Stroke, vec2,
};

/// 1四分音符の横幅の既定値 (ピクセル)。
/// 実際の幅は `EditorState::ppq` (横ズームで変わる)
const PPQ: f32 = 80.0;
/// 1四分音符の横幅の下限。小節線が潰れて拍が読めなくなるので、これ以上は縮めない
const MIN_PPQ: f32 = 16.0;
/// 1四分音符の横幅の上限
const MAX_PPQ: f32 = 480.0;
/// Ctrl+中ドラッグ 1px あたりの横ズーム倍率。
///
/// 画面の端から端まで (およそ 900px) 動かして 200 倍前後になる大きさ。
/// これより小さいと、下限から上限まで動かすのに何往復も必要になる。
const PPQ_ZOOM_PER_PIXEL: f32 = 1.006;
/// 段の高さの既定値。実際の高さは `EditorState::row_h` (縦ズームで変わる)
const ROW_H: f32 = 24.0;
/// 段の高さの下限。左のトラック欄のボタンが潰れるので、これ以上は縮めない
const MIN_ROW_H: f32 = 12.0;
/// トラック欄2段目 (段の増減ボタン) の上端。1段目のボタンの下に置く。
///
/// トラックの高さは段数 × 段の高さで決まり、グリッドと揃える必要があるため
/// 広げられない。**入らない高さのときは1段目に並べる** (隠れると段を増やす
/// 手立てが無くなるため)。
const LANE_BUTTON_ROW_Y: f32 = 22.0;
/// 2段目の高さ。これが入らなければ1段目へ回す
const LANE_BUTTON_ROW_H: f32 = 18.0;
/// 段の帯 (全選択・入れ替え) の幅。トラック欄の右端に置く
const LANE_STRIP_W: f32 = 10.0;
/// 段の高さの上限
const MAX_ROW_H: f32 = 96.0;
/// Ctrl+ホイール1ノッチあたりの倍率 (段の高さ)
const ROW_ZOOM_STEP: f32 = 1.15;
/// ルーラーの高さ
const RULER_H: f32 = 22.0;
/// ノート右端のリサイズ判定幅
const EDGE_W: f32 = 10.0;

/// ノートに載せる音名ラベルの文字サイズ。
/// これが入らない高さまで縮めたらラベルを省く
const NOTE_LABEL_SIZE: f32 = 11.0;

/// 左のトラック欄の幅
const GUTTER_W: f32 = 200.0;

/// Alt+ホイール1ノッチあたりのベロシティ変化量
const VELOCITY_WHEEL_STEP: i32 = 4;

/// 指定できるオクターブの範囲
const MIN_OCTAVE: i32 = -2;
const MAX_OCTAVE: i32 = 8;

/// ベロシティに満たない部分 (ゴースト) の不透明度。
/// 0 にするとノートが痩せて見えるので、色相と輪郭が残る程度に薄く塗る。
const VELOCITY_GHOST_ALPHA: f32 = 0.3;

/// ノートの色相 (Oklch, 度)。低い音から高い音へ、
/// ティール → 青 → 藤 → 蘭 → 紅紫 → 珊瑚 → 琥珀。
///
/// **Oklch の色相角**なので、HSV の角度とは対応しない。おおよそ
/// ティール 195°・青 265°・紅紫 325°・赤 389° (= 29°)・琥珀 430° (= 70°)。
/// Oklch は色相を**知覚的に等間隔**に並べるので、HSV でやったときのように
/// 青〜紫が「どれも青っぽい」と潰れることがない。
///
/// 幅を 275° と広めに取っているのは色域の都合。**sRGB では水色〜藤色の帯だけ
/// 彩度が出せない** (明度 0.68 でクロマ上限 0.13 前後、紅紫は 0.29)。
/// 幅が狭いと、いちばん使うオクターブ4〜5 がその帯に居座って淡くなる。
/// 広げると4〜5 が紅紫側へ抜けるので、上限が 0.13 → 0.24 に上がる。
/// 引き換えに、上端は赤を通り越して琥珀色になる。
const NOTE_HUE_LOW: f32 = 195.0;
const NOTE_HUE_HIGH: f32 = 470.0;

/// ノートの明度 (Oklch)。音高によらず一定。
///
/// 一度は音高で動かしてみたが、**明るくするほど彩度が犠牲になる**。
/// 高い明度を要求すると色域が狭まり、クロマを削る処理が強く働くため、
/// 高音側が淡くなってしまった。色相だけで音高を表し、明度は
/// 彩度がいちばん稼げるところに固定するほうが見分けやすい。
///
/// 0.68 という値は上下から挟まれている。**下げるほど青と藤色の彩度が上がる**が、
/// ノート名のラベルが読めなくなる (塗りが届いた部分は [`palette::BG`] で
/// 抜いているため)。この値で全音域のコントラスト比が 5:1 以上ある。
const NOTE_LIGHTNESS: f32 = 0.68;

/// ノートの彩度 (Oklch のクロマ)。
///
/// sRGB に収まらない組み合わせは色相と明度を保ったままクロマを削るので、
/// ここは「出せるだけ出す」ための要求値でよい。色相ごとに出せる上限が
/// 違う (水色は低く、紅紫は高い) ため、一律の値は取れない。
const NOTE_CHROMA: f32 = 0.17;

/// 色を割り当てる音域。この外は端の色で頭打ちにする。
///
/// 指定できる範囲 ([`MIN_OCTAVE`]〜[`MAX_OCTAVE`]) 全体に広げると、実際に
/// よく使う音域が色相のごく一部に収まって隣同士の差が出ない。常用域に寄せて、
/// そこで色が大きく動くようにしてある。
/// 代償として、極端に高い音どうし・低い音どうしは同じ色になる。
const NOTE_COLOR_MIN_OCTAVE: i32 = 1;
const NOTE_COLOR_MAX_OCTAVE: i32 = 7;

/// ノートの塗り色。
///
/// **音高が上がるほど寒色から暖色へ変わる連続的なグラデーション**にしてある。
/// [`NOTE_COLOR_MIN_OCTAVE`]〜[`NOTE_COLOR_MAX_OCTAVE`] を色相の幅に割り当て、
/// その外は端の色で頭打ちにする。割り当ては音高そのものに紐づくので、
/// 同じ音なら常に同じ色になる (画面に出ている範囲で正規化すると、
/// スクロールしただけで色が変わってしまう)。
///
/// オクターブ内も半音で連続的に変える。オクターブ境界で色が飛ばないので、
/// 隣り合う音の高低が色の変化として読める。
fn note_fill(note: &Note, scale: ScaleMode) -> Color32 {
    let steps = scale.steps_per_octave().max(1);
    // オクターブ + オクターブ内の位置。BP 音律なら13ステップで割る
    let position = note.octave as f32 + note.semitone.clamp(0, steps) as f32 / steps as f32;

    let span = (NOTE_COLOR_MAX_OCTAVE - NOTE_COLOR_MIN_OCTAVE + 1) as f32;
    let t = ((position - NOTE_COLOR_MIN_OCTAVE as f32) / span).clamp(0.0, 1.0);

    let hue = NOTE_HUE_LOW + (NOTE_HUE_HIGH - NOTE_HUE_LOW) * t;
    oklch_to_color(NOTE_LIGHTNESS, NOTE_CHROMA, hue)
}

/// Oklch (明度・クロマ・色相) から sRGB へ。`hue` は度。
///
/// Oklch を使うのは、**色相を知覚的に等間隔に並べたい**のと、`lightness` が
/// 見た目の明るさそのものになるため。HSV の `value` は明るさではなく
/// (シアンの V=0.62 はピンクの V=0.95 より明るく見える)、色相の間隔も
/// 知覚と合わない。
///
/// 指定した色が sRGB に収まらないときは、**色相と明度を保ったままクロマを削る**。
/// RGB を切り詰めると色相と明度がずれるので、そちらは採らない。
fn oklch_to_color(lightness: f32, chroma: f32, hue: f32) -> Color32 {
    let radians = hue.to_radians();
    let (sin, cos) = radians.sin_cos();

    // sRGB に収まる最大のクロマを二分探索で詰める。
    // 収まるなら1回で終わり、外れていても十数回で十分な精度になる。
    let mut low = 0.0f32;
    let mut high = chroma.max(0.0);
    if !fits_in_srgb(lightness, high, cos, sin) {
        for _ in 0..16 {
            let middle = (low + high) * 0.5;
            if fits_in_srgb(lightness, middle, cos, sin) {
                low = middle;
            } else {
                high = middle;
            }
        }
    } else {
        low = high;
    }

    let (r, g, b) = oklab_to_linear_srgb(lightness, low * cos, low * sin);
    Color32::from_rgb(linear_to_byte(r), linear_to_byte(g), linear_to_byte(b))
}

/// この明度・クロマ・色相が sRGB の範囲に収まるか
fn fits_in_srgb(lightness: f32, chroma: f32, cos: f32, sin: f32) -> bool {
    let (r, g, b) = oklab_to_linear_srgb(lightness, chroma * cos, chroma * sin);
    // 丸めで境界を割る分の余裕を見る
    let inside = |x: f32| (-1e-4..=1.0 + 1e-4).contains(&x);
    inside(r) && inside(g) && inside(b)
}

/// Oklab から線形 sRGB へ (Björn Ottosson の定義そのまま)
fn oklab_to_linear_srgb(lightness: f32, a: f32, b: f32) -> (f32, f32, f32) {
    let l_ = lightness + 0.396_337_78 * a + 0.215_803_76 * b;
    let m_ = lightness - 0.105_561_346 * a - 0.063_854_17 * b;
    let s_ = lightness - 0.089_484_18 * a - 1.291_485_5 * b;

    let l = l_ * l_ * l_;
    let m = m_ * m_ * m_;
    let s = s_ * s_ * s_;

    (
        4.076_741_7 * l - 3.307_711_6 * m + 0.230_969_94 * s,
        -1.268_438 * l + 2.609_757_4 * m - 0.341_319_38 * s,
        -0.004_196_086_3 * l - 0.703_418_6 * m + 1.707_614_7 * s,
    )
}

/// 線形値に sRGB のガンマをかけてバイトにする
fn linear_to_byte(value: f32) -> u8 {
    let value = value.clamp(0.0, 1.0);
    let encoded = if value <= 0.003_130_8 {
        value * 12.92
    } else {
        1.055 * value.powf(1.0 / 2.4) - 0.055
    };
    (encoded * 255.0).round().clamp(0.0, 255.0) as u8
}

/// 新規ノートに引き継ぐ設定。
/// 選択が外れても保持する必要があるため (ダブルクリックの1打目で選択が解除される)、
/// 選択インデックスとは別に持つ。
#[derive(Clone, Copy)]
pub struct NoteDefaults {
    pub semitone: i32,
    pub octave: i32,
    pub duration: f32,
    pub velocity: u8,
}

impl Default for NoteDefaults {
    fn default() -> Self {
        Self {
            semitone: 0,
            octave: 4,
            duration: 0.25,
            velocity: 100,
        }
    }
}

impl From<&Note> for NoteDefaults {
    fn from(note: &Note) -> Self {
        Self {
            semitone: note.semitone,
            octave: note.octave,
            duration: note.duration,
            velocity: note.velocity,
        }
    }
}

/// エディタの UI 状態 (プラグインのロードをまたいで保持される)
pub struct EditorState {
    pub editor: MidiEditor,
    /// スナップ幅 (四分音符単位)
    pub snap: f32,
    /// 連符モード。1 = オフ、3..=7 は N 連符
    pub tuplet: u32,
    /// ノートの左端でも音価を変更できるようにする (左端は終端を固定して頭を動かす)
    pub left_resize: bool,
    pub looping: bool,
    /// 詳細編集の対象 (選択が1個のときだけ Some)
    pub selected: Option<usize>,
    /// 選択中のノート (範囲選択で複数になる)
    pub selection: Vec<usize>,
    /// 最後に選択・編集したノートの設定 (新規ノートの初期値)
    pub last_note: NoteDefaults,
    /// 変更があり、オーディオスレッドへの再送が必要
    pub dirty: bool,
    /// アンドゥ / リドゥ履歴
    history: History,
    /// 操作ガイドのウィンドウを開いているか
    show_help: bool,
    /// グリッドの縦スクロール位置 (左のトラック欄を追従させるために持つ)
    grid_scroll_y: f32,
    /// 段1つの高さ (縦ズーム)。段が少ないときに広げて操作しやすくするためのもの。
    /// MIN_ROW_H..=MAX_ROW_H に収める (set_row_h を通すこと)。
    row_h: f32,
    /// 四分音符1つの横幅 (横ズーム)。
    /// MIN_PPQ..=MAX_PPQ に収める (set_ppq を通すこと)。
    ppq: f32,
    /// プロジェクトの保存先 (表示用。実際のパス管理は main 側)
    pub project_path: Option<String>,
    /// トラックごとに載っている音源の名前 (表示用。main 側が毎フレーム更新する)
    pub track_plugins: Vec<Option<String>>,
    /// 再生を始めたときの再生ヘッド位置。停止・終端到達でここへ戻す。
    play_return: Option<f64>,
    /// 前フレームの再生状態 (終端に達して自分で止まったことの検出用)
    was_playing: bool,
    /// 現在の選択を last_note に反映してよいか。
    /// 範囲選択やペーストで選ばれたノートは「ユーザーが指し示したノート」では
    /// ないため反映しない (新規ノートの設定が意図せず書き換わるのを防ぐ)。
    track_last_note: bool,
    drag: Option<DragState>,
    /// 中クリックドラッグの用途 (押していなければ None)
    middle_drag: Option<MiddleDrag>,
    /// 次のフレームでグリッドに強制する横スクロール位置。
    ///
    /// 横ズームの補正に使う。**差分 (`scroll_with_delta`) では駄目**で、
    /// 理由は横ズームを反映している箇所に書いてある。
    pending_scroll_x: Option<f32>,
    /// 一度だけ画面に入れたい再生ヘッドの位置 (停止したときに使う)
    scroll_to_quarters: Option<f64>,
    /// 段の種別を設定する一覧を開いているトラック (閉じていれば None)
    lane_config_track: Option<usize>,
    /// Opus 書き出しのビットレート (kbps)。メニューで選ぶ
    pub opus_bitrate_kbps: u32,
    /// 入れ替えの相手待ちになっている段 (track, lane)。
    ///
    /// 段の帯を押すとここに入り (帯が赤くなる)、次に別の段の帯を押すと入れ替わる。
    lane_swap_source: Option<(usize, usize)>,
    /// コピー・カットしたノートの控え (先頭を 0 とした相対位置)。
    ///
    /// **OS のクリップボードは使わない。** 使うと、ノートをコピーするたびに
    /// ユーザーが他所でコピーした内容を壊してしまう。別インスタンスとの
    /// やり取りは要らないと判断したので、ここに持つ。
    note_clipboard: Vec<Note>,
}

/// 中ボタンドラッグで何をするか。
///
/// **押し始めに決めて、離すまで変えない。**途中で Ctrl を足したり離したりしても
/// 切り替わらないので、ドラッグの最中に挙動が変わって驚くことがない。
#[derive(Clone, Copy, PartialEq, Debug)]
enum MiddleDrag {
    /// スクロール (修飾キーなし)
    Pan,
    /// 横ズーム (Ctrl 併用)。左へ動かすと拡大、右へ動かすと縮小。
    ZoomHorizontally {
        /// 押した位置の四分音符。ズームしてもここが動かないようスクロールを合わせる。
        /// カーソルの現在位置ではなく**押した位置**を使うので、
        /// ドラッグ中に掴んだ場所が逃げていかない。
        anchor_quarters: f32,
    },
}

impl Default for EditorState {
    fn default() -> Self {
        let editor = MidiEditor::default();
        Self {
            history: History::new(&editor),
            editor,
            snap: 0.25,
            tuplet: 1,
            left_resize: false,
            looping: false,
            selected: None,
            selection: Vec::new(),
            last_note: NoteDefaults::default(),
            dirty: true, // 初回コミットのため
            show_help: false,
            grid_scroll_y: 0.0,
            row_h: ROW_H,
            ppq: PPQ,
            project_path: None,
            track_plugins: Vec::new(),
            play_return: None,
            was_playing: false,
            track_last_note: false,
            drag: None,
            middle_drag: None,
            pending_scroll_x: None,
            scroll_to_quarters: None,
            lane_config_track: None,
            opus_bitrate_kbps: crate::opus::DEFAULT_BITRATE_KBPS,
            lane_swap_source: None,
            note_clipboard: Vec::new(),
        }
    }
}

/// 段ごとに「音符を置く段」か「CC を書く段」かを決める一覧。
///
/// 段は何段にも増えるので、左のトラック欄に並べず別窓にしている。
///
/// **CC 段では、ブロックの頭で値を送り、尻で 0 に戻す。** MIDI に「CC 無し」は
/// 無いので、書いていない区間は 0 を送ることで表す (`CC_RELEASE`)。停止・シークでも
/// 同じように戻すので、ペダルが踏みっぱなしで残ることはない。
fn lane_config_window(ctx: &egui::Context, state: &mut EditorState) {
    let Some(track) = state.lane_config_track else {
        return;
    };
    if track >= state.editor.track_count() {
        state.lane_config_track = None;
        return;
    }

    let mut open = true;
    egui::Window::new(format!("CC 段 — {}", state.editor.tracks[track].name))
        .collapsible(false)
        .resizable(false)
        .open(&mut open)
        .show(ctx, |ui| {
            // 追加・削除はここにも置く。トラック欄が狭いときは行に出ないので、
            // ここが唯一の入口になる。
            ui.horizontal(|ui| {
                ui.label("CC 段:");
                cc_lane_buttons(ui, state, track);
            });
            ui.separator();

            let lanes = state.editor.lanes(track);
            let first_cc = state.editor.tracks[track].normal_lanes();
            if first_cc >= lanes {
                ui.label("このトラックに CC 段はありません。");
                ui.weak("上の「+」で最下段に追加できます。");
                return;
            }

            ui.label("置いたブロックの長さだけ CC が効きます。");
            ui.weak("値はベロシティをそのまま使います。書いていない区間は 0 です。");
            ui.separator();

            for lane in first_cc..lanes {
                let current = state.editor.lane_cc(track, lane);
                ui.horizontal(|ui| {
                    ui.label(format!("段 {}", lane + 1));

                    if let Some(number) = current {
                        let mut number = number;
                        if ui
                            .add(
                                egui::DragValue::new(&mut number)
                                    .range(0..=127)
                                    .speed(0.25)
                                    .prefix("CC "),
                            )
                            .changed()
                        {
                            state.history.record(EditGroup::Once);
                            state.editor.tracks[track].set_lane_cc(lane, Some(number));
                            state.dirty = true;
                        }

                        egui::ComboBox::from_id_salt(("cc_preset", track, lane))
                            .selected_text(
                                COMMON_CCS
                                    .iter()
                                    .find(|(n, _)| *n == number)
                                    .map_or("その他", |(_, name)| name),
                            )
                            .show_ui(ui, |ui| {
                                for (n, name) in COMMON_CCS {
                                    if ui
                                        .selectable_label(n == number, format!("CC{n} {name}"))
                                        .clicked()
                                    {
                                        state.history.record(EditGroup::Once);
                                        state.editor.tracks[track].set_lane_cc(lane, Some(n));
                                        state.dirty = true;
                                    }
                                }
                            });

                        // 0 が「効いていない」に当たらない CC は、書いていない区間が
                        // 無音・左端・片寄りになってしまう。黙って壊れないよう断っておく。
                        // (7 音量 / 8 バランス / 10 パン)
                        if matches!(number, 7 | 8 | 10) {
                            ui.colored_label(palette::YELLOW, "⚠ 0 が中立ではありません")
                                .on_hover_text(
                                    "書いていない区間が 0 になります。\
                                     音量やパンでは無音・左端になってしまうので、\
                                     区間を切らずに書いてください。",
                                );
                        }
                    }
                });
            }
        });

    if !open {
        state.lane_config_track = None;
    }
}

/// クリップボードが空のときにだけ置く印。
///
/// 中身に意味は無く、**Ctrl+V を発火させるためだけ**のもの。他所へ貼られても
/// 何が起きたか分かる文言にしてある。
const CLIPBOARD_MARKER: &str = "clap-host-test: ノートをコピーしました";

/// クリップボードにテキストが入っているか。
///
/// egui はクリップボードを読む口をアプリへ出していない (貼り付けはイベントで
/// 届くだけ) ので、Win32 に直接聞く。`IsClipboardFormatAvailable` は
/// **開かずに問い合わせられる**ので、他アプリのコピー操作と競合しない。
///
/// 見るのは Unicode テキストと ANSI テキストの2つ。egui-winit の裏にいる
/// `arboard` が読むのがこれらなので、判定を揃えておく (画像だけが入っている
/// ときは「テキスト無し」= Ctrl+V が発火しない、という実際の挙動に合う)。
fn clipboard_has_text() -> bool {
    use windows_sys::Win32::System::DataExchange::IsClipboardFormatAvailable;
    use windows_sys::Win32::System::Ole::{CF_TEXT, CF_UNICODETEXT};

    // SAFETY: 引数を取るだけで、こちらの資源には触れない問い合わせ
    unsafe {
        IsClipboardFormatAvailable(CF_UNICODETEXT as u32) != 0
            || IsClipboardFormatAvailable(CF_TEXT as u32) != 0
    }
}

/// CC 段の設定でよく使う番号。名前を出しておかないと番号だけでは分からない。
const COMMON_CCS: [(u8, &str); 6] = [
    (1, "モジュレーション"),
    (11, "エクスプレッション"),
    (64, "ペダル (サステイン)"),
    (66, "ソステヌート"),
    (67, "ソフト"),
    (7, "音量"),
];

impl EditorState {
    /// 実際に使うスナップ幅 (四分音符単位)。連符モードなら連符1音分。
    ///
    /// N 連符は「スナップ幅2つ分を N 等分」する。つまり連符 N 音でスナップ2つ分に
    /// ちょうど収まる (スナップ 1/8 なら、3連符でも5連符でも四分音符1つ分に N 音)。
    pub fn snap_unit(&self) -> f32 {
        let n = self.tuplet.clamp(1, 7);
        if n <= 1 {
            return self.snap;
        }
        self.snap * 2.0 / n as f32
    }

    /// 段の高さを変える。範囲外は丸める。実際に変わった量 (新 - 旧) を返す。
    ///
    /// 戻り値はズーム後もカーソル下の段を動かさないためのスクロール補正に使う。
    fn set_row_h(&mut self, row_h: f32) -> f32 {
        let clamped = row_h.clamp(MIN_ROW_H, MAX_ROW_H);
        let delta = clamped - self.row_h;
        self.row_h = clamped;
        delta
    }

    /// 四分音符1つの横幅を変える。範囲外は丸める。実際に変わった量 (新 - 旧) を返す。
    ///
    /// 戻り値はズーム後も掴んだ位置を動かさないためのスクロール補正に使う
    /// ([`set_row_h`](Self::set_row_h) と同じ形)。
    fn set_ppq(&mut self, ppq: f32) -> f32 {
        let clamped = ppq.clamp(MIN_PPQ, MAX_PPQ);
        let delta = clamped - self.ppq;
        self.ppq = clamped;
        delta
    }

    fn is_selected(&self, idx: usize) -> bool {
        self.selection.contains(&idx)
    }

    /// 1つだけを選ぶ (ユーザーが直接指したノート。last_note に反映する)
    fn select_single(&mut self, idx: usize) {
        self.selection.clear();
        self.selection.push(idx);
        self.selected = Some(idx);
        self.track_last_note = true;
        self.lane_swap_source = None;
    }

    /// その段のノートを全部選ぶ。
    ///
    /// 段の帯を押したときの選択。ユーザーが個々のノートを指したわけではないので、
    /// 新規ノートの設定 (`last_note`) には引き継がない。
    fn select_lane(&mut self, track: usize, lane: usize) {
        let idxs = self
            .editor
            .notes
            .iter()
            .enumerate()
            .filter(|(_, note)| note.track == track && note.lane == lane)
            .map(|(idx, _)| idx)
            .collect();
        self.select_many(idxs);
    }

    /// 範囲選択・ペーストによる選択。last_note は更新しない。
    ///
    /// **入れ替えの相手待ちは解除する** ([`clear_selection`](Self::clear_selection) を参照)。
    /// 段の帯を押した直後だけは待ち状態にしたいので、**呼び出し側がこれを呼んだ
    /// あとに** `lane_swap_source` を立てること。
    fn select_many(&mut self, idxs: Vec<usize>) {
        self.selected = if idxs.len() == 1 { Some(idxs[0]) } else { None };
        self.selection = idxs;
        self.track_last_note = false;
        self.lane_swap_source = None;
    }

    /// 選択を解除する。
    ///
    /// **段の入れ替えの相手待ちも一緒に解く。** 帯の赤は「その段を選んでいる」印
    /// なので、選択が消えたのに赤いままだと、何もない場所を押したあとに別の段を
    /// 押しただけで入れ替わってしまう。
    fn clear_selection(&mut self) {
        self.selection.clear();
        self.selected = None;
        self.track_last_note = false;
        self.lane_swap_source = None;
    }

    /// 選択中のノートを昇順・重複なしで返す
    fn selection_sorted(&self) -> Vec<usize> {
        let mut idxs: Vec<usize> = self
            .selection
            .iter()
            .copied()
            .filter(|i| *i < self.editor.notes.len())
            .collect();
        idxs.sort_unstable();
        idxs.dedup();
        idxs
    }

    /// 選択中のノートの頭を、最も早いノートに揃える。動いたら true。
    fn align_selection_starts(&mut self) -> bool {
        let idxs = self.selection_sorted();
        if idxs.len() < 2 {
            return false;
        }
        let head = idxs
            .iter()
            .map(|i| self.editor.notes[*i].start_tick)
            .fold(f32::INFINITY, f32::min);

        let mut changed = false;
        for idx in idxs {
            if self.editor.notes[idx].start_tick != head {
                self.editor.notes[idx].start_tick = head;
                changed = true;
            }
        }
        changed
    }

    /// 読み込んだプロジェクトで丸ごと置き換える (アンドゥで戻せる)。
    ///
    /// 中身は `project::from_str` が検証済みなので、ここでは丸めない。
    pub fn replace_project(&mut self, editor: MidiEditor) {
        self.history.record(EditGroup::Once);
        self.editor = editor;
        // 保存されたノートが全部見えるようにトラックと段を確保する
        self.editor.ensure_capacity_for_notes();
        self.clear_selection();
        self.dirty = true;
    }

    /// 読み込んだシーケンスで中身を置き換える (アンドゥで戻せる)。
    /// テンポと拍子はファイルに入っていたときだけ反映する。
    pub fn replace_sequence(
        &mut self,
        notes: Vec<Note>,
        tempo: Option<u32>,
        time_signature: Option<(u32, u32)>,
        lane_ccs: &[(usize, usize, u8)],
    ) {
        self.history.record(EditGroup::Once);
        self.editor.notes = notes;
        // 読み込んだノートが全部見えるようにトラックと段を用意する
        self.editor.tracks.truncate(1);
        self.editor.ensure_capacity_for_notes();
        // 段が揃ってから種別を付ける (先に付けると段が伸びたときに位置がずれる)
        for (track, lane, number) in lane_ccs {
            if let Some(info) = self.editor.tracks.get_mut(*track) {
                info.set_lane_cc(*lane, Some(*number));
            }
        }
        if let Some(tempo) = tempo {
            self.editor.tempo = tempo.clamp(20, 300);
        }
        if let Some((beats, beat_type)) = time_signature {
            self.editor.beats = beats.clamp(1, 16);
            // 対応している分母だけ受け付ける
            if matches!(beat_type, 1 | 2 | 4 | 8 | 16 | 32) {
                self.editor.beat_type = beat_type;
            }
        }
        // 音階モードで使えない半音が入らないようにする
        let max_semitone = self.editor.scale.max_semitone();
        for note in &mut self.editor.notes {
            note.semitone = note.semitone.clamp(0, max_semitone);
        }
        self.clear_selection();
        self.dirty = true;
    }

    /// 選択中のノートを半音単位で移調する。変わったら true。
    ///
    /// 半音とオクターブを「通し番号」に直して増減するので、半音が上限を超えると
    /// 自動的にオクターブへ繰り上がる (12平均律なら (11,4) の1つ上は (0,5)、
    /// ボーレン・ピアースなら (12,4) の1つ上が (0,5))。オクターブ単位の移調は
    /// steps に1オクターブ分のステップ数を渡せばよい。
    /// 選択の一部が範囲外に出る場合は、全員が収まるところまで移調量を抑える
    /// (音程の関係を崩さないため)。
    fn transpose_selection(&mut self, steps: i32) -> bool {
        let idxs = self.selection_sorted();
        if idxs.is_empty() || steps == 0 {
            return false;
        }
        let per_octave = self.editor.scale.steps_per_octave().max(1);
        let position = |note: &Note| note.octave * per_octave + note.semitone;

        let lowest = idxs
            .iter()
            .map(|i| position(&self.editor.notes[*i]))
            .min()
            .unwrap_or(0);
        let highest = idxs
            .iter()
            .map(|i| position(&self.editor.notes[*i]))
            .max()
            .unwrap_or(0);
        let steps = steps
            .max(MIN_OCTAVE * per_octave - lowest)
            .min(MAX_OCTAVE * per_octave + per_octave - 1 - highest);
        if steps == 0 {
            return false;
        }

        for idx in idxs {
            let note = &mut self.editor.notes[idx];
            let moved = position(note) + steps;
            note.octave = moved.div_euclid(per_octave);
            note.semitone = moved.rem_euclid(per_octave);
        }
        true
    }

    /// 選択中のノートのベロシティを delta だけ増減する (1..=127 に収める)。
    /// 変わったら true。
    fn change_selection_velocity(&mut self, delta: i32) -> bool {
        let mut changed = false;
        for idx in self.selection_sorted() {
            let note = &mut self.editor.notes[idx];
            let velocity = (note.velocity as i32 + delta).clamp(1, 127) as u8;
            if note.velocity != velocity {
                note.velocity = velocity;
                changed = true;
            }
        }
        changed
    }

    /// 選択中のノートの尾を、最も終了が遅いノートに揃える。変わったら true。
    /// 開始位置は動かさず、音価を伸ばして終端を合わせる。
    fn align_selection_ends(&mut self) -> bool {
        let idxs = self.selection_sorted();
        if idxs.len() < 2 {
            return false;
        }
        let tail = idxs
            .iter()
            .map(|i| self.editor.notes[*i].end_tick())
            .fold(f32::NEG_INFINITY, f32::max);

        let mut changed = false;
        for idx in idxs {
            let note = &mut self.editor.notes[idx];
            let duration = tail - note.start_tick;
            if duration > 0.0 && note.duration != duration {
                note.duration = duration;
                changed = true;
            }
        }
        changed
    }

    /// 選択中のノートを削除する。削除したら true。
    fn delete_selection(&mut self) -> bool {
        let idxs = self.selection_sorted();
        if idxs.is_empty() {
            return false;
        }
        // インデックスがずれないよう後ろから消す
        for idx in idxs.iter().rev() {
            self.editor.notes.remove(*idx);
        }
        self.clear_selection();
        true
    }

    /// 選択中のノートを控えの形にする。
    /// 開始位置は先頭ノートを 0 とした相対値にする (貼り付け先で再生ヘッドに合わせるため)。
    fn copy_selection(&self) -> Option<Vec<Note>> {
        let mut notes: Vec<Note> = self
            .selection_sorted()
            .iter()
            .map(|i| self.editor.notes[*i])
            .collect();
        if notes.is_empty() {
            return None;
        }
        let base = notes
            .iter()
            .map(|n| n.start_tick)
            .fold(f32::INFINITY, f32::min);
        for note in &mut notes {
            note.start_tick -= base;
        }
        notes.sort_by(|a, b| a.start_tick.total_cmp(&b.start_tick));
        Some(notes)
    }

    /// ノート列を quarters の位置を先頭として貼り付け、貼った分を選択する。
    fn paste_notes(&mut self, notes: &[Note], quarters: f32) -> bool {
        if notes.is_empty() {
            return false;
        }
        let max_semitone = self.editor.scale.max_semitone();
        let first = self.editor.notes.len();
        for note in notes {
            let mut note = *note;
            note.start_tick = (quarters + note.start_tick).max(0.0);
            // 別の音階モードでコピーされたノートが範囲外にならないようにする
            note.semitone = note.semitone.clamp(0, max_semitone);
            self.editor.notes.push(note);
        }
        // 貼り付け先のトラック・段が足りなければ広げる
        self.editor.ensure_capacity_for_notes();
        self.select_many((first..self.editor.notes.len()).collect());
        true
    }
}

/// アンドゥ1ステップ分の状態。
/// 音階モードは半音の値を書き換えるので一緒に戻す必要がある。
/// トラック構成 (段数の増減) も元に戻せるように含める。
#[derive(Clone, PartialEq)]
struct Snapshot {
    notes: Vec<Note>,
    tracks: Vec<TrackInfo>,
    scale: ScaleMode,
}

impl Snapshot {
    fn capture(editor: &MidiEditor) -> Self {
        Self {
            notes: editor.notes.clone(),
            tracks: editor.tracks.clone(),
            scale: editor.scale,
        }
    }

    fn restore(&self, editor: &mut MidiEditor) {
        editor.tracks = self.tracks.clone();
        editor.notes = self.notes.clone();
        editor.scale = self.scale;
    }
}

/// 連続した操作を1ステップにまとめるための区分。
/// 同じ区分が続く間 (ドラッグ中など) は履歴を積み増さない。
#[derive(Clone, Copy, PartialEq, Eq)]
enum EditGroup {
    /// 1回で完結する操作 (追加・削除・ペーストなど)
    Once,
    Move,
    Resize,
    Semitone,
    Octave,
    Velocity,
}

/// 履歴の上限 (これを超えたら古いものから捨てる)
const HISTORY_LIMIT: usize = 200;

/// アンドゥ / リドゥ履歴。
///
/// 「このフレームが始まる前の状態」を毎フレーム控えておき、変更が起きた時点で
/// それを1ステップとして積む。各操作の側で「変更前」を用意しなくて済む。
struct History {
    /// 今フレームの開始時点の状態
    baseline: Snapshot,
    undo: Vec<Snapshot>,
    redo: Vec<Snapshot>,
    /// まとめ中の操作区分
    group: Option<EditGroup>,
}

impl History {
    fn new(editor: &MidiEditor) -> Self {
        Self {
            baseline: Snapshot::capture(editor),
            undo: Vec::new(),
            redo: Vec::new(),
            group: None,
        }
    }

    /// 変更が起きたことを記録する。ドラッグのように毎フレーム呼ばれても、
    /// 同じ区分が続く間は1ステップにまとまる。
    fn record(&mut self, group: EditGroup) {
        if self.group == Some(group) {
            return;
        }
        // 同一フレーム内に2回積むと差分ゼロのステップができるので弾く
        if self.undo.last() != Some(&self.baseline) {
            self.undo.push(self.baseline.clone());
            if self.undo.len() > HISTORY_LIMIT {
                self.undo.remove(0);
            }
        }
        self.redo.clear();
        self.group = (group != EditGroup::Once).then_some(group);
    }

    /// まとめを打ち切る (ドラッグ終了・入力欄からフォーカスが外れたとき)
    fn end_group(&mut self) {
        self.group = None;
    }

    fn can_undo(&self) -> bool {
        !self.undo.is_empty()
    }

    fn can_redo(&self) -> bool {
        !self.redo.is_empty()
    }

    fn undo(&mut self, editor: &mut MidiEditor) -> bool {
        let Some(prev) = self.undo.pop() else {
            return false;
        };
        self.redo.push(Snapshot::capture(editor));
        prev.restore(editor);
        // 戻した直後の状態が次の「変更前」になる
        self.baseline = prev;
        self.group = None;
        true
    }

    fn redo(&mut self, editor: &mut MidiEditor) -> bool {
        let Some(next) = self.redo.pop() else {
            return false;
        };
        self.undo.push(Snapshot::capture(editor));
        next.restore(editor);
        self.baseline = next;
        self.group = None;
        true
    }

    /// フレームの終わりに、次フレームの「変更前」を控える
    fn end_frame(&mut self, editor: &MidiEditor) {
        if self.baseline.notes != editor.notes
            || self.baseline.tracks != editor.tracks
            || self.baseline.scale != editor.scale
        {
            self.baseline = Snapshot::capture(editor);
        }
    }
}

enum DragKind {
    /// ノート本体: 横 = 移動、縦 = 段の移動
    Move,
    /// ノートの端: 音価変更。from_left なら終端を固定して頭を動かす。
    Resize { from_left: bool },
    /// ルーラー上のシーク
    Seek,
    /// 空白からのドラッグ: 範囲選択
    Marquee,
}

struct DragState {
    kind: DragKind,
    /// 操作対象 (インデックスとドラッグ開始時のノート)。
    /// 先頭が掴んだノートで、スナップの基準になる。範囲選択・シークでは空。
    targets: Vec<(usize, Note)>,
    /// 範囲選択の開始時点で選ばれていたノート (Shift 押下時の追加選択用)
    base_selection: Vec<usize>,
    /// 掴んだ位置を「楽譜上の座標」で保持する (x = 四分音符単位, y = 段)。
    ///
    /// 画面座標で持つと、自動スクロール中にカーソルを止めたときに
    /// 画面座標の差分が変わらず、ノートがカーソルから置き去りになる。
    /// グリッド原点はスクロールに追従して動くので、楽譜座標で持てば
    /// スクロールした分だけ「カーソル下の位置」が変化して自然に追従する。
    origin: Pos2,
}

/// 画面座標を楽譜座標 (x = 四分音符単位, y = 段) に変換する。
/// `ppq` と `row_h` はズームで変わるので呼び出し側から渡す。
fn to_content_pos(grid_origin: Pos2, screen: Pos2, ppq: f32, row_h: f32) -> Pos2 {
    Pos2::new(
        (screen.x - grid_origin.x) / ppq,
        (screen.y - grid_origin.y - RULER_H) / row_h,
    )
}

/// 楽譜座標を画面座標に戻す (to_content_pos の逆変換)
fn to_screen_pos(grid_origin: Pos2, content: Pos2, ppq: f32, row_h: f32) -> Pos2 {
    Pos2::new(
        grid_origin.x + content.x * ppq,
        grid_origin.y + RULER_H + content.y * row_h,
    )
}

/// エディタからメインスレッドへの指示
pub enum EditorCommand {
    /// シーケンスをオーディオスレッドに再送する
    Commit,
    Play,
    Stop,
    Seek { quarters: f64 },
    SetLoop(bool),
    /// MIDI ファイルを選んで読み込む
    ImportMidi,
    /// 保存先を選んで MIDI ファイルに書き出す
    ExportMidi,
    /// プロジェクトを開く (.ron)
    OpenProject,
    /// プロジェクトを保存する (保存先が未設定なら選ばせる)
    SaveProject,
    /// 保存先を選んでプロジェクトを保存する
    SaveProjectAs,
    /// シーケンス全体を音声ファイル (WAV) に書き出す
    ExportWav,
    /// シーケンス全体を Ogg/Opus に書き出す
    ExportOpus,
    /// 1トラック目を CeVIO のプロジェクトファイル (CCS) に書き出す
    ExportCcs,
    /// 指定トラックに音源 (.clap / .vst3) を読み込む
    LoadPlugin { track: usize },
}

/// エディタパネルを描画する。pos_quarters は現在の再生位置 (四分音符単位)。
pub fn editor_panel(
    ui: &mut egui::Ui,
    state: &mut EditorState,
    pos_quarters: f64,
    playing: bool,
) -> Vec<EditorCommand> {
    let mut commands = Vec::new();

    // 終端まで再生して自分で止まったときも、再生を始めた位置へ戻す
    if state.was_playing && !playing {
        if let Some(quarters) = state.play_return.take() {
            commands.push(EditorCommand::Seek { quarters });
            // 再生中は追従スクロールで画面が流れているので、位置と一緒に画面も戻す
            state.scroll_to_quarters = Some(quarters);
        }
    }
    state.was_playing = playing;

    toolbar(ui, state, playing, pos_quarters, &mut commands);
    ui.separator();

    // 左下のヘルプボタン用に1行分を残してグリッドを描く
    const HELP_ROW_H: f32 = 26.0;
    let grid_height = (ui.available_height() - HELP_ROW_H).max(80.0);

    ui.horizontal_top(|ui| {
        // 左のトラック欄。グリッドとは別のスクロール領域にして、
        // 横スクロールしても隠れないようにする (縦だけグリッドに追従させる)。
        ui.vertical(|ui| {
            // 見出しはスクロールの外に出す。中に入れるとトラックが増えたときに
            // 追加・削除ボタンまでスクロールできなくなる。
            // 高さをルーラーと同じにして、下の一覧がグリッドの段と揃うようにする。
            track_gutter_header(ui, state);

            egui::ScrollArea::vertical()
                .id_salt("track_gutter")
                .auto_shrink([false, false])
                .max_height((grid_height - RULER_H).max(state.row_h))
                .max_width(GUTTER_W)
                // 縦位置はグリッドから毎フレーム上書きされるので、
                // ここでドラッグしても一瞬ずれて戻るだけになる
                .drag_to_scroll(false)
                .scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::AlwaysHidden)
                .vertical_scroll_offset(state.grid_scroll_y)
                .show(ui, |ui| {
                    track_gutter(ui, state, &mut commands);
                });
        });

        let mut grid_area = egui::ScrollArea::both()
            .id_salt("grid")
            .auto_shrink([false, false])
            // 左ドラッグは範囲選択に使うので、スクロールには割り当てない。
            // スクロールは中ドラッグ・ホイール・スクロールバーで行う。
            .drag_to_scroll(false)
            .max_height(grid_height);
        // 横ズームの補正だけは、この場で位置を指定する。**ppq が変わるフレームと
        // 同じフレームで横位置も決まる**ので、両者がずれて画面が揺れることがない。
        // 指定するのはズームした次のフレームだけで、普段のスクロールには触らない。
        if let Some(x) = state.pending_scroll_x.take() {
            grid_area = grid_area.horizontal_scroll_offset(x);
        }
        let grid_scroll = grid_area
            .show(ui, |ui| {
                grid(ui, state, pos_quarters, playing, &mut commands);
            });
        // 次フレームで左欄を同じ位置に合わせる
        state.grid_scroll_y = grid_scroll.state.offset.y;
    });

    ui.horizontal(|ui| {
        if ui
            .button("? 操作ガイド")
            .on_hover_text("マウス・キーボード操作の一覧")
            .clicked()
        {
            state.show_help = !state.show_help;
        }

        ui.separator();

        // 保存 (.ron) と MIDI の入出力をまとめる。
        // 並べるとボタン列が長くなりすぎるため。
        ui.menu_button("ファイル", |ui| {
            if ui
                .button("開く…")
                .on_hover_text("プロジェクト (.ron) を読み込む (今の内容は置き換わります)")
                .clicked()
            {
                commands.push(EditorCommand::OpenProject);
                ui.close_menu();
            }
            if ui
                .button("保存 (Ctrl+S)")
                .on_hover_text("プロジェクトを保存する (保存先が未設定なら選びます)")
                .clicked()
            {
                commands.push(EditorCommand::SaveProject);
                ui.close_menu();
            }
            if ui
                .button("名前を付けて保存…")
                .on_hover_text("保存先を選んでプロジェクトを保存する")
                .clicked()
            {
                commands.push(EditorCommand::SaveProjectAs);
                ui.close_menu();
            }

            ui.separator();

            if ui
                .button("MIDI インポート")
                .on_hover_text("MIDI ファイルを読み込む (今のノートは置き換わります)")
                .clicked()
            {
                commands.push(EditorCommand::ImportMidi);
                ui.close_menu();
            }
            if ui
                .button("MIDI エクスポート")
                .on_hover_text(
                    "MIDI ファイルに書き出す (スウィングが乗ります。編集の保存には使わないこと)",
                )
                .clicked()
            {
                commands.push(EditorCommand::ExportMidi);
                ui.close_menu();
            }
        });

        // 書き出し形式は増える見込みなのでメニューにしておく
        ui.menu_button("出力", |ui| {
            if ui
                .button("WAV ファイルとして出力")
                .on_hover_text("シーケンス全体を音声ファイルに書き出す (16bit PCM)")
                .clicked()
            {
                commands.push(EditorCommand::ExportWav);
                ui.close_menu();
            }
            // Opus はビットレートを選んでから。48kHz へ切り替えて書き出すので、
            // WAV より時間がかかることも添える。
            ui.menu_button("Opus ファイルとして出力", |ui| {
                ui.weak("48kHz で書き出します (音源を一時的に切り替えます)");
                for kbps in crate::opus::BITRATES_KBPS {
                    let label = if kbps == crate::opus::DEFAULT_BITRATE_KBPS {
                        format!("{kbps} kbps (既定)")
                    } else {
                        format!("{kbps} kbps")
                    };
                    if ui.selectable_label(state.opus_bitrate_kbps == kbps, label).clicked() {
                        state.opus_bitrate_kbps = kbps;
                        commands.push(EditorCommand::ExportOpus);
                        ui.close_menu();
                    }
                }
            });
            if ui
                .button("CCS ファイルとして出力")
                .on_hover_text("CeVIO のプロジェクトファイルに書き出す (1トラック目のみ)")
                .clicked()
            {
                commands.push(EditorCommand::ExportCcs);
                ui.close_menu();
            }
        });

        if let Some(path) = &state.project_path {
            ui.weak(format!("保存先: {path}"));
        }
    });
    help_window(ui.ctx(), &mut state.show_help);
    lane_config_window(ui.ctx(), state);

    shortcuts(ui, state, pos_quarters, playing, &mut commands);

    // ドラッグ中でなければ変更をコミットする
    if state.dirty && state.drag.is_none() {
        commands.insert(0, EditorCommand::Commit);
        state.dirty = false;
    }

    // 次フレームの「変更前の状態」を控える
    state.history.end_frame(&state.editor);

    commands
}

/// 操作ガイドのウィンドウ。
/// ホストのウィンドウが小さいと内容が入りきらないので、縦スクロールを付けて
/// 高さを画面内に収める (見切れると下の項目が読めなくなるため)。
fn help_window(ctx: &egui::Context, open: &mut bool) {
    let screen = ctx.screen_rect();
    let max_height = (screen.height() - 72.0).max(160.0);

    egui::Window::new("操作ガイド")
        .open(open)
        .resizable(true)
        .vscroll(true)
        .default_pos([32.0, 32.0])
        .max_height(max_height)
        .show(ctx, |ui| {
            help_section(
                ui,
                "マウス",
                &[
                    ("ダブルクリック", "ノートを置く (直前のノートの設定を引き継ぐ)"),
                    ("ドラッグ", "移動 (縦に動かすと段が変わる)"),
                    ("右端ドラッグ", "音価を変更 (頭は固定)"),
                    ("左端ドラッグ", "音価を変更 (終端を固定。「左端音価」ON のとき)"),
                    (
                        "空白をドラッグ",
                        "範囲選択 (いちばん下の段より下から始めても可。Shift で選択に追加)",
                    ),
                    ("Shift+クリック", "選択に追加 / 選択から外す"),
                    ("右クリック", "削除 (選択中のノートなら選択ごと)"),
                    ("中ドラッグ", "スクロール"),
                    (
                        "Ctrl+中ドラッグ",
                        "横幅を拡大 / 縮小 (左へ動かすと拡大。掴んだ位置は動きません)",
                    ),
                    ("Alt+ホイール", "選択中ノートのベロシティを増減"),
                    ("Ctrl+ホイール", "段の縦幅を拡大 / 縮小 (カーソルの下の段は動きません)"),
                    ("段の帯 (トラック欄の右端)", "その段を全選択。続けて別の段を押すと入れ替え"),
                    ("ルーラーをクリック", "再生ヘッドを移動 (拍にスナップ / Alt で自由)"),
                ],
            );

            help_section(
                ui,
                "キーボード",
                &[
                    ("Space", "再生ヘッドから再生 / 停止 (停止すると開始位置へ戻る)"),
                    ("Shift+Space", "先頭から再生"),
                    ("Delete", "選択中のノートを削除"),
                    ("↑ / ↓", "選択中のノートを半音上げ / 下げ (上限でオクターブへ繰り上がる)"),
                    ("Shift+↑ / ↓", "選択中のノートをオクターブ上げ / 下げ"),
                    ("←", "選択の頭を、いちばん早いノートに揃える"),
                    ("→", "選択の尾を、いちばん遅い終端に揃える (音価を伸ばす)"),
                    ("Ctrl+C / X / V", "コピー / カット / 貼り付け (貼り付けは再生ヘッド位置)"),
                    ("Ctrl+B", "貼り付け (Ctrl+V と同じ。押しやすい方で)"),
                    ("Ctrl+Z / Ctrl+Y", "元に戻す / やり直し"),
                    ("Ctrl+S", "プロジェクトを保存 (保存先が未設定なら選ぶ)"),
                ],
            );

            help_section(
                ui,
                "複数選択中",
                &[
                    ("ドラッグ", "まとめて移動"),
                    ("端ドラッグ", "掴んだ端を全員同じだけ伸縮 (音価の相対関係は保つ)"),
                    ("Alt+ホイール", "全員のベロシティをまとめて増減"),
                ],
            );

            help_section(
                ui,
                "トラック欄 (左)",
                &[
                    ("M", "ミュート (このトラックを鳴らさない)"),
                    ("S", "ソロ (どれか1つでもソロなら、それ以外は鳴らない)"),
                    ("W", "スウィング (N/4 拍子のときだけ。深さはツールバーで設定)"),
                    ("♪", "音源 (.clap / .vst3) を読み込む / 差し替える"),
                    ("+ / −", "そのトラックの段を増減する (ノートのある段は消せません)"),
                    ("上の + / −", "トラックを増減する"),
                ],
            );

            help_section(
                ui,
                "ファイル (下のボタン)",
                &[
                    ("ファイル > 開く", "プロジェクト (.ron) を読み込む"),
                    ("ファイル > 保存", "プロジェクトを保存する。記譜位置と設定をそのまま残します"),
                    ("MIDI インポート", "MIDI ファイルを読み込む (今のノートは置き換わります)"),
                    (
                        "MIDI エクスポート",
                        "MIDI ファイルに書き出す。スウィングが乗るので、編集の保存には使わないこと",
                    ),
                    (
                        "出力 > WAV",
                        "音源を鳴らして音声ファイルに書き出す (音源のロードが必要。書き出し中は画面が固まります)",
                    ),
                    (
                        "出力 > CCS",
                        "CeVIO のプロジェクトに書き出す (1トラック目のみ。段ごとにパートを分けます)",
                    ),
                ],
            );

            help_section(
                ui,
                "表示",
                &[
                    ("ノートの色", "音高 (オクターブごとの色 + 半音でのグラデーション)"),
                    ("塗りの高さ", "ベロシティ (薄い部分は最大値までの残り)"),
                    (
                        "段幅",
                        "段の縦幅を変える (Ctrl+ホイールも同じ。段が少ないときに広げると掴みやすくなります)",
                    ),
                    ("連符", "スナップ幅2つ分を N 等分する (1/8 の5連符なら四分音符に5音)"),
                    (
                        "スウィング",
                        "跳ねの深さ (1.00 = 直線 / 2.00 = 三連)。記譜は8分の等分のままで、跳ねは再生・書き出しにだけ乗ります",
                    ),
                ],
            );
        });
}

/// 操作ガイドの1セクション (キー = 説明 の2列)
fn help_section(ui: &mut egui::Ui, title: &str, rows: &[(&str, &str)]) {
    ui.strong(title);
    egui::Grid::new(title)
        .num_columns(2)
        .spacing([16.0, 4.0])
        .show(ui, |ui| {
            for (key, description) in rows {
                ui.label(egui::RichText::new(*key).color(palette::YELLOW));
                ui.label(*description);
                ui.end_row();
            }
        });
    ui.add_space(10.0);
}

/// 再生を始める。`from` を渡すとその位置へ移動してから再生する。
/// 停止したときに戻れるよう、開始時点の再生ヘッド位置を覚えておく。
fn start_playback(
    state: &mut EditorState,
    pos_quarters: f64,
    playing: bool,
    from: Option<f64>,
    commands: &mut Vec<EditorCommand>,
) {
    // すでに再生中なら、最初に再生を始めた位置を保ったままにする。
    // 止まっているときは常に今の位置を覚え直す (プラグイン未ロードで
    // 再生が始まらなかったときなどに、古い位置が残らないように)。
    if !playing {
        state.play_return = Some(pos_quarters.max(0.0));
    }
    if let Some(quarters) = from {
        commands.push(EditorCommand::Seek { quarters });
    }
    commands.push(EditorCommand::Play);
}

/// 停止して、再生を始めた位置へ再生ヘッドを戻す
fn stop_playback(state: &mut EditorState, commands: &mut Vec<EditorCommand>) {
    commands.push(EditorCommand::Stop);
    if let Some(quarters) = state.play_return.take() {
        commands.push(EditorCommand::Seek { quarters });
        // 再生中は追従スクロールで画面が流れているので、位置と一緒に画面も戻す
        state.scroll_to_quarters = Some(quarters);
    }
}

/// このフレームに押されたショートカット
#[derive(Default)]
struct Shortcuts {
    copy: bool,
    cut: bool,
    /// 貼り付けが要求された (中身は控えから取るので、テキストは見ない)
    paste: bool,
    undo: bool,
    redo: bool,
    delete: bool,
    /// 頭を揃える (←)
    align_head: bool,
    /// 尾を揃える (→)
    align_tail: bool,
    /// 移調量 (半音単位。↑↓ で ±1、Shift+↑↓ で ±1オクターブ)
    transpose: i32,
    /// MIDI として保存する (Ctrl+S)
    save: bool,
}

/// キーボードショートカット (再生 / 移調 / コピー / ペースト / 削除 / アンドゥ)
fn shortcuts(
    ui: &mut egui::Ui,
    state: &mut EditorState,
    pos_quarters: f64,
    playing: bool,
    commands: &mut Vec<EditorCommand>,
) {
    use egui::{Key, Modifiers};

    // 数値入力欄にフォーカスがあるときは横取りしない。
    // ドラッグ中も無効にする (アンドゥでノートが消えると掴んでいる対象がなくなるため)。
    if ui.ctx().wants_keyboard_input() || state.drag.is_some() {
        return;
    }

    // Space: 再生ヘッドから再生 / 停止、Shift+Space: 先頭から再生。
    // ボタンなどにフォーカスがあるときは、そちらの操作キーなので横取りしない。
    //
    // 注意: consume_key は「パターンが要求する修飾キーが押されているか」しか見ない。
    // Modifiers::NONE は Shift 併用でも一致してしまうので、必ず修飾キー付きを
    // 先に消費すること (先に Space を消すと Shift+Space が届かなくなる)。
    if ui.ctx().memory(|m| m.focused()).is_none() {
        let (from_start, toggle) = ui.input_mut(|i| {
            let from_start = i.consume_key(Modifiers::SHIFT, Key::Space);
            let toggle = i.consume_key(Modifiers::NONE, Key::Space);
            (from_start, toggle)
        });
        if from_start {
            if playing {
                commands.push(EditorCommand::Stop);
            }
            start_playback(state, pos_quarters, playing, Some(0.0), commands);
        } else if toggle {
            if playing {
                stop_playback(state, commands);
            } else {
                start_playback(state, pos_quarters, playing, None, commands);
            }
        }
    }

    // Ctrl+C / X / V は egui-winit がキーイベントではなく Copy/Cut/Paste イベントに
    // 変換する (キーとしては届かない) ため、イベントとして拾う。
    //
    // **ノートの受け渡しに OS のクリップボードは使わない** (`note_clipboard`)。
    // Copy と Cut のイベントは無条件で飛ぶので取りこぼしはないが、Paste だけは
    // クリップボードに文字列があるときにしか作られない。そのため Ctrl+B も
    // 貼り付けに割り当ててある (下記)。
    let per_octave = state.editor.scale.steps_per_octave();
    let keys = ui.input_mut(|i| {
        let mut keys = Shortcuts::default();
        i.events.retain(|event| match event {
            egui::Event::Copy => {
                keys.copy = true;
                false
            }
            egui::Event::Cut => {
                keys.cut = true;
                false
            }
            // 中身は見ない。何が入っていても「貼り付けが押された」合図として扱い、
            // 実際に貼るのは自前の控え (`note_clipboard`)。
            egui::Event::Paste(_) => {
                keys.paste = true;
                false
            }
            _ => true,
        });

        // 修飾キーの多い組み合わせから先に消費する (Modifiers::NONE や COMMAND は
        // Shift 併用でも一致するため、Ctrl+Z を先に消すと Ctrl+Shift+Z が届かない)
        keys.redo = i.consume_key(Modifiers::COMMAND | Modifiers::SHIFT, Key::Z)
            || i.consume_key(Modifiers::COMMAND, Key::Y);
        keys.undo = i.consume_key(Modifiers::COMMAND, Key::Z);

        // Shift+↑↓ はオクターブ、修飾なしの ↑↓ は半音
        if i.consume_key(Modifiers::SHIFT, Key::ArrowUp) {
            keys.transpose += per_octave;
        }
        if i.consume_key(Modifiers::SHIFT, Key::ArrowDown) {
            keys.transpose -= per_octave;
        }
        if i.consume_key(Modifiers::NONE, Key::ArrowUp) {
            keys.transpose += 1;
        }
        if i.consume_key(Modifiers::NONE, Key::ArrowDown) {
            keys.transpose -= 1;
        }

        // Ctrl+V の代わりにも使える口。egui-winit は Ctrl+V をキーとして通さず、
        // クリップボードにテキストがあるときしか `Event::Paste` を作らないので、
        // **横取りされないこの組み合わせは常に届く**。
        // (コピー時に空のクリップボードへ印を置いているので Ctrl+V も効くが、
        //  クリップボードの状態に依存しない道を1本残しておく)
        keys.paste |= i.consume_key(Modifiers::COMMAND, Key::B);

        keys.save = i.consume_key(Modifiers::COMMAND, Key::S);
        keys.delete = i.consume_key(Modifiers::NONE, Key::Delete);
        keys.align_head = i.consume_key(Modifiers::NONE, Key::ArrowLeft);
        keys.align_tail = i.consume_key(Modifiers::NONE, Key::ArrowRight);
        keys
    });
    let Shortcuts {
        copy,
        cut,
        paste,
        undo,
        redo,
        delete,
        align_head,
        align_tail,
        transpose,
        save,
    } = keys;

    if save {
        commands.push(EditorCommand::SaveProject);
    }

    // ↑↓: 選択中のノートを移調する (半音の上限を超えるとオクターブへ繰り上がる)
    if transpose != 0 && state.transpose_selection(transpose) {
        state.history.record(EditGroup::Semitone);
        state.dirty = true;
    }

    // 左キー: 頭を最も開始が早いノートに揃える (ノートごと移動)。
    // 右キー: 尾を最も終了が遅いノートに揃える (開始位置はそのまま音価を伸ばす)。
    // 変更が起きたときだけ履歴に積む (履歴は「フレーム開始時点」を積むので順序は問わない)。
    if align_head && state.align_selection_starts() {
        state.history.record(EditGroup::Once);
        state.dirty = true;
    }
    if align_tail && state.align_selection_ends() {
        state.history.record(EditGroup::Once);
        state.dirty = true;
    }

    // ノートの中身は控えに取るだけで、OS のクリップボードには書かない。
    //
    // **ただし空のときだけ印を書く。** egui-winit は Ctrl+V をキーとして通さず、
    // `Event::Paste` を作るのは**クリップボードにテキストがあるときだけ**なので、
    // 空のままだと Ctrl+V が何も起こさない (`clipboard_has_text` を参照)。
    // 空のときに限れば、書いてもユーザーの持ち物は壊さない。
    if copy || cut {
        if let Some(notes) = state.copy_selection() {
            state.note_clipboard = notes;
            if !clipboard_has_text() {
                ui.ctx().copy_text(CLIPBOARD_MARKER.to_string());
            }
        }
    }

    if cut && !state.selection.is_empty() {
        state.history.record(EditGroup::Once);
        state.delete_selection();
        state.dirty = true;
    }

    // 貼り付け位置は再生ヘッド。控えが空なら何もしない。
    if paste && !state.note_clipboard.is_empty() {
        let notes = std::mem::take(&mut state.note_clipboard);
        state.history.record(EditGroup::Once);
        state.paste_notes(&notes, pos_quarters.max(0.0) as f32);
        state.note_clipboard = notes; // 何度でも貼れるように戻す
        state.dirty = true;
    }

    if delete && !state.selection.is_empty() {
        state.history.record(EditGroup::Once);
        state.delete_selection();
        state.dirty = true;
    }

    // アンドゥ/リドゥでノートの並びが変わるので選択は解除する
    if undo && state.history.undo(&mut state.editor) {
        state.clear_selection();
        state.dirty = true;
    }
    if redo && state.history.redo(&mut state.editor) {
        state.clear_selection();
        state.dirty = true;
    }
}

/// 上部ツールバー
fn toolbar(
    ui: &mut egui::Ui,
    state: &mut EditorState,
    playing: bool,
    pos_quarters: f64,
    commands: &mut Vec<EditorCommand>,
) {
    // 項目が増えて 1100px に収まらなくなったので折り返す。
    // 収まらないぶんが画面外へ消えると、そこだけ操作できなくなるため。
    ui.horizontal_wrapped(|ui| {
        if playing {
            if ui
                .button("⏹ 停止")
                .on_hover_text("停止して再生開始位置へ戻す (Space)")
                .clicked()
            {
                stop_playback(state, commands);
            }
        } else if ui
            .button("▶ 再生")
            .on_hover_text("再生ヘッドから再生 (Space) / 先頭から再生 (Shift+Space)")
            .clicked()
        {
            start_playback(state, pos_quarters, playing, None, commands);
        }

        if ui.checkbox(&mut state.looping, "ループ").changed() {
            commands.push(EditorCommand::SetLoop(state.looping));
        }

        ui.separator();

        ui.label("テンポ");
        if ui
            .add(egui::DragValue::new(&mut state.editor.tempo).range(20..=300))
            .changed()
        {
            state.dirty = true;
        }

        ui.label("拍子");
        if ui
            .add(egui::DragValue::new(&mut state.editor.beats).range(1..=16))
            .changed()
        {
            state.dirty = true;
        }
        ui.label("/");
        egui::ComboBox::from_id_salt("beat_type")
            .selected_text(state.editor.beat_type.to_string())
            .width(48.0)
            .show_ui(ui, |ui| {
                for bt in [1u32, 2, 4, 8, 16, 32] {
                    if ui
                        .selectable_value(&mut state.editor.beat_type, bt, bt.to_string())
                        .changed()
                    {
                        state.dirty = true;
                    }
                }
            });

        ui.separator();

        // 音階モード。切り替え時に範囲外になる半音を丸める。
        ui.label("音階");
        let mut scale = state.editor.scale;
        egui::ComboBox::from_id_salt("scale")
            .selected_text(scale.label())
            .width(96.0)
            .show_ui(ui, |ui| {
                for mode in [ScaleMode::Equal12, ScaleMode::BohlenPierce13] {
                    ui.selectable_value(&mut scale, mode, mode.label());
                }
            });
        if scale != state.editor.scale {
            state.history.record(EditGroup::Once);
            state.editor.scale = scale;
            let max = scale.max_semitone();
            for note in &mut state.editor.notes {
                note.semitone = note.semitone.min(max);
            }
            state.last_note.semitone = state.last_note.semitone.min(max);
            state.dirty = true;
        }

        ui.separator();

        ui.label("スナップ");
        egui::ComboBox::from_id_salt("snap")
            .selected_text(snap_label(state.snap))
            .width(56.0)
            .show_ui(ui, |ui| {
                for (label, value) in [
                    ("1/1", 4.0),
                    ("1/2", 2.0),
                    ("1/4", 1.0),
                    ("1/8", 0.5),
                    ("1/16", 0.25),
                    ("1/32", 0.125),
                ] {
                    ui.selectable_value(&mut state.snap, value, label);
                }
            });

        // 連符モード: スナップ幅と新規ノートの音価が連符1音分になる
        ui.label("連符");
        egui::ComboBox::from_id_salt("tuplet")
            .selected_text(tuplet_label(state.tuplet))
            .width(64.0)
            .show_ui(ui, |ui| {
                for n in [1u32, 3, 4, 5, 6, 7] {
                    ui.selectable_value(&mut state.tuplet, n, tuplet_label(n));
                }
            })
            .response
            .on_hover_text("スナップ幅2つ分を N 等分します (例: スナップ 1/8 の5連符なら四分音符1つに5音)");

        ui.checkbox(&mut state.left_resize, "左端音価")
            .on_hover_text("右端に加えて、ノートの左端でも音価を変更できるようにする (左端は終端を固定して頭を動かす)");

        ui.separator();

        // 段の縦幅 (縦ズーム)。段が少ないときに広げて掴みやすくするためのもの
        ui.label("段幅");
        let mut row_h = state.row_h;
        if ui
            .add(
                egui::DragValue::new(&mut row_h)
                    .range(MIN_ROW_H..=MAX_ROW_H)
                    .speed(0.5)
                    .fixed_decimals(0)
                    .suffix("px"),
            )
            .on_hover_text("段の縦幅 (グリッドの上で Ctrl+ホイールでも変えられます)")
            .changed()
        {
            state.set_row_h(row_h);
        }
        // 横ズームはドラッグでしか変えられないので、戻す口をここに置く
        // (ここが無いと、行きすぎたときに手で戻すしかない)
        if ui
            .small_button("既定")
            .on_hover_text(format!(
                "拡大率を既定に戻す (段幅 {ROW_H:.0}px / 四分音符 {PPQ:.0}px)"
            ))
            .clicked()
        {
            state.set_row_h(ROW_H);
            state.set_ppq(PPQ);
        }

        ui.separator();

        // アンドゥ / リドゥ (Ctrl+Z / Ctrl+Y)
        if ui
            .add_enabled(state.history.can_undo(), egui::Button::new("↶"))
            .on_hover_text("元に戻す (Ctrl+Z)")
            .clicked()
            && state.history.undo(&mut state.editor)
        {
            state.clear_selection();
            state.dirty = true;
        }
        if ui
            .add_enabled(state.history.can_redo(), egui::Button::new("↷"))
            .on_hover_text("やり直し (Ctrl+Y)")
            .clicked()
            && state.history.redo(&mut state.editor)
        {
            state.clear_selection();
            state.dirty = true;
        }

        ui.separator();

        // スウィングの深さ (200BPM 時の裏拍の比)。掛けるトラックはトラック欄の「W」で選ぶ。
        // N/4 拍子でしか効かないので、それ以外では触らせない。
        //
        // ラベルとスライダーはこの折り返しレイアウトの「直接の子」にしておくこと。
        // add_enabled_ui や scope で包むと入れ子の Ui になり、折り返しに参加せず
        // 右端からはみ出して見えなくなる。
        let usable = swing::applies_to(state.editor.beat_type);
        let hint = if usable {
            "跳ねの深さ (1.00 = 直線 / 2.00 = 三連)。掛けるトラックは左の欄の「W」で選びます"
        } else {
            "スウィングは N/4 拍子のときだけ使えます"
        };
        // 折り返しレイアウトなので、そのままだと文字の途中で改行される
        // (「ス」と「ウィング」に割れる)。1語として次の行へ送る。
        ui.add_enabled(
            usable,
            egui::Label::new("スウィング").wrap_mode(egui::TextWrapMode::Extend),
        );

        // 既定の 100px はツールバーには広すぎる。元に戻してから抜ける
        let slider_width = ui.spacing().slider_width;
        ui.spacing_mut().slider_width = 84.0;
        let mut peak = state.editor.swing_peak_ratio;
        let changed = ui
            .add_enabled(
                usable,
                egui::Slider::new(&mut peak, swing::MIN_PEAK_RATIO..=swing::MAX_PEAK_RATIO)
                    .fixed_decimals(2),
            )
            .on_hover_text(hint)
            .changed();
        ui.spacing_mut().slider_width = slider_width;

        if changed {
            state.editor.swing_peak_ratio = peak;
            // 鳴り方が変わるのでシーケンスを送り直す (編集ではないので履歴には積まない)
            state.dirty = true;
        }
    });

    // 選択中ノートの詳細
    let max_semitone = state.editor.scale.max_semitone();
    ui.horizontal(|ui| {
        if let Some(idx) = state.selected {
            if let Some(note) = state.editor.notes.get_mut(idx) {
				/* どうやってもUIがズレるので削除しました。→後続のラベルがあるため基本不要
                // 表示名は桁数で幅が変わる ((9,4) → (10,4)) ため、
                // 固定幅の領域に左寄せで置いて後続のウィジェットを動かさない
                let name = format!("選択中: {}", note.name());
                ui.allocate_ui_with_layout(
                    vec2(120.0, 18.0),
                    egui::Layout::left_to_right(egui::Align::Center),
                    |ui| {
                        ui.label(name);
                    },
                );
				*/

                // 2桁分の幅を確保して、9→10 でレイアウトが動かないようにする
                ui.label("半音");
                let resp = ui.add_sized(
                    [44.0, 18.0],
                    egui::DragValue::new(&mut note.semitone).range(0..=max_semitone),
                );
                if resp.changed() {
                    state.history.record(EditGroup::Semitone);
                    state.dirty = true;
                }
                if resp.drag_stopped() || resp.lost_focus() {
                    state.history.end_group();
                }

                ui.label("オクターブ");
                let resp = ui.add_sized(
                    [44.0, 18.0],
                    egui::DragValue::new(&mut note.octave).range(MIN_OCTAVE..=MAX_OCTAVE),
                );
                if resp.changed() {
                    state.history.record(EditGroup::Octave);
                    state.dirty = true;
                }
                if resp.drag_stopped() || resp.lost_focus() {
                    state.history.end_group();
                }

                ui.label("ベロシティ");
                let resp = ui.add(egui::Slider::new(&mut note.velocity, 1..=127));
                if resp.changed() {
                    state.history.record(EditGroup::Velocity);
                    state.dirty = true;
                }
                if resp.drag_stopped() || resp.lost_focus() {
                    state.history.end_group();
                }

                if ui.button("削除").clicked() {
                    state.history.record(EditGroup::Once);
                    state.delete_selection();
                    state.dirty = true;
                }
            } else {
                state.clear_selection();
            }
        } else if state.selection.len() > 1 {
            ui.label(format!("{} 個選択中", state.selection.len()));
            if ui.button("まとめて削除").clicked() {
                state.history.record(EditGroup::Once);
                state.delete_selection();
                state.dirty = true;
            }
            ui.weak("Alt+ホイール: ベロシティ / ←→: 頭・尾を揃える");
        } else {
            ui.weak("操作の一覧は左下の「操作ガイド」から");
        }
    });
}

fn tuplet_label(tuplet: u32) -> &'static str {
    match tuplet {
        3 => "3連符",
        4 => "4連符",
        5 => "5連符",
        6 => "6連符",
        7 => "7連符",
        _ => "オフ",
    }
}

fn snap_label(snap: f32) -> &'static str {
    match snap {
        s if s >= 4.0 => "1/1",
        s if s >= 2.0 => "1/2",
        s if s >= 1.0 => "1/4",
        s if s >= 0.5 => "1/8",
        s if s >= 0.25 => "1/16",
        _ => "1/32",
    }
}

/// ホイールの回転量 (ノッチ数) を取り出す。`wanted` が真のときだけ横取りする。
///
/// ScrollArea は内容を描き終えたあとに smooth_scroll_delta を読むので、
/// ここでイベントごと消しておけば、割り当てた操作とスクロールが同時に起きない。
fn take_wheel_notches(ui: &mut egui::Ui, wanted: impl Fn(&egui::Modifiers) -> bool) -> f32 {
    ui.input_mut(|i| {
        if !wanted(&i.modifiers) {
            return 0.0;
        }
        let mut notches = 0.0;
        i.events.retain(|event| match event {
            egui::Event::MouseWheel { unit, delta, .. } => {
                notches += match unit {
                    egui::MouseWheelUnit::Line => delta.y,
                    egui::MouseWheelUnit::Point => delta.y / 50.0,
                    egui::MouseWheelUnit::Page => delta.y * 3.0,
                };
                false
            }
            _ => true,
        });
        i.smooth_scroll_delta = egui::Vec2::ZERO;
        i.raw_scroll_delta = egui::Vec2::ZERO;
        notches
    })
}

/// グリッド本体 (ルーラー + ノートレーン)
fn grid(
    ui: &mut egui::Ui,
    state: &mut EditorState,
    pos_quarters: f64,
    playing: bool,
    commands: &mut Vec<EditorCommand>,
) {
    // ---- Alt+ホイールで選択ノートのベロシティを変更 ----
    let notches = take_wheel_notches(ui, |m| m.alt);
    if notches != 0.0 {
        let delta = (notches * VELOCITY_WHEEL_STEP as f32).round() as i32;
        if delta != 0 && state.change_selection_velocity(delta) {
            state.history.record(EditGroup::Velocity);
            state.dirty = true;
        }
    }

    // ---- Ctrl+ホイールで段の縦幅をズーム ----
    // ホイールは画面のどこで回しても届くので、グリッドの見えている範囲
    // (ScrollArea のビューポート = クリップ矩形) にカーソルがあるときだけ拾う。
    let pointer_in_grid = ui
        .input(|i| i.pointer.hover_pos())
        .is_some_and(|pos| ui.clip_rect().contains(pos));
    // Alt が優先 (上で消費済み)。両方押されていてもズームはしない
    let zoom_notches = if pointer_in_grid {
        take_wheel_notches(ui, |m| m.command && !m.alt)
    } else {
        0.0
    };

    // 段の高さと四分音符の横幅は、このフレームの間ずっと同じ値を使う
    // (ズームの反映は次のフレーム)
    let row_h = state.row_h;
    let ppq = state.ppq;

    // 表示する行数 = 全トラックの段の合計。
    // (フェーズ1ではトラックは1本なので、そのトラックの段数と同じ)
    let display_rows = state.editor.total_rows().max(1);
    // 各トラックが何行目から始まるか (ノートの y 位置に使う)
    let row_offsets = track_row_offsets(&state.editor);

    let qpb = state.editor.quarters_per_bar();
    // 表示範囲: ノートの終端を小節に切り上げ + 余白2小節 (最低8小節)
    let total_quarters = (state.editor.length_quarters_bar_aligned() + qpb * 2.0).max(qpb * 8.0);

    // 中身を描く高さ (ルーラー + 全段)
    let content_h = RULER_H + display_rows as f32 * row_h;
    // 確保する高さは見えている範囲まで広げる。
    //
    // 段の下に隙間を残すと、そこは ScrollArea のものになってドラッグが
    // スクロールに化けてしまい、段の外から範囲選択を始められない。
    // 広げるのは当たり判定だけで、描画はすべて content_h までに留める
    // (空白に段があるように見せないため)。
    let size = vec2(total_quarters * ppq, content_h.max(ui.clip_rect().height()));
    let (response, painter) = ui.allocate_painter(size, Sense::click_and_drag());
    let origin = response.rect.min;

    let to_x = |quarters: f32| origin.x + quarters * ppq;

    // ---- 背景 ----
    painter.rect_filled(
        Rect::from_min_size(origin, vec2(size.x, RULER_H)),
        CornerRadius::ZERO,
        palette::BG_LIGHT,
    );
    painter.rect_filled(
        Rect::from_min_size(origin + vec2(0.0, RULER_H), vec2(size.x, content_h - RULER_H)),
        CornerRadius::ZERO,
        palette::BG_DARK,
    );

    // ---- トラックの地色 ----
    // 1つおきに薄く敷いて、どこからどこまでが1トラックか分かるようにする
    for (track, offset) in row_offsets.iter().enumerate() {
        if track % 2 == 1 {
            let top = origin.y + RULER_H + *offset as f32 * row_h;
            let height = state.editor.lanes(track) as f32 * row_h;
            painter.rect_filled(
                Rect::from_min_size(Pos2::new(origin.x, top), vec2(size.x, height)),
                CornerRadius::ZERO,
                palette::BG.gamma_multiply(0.5),
            );
        }
    }

    // ---- CC 段の地色 ----
    // 音符段と見分けが付かないと、音高のつもりで置いた音が CC になってしまう。
    // トラックの地色より上に敷いて、どちらのトラックでも同じ色に見えるようにする。
    for (track, offset) in row_offsets.iter().enumerate() {
        for lane in 0..state.editor.lanes(track) {
            if state.editor.lane_cc(track, lane).is_none() {
                continue;
            }
            let top = origin.y + RULER_H + (*offset + lane) as f32 * row_h;
            painter.rect_filled(
                Rect::from_min_size(Pos2::new(origin.x, top), vec2(size.x, row_h)),
                CornerRadius::ZERO,
                palette::GREEN.gamma_multiply(0.14),
            );
        }
    }

    // ---- 段の区切り線 ----
    let lane_stroke = Stroke::new(0.5, palette::BG_SELECT.gamma_multiply(0.7));
    for row in 0..=display_rows {
        let y = origin.y + RULER_H + row as f32 * row_h;
        painter.line_segment(
            [Pos2::new(origin.x, y), Pos2::new(origin.x + size.x, y)],
            lane_stroke,
        );
    }

    // ---- トラックの区切り線 (段の線より目立たせる) ----
    let track_stroke = Stroke::new(1.5, palette::FG_DIM);
    for offset in row_offsets.iter().skip(1) {
        let y = origin.y + RULER_H + *offset as f32 * row_h;
        painter.line_segment(
            [Pos2::new(origin.x, y), Pos2::new(origin.x + size.x, y)],
            track_stroke,
        );
    }

    // ---- 連符の補助線 ----
    // 拍線・小節線より先に描いて、重なる位置は上から塗り潰させる
    let unit = state.snap_unit();
    if state.tuplet > 1 && unit * ppq >= 6.0 {
        let tuplet_stroke = Stroke::new(0.5, palette::PURPLE.gamma_multiply(0.45));
        let mut q = unit;
        while q <= total_quarters {
            let x = to_x(q);
            painter.line_segment(
                [
                    Pos2::new(x, origin.y + RULER_H),
                    Pos2::new(x, origin.y + content_h),
                ],
                tuplet_stroke,
            );
            q += unit;
        }
    }

    // ---- 拍線・小節線 ----
    let qpbeat = state.editor.quarters_per_beat();
    let beat_stroke = Stroke::new(1.0, palette::BG_SELECT);
    let bar_stroke = Stroke::new(1.5, palette::FG_DIM);

    let mut q = 0.0f32;
    let mut beat_index = 0u32;
    let mut bar_number = 1u32;
    while q <= total_quarters {
        let x = to_x(q);
        let is_bar = beat_index % state.editor.beats.max(1) == 0;
        let stroke = if is_bar { bar_stroke } else { beat_stroke };
        let top = if is_bar { origin.y } else { origin.y + RULER_H };
        painter.line_segment(
            [Pos2::new(x, top), Pos2::new(x, origin.y + content_h)],
            stroke,
        );
        if is_bar {
            painter.text(
                Pos2::new(x + 4.0, origin.y + RULER_H * 0.5),
                Align2::LEFT_CENTER,
                bar_number.to_string(),
                FontId::proportional(11.0),
                palette::FG_DIM,
            );
            bar_number += 1;
        }
        q += qpbeat;
        beat_index += 1;
    }

    // ---- ノート ----
    for (idx, note) in state.editor.notes.iter().enumerate() {
        let rect = note_rect(origin, note_row(&row_offsets, note), note, ppq, row_h);
        // CC ブロックは音高で色を変えても意味が無い。音符と取り違えないよう、
        // 段の地色と同じ緑で塗り分ける (ベロシティの塗り高さはそのまま使える)。
        let fill = match state.editor.lane_cc(note.track, note.lane) {
            Some(_) => palette::GREEN,
            None => note_fill(note, state.editor.scale),
        };
        // ベロシティは「下からの塗りの高さ」で表す。明度やアルファを直接下げると
        // ダークな背景で弱いノートが見えなくなるため、色相はそのままに
        // 満たない部分をゴーストとして残す (輪郭と音高の色は常に見える)。
        painter.rect_filled(
            rect,
            CornerRadius::same(4),
            fill.gamma_multiply(VELOCITY_GHOST_ALPHA),
        );
        let level = velocity_fill_rect(rect, note.velocity);
        painter.rect_filled(
            level,
            if level.height() >= rect.height() - 0.5 {
                CornerRadius::same(4)
            } else {
                // 上辺はゴーストとの境目なので角を丸めない
                CornerRadius {
                    nw: 0,
                    ne: 0,
                    sw: 4,
                    se: 4,
                }
            },
            fill,
        );

        if state.is_selected(idx) {
            painter.rect_stroke(
                rect,
                CornerRadius::same(4),
                Stroke::new(2.0, palette::FG),
                egui::StrokeKind::Outside,
            );
        }

        // 縮めたときは文字がはみ出して読めないので、入る高さのときだけ書く
        if rect.width() > 26.0 && rect.height() >= NOTE_LABEL_SIZE + 2.0 {
            // 文字の位置まで実塗りが来ていれば背景色で抜き、
            // ゴーストの上に載るときは前景色にして読めるようにする
            let color = if level.top() <= rect.center().y {
                palette::BG
            } else {
                palette::FG
            };
            // CC 段では音名に意味が無いので、送る値のほうを出す
            let label = match state.editor.lane_cc(note.track, note.lane) {
                Some(number) => format!("CC{number}={}", note.velocity.min(127)),
                None => note.name(),
            };
            painter.text(
                rect.left_center() + vec2(4.0, 0.0),
                Align2::LEFT_CENTER,
                label,
                FontId::proportional(NOTE_LABEL_SIZE),
                color,
            );
        }
    }

    // ---- 再生線 ----
    let playhead_x = to_x(pos_quarters as f32);
    painter.line_segment(
        [
            Pos2::new(playhead_x, origin.y),
            Pos2::new(playhead_x, origin.y + content_h),
        ],
        Stroke::new(2.0, palette::RED),
    );

    // 再生中は再生線が見えるように追従スクロール
    if playing {
        let visible = ui.clip_rect();
        if playhead_x > visible.right() - 40.0 || playhead_x < visible.left() {
            ui.scroll_to_rect(
                Rect::from_min_size(Pos2::new(playhead_x, origin.y), vec2(ppq * 4.0, 1.0)),
                Some(egui::Align::Min),
            );
        }
    }

    // 停止したときは、戻った再生ヘッドを画面に入れる。
    //
    // 再生中は追従スクロールで画面が先へ流れているため、そのままだと戻った
    // 再生ヘッドが画面外に残る。**見えているときは動かさない**ので、
    // 少し再生して止めただけのときに画面が跳ねることはない。
    if let Some(quarters) = state.scroll_to_quarters.take() {
        let target_x = to_x(quarters as f32);
        let visible = ui.clip_rect();
        if target_x < visible.left() || target_x > visible.right() - 40.0 {
            ui.scroll_to_rect(
                Rect::from_min_size(Pos2::new(target_x, origin.y), vec2(ppq * 4.0, 1.0)),
                Some(egui::Align::Center),
            );
        }
    }

    // ---- 操作 ----
    let snap = state.snap_unit().max(0.03125);
    let left_resize = state.left_resize;
    let snap_floor = |q: f32| (q / snap).floor() * snap;

    // シーク位置の算出: 拍の間隔にスナップ (Alt 押下中は自由)。
    // 選択中ノートの頭が近ければそちらを優先する。
    let alt_held = ui.input(|i| i.modifiers.alt);
    let beat = state.editor.quarters_per_beat();
    let selected_start = state
        .selected
        .and_then(|i| state.editor.notes.get(i))
        .map(|n| n.start_tick.max(0.0));
    let seek_quarters = move |x: f32| -> f64 {
        let raw = ((x - origin.x) / ppq).max(0.0);
        seek_target(raw, beat, selected_start, alt_held)
    };

    // ---- 中クリックドラッグでスクロール / Ctrl 併用で横ズーム ----
    // egui のドラッグ判定はボタンを問わない (any_down) ため、
    // ここで掴んでいる間はノート編集のドラッグ処理を止める必要がある。
    let (middle_down, pointer_delta, zoom_modifier) = ui.input(|i| {
        (
            i.pointer.middle_down(),
            i.pointer.delta(),
            i.modifiers.command,
        )
    });
    if middle_down {
        if state.middle_drag.is_none() && response.contains_pointer() {
            // 用途は押した瞬間に決める (ドラッグ中に Ctrl を足しても変わらない)
            state.middle_drag = Some(if zoom_modifier {
                let anchor_quarters = ui
                    .input(|i| i.pointer.hover_pos())
                    .map_or(0.0, |pos| ((pos.x - origin.x) / ppq).max(0.0));
                MiddleDrag::ZoomHorizontally { anchor_quarters }
            } else {
                MiddleDrag::Pan
            });
            state.drag = None; // ノート編集とは排他
        }
    } else {
        state.middle_drag = None;
    }

    // 横ズームの量 (ピクセル)。実際の反映は描画のあと
    let mut zoom_pixels = 0.0;
    match state.middle_drag {
        Some(MiddleDrag::Pan) => {
            // 掴んだ位置がカーソルに追従するよう、アニメーションなしで即時スクロールする
            ui.scroll_with_delta_animation(pointer_delta, egui::style::ScrollAnimation::none());
            ui.output_mut(|o| o.cursor_icon = CursorIcon::Grabbing);
        }
        Some(MiddleDrag::ZoomHorizontally { .. }) => {
            zoom_pixels = pointer_delta.x;
            ui.output_mut(|o| o.cursor_icon = CursorIcon::ResizeHorizontal);
        }
        None => {}
    }

    // カーソル形状のフィードバック
    if state.drag.is_none() && state.middle_drag.is_none() {
        if let Some(hover) = response.hover_pos() {
            match hit_note(&state.editor.notes, &row_offsets, origin, hover, left_resize, ppq, row_h) {
                Some((_, Hit::ResizeRight | Hit::ResizeLeft)) => {
                    ui.output_mut(|o| o.cursor_icon = CursorIcon::ResizeHorizontal)
                }
                Some((_, Hit::Body)) => ui.output_mut(|o| o.cursor_icon = CursorIcon::Grab),
                None => {}
            }
        }
    }

    // ドラッグ開始。
    // 注意: interact_pointer_pos はドラッグ判定が成立した時点の位置なので、
    // ノート右端のような狭い領域のヒットテストには「押下位置」を使う。
    if response.drag_started() && state.middle_drag.is_none() {
        let press_pos = ui
            .input(|i| i.pointer.press_origin())
            .or_else(|| response.interact_pointer_pos());
        if let Some(pos) = press_pos {
            if pos.y < origin.y + RULER_H {
                state.drag = Some(DragState {
                    kind: DragKind::Seek,
                    targets: Vec::new(),
                    base_selection: Vec::new(),
                    origin: to_content_pos(origin, pos, ppq, row_h),
                });
            } else if let Some((idx, hit)) =
                hit_note(&state.editor.notes, &row_offsets, origin, pos, left_resize, ppq, row_h)
            {
                // 複数選択中のノートを掴んだら選択全体を操作する (選択は変えない)
                let bulk = state.selection.len() > 1 && state.is_selected(idx);
                let targets = if bulk {
                    // 掴んだノートを先頭に置く (移動量・スナップの基準になる)
                    let mut targets = vec![(idx, state.editor.notes[idx])];
                    targets.extend(
                        state
                            .selection_sorted()
                            .iter()
                            .filter(|i| **i != idx)
                            .map(|i| (*i, state.editor.notes[*i])),
                    );
                    targets
                } else {
                    state.select_single(idx);
                    vec![(idx, state.editor.notes[idx])]
                };
                state.drag = Some(DragState {
                    kind: match hit {
                        Hit::Body => DragKind::Move,
                        Hit::ResizeRight => DragKind::Resize { from_left: false },
                        Hit::ResizeLeft => DragKind::Resize { from_left: true },
                    },
                    targets,
                    base_selection: Vec::new(),
                    origin: to_content_pos(origin, pos, ppq, row_h),
                });
            } else {
                // 空白からのドラッグは範囲選択。Shift 押下なら既存の選択に足す。
                let base_selection = if ui.input(|i| i.modifiers.shift) {
                    state.selection_sorted()
                } else {
                    Vec::new()
                };
                state.drag = Some(DragState {
                    kind: DragKind::Marquee,
                    targets: Vec::new(),
                    base_selection,
                    origin: to_content_pos(origin, pos, ppq, row_h),
                });
            }
        }
    }

    // ドラッグ中
    if response.dragged() && state.middle_drag.is_none() {
        // 範囲選択の結果は借用が終わってから反映する
        let mut marquee_selection = None;

        if let (Some(drag), Some(pos)) = (&state.drag, response.interact_pointer_pos()) {
            // 掴んだ位置も現在位置も楽譜座標で扱う (自動スクロール中の追従のため)
            let content = to_content_pos(origin, pos, ppq, row_h);

            match drag.kind {
                DragKind::Seek => {
                    commands.push(EditorCommand::Seek {
                        quarters: seek_quarters(pos.x),
                    });
                }
                DragKind::Move => {
                    // 縦移動は「画面の行」で扱う。行はトラックをまたいで通しなので、
                    // 動かした先のトラックと段に変換して書き戻す。
                    let (dx, d_row) = move_delta(
                        &drag.targets,
                        &row_offsets,
                        &state.editor,
                        content.x - drag.origin.x,
                        (content.y - drag.origin.y).round() as i32,
                        snap,
                        display_rows,
                    );

                    for (idx, orig) in &drag.targets {
                        let mut note = *orig;
                        note.start_tick = (orig.start_tick + dx).max(0.0);
                        let row = (note_row(&row_offsets, orig) as i32 + d_row).max(0) as usize;
                        let (track, lane) = row_to_track_lane(&row_offsets, &state.editor, row);
                        note.track = track;
                        note.lane = lane;
                        state.editor.notes[*idx] = note;
                    }
                    if dx != 0.0 || d_row != 0 {
                        state.history.record(EditGroup::Move);
                    }
                }
                DragKind::Resize { from_left } => {
                    // 掴んだ端を、選択全体で同じ量だけ動かす。音価を揃え直さないので
                    // 尾揃え・頭揃えで作った相対関係がそのまま保たれる。
                    let dx = resize_delta(
                        &drag.targets,
                        content.x - drag.origin.x,
                        snap,
                        from_left,
                    );

                    for (idx, orig) in &drag.targets {
                        let mut note = *orig;
                        if from_left {
                            // 頭を動かす (終端は固定)
                            note.start_tick = orig.start_tick + dx;
                            note.duration = orig.duration - dx;
                        } else {
                            // 終端を動かす (頭は固定)
                            note.duration = orig.duration + dx;
                        }
                        state.editor.notes[*idx] = note;
                    }
                    if dx != 0.0 {
                        state.history.record(EditGroup::Resize);
                    }
                }
                DragKind::Marquee => {
                    let rect = Rect::from_two_pos(to_screen_pos(origin, drag.origin, ppq, row_h), pos);
                    let mut selection = drag.base_selection.clone();
                    for (idx, note) in state.editor.notes.iter().enumerate() {
                        if rect.intersects(note_rect(origin, note_row(&row_offsets, note), note, ppq, row_h))
                            && !selection.contains(&idx)
                        {
                            selection.push(idx);
                        }
                    }
                    marquee_selection = Some(selection);

                    painter.rect_filled(
                        rect,
                        CornerRadius::same(2),
                        palette::BG_HOVER.gamma_multiply(0.35),
                    );
                    painter.rect_stroke(
                        rect,
                        CornerRadius::same(2),
                        Stroke::new(1.0, palette::FG_DIM),
                        egui::StrokeKind::Inside,
                    );
                    // 選択枠はノートより後に描くので、次フレームで囲みを反映させる
                    ui.ctx().request_repaint();
                }
            }

            // ---- 画面端での自動スクロール ----
            // ノートを掴んだまま端に寄せると、その方向へスクロールし続ける。
            if !matches!(drag.kind, DragKind::Seek) {
                let vertical = matches!(drag.kind, DragKind::Move | DragKind::Marquee);
                let delta = edge_scroll_delta(ui.clip_rect(), pos, vertical);
                if delta != egui::Vec2::ZERO {
                    ui.scroll_with_delta_animation(delta, egui::style::ScrollAnimation::none());
                    // カーソルが静止していてもスクロールを続けるため再描画を要求する
                    ui.ctx().request_repaint();
                }
            }
        }

        if let Some(selection) = marquee_selection {
            state.select_many(selection);
        }
    }

    // ドラッグ終了
    if response.drag_stopped() {
        if let Some(drag) = state.drag.take() {
            if matches!(drag.kind, DragKind::Move | DragKind::Resize { .. }) {
                state.dirty = true;
            }
            state.history.end_group();
        }
    }

    // クリック (選択 / ルーラーシーク)
    if response.clicked() {
        if let Some(pos) = response.interact_pointer_pos() {
            if pos.y < origin.y + RULER_H {
                commands.push(EditorCommand::Seek {
                    quarters: seek_quarters(pos.x),
                });
            } else {
                match hit_note(&state.editor.notes, &row_offsets, origin, pos, left_resize, ppq, row_h) {
                    // Shift+クリックは選択に追加 / 解除
                    Some((idx, _)) if ui.input(|i| i.modifiers.shift) => {
                        let mut selection = state.selection_sorted();
                        match selection.iter().position(|i| *i == idx) {
                            Some(at) => {
                                selection.remove(at);
                            }
                            None => selection.push(idx),
                        }
                        state.select_many(selection);
                    }
                    Some((idx, _)) => state.select_single(idx),
                    None => state.clear_selection(),
                }
            }
        }
    }

    // ダブルクリックで新規ノート (クリックした段に置く)
    if response.double_clicked() {
        if let Some(pos) = response.interact_pointer_pos() {
            if is_inside_lanes(origin, pos, content_h)
                && hit_note(&state.editor.notes, &row_offsets, origin, pos, left_resize, ppq, row_h).is_none()
            {
                // 最後に選択・編集したノートの設定を引き継ぐ。
                // (ダブルクリックの1打目で選択が解除されるため state.selected は使えない)
                let defaults = state.last_note;
                let row = (((pos.y - origin.y - RULER_H) / row_h).floor() as i32)
                    .clamp(0, display_rows as i32 - 1) as usize;
                let (track, lane) = row_to_track_lane(&row_offsets, &state.editor, row);
                let start = snap_floor((pos.x - origin.x) / ppq).max(0.0);
                state.history.record(EditGroup::Once);
                // 連符モード中は連符1音分で置く (直前のノートの音価は引き継がない)
                let duration = if state.tuplet > 1 {
                    snap
                } else {
                    defaults.duration.max(snap)
                };
                state.editor.notes.push(Note {
                    start_tick: start,
                    duration,
                    semitone: defaults.semitone,
                    octave: defaults.octave,
                    velocity: defaults.velocity,
                    track,
                    lane,
                });
                state.select_single(state.editor.notes.len() - 1);
                state.dirty = true;
            }
        }
    }

    // 右クリックで削除 (選択中のノートを指したら選択ごと消す)
    if response.secondary_clicked() {
        if let Some(pos) = response.interact_pointer_pos() {
            if let Some((idx, _)) =
                hit_note(&state.editor.notes, &row_offsets, origin, pos, left_resize, ppq, row_h)
            {
                state.history.record(EditGroup::Once);
                if state.is_selected(idx) {
                    state.delete_selection();
                } else {
                    state.editor.notes.remove(idx);
                    state.clear_selection();
                }
                state.dirty = true;
            }
        }
    }

    // 選択中ノートの設定を新規ノート用に控えておく。
    // 選択・ツールバーでの編集・音価ドラッグのすべてがここを通る。
    // 選択が外れたときは更新しない (直前の設定を保持したいため)。
    // 範囲選択やペーストで選ばれたノート (track_last_note = false) も対象外。
    if state.track_last_note {
        if let Some(note) = state.selected.and_then(|i| state.editor.notes.get(i)) {
            state.last_note = NoteDefaults::from(note);
        }
    }

    // ---- 縦ズームの反映 ----
    // 描画・当たり判定を1フレーム分の row_h で通したあとに変える。
    // ホイール中は毎フレーム描き直されるので、1フレームの遅れは見えない。
    if zoom_notches != 0.0 {
        // カーソルの下にある段。ズームしてもここが動かないようスクロールを合わせる
        let anchor_row = ui
            .input(|i| i.pointer.hover_pos())
            .map_or(0.0, |pos| to_content_pos(origin, pos, ppq, row_h).y);
        let grew = state.set_row_h(row_h * ROW_ZOOM_STEP.powf(zoom_notches));

        // ScrollArea は offset -= delta で動くので、伸びた分だけ負の値を渡す。
        // ルーラーの上 (anchor_row が負) で回したときは先頭を見ているので補正しない。
        if grew != 0.0 && anchor_row > 0.0 {
            ui.scroll_with_delta_animation(
                vec2(0.0, -anchor_row * grew),
                egui::style::ScrollAnimation::none(),
            );
        }
    }

    // ---- 横ズームの反映 ----
    // 縦ズームと同じく、1フレーム分の ppq で描き終えてから変える。
    //
    // **スクロールの補正は差分ではなく絶対位置で渡す。** 差分
    // (`scroll_with_delta`) だと、ppq の変更が効くフレームと補正が効くフレームが
    // 1つずれる。ドラッグ中はフレームごとの移動量が細かく揺れるので、そのずれが
    // そのまま画面の左右の揺れになって見える。掴んだ位置が画面上のどこに居るべきかは
    // 毎フレーム一意に決まるので、そこから**その場のオフセットを直接求める**。
    // こうすると ppq と横位置が同じフレームで揃い、揺れようがない。
    if let Some(MiddleDrag::ZoomHorizontally { anchor_quarters }) = state.middle_drag {
        if zoom_pixels != 0.0 {
            // 掴んだ位置が今いる画面上の x。ズームしてもここから動かさない
            let anchor_x = origin.x + anchor_quarters * ppq;
            // 左へ動かすと拡大したいので、x の変化量の符号を反転して指数に使う
            state.set_ppq(ppq * PPQ_ZOOM_PER_PIXEL.powf(-zoom_pixels));
            state.pending_scroll_x = Some(horizontal_offset_for_anchor(
                ui.clip_rect().left(),
                anchor_x,
                anchor_quarters,
                state.ppq,
            ));
        }
    }
}

/// 掴んだ位置 (`anchor_quarters`) を画面上の `anchor_x` に留めるための横スクロール量。
///
/// `ScrollArea` のオフセットは「内容の左端からビューポート左端までの距離」なので、
/// **内容上での位置から、画面上でのビューポート左端からのずれを引けば**求まる。
/// 左端より手前へは送れないので 0 で止める。
fn horizontal_offset_for_anchor(
    viewport_left: f32,
    anchor_x: f32,
    anchor_quarters: f32,
    ppq: f32,
) -> f32 {
    (anchor_quarters * ppq - (anchor_x - viewport_left)).max(0.0)
}

/// その位置が段の並んでいる範囲に入っているか。
///
/// ルーラーの上と、段の下に広げた当たり判定の余白を除く。
/// 余白は範囲選択を始めるためだけの場所なので、ここにノートを作らせない
/// (行番号は最終段に丸められるため、放っておくと離れた場所の操作で
/// 最終段にノートができてしまう)。
fn is_inside_lanes(origin: Pos2, pos: Pos2, content_h: f32) -> bool {
    pos.y >= origin.y + RULER_H && pos.y < origin.y + content_h
}

/// 画面端の自動スクロール量を求める。
///
/// `visible` の内側 `EDGE_SCROLL_MARGIN` に `pointer` が入ると、端に近いほど速く
/// その方向へスクロールする量を返す。`vertical` が false なら横方向のみ。
///
/// 返す値は `scroll_with_delta` に渡す形式で、符号は「内容が動く向き」。
/// 例えば右端に寄せたときは先の内容を見せたいので x は負になる。
fn edge_scroll_delta(visible: Rect, pointer: Pos2, vertical: bool) -> egui::Vec2 {
    /// 端から何ピクセルを反応領域とするか
    const EDGE_SCROLL_MARGIN: f32 = 48.0;
    /// 反応領域の最も端での速度 (ピクセル/フレーム)
    const EDGE_SCROLL_MAX_SPEED: f32 = 14.0;

    // 端からの食い込み具合 (0.0..=1.0) を速度に変換する
    let speed = |depth: f32| (depth / EDGE_SCROLL_MARGIN).clamp(0.0, 1.0) * EDGE_SCROLL_MAX_SPEED;

    let mut delta = egui::Vec2::ZERO;

    if pointer.x > visible.right() - EDGE_SCROLL_MARGIN {
        delta.x = -speed(pointer.x - (visible.right() - EDGE_SCROLL_MARGIN));
    } else if pointer.x < visible.left() + EDGE_SCROLL_MARGIN {
        delta.x = speed((visible.left() + EDGE_SCROLL_MARGIN) - pointer.x);
    }

    if vertical {
        if pointer.y > visible.bottom() - EDGE_SCROLL_MARGIN {
            delta.y = -speed(pointer.y - (visible.bottom() - EDGE_SCROLL_MARGIN));
        } else if pointer.y < visible.top() + EDGE_SCROLL_MARGIN {
            delta.y = speed((visible.top() + EDGE_SCROLL_MARGIN) - pointer.y);
        }
    }

    delta
}

/// 一括移動の移動量 (四分音符, 段) を求める。
///
/// 移動量は掴んだノート (targets の先頭) を基準に1度だけスナップし、全員に同じだけ
/// 適用する。個別にスナップすると選択内の相対位置が崩れるため。
/// 選択全体が範囲 (開始位置 >= 0、段 0..rows) に収まるよう移動量を抑える。
fn move_delta(
    targets: &[(usize, Note)],
    row_offsets: &[usize],
    editor: &MidiEditor,
    dx_raw: f32,
    d_row_raw: i32,
    snap: f32,
    rows: usize,
) -> (f32, i32) {
    let Some((_, grabbed)) = targets.first() else {
        return (0.0, 0);
    };
    let snapped = ((grabbed.start_tick + dx_raw) / snap).round() * snap;
    let dx = snapped - grabbed.start_tick;

    let min_start = targets
        .iter()
        .map(|(_, n)| n.start_tick)
        .fold(f32::INFINITY, f32::min);
    let min_row = targets
        .iter()
        .map(|(_, n)| note_row(row_offsets, n))
        .min()
        .unwrap_or(0) as i32;
    let max_row = targets
        .iter()
        .map(|(_, n)| note_row(row_offsets, n))
        .max()
        .unwrap_or(0) as i32;

    // まず画面の範囲へ収め、そのうえで段の種別をまたぐぶんを削る。
    // 0 へ向かって1段ずつ戻すので、**行けるところまでは行く**
    // (CC 段が下にあるトラックでも、その手前までは動かせる)。
    let mut d_row = d_row_raw.clamp(-min_row, rows as i32 - 1 - max_row);
    while d_row != 0 && !keeps_lane_kind(targets, row_offsets, editor, d_row) {
        d_row -= d_row.signum();
    }

    (dx.max(-min_start), d_row)
}

/// その移動量で、全員が自分と同じ種別の段に着地するか。
///
/// **CC 段のノートは段から動かさない。** CC は段に割り当てた番号で意味が決まるので、
/// 別の段へ移すと黙って別の CC になってしまう。逆に、音符が CC 段へ入ると
/// 音として鳴らずに CC を送ってしまう。どちらも見た目では気付きにくいので、
/// そもそも動かせないようにする (横方向は自由)。
fn keeps_lane_kind(
    targets: &[(usize, Note)],
    row_offsets: &[usize],
    editor: &MidiEditor,
    d_row: i32,
) -> bool {
    targets.iter().all(|(_, note)| {
        let row = (note_row(row_offsets, note) as i32 + d_row).max(0) as usize;
        let (track, lane) = row_to_track_lane(row_offsets, editor, row);
        match editor.lane_cc(note.track, note.lane) {
            // CC 段のノートは、その段のまま以外を認めない
            Some(_) => track == note.track && lane == note.lane,
            // 音符は CC 段へ入れない
            None => editor.lane_cc(track, lane).is_none(),
        }
    })
}

/// 再生ヘッドの移動先 (四分音符単位) を求める。
///
/// 拍の間隔にスナップする。四分音符固定にすると 3/8 拍子のように1小節が
/// 四分音符の整数倍にならない拍子で小節線に乗らなくなるため、拍を単位にする
/// (拍は必ず小節を割り切るので、小節線は常にスナップ先に含まれる)。
/// 選択中ノートの頭がスナップ先より近ければ、そちらを優先する。
/// `free` が true なら (Alt 押下中) スナップしない。
fn seek_target(raw_quarters: f32, beat: f32, selected_start: Option<f32>, free: bool) -> f64 {
    let raw = raw_quarters.max(0.0);
    if free {
        return raw as f64;
    }
    let beat = if beat > 0.0 { beat } else { 1.0 };
    let snapped = ((raw / beat).round() * beat).max(0.0);
    match selected_start {
        Some(start) if (raw - start).abs() < (raw - snapped).abs() => start as f64,
        _ => snapped as f64,
    }
}

/// 一括リサイズで、掴んだ端を動かす量 (四分音符) を求める。
///
/// 掴んだノート (targets の先頭) の端がグリッドに乗るようスナップし、その差分を
/// 選択全体に同じだけ適用する。音価を揃え直さないので、尾揃え・頭揃えで作った
/// 相対関係がドラッグ後も保たれる。
/// どのノートも音価が snap を下回らず、頭が 0 より前に出ないよう量を抑える。
fn resize_delta(targets: &[(usize, Note)], dx_raw: f32, snap: f32, from_left: bool) -> f32 {
    let Some((_, grabbed)) = targets.first() else {
        return 0.0;
    };
    let snap_round = |q: f32| (q / snap).round() * snap;

    let min_duration = targets
        .iter()
        .map(|(_, n)| n.duration)
        .fold(f32::INFINITY, f32::min);

    if from_left {
        // 頭を動かす (終端は固定): 音価は dx だけ縮む
        let dx = snap_round(grabbed.start_tick + dx_raw) - grabbed.start_tick;
        let min_start = targets
            .iter()
            .map(|(_, n)| n.start_tick)
            .fold(f32::INFINITY, f32::min);
        dx.max(-min_start).min(min_duration - snap)
    } else {
        // 終端を動かす (頭は固定): 音価は dx だけ伸びる
        let end = grabbed.end_tick();
        let dx = snap_round(end + dx_raw) - end;
        dx.max(snap - min_duration)
    }
}

/// 左のトラック欄。トラックごとに名前と段の増減ボタンを、
/// グリッドの段と同じ高さで並べる (行の位置が揃うようにするため)。
/// トラック欄の見出し (常に見える位置に置く)。高さはルーラーと同じにして、
/// 下のトラック一覧の1行目がグリッドの1段目と揃うようにする。
fn track_gutter_header(ui: &mut egui::Ui, state: &mut EditorState) {
    let (rect, _) = ui.allocate_exact_size(vec2(GUTTER_W, RULER_H), Sense::hover());
    ui.painter_at(rect)
        .rect_filled(rect, CornerRadius::ZERO, palette::BG_LIGHT);

    let mut content = ui.new_child(
        egui::UiBuilder::new()
            .max_rect(rect.shrink2(vec2(6.0, 2.0)))
            .layout(egui::Layout::left_to_right(egui::Align::Center)),
    );
    content.set_clip_rect(rect);
    content.label(egui::RichText::new("トラック").size(11.0).color(palette::FG_DIM));

    if content
        .small_button("+")
        .on_hover_text("トラックを追加")
        .clicked()
    {
        state.history.record(EditGroup::Once);
        state.editor.add_track();
    }

    // ノートの入った最後のトラックと、最後の1本は消せない
    let last = state.editor.track_count() - 1;
    let removable =
        state.editor.track_count() > 1 && !state.editor.notes.iter().any(|n| n.track == last);
    let response = content.add_enabled(removable, egui::Button::new("−").small());
    if response.clicked() {
        state.history.record(EditGroup::Once);
        state.editor.remove_last_track();
    }
    if !removable {
        response.on_hover_text("最後のトラックにノートがある (またはトラックが1つ) ので消せません");
    }
}

/// 左のトラック欄。トラックごとに名前と段の増減ボタンを、
/// グリッドの段と同じ高さで並べる (行の位置が揃うようにするため)。
fn track_gutter(ui: &mut egui::Ui, state: &mut EditorState, commands: &mut Vec<EditorCommand>) {
    // ScrollArea の中身は親のレイアウトを引き継ぐ (ここは横並びの中なので)。
    // 縦並びを明示しないとトラックが横に並んでしまう。
    ui.vertical(|ui| track_gutter_content(ui, state, commands));
}

fn track_gutter_content(
    ui: &mut egui::Ui,
    state: &mut EditorState,
    commands: &mut Vec<EditorCommand>,
) {
    for track in 0..state.editor.track_count() {
        let lanes = state.editor.lanes(track);
        // グリッドの段と行の位置が揃うよう、縦ズームに追従させる
        let height = lanes as f32 * state.row_h;
        let (rect, _) = ui.allocate_exact_size(vec2(GUTTER_W, height), Sense::hover());

        // トラックの区切りが分かるように枠と背景を敷く
        let painter = ui.painter_at(rect);
        painter.rect_filled(rect, CornerRadius::ZERO, palette::BG_LIGHT);
        painter.line_segment(
            [rect.left_top(), rect.right_top()],
            Stroke::new(1.5, palette::FG_DIM),
        );

        // 段の帯 (トラック欄といちばん左の小節の間)。段ごとに1本ずつ。
        lane_strips(ui, state, track, rect);

        // 段が1つ (高さ = 段1つ分) でも収まるよう、名前とボタンは1行に並べる。
        // 縦に縮めたときは余白から先に削る (ボタンの高さは変えられないため)
        let pad_y = (rect.height() * 0.1).min(2.0);
        let mut content = ui.new_child(
            egui::UiBuilder::new()
                // 右端は段の帯にあけておく
                .max_rect(rect.shrink2(vec2(6.0, pad_y)).with_max_x(rect.right() - LANE_STRIP_W - 4.0))
                .layout(egui::Layout::left_to_right(egui::Align::Min)),
        );
        content.set_clip_rect(rect);
        // トグル3つ + 名前 + ボタン3つを 200px に収めるため間隔を詰める
        content.spacing_mut().item_spacing.x = 2.0;

        // ミュート / ソロ。編集ではないのでアンドゥ履歴には積まない。
        // 変更したらシーケンスを送り直す (鳴らす/止めるの切り替えのため)。
        let muted = state.editor.tracks[track].muted;
        let soloed = state.editor.tracks[track].soloed;
        if content
            .add(egui::SelectableLabel::new(
                muted,
                egui::RichText::new("M").size(10.0).color(if muted {
                    palette::RED
                } else {
                    palette::FG_DIM
                }),
            ))
            .on_hover_text("ミュート")
            .clicked()
        {
            state.editor.tracks[track].muted = !muted;
            state.dirty = true;
        }
        if content
            .add(egui::SelectableLabel::new(
                soloed,
                egui::RichText::new("S").size(10.0).color(if soloed {
                    palette::YELLOW
                } else {
                    palette::FG_DIM
                }),
            ))
            .on_hover_text("ソロ (どれか1つでもソロなら、それ以外は鳴りません)")
            .clicked()
        {
            state.editor.tracks[track].soloed = !soloed;
            state.dirty = true;
        }

        // スウィング。伴奏は正確な拍のまま、ソロだけ跳ねさせる使い方をするので
        // トラックごとに持つ。深さはツールバーの「スウィング」で全体設定。
        let swinging = state.editor.tracks[track].swing;
        let swing_enabled = swing::applies_to(state.editor.beat_type);
        let response = content.add_enabled(
            swing_enabled,
            egui::SelectableLabel::new(
                swinging,
                egui::RichText::new("W").size(10.0).color(if swinging {
                    palette::CYAN
                } else {
                    palette::FG_DIM
                }),
            ),
        );
        if response.clicked() {
            state.editor.tracks[track].swing = !swinging;
            state.dirty = true;
        }
        response.on_hover_text(if swing_enabled {
            "スウィング (跳ねの深さはツールバーで設定)"
        } else {
            "スウィングは N/4 拍子のときだけ使えます"
        });

        // 狭いときは名前の枠を詰める (右のボタンを見切れさせないため)
        let name_w = if rect.height() >= LANE_BUTTON_ROW_Y + LANE_BUTTON_ROW_H {
            44.0
        } else {
            32.0
        };
        let name = state.editor.tracks[track].name.clone();
        content.allocate_ui(vec2(name_w, ROW_H - 4.0), |ui| {
            ui.add(
                egui::Label::new(egui::RichText::new(name).size(11.0).color(palette::FG))
                    .truncate(),
            );
        });

        // 音源 (.clap / .vst3) の読み込み。載っていればボタンを色付きにして名前を出す
        let plugin = state
            .track_plugins
            .get(track)
            .cloned()
            .flatten();
        let label = egui::RichText::new("♪").size(11.0).color(match &plugin {
            Some(_) => palette::GREEN,
            None => palette::FG_DIM,
        });
        let response = content.add(egui::Button::new(label).small());
        if response.clicked() {
            commands.push(EditorCommand::LoadPlugin { track });
        }
        response.on_hover_text(match &plugin {
            Some(name) => format!("音源: {name} (クリックで差し替え)"),
            None => "音源 (.clap / .vst3) を読み込む".to_string(),
        });

        // 段の増減は2段目へ回す。1行に詰めると 200px に収まらないうえ、
        // **通常段と CC 段で別のボタンが要る** (最下段は CC 段のことがあるので、
        // 1組では「消したい段と違うものが消える」)。
        //
        // ただし2段目が入らない高さのときは、同じ行に続けて置く。
        // **隠れると段を増やす手立てが無くなる**ので、窮屈なほうがまだよい
        // (段が1つのトラックは既定でこの高さになる)。
        if rect.height() >= LANE_BUTTON_ROW_Y + LANE_BUTTON_ROW_H {
            drop(content);
            let mut second = ui.new_child(
                egui::UiBuilder::new()
                    .max_rect(Rect::from_min_max(
                        Pos2::new(rect.left() + 6.0, rect.top() + LANE_BUTTON_ROW_Y),
                        rect.right_bottom() - vec2(6.0, pad_y),
                    ))
                    .layout(egui::Layout::left_to_right(egui::Align::Min)),
            );
            second.set_clip_rect(rect);
            second.spacing_mut().item_spacing.x = 2.0;
            lane_buttons(&mut second, state, track, false);
        } else {
            lane_buttons(&mut content, state, track, true);
        }
    }
}

/// 段ごとの帯。押すとその段のノートを全選択し、続けて別の段を押すと入れ替える。
///
/// トラック欄の右端 (= いちばん左の小節との境) に縦棒として並べる。グリッドの
/// 段と同じ高さなので、行の位置がそのまま揃う。
///
/// **押した段は赤くなる (相手待ち)。** そこで別の段を押すと入れ替わり、同じ段を
/// もう一度押すと取り消す。入れ替えは種別が同じ段どうしでしかできないので、
/// 選べない相手はその場で分かるよう暗く描く。
fn lane_strips(ui: &mut egui::Ui, state: &mut EditorState, track: usize, track_rect: Rect) {
    let row_h = state.row_h;
    let painter = ui.painter_at(track_rect);

    for lane in 0..state.editor.lanes(track) {
        let rect = Rect::from_min_size(
            Pos2::new(
                track_rect.right() - LANE_STRIP_W,
                track_rect.top() + lane as f32 * row_h,
            ),
            vec2(LANE_STRIP_W, row_h),
        );
        let response = ui.interact(
            rect,
            ui.id().with(("lane_strip", track, lane)),
            Sense::click(),
        );

        let armed = state.lane_swap_source == Some((track, lane));
        // 相手待ちの段があるとき、種別が違えば入れ替えられない
        let selectable = match state.lane_swap_source {
            Some((source_track, source_lane)) => {
                state.editor.lane_cc(source_track, source_lane).is_some()
                    == state.editor.lane_cc(track, lane).is_some()
            }
            None => true,
        };

        let color = if armed {
            palette::RED
        } else if !selectable {
            palette::FG_DIM.gamma_multiply(0.4)
        } else if response.hovered() {
            palette::GREEN
        } else {
            palette::GREEN.gamma_multiply(0.55)
        };
        painter.rect_filled(rect.shrink2(vec2(2.0, 1.0)), CornerRadius::same(2), color);

        // 選択を変えると相手待ちは解除される (`select_many` を参照) ので、
        // **待ち状態にするのは選択したあと**。
        if response.clicked() {
            match state.lane_swap_source {
                // もう一度押したら取り消す
                Some(source) if source == (track, lane) => state.lane_swap_source = None,
                Some(source) => {
                    state.history.record(EditGroup::Once);
                    if state.editor.swap_lanes(source, (track, lane)) {
                        state.dirty = true;
                    }
                    state.select_lane(track, lane);
                    state.lane_swap_source = None;
                }
                None => {
                    state.select_lane(track, lane);
                    state.lane_swap_source = Some((track, lane));
                }
            }
        }

        response.on_hover_text(if armed {
            "入れ替える相手の段を押してください (もう一度押すと取り消し)"
        } else if selectable {
            "この段のノートを全選択 (続けて別の段を押すと入れ替え)"
        } else {
            "音符段と CC 段は入れ替えられません"
        });
    }
}

/// 段の増減 (通常段と CC 段で別々)。トラック欄の2段目に置く。
///
/// `compact` のときは、同じ行に続けて置くので見出しと区切りを省く。
fn lane_buttons(ui: &mut egui::Ui, state: &mut EditorState, track: usize, compact: bool) {
    let lanes = state.editor.lanes(track);
    let normal = state.editor.tracks[track].normal_lanes();

    // ---- 通常段 ----
    if !compact {
        ui.label(egui::RichText::new("段").size(10.0).color(palette::FG_DIM));
    }
    if ui
        .small_button("+")
        .on_hover_text("段を追加 (CC 段より上に入ります)")
        .clicked()
    {
        state.history.record(EditGroup::Once);
        state.editor.add_lane(track);
    }

    // 通常段の最下段。ノートがあるとき、通常段が1つのときは消せない
    let removable = normal > 1
        && !state
            .editor
            .notes
            .iter()
            .any(|note| note.track == track && note.lane + 1 == normal);
    let response = ui.add_enabled(removable, egui::Button::new("−").small());
    if response.clicked() {
        state.history.record(EditGroup::Once);
        state.editor.remove_last_normal_lane(track);
    }
    response.on_hover_text(if removable {
        "いちばん下の段を削除 (CC 段には触れません)"
    } else {
        "その段にノートがある (または段が1つ) ので消せません"
    });

    if !compact {
        ui.separator();
    }

    // ---- CC 段 ----
    let has_cc = normal < lanes;
    let label = egui::RichText::new("CC").size(10.0).color(if has_cc {
        palette::GREEN
    } else {
        palette::FG_DIM
    });
    let response = ui.add(egui::Button::new(label).small());
    if response.clicked() {
        state.lane_config_track = (state.lane_config_track != Some(track)).then_some(track);
    }
    response.on_hover_text("CC 段の番号の一覧を開く");

    // 狭いときは CC の増減を行に出さない。**押せるが見切れる**より、
    // 一覧 (上の「CC」ボタン) にまとめたほうが扱える。一覧側にも同じ操作がある。
    if !compact {
        cc_lane_buttons(ui, state, track);
    }
}

/// CC 段の追加・削除。トラック欄の2段目と、CC 段の一覧の両方から使う。
fn cc_lane_buttons(ui: &mut egui::Ui, state: &mut EditorState, track: usize) {
    let lanes = state.editor.lanes(track);
    let has_cc = state.editor.tracks[track].normal_lanes() < lanes;

    if ui
        .small_button("+")
        .on_hover_text("CC 段を最下段に追加 (ペダル CC64。番号は一覧で変えられます)")
        .clicked()
    {
        state.history.record(EditGroup::Once);
        // 既定はペダル。いちばん使ううえ、0 が「離す」で自然に効く
        state.editor.add_cc_lane(track, 64);
        state.lane_config_track = Some(track);
        state.dirty = true;
    }

    let removable = has_cc
        && !state
            .editor
            .notes
            .iter()
            .any(|note| note.track == track && note.lane + 1 == lanes);
    let response = ui.add_enabled(removable, egui::Button::new("−").small());
    if response.clicked() {
        state.history.record(EditGroup::Once);
        state.editor.remove_last_cc_lane(track);
        state.dirty = true;
    }
    response.on_hover_text(if removable {
        "いちばん下の CC 段を削除"
    } else if has_cc {
        "その CC 段にブロックがあるので消せません"
    } else {
        "CC 段がありません"
    });
}

/// 各トラックの段が画面の何行目から始まるかを求める。
/// 画面の行は「トラック0の段0..n, トラック1の段0..m, ...」と上から並ぶ。
fn track_row_offsets(editor: &MidiEditor) -> Vec<usize> {
    let mut offsets = Vec::with_capacity(editor.tracks.len());
    let mut row = 0;
    for info in &editor.tracks {
        offsets.push(row);
        row += info.lanes.max(1);
    }
    offsets
}

/// ノートが画面の何行目に描かれるか
fn note_row(offsets: &[usize], note: &Note) -> usize {
    offsets.get(note.track).copied().unwrap_or(0) + note.lane
}

/// 画面の行番号を (トラック, 段) に戻す
fn row_to_track_lane(offsets: &[usize], editor: &MidiEditor, row: usize) -> (usize, usize) {
    for track in (0..offsets.len()).rev() {
        if row >= offsets[track] {
            let lane = (row - offsets[track]).min(editor.lanes(track).saturating_sub(1));
            return (track, lane);
        }
    }
    (0, 0)
}

/// ノート矩形のうち、ベロシティ分を実塗りする部分 (下側) を求める。
/// ベロシティ127で矩形全体、小さいほど下に薄く残る。
fn velocity_fill_rect(rect: Rect, velocity: u8) -> Rect {
    let level = velocity.min(127) as f32 / 127.0;
    let height = rect.height() * level;
    Rect::from_min_max(
        Pos2::new(rect.left(), rect.bottom() - height),
        Pos2::new(rect.right(), rect.bottom()),
    )
}

/// ノートの表示矩形を計算する。`row` は画面の行番号 (トラックの段を通しで数えた値)。
///
/// 段の上下に空ける余白は高さの 1/12 (既定の 24px で従来どおり上下2px)。
/// 固定値にすると、縮めたときに矩形が潰れ、広げたときに隙間が目立たなくなる。
fn note_rect(origin: Pos2, row: usize, note: &Note, ppq: f32, row_h: f32) -> Rect {
    let margin = row_h / 12.0;
    Rect::from_min_size(
        Pos2::new(
            origin.x + note.start_tick * ppq,
            origin.y + RULER_H + row as f32 * row_h + margin,
        ),
        vec2((note.duration * ppq - 1.0).max(4.0), row_h - margin * 2.0),
    )
}

/// ノートのどこを掴んだか
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Hit {
    /// 本体 (移動)
    Body,
    /// 右端: 頭を固定して終端を動かす
    ResizeRight,
    /// 左端: 終端を固定して頭を動かす (左端音価モードが ON のときだけ)
    ResizeLeft,
}

/// pos にあるノートと、掴んだ位置の種別を返す。
///
/// 右端は常にリサイズ領域。`left_resize` が true のときは左端も加わる。
/// 短いノートでは左右の判定領域が重なってしまうため、両方有効なときは
/// ノートの中央で分け合う (右端を優先して左端が掴めなくなるのを防ぐ)。
fn hit_note(
    notes: &[Note],
    offsets: &[usize],
    origin: Pos2,
    pos: Pos2,
    left_resize: bool,
    ppq: f32,
    row_h: f32,
) -> Option<(usize, Hit)> {
    for (idx, note) in notes.iter().enumerate().rev() {
        let rect = note_rect(origin, note_row(offsets, note), note, ppq, row_h);
        let split = rect.center().x;

        let right_from = if left_resize {
            (rect.right() - EDGE_W).max(split)
        } else {
            rect.right() - EDGE_W
        };
        let right = Rect::from_min_max(
            Pos2::new(right_from, rect.top()),
            Pos2::new(rect.right() + 2.0, rect.bottom()),
        );
        if right.contains(pos) {
            return Some((idx, Hit::ResizeRight));
        }

        if left_resize {
            let left = Rect::from_min_max(
                Pos2::new(rect.left() - 2.0, rect.top()),
                Pos2::new((rect.left() + EDGE_W).min(split), rect.bottom()),
            );
            if left.contains(pos) {
                return Some((idx, Hit::ResizeLeft));
            }
        }

        if rect.contains(pos) {
            return Some((idx, Hit::Body));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn viewport() -> Rect {
        Rect::from_min_max(Pos2::new(0.0, 0.0), Pos2::new(1000.0, 400.0))
    }

    fn pitched(semitone: i32, octave: i32) -> Note {
        Note {
            start_tick: 0.0,
            duration: 1.0,
            semitone,
            octave,
            velocity: 100,
            track: 0,
            lane: 0,
        }
    }

    /// テスト用: トラック1本 (段は十分にある) の行オフセット
    fn single_track_rows() -> Vec<usize> {
        vec![0]
    }

    /// 段が16ある1トラックだけのエディタ (全部が音符段)
    fn single_track_editor() -> MidiEditor {
        let mut editor = MidiEditor::default();
        editor.tracks[0].lanes = 16;
        editor
    }

    /// いちばん低い音は寒色、いちばん高い音は赤い
    #[test]
    fn note_fill_runs_from_blue_to_red() {
        let scale = ScaleMode::Equal12;
        let lowest = note_fill(&pitched(0, MIN_OCTAVE), scale);
        let highest = note_fill(&pitched(11, MAX_OCTAVE), scale);

        assert!(
            lowest.b() > lowest.r(),
            "最低音は赤より青が強いこと: {lowest:?}"
        );
        assert!(
            highest.r() > highest.b(),
            "最高音は青より赤が強いこと: {highest:?}"
        );
    }

    /// 色を割り当てた音域の外は、端の色で頭打ちになること。
    ///
    /// 上端に届くのは「最上位オクターブの次の頭」= `NOTE_COLOR_MAX_OCTAVE + 1` の
    /// 半音0。最上位オクターブの最上位半音はまだ1半音ぶん手前にある。
    #[test]
    fn note_fill_clamps_outside_the_colored_range() {
        let scale = ScaleMode::Equal12;
        let bottom = note_fill(&pitched(0, NOTE_COLOR_MIN_OCTAVE), scale);
        let top = note_fill(&pitched(0, NOTE_COLOR_MAX_OCTAVE + 1), scale);

        for octave in MIN_OCTAVE..NOTE_COLOR_MIN_OCTAVE {
            for semitone in [0, 11] {
                assert_eq!(
                    note_fill(&pitched(semitone, octave), scale),
                    bottom,
                    "オクターブ {octave} 半音 {semitone} が下端の色で止まっていない"
                );
            }
        }
        for octave in (NOTE_COLOR_MAX_OCTAVE + 1)..=MAX_OCTAVE {
            for semitone in [0, 11] {
                assert_eq!(
                    note_fill(&pitched(semitone, octave), scale),
                    top,
                    "オクターブ {octave} 半音 {semitone} が上端の色で止まっていない"
                );
            }
        }
    }

    /// 音高が上がるほど暖色へ進み、途中で寒色へ戻らないこと。
    ///
    /// 色を割り当てた音域では、**どの2つのオクターブも**別の色になること。
    ///
    /// 隣同士だけ見ていると、色相環を一周して離れたオクターブが同じ色になる
    /// 事故を見逃す。総当たりで確かめる。
    ///
    /// なお「赤 − 青」のような一方向の指標では単調性を見られない。
    /// ティールから青へ向かう区間は**赤から遠ざかる**ためで、これは経路として
    /// 正しい (寒色→暖色は端どうしの関係であって、途中まで単調ではない)。
    #[test]
    fn every_octave_gets_its_own_color() {
        let scale = ScaleMode::Equal12;
        let color = |octave: i32| note_fill(&pitched(0, octave), scale);
        let distance = |a: Color32, b: Color32| {
            (a.r() as i32 - b.r() as i32).abs()
                + (a.g() as i32 - b.g() as i32).abs()
                + (a.b() as i32 - b.b() as i32).abs()
        };

        for low in NOTE_COLOR_MIN_OCTAVE..=NOTE_COLOR_MAX_OCTAVE {
            for high in (low + 1)..=NOTE_COLOR_MAX_OCTAVE {
                let gap = distance(color(low), color(high));
                assert!(
                    gap > 30,
                    "オクターブ {low} と {high} の色差が小さすぎる (差 {gap}): \
                     {:?} / {:?}",
                    color(low),
                    color(high)
                );
            }
        }
    }

    /// オクターブ境界で色が飛ばないこと。
    ///
    /// 半音でも連続的に変えているので、あるオクターブの最上位半音と
    /// 次のオクターブの半音0 は隣り合う色になる。
    #[test]
    fn note_fill_is_continuous_across_octaves() {
        let scale = ScaleMode::Equal12;
        let top = note_fill(&pitched(11, 4), scale);
        let next_bottom = note_fill(&pitched(0, 5), scale);

        let gap = (top.r() as i32 - next_bottom.r() as i32).abs()
            + (top.g() as i32 - next_bottom.g() as i32).abs()
            + (top.b() as i32 - next_bottom.b() as i32).abs();

        // 1半音ぶんの差しかないので、ごくわずかしか離れない
        assert!(gap < 24, "オクターブ境界で色が飛んでいる (差 {gap})");
        assert_ne!(top, next_bottom, "半音が変われば色も変わること");
    }

    /// sRGB の相対輝度 (WCAG の定義)。見た目の明るさの目安に使う。
    fn relative_luminance(color: Color32) -> f32 {
        let channel = |byte: u8| {
            let value = byte as f32 / 255.0;
            if value <= 0.040_45 {
                value / 12.92
            } else {
                ((value + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * channel(color.r()) + 0.7152 * channel(color.g()) + 0.0722 * channel(color.b())
    }

    /// どの音高でも、塗りの上に抜くラベルが読めること。
    ///
    /// 塗りが届いた部分のラベルは [`palette::BG`] で抜いている。
    /// [`NOTE_LIGHTNESS`] の下限はこの条件から決めた。Oklch の明度が一定でも
    /// 見た目の輝度は色相で変わるので、全音域を見る。
    #[test]
    fn every_note_keeps_its_label_readable() {
        let scale = ScaleMode::Equal12;
        let background = relative_luminance(palette::BG);

        for octave in MIN_OCTAVE..=MAX_OCTAVE {
            for semitone in 0..12 {
                let fill = note_fill(&pitched(semitone, octave), scale);
                let contrast = (relative_luminance(fill) + 0.05) / (background + 0.05);
                assert!(
                    contrast >= 4.5,
                    "オクターブ {octave} 半音 {semitone} でラベルのコントラストが不足: \
                     {contrast:.2}:1 ({fill:?})"
                );
            }
        }
    }

    /// どの音高でも sRGB からはみ出さないこと。
    ///
    /// はみ出したまま切り詰めると、色相と明度が指定からずれる。
    /// クロマを削って収める処理が効いているかを見る。
    #[test]
    fn note_colors_stay_inside_srgb() {
        let scale = ScaleMode::Equal12;
        for octave in MIN_OCTAVE..=MAX_OCTAVE {
            for semitone in 0..12 {
                let color = note_fill(&pitched(semitone, octave), scale);
                // 3チャンネルとも振り切れていたら、削り切れずに潰れている
                let clipped = [color.r(), color.g(), color.b()]
                    .iter()
                    .filter(|byte| **byte == 0 || **byte == 255)
                    .count();
                assert!(
                    clipped < 3,
                    "オクターブ {octave} 半音 {semitone} で色が潰れている: {color:?}"
                );
            }
        }
    }

    fn placed(start: f32, lane: usize) -> Note {
        Note {
            start_tick: start,
            duration: 0.5,
            semitone: 0,
            octave: 4,
            velocity: 100,
            track: 0,
            lane,
        }
    }

    /// 一括移動: 掴んだノートを基準にスナップし、全員が同じだけ動くこと
    #[test]
    fn bulk_move_keeps_relative_positions() {
        // 0.1 だけずれた位置にあるノートも、掴んだノートと同じ移動量で動く
        let targets = vec![(0, placed(1.0, 2)), (1, placed(1.1, 3))];
        let (dx, d_lane) = move_delta(&targets, &single_track_rows(), &single_track_editor(), 0.6, 1, 0.25, 16);
        assert_eq!(dx, 0.5, "掴んだノートが 1.5 に来るようスナップされること");
        assert_eq!(d_lane, 1);
    }

    /// 選択が変わったら、段の入れ替えの相手待ちも解けること。
    ///
    /// **解けないと、何もない場所を押したあとに別の段の帯を押しただけで
    /// 入れ替わってしまう** (帯は赤く残ったままなので気付けない)。
    #[test]
    fn changing_the_selection_disarms_the_lane_swap() {
        let mut state = EditorState::default();
        state.editor.notes = vec![placed(0.0, 0)];

        for change in [
            "clear_selection",
            "select_single",
            "select_many",
            "select_lane",
        ] {
            state.lane_swap_source = Some((0, 0));
            match change {
                "clear_selection" => state.clear_selection(),
                "select_single" => state.select_single(0),
                "select_many" => state.select_many(vec![0]),
                _ => state.select_lane(0, 0),
            }
            assert_eq!(state.lane_swap_source, None, "{change} で解けること");
        }
    }

    /// CC 段のブロックをコピーして貼り付けられること。
    ///
    /// 貼り付け先は元と同じ段でなければならない (別の段に落ちると、
    /// 黙って別の CC になるか、音符として鳴ってしまう)。
    #[test]
    fn cc_blocks_can_be_copied_and_pasted() {
        let mut state = EditorState::default();
        state.editor.tracks[0].lanes = 1;
        state.editor.add_cc_lane(0, 64); // 段1 が CC
        state.editor.notes = vec![Note {
            lane: 1,
            velocity: 100,
            ..placed(1.0, 1)
        }];
        state.select_many(vec![0]);

        let copied = state.copy_selection().expect("コピーできること");
        assert_eq!(copied.len(), 1);
        assert_eq!(copied[0].lane, 1, "段を保つこと");

        assert!(state.paste_notes(&copied, 4.0));
        assert_eq!(state.editor.notes.len(), 2);
        let pasted = state.editor.notes[1];
        assert_eq!(pasted.start_tick, 4.0);
        assert_eq!(pasted.lane, 1, "CC 段に貼られること");
        assert_eq!(pasted.velocity, 100, "CC 値を保つこと");
        assert_eq!(
            state.editor.lane_cc(pasted.track, pasted.lane),
            Some(64),
            "貼り付け先が CC 段のままであること"
        );
    }

    /// 段が16の1トラックで、下2段 (14,15) を CC 段にしたエディタ
    fn editor_with_cc_lanes() -> MidiEditor {
        let mut editor = single_track_editor();
        editor.tracks[0].set_lane_cc(14, Some(64));
        editor.tracks[0].set_lane_cc(15, Some(1));
        editor
    }

    /// 音符は CC 段へ入れないこと。**手前までは動ける。**
    #[test]
    fn notes_stop_above_the_cc_lanes() {
        let editor = editor_with_cc_lanes();
        let targets = vec![(0, placed(1.0, 10))];

        // 下へ大きく動かしても、CC 段の1つ上 (13段目) で止まる
        let (_, d_lane) = move_delta(&targets, &single_track_rows(), &editor, 0.0, 5, 0.25, 16);
        assert_eq!(d_lane, 3, "13段目まで (10 + 3) しか下がらないこと");

        // 上方向は普通に動ける
        let (_, d_lane) = move_delta(&targets, &single_track_rows(), &editor, 0.0, -4, 0.25, 16);
        assert_eq!(d_lane, -4);
    }

    /// CC 段のノートは、その段から動かせないこと (横だけ動く)
    #[test]
    fn cc_notes_cannot_leave_their_lane() {
        let editor = editor_with_cc_lanes();
        let targets = vec![(0, placed(1.0, 14))];

        for requested in [-1, 1, -5, 5] {
            let (dx, d_lane) =
                move_delta(&targets, &single_track_rows(), &editor, 0.5, requested, 0.25, 16);
            assert_eq!(d_lane, 0, "段を移動できないこと (要求 {requested})");
            assert_eq!(dx, 0.5, "横方向は動かせること");
        }
    }

    /// 一括移動: 選択の端が範囲外に出ないよう移動量が抑えられること
    #[test]
    fn bulk_move_clamps_to_bounds() {
        let targets = vec![(0, placed(1.0, 1)), (1, placed(0.5, 15))];
        // 左へ大きく動かしても、先頭のノートが 0 未満にならない分しか動かない
        let (dx, _) = move_delta(&targets, &single_track_rows(), &single_track_editor(), -5.0, 0, 0.25, 16);
        assert_eq!(dx, -0.5);
        // 下へ動かしても、最下段のノートが 15 段目に留まる
        let (_, d_lane) = move_delta(&targets, &single_track_rows(), &single_track_editor(), 0.0, 5, 0.25, 16);
        assert_eq!(d_lane, 0);
        // 上へ動かすときは最上段のノートが 0 段目で止まる
        let (_, d_lane) = move_delta(&targets, &single_track_rows(), &single_track_editor(), 0.0, -5, 0.25, 16);
        assert_eq!(d_lane, -1);
    }

    /// コピーは相対位置を保ち、ペーストは再生ヘッド位置を先頭にすること
    #[test]
    fn copy_paste_anchors_at_playhead() {
        let mut state = EditorState::default();
        state.editor.notes = vec![placed(2.0, 1), placed(2.5, 3), placed(9.0, 0)];
        state.select_many(vec![0, 1]);

        // 実際の経路と同じく、控えを経由して貼り付ける
        let notes = state.copy_selection().expect("コピーできること");
        state.paste_notes(&notes, 4.0);

        assert_eq!(state.editor.notes.len(), 5);
        // 先頭が再生ヘッド、2つ目は元の間隔 (0.5) を保つ。段はそのまま。
        assert_eq!(state.editor.notes[3].start_tick, 4.0);
        assert_eq!(state.editor.notes[3].lane, 1);
        assert_eq!(state.editor.notes[4].start_tick, 4.5);
        assert_eq!(state.editor.notes[4].lane, 3);
        // 貼り付けた分が選択される
        assert_eq!(state.selection, vec![3, 4]);
        // 範囲選択・ペーストの選択は新規ノートの設定に引き継がない
        assert!(!state.track_last_note);
    }

    /// ← キー: 選択中のノートの頭が、最も早いノートに揃うこと
    #[test]
    fn align_selection_starts_to_earliest() {
        let mut state = EditorState::default();
        state.editor.notes = vec![placed(2.0, 0), placed(0.5, 1), placed(3.0, 2)];
        state.select_many(vec![0, 2]);

        assert!(state.align_selection_starts());
        assert_eq!(state.editor.notes[0].start_tick, 2.0, "選択内の最小は 2.0");
        assert_eq!(state.editor.notes[2].start_tick, 2.0);
        assert_eq!(state.editor.notes[1].start_tick, 0.5, "選択外は動かないこと");

        // 揃っていれば何もしない (無駄なアンドゥ履歴を作らないため)
        assert!(!state.align_selection_starts());
        // 1個だけの選択でも何も起きない
        state.select_many(vec![1]);
        assert!(!state.align_selection_starts());
    }

/// → キー: 開始位置はそのままに、音価を伸ばして尾が揃うこと
    #[test]
    fn align_selection_ends_to_latest() {
        let mut state = EditorState::default();
        // placed の音価は 0.5 なので終端は 2.5 / 1.0 / 3.5
        state.editor.notes = vec![placed(2.0, 0), placed(0.5, 1), placed(3.0, 2)];
        state.select_many(vec![0, 2]);

        assert!(state.align_selection_ends());
        assert_eq!(state.editor.notes[0].start_tick, 2.0, "開始位置は動かさないこと");
        assert_eq!(state.editor.notes[0].duration, 1.5, "音価を伸ばして尾を揃える");
        assert_eq!(state.editor.notes[0].end_tick(), 3.5, "選択内の最遅の終端に揃う");
        assert_eq!(state.editor.notes[2].duration, 0.5, "基準ノートは変わらない");
        assert_eq!(state.editor.notes[1].duration, 0.5, "選択外は変わらないこと");

        // 揃っていれば何もしない
        assert!(!state.align_selection_ends());
        // 1個だけの選択でも何も起きない
        state.select_many(vec![1]);
        assert!(!state.align_selection_ends());
    }

    /// ノートを作れるのは段の中だけで、範囲選択用に広げた余白では作らないこと。
    /// (行番号は最終段に丸められるので、余白を許すと最終段にノートが湧く)
    #[test]
    fn notes_are_created_only_inside_the_lanes() {
        let origin = Pos2::ZERO;
        let content_h = RULER_H + ROW_H * 2.0; // 2段

        assert!(
            !is_inside_lanes(origin, Pos2::new(0.0, RULER_H - 1.0), content_h),
            "ルーラーの上では作らないこと"
        );
        assert!(is_inside_lanes(origin, Pos2::new(0.0, RULER_H + 1.0), content_h));
        assert!(
            is_inside_lanes(origin, Pos2::new(0.0, content_h - 1.0), content_h),
            "最終段の下端までは作れること"
        );
        assert!(
            !is_inside_lanes(origin, Pos2::new(0.0, content_h + 1.0), content_h),
            "段の下の余白では作らないこと"
        );

        // 原点がずれても同じ判定になること (スクロール中でも成り立つ)
        let moved = Pos2::new(30.0, 100.0);
        assert!(is_inside_lanes(moved, moved + vec2(0.0, RULER_H + 1.0), content_h));
        assert!(!is_inside_lanes(moved, moved + vec2(0.0, content_h + 1.0), content_h));
    }

    /// 縦ズームは上下限で頭打ちになり、実際に変わった量を返すこと
    #[test]
    fn row_zoom_is_clamped() {
        let mut state = EditorState::default();
        assert_eq!(state.row_h, ROW_H, "既定は従来の段の高さ");

        assert_eq!(state.set_row_h(ROW_H * 2.0), ROW_H, "変わった量を返すこと");
        assert_eq!(state.row_h, ROW_H * 2.0);

        // 上限・下限を越えたら丸める
        state.set_row_h(MAX_ROW_H * 10.0);
        assert_eq!(state.row_h, MAX_ROW_H);
        state.set_row_h(0.0);
        assert_eq!(state.row_h, MIN_ROW_H);
        // 頭打ちのあとは何も変わらない (スクロール補正を打たないため)
        assert_eq!(state.set_row_h(-100.0), 0.0);
    }

    /// 横ズームの補正: 掴んだ位置が画面上で動かないこと。
    ///
    /// ここが狂うとズーム中に画面が左右に揺れる。**倍率を変えても、掴んだ位置の
    /// 画面座標は変わらない**ことを、拡大・縮小の両方で確かめる。
    #[test]
    fn horizontal_zoom_keeps_the_anchor_on_screen() {
        let viewport_left = 200.0;
        let anchor_quarters = 12.0;
        let ppq = 40.0;

        // 掴んだ位置がビューポート左端から 150px のところに見えている状態
        let anchor_x = viewport_left + 150.0;

        for new_ppq in [ppq * 2.0, ppq * 0.5, ppq * 8.0, ppq] {
            let offset = horizontal_offset_for_anchor(viewport_left, anchor_x, anchor_quarters, new_ppq);
            // オフセットから逆算した画面上の位置が元と一致すること
            let origin_x = viewport_left - offset;
            let shown_x = origin_x + anchor_quarters * new_ppq;
            assert!(
                (shown_x - anchor_x).abs() < 0.001,
                "ppq={new_ppq} で掴んだ位置が動いた ({shown_x} != {anchor_x})"
            );
        }
    }

    /// 左端より手前へは送らないこと (ScrollArea が受け付けないため)
    #[test]
    fn horizontal_zoom_offset_never_goes_negative() {
        // 内容上の位置より画面上のずれのほうが大きい = 左端が見えている状態
        let offset = horizontal_offset_for_anchor(0.0, 500.0, 1.0, 10.0);
        assert_eq!(offset, 0.0);
    }

    /// 横ズームは上下限で頭打ちになり、実際に変わった量を返すこと
    #[test]
    fn horizontal_zoom_is_clamped() {
        let mut state = EditorState::default();
        assert_eq!(state.ppq, PPQ, "既定は従来の横幅");

        assert_eq!(state.set_ppq(PPQ * 2.0), PPQ, "変わった量を返すこと");
        assert_eq!(state.ppq, PPQ * 2.0);

        // 上限・下限を越えたら丸める
        state.set_ppq(MAX_PPQ * 10.0);
        assert_eq!(state.ppq, MAX_PPQ);
        state.set_ppq(0.0);
        assert_eq!(state.ppq, MIN_PPQ);
        // 頭打ちのあとは何も変わらない (スクロール補正を打たないため)
        assert_eq!(state.set_ppq(-100.0), 0.0);
    }

    /// Ctrl+中ドラッグ 1px あたりの倍率が、実用的な速さになっていること。
    ///
    /// 小さすぎると下限から上限まで何往復も必要になり、大きすぎると
    /// 少し動かしただけで飛ぶ。画面幅ぶん動かして全域を通り抜けるくらいが目安。
    #[test]
    fn horizontal_zoom_speed_covers_the_range_in_one_sweep() {
        // 900px は 1100px 幅のウィンドウでグリッドに使える程度の距離
        let sweep = PPQ_ZOOM_PER_PIXEL.powf(900.0);
        let full_range = MAX_PPQ / MIN_PPQ;
        assert!(
            sweep > full_range,
            "画面幅ぶん動かしても全域を通れない (倍率 {sweep:.0} / 必要 {full_range:.0})"
        );
        // 逆に速すぎないこと。少し動かしただけで飛ぶと、狙った倍率で止められない
        let nudge = PPQ_ZOOM_PER_PIXEL.powf(10.0);
        assert!(nudge < 1.5, "10px で {nudge:.2} 倍は速すぎる");
    }

    /// ノート矩形が段の高さに追従し、上下の余白も一緒に伸び縮みすること
    #[test]
    fn note_rect_follows_the_row_height() {
        let origin = Pos2::new(0.0, 0.0);
        let note = placed(0.0, 1); // 2段目

        // 既定の 24px では従来どおり上下2px の余白
        let normal = note_rect(origin, 1, &note, PPQ, ROW_H);
        assert_eq!(normal.top(), RULER_H + ROW_H + 2.0);
        assert_eq!(normal.height(), ROW_H - 4.0);

        // 倍に広げれば位置も高さも倍 (余白も倍なので比率は変わらない)
        let zoomed = note_rect(origin, 1, &note, PPQ, ROW_H * 2.0);
        assert_eq!(zoomed.top(), RULER_H + ROW_H * 2.0 + 4.0);
        assert_eq!(zoomed.height(), (ROW_H - 4.0) * 2.0);
        assert_eq!(
            zoomed.width(),
            normal.width(),
            "縦ズームでは横幅が変わらないこと"
        );

        // 下限まで縮めても矩形は残ること (固定余白だと潰れてしまう)
        assert!(note_rect(origin, 0, &note, PPQ, MIN_ROW_H).height() > 0.0);
    }

    /// ノート矩形が横ズームに追従し、縦は変わらないこと
    #[test]
    fn note_rect_follows_the_horizontal_zoom() {
        let origin = Pos2::new(0.0, 0.0);
        let note = placed(2.0, 0); // 2拍目から

        let normal = note_rect(origin, 0, &note, PPQ, ROW_H);
        let zoomed = note_rect(origin, 0, &note, PPQ * 2.0, ROW_H);

        assert_eq!(zoomed.left(), normal.left() * 2.0, "位置も倍になること");
        // 幅は「音価 × ppq − 1px」なので、倍にすると 1px 分だけ端数が出る
        assert_eq!(zoomed.width(), normal.width() * 2.0 + 1.0);
        assert_eq!(zoomed.top(), normal.top(), "縦位置は変わらないこと");
        assert_eq!(zoomed.height(), normal.height(), "高さは変わらないこと");

        // 下限まで縮めても矩形は残ること
        assert!(note_rect(origin, 0, &note, MIN_PPQ, ROW_H).width() > 0.0);
    }

    /// 当たり判定も段の高さに追従すること (広げた段の中央で掴めること)
    #[test]
    fn hit_note_follows_the_row_height() {
        let origin = Pos2::new(0.0, 0.0);
        let notes = vec![placed(0.0, 1)]; // 2段目
        let rows = single_track_rows();
        let tall = ROW_H * 3.0;

        // 広げた2段目の中央
        let inside = Pos2::new(4.0, RULER_H + tall * 1.5);
        assert_eq!(hit_note(&notes, &rows, origin, inside, false, PPQ, tall), Some((0, Hit::Body)));
        // 同じ点も、既定の高さでは2段目の外なので当たらない
        assert_eq!(hit_note(&notes, &rows, origin, inside, false, PPQ, ROW_H), None);
    }

    /// 左端音価が ON でも、右端は従来どおりリサイズ領域であること。
    /// 短いノートで左右が食い合わないよう、中央で分かれること。
    #[test]
    fn hit_note_edges_split_at_center() {
        let origin = Pos2::new(0.0, 0.0);
        // 0.25拍 = 20px 幅のノート (既定の音価)。矩形は x 0..19
        let notes = vec![Note {
            duration: 0.25,
            ..placed(0.0, 0)
        }];
        let y = RULER_H + ROW_H * 0.5;
        let left_pos = Pos2::new(2.0, y);
        let right_pos = Pos2::new(17.0, y);

        // OFF: 左端は本体扱い (移動)、右端はリサイズ
        assert_eq!(hit_note(&notes, &single_track_rows(), origin, left_pos, false, PPQ, ROW_H), Some((0, Hit::Body)));
        assert_eq!(
            hit_note(&notes, &single_track_rows(), origin, right_pos, false, PPQ, ROW_H),
            Some((0, Hit::ResizeRight))
        );

        // ON: 左端が左リサイズになり、右端は引き続き右リサイズ
        assert_eq!(
            hit_note(&notes, &single_track_rows(), origin, left_pos, true, PPQ, ROW_H),
            Some((0, Hit::ResizeLeft))
        );
        assert_eq!(
            hit_note(&notes, &single_track_rows(), origin, right_pos, true, PPQ, ROW_H),
            Some((0, Hit::ResizeRight))
        );
    }

    fn pitched_at(semitone: i32, octave: i32) -> Note {
        Note {
            semitone,
            octave,
            ..placed(0.0, 0)
        }
    }

    /// ↑↓ の半音移調は、半音の上限を超えるとオクターブへ繰り上がること (12平均律)
    #[test]
    fn transpose_carries_into_the_octave() {
        let mut state = EditorState::default(); // 12平均律 (半音 0..=11)
        state.editor.notes = vec![pitched_at(11, 4), pitched_at(0, 4)];
        state.select_many(vec![0, 1]);

        assert!(state.transpose_selection(1));
        assert_eq!(
            (state.editor.notes[0].semitone, state.editor.notes[0].octave),
            (0, 5),
            "(11,4) の1つ上は (0,5)"
        );
        assert_eq!(
            (state.editor.notes[1].semitone, state.editor.notes[1].octave),
            (1, 4)
        );

        // 下げると元に戻る (繰り下がりも同じ規則)
        assert!(state.transpose_selection(-1));
        assert_eq!(
            (state.editor.notes[0].semitone, state.editor.notes[0].octave),
            (11, 4)
        );

        // さらに下げると (0,4) が (11,3) になる
        state.transpose_selection(-1);
        assert_eq!(
            (state.editor.notes[1].semitone, state.editor.notes[1].octave),
            (11, 3)
        );
    }

    /// ボーレン・ピアース (13ステップ) でも同じ規則で繰り上がること
    #[test]
    fn transpose_carries_in_bohlen_pierce() {
        let mut state = EditorState::default();
        state.editor.scale = ScaleMode::BohlenPierce13; // 半音 0..=12
        state.editor.notes = vec![pitched_at(12, 4)];
        state.select_many(vec![0]);

        assert!(state.transpose_selection(1));
        assert_eq!(
            (state.editor.notes[0].semitone, state.editor.notes[0].octave),
            (0, 5),
            "(12,4) の1つ上は (0,5)"
        );

        // Shift+↑ 相当 (1オクターブ = 13ステップ)
        assert!(state.transpose_selection(13));
        assert_eq!(
            (state.editor.notes[0].semitone, state.editor.notes[0].octave),
            (0, 6)
        );
    }

    /// Shift+↑↓ のオクターブ移調と、範囲の頭打ち
    #[test]
    fn transpose_clamps_to_the_octave_range() {
        let mut state = EditorState::default();
        state.editor.notes = vec![pitched_at(0, 7), pitched_at(3, 4)];
        state.select_many(vec![0, 1]);

        // 1オクターブ上げ: どちらも範囲内
        assert!(state.transpose_selection(12));
        assert_eq!(state.editor.notes[0].octave, 8);
        assert_eq!(state.editor.notes[1].octave, 5);

        // さらに上げようとしても、上限のノートが (11,8) を超えないところで止まる。
        // 音程の関係を崩さないよう、選択全体が同じ量だけ抑えられる。
        state.transpose_selection(12);
        assert_eq!(
            (state.editor.notes[0].semitone, state.editor.notes[0].octave),
            (11, 8),
            "上限まで上がって止まること"
        );
        assert_eq!(
            (state.editor.notes[1].semitone, state.editor.notes[1].octave),
            (2, 6),
            "他のノートも同じ量だけ動くこと"
        );

        // 上限に張り付いていれば何も起きない
        assert!(!state.transpose_selection(1));
    }

    /// Alt+ホイールのベロシティ変更は選択全体に効き、1..=127 に収まること
    #[test]
    fn velocity_wheel_changes_selection_within_range() {
        let mut state = EditorState::default();
        state.editor.notes = vec![
            Note {
                velocity: 100,
                ..placed(0.0, 0)
            },
            Note {
                velocity: 20,
                ..placed(1.0, 1)
            },
            Note {
                velocity: 64,
                ..placed(2.0, 2)
            },
        ];
        state.select_many(vec![0, 1]);

        assert!(state.change_selection_velocity(8));
        assert_eq!(state.editor.notes[0].velocity, 108);
        assert_eq!(state.editor.notes[1].velocity, 28);
        assert_eq!(state.editor.notes[2].velocity, 64, "選択外は変わらないこと");

        // 上限・下限で止まる
        state.change_selection_velocity(100);
        assert_eq!(state.editor.notes[0].velocity, 127);
        state.change_selection_velocity(-1000);
        assert_eq!(state.editor.notes[0].velocity, 1, "0 にはならないこと");
        assert_eq!(state.editor.notes[1].velocity, 1);

        // 全員が端に張り付いていれば変化なし
        assert!(!state.change_selection_velocity(-5));
    }

    /// ベロシティの塗りは下端を基準に、127で矩形全体を埋めること
    #[test]
    fn velocity_fill_grows_from_the_bottom() {
        let rect = Rect::from_min_max(Pos2::new(10.0, 100.0), Pos2::new(30.0, 120.0));

        let full = velocity_fill_rect(rect, 127);
        assert_eq!(full, rect, "最大ベロシティでは全体が実塗り");

        let half = velocity_fill_rect(rect, 64);
        assert_eq!(half.bottom(), rect.bottom(), "下端は矩形と揃うこと");
        assert!(
            (half.height() - rect.height() * 64.0 / 127.0).abs() < 1e-4,
            "高さがベロシティに比例すること: {}",
            half.height()
        );

        // 最小ベロシティでも幅は保たれる (ノートの存在が消えない)
        let min = velocity_fill_rect(rect, 1);
        assert_eq!(min.width(), rect.width());
        assert!(min.height() > 0.0);
    }

    /// シークは拍にスナップすること。
    /// 3/8 拍子 (1小節 = 1.5拍分の四分音符) でも小節線に乗ること。
    #[test]
    fn seek_snaps_to_beats_so_bar_lines_are_reachable() {
        // 4/4: 拍 = 四分音符
        assert_eq!(seek_target(2.2, 1.0, None, false), 2.0);
        assert_eq!(seek_target(2.6, 1.0, None, false), 3.0);

        // 3/8: 拍 = 8分音符 (0.5)。小節線は 1.5 / 3.0 に来る
        assert_eq!(seek_target(1.4, 0.5, None, false), 1.5, "小節線に乗ること");
        assert_eq!(seek_target(2.9, 0.5, None, false), 3.0);

        // Alt 押下中はスナップしない
        assert_eq!(seek_target(1.4, 0.5, None, true), 1.4f32 as f64);

        // 選択中ノートの頭が近ければそちらを優先する
        assert_eq!(seek_target(1.42, 0.5, Some(1.4), false), 1.4f32 as f64);
        assert_eq!(seek_target(1.49, 0.5, Some(1.2), false), 1.5, "遠ければ拍が優先");

        // 負にならないこと
        assert_eq!(seek_target(-3.0, 1.0, None, false), 0.0);
    }

    /// 一括リサイズは全員の端を同じ量だけ動かし、音価の相対関係を保つこと。
    /// (尾揃えの結果が右端ドラッグで壊れないことの担保)
    #[test]
    fn resize_moves_every_edge_by_the_same_amount() {
        // 尾揃え後を想定: 開始が違い、終端が 3.5 に揃っている
        let targets = vec![
            (
                0,
                Note {
                    duration: 1.5,
                    ..placed(2.0, 0)
                },
            ),
            (
                1,
                Note {
                    duration: 3.0,
                    ..placed(0.5, 1)
                },
            ),
        ];

        // 掴んだノート (先頭) の終端 3.5 を 4.0 へ → 全員 +0.5
        let dx = resize_delta(&targets, 0.5, 0.25, false);
        assert_eq!(dx, 0.5);
        let ends: Vec<f32> = targets
            .iter()
            .map(|(_, n)| n.end_tick() + dx)
            .collect();
        assert_eq!(ends, vec![4.0, 4.0], "終端が揃ったままであること");

        // 縮めすぎても、いちばん短いノートが snap を下回らない
        let dx = resize_delta(&targets, -10.0, 0.25, false);
        assert_eq!(dx, 0.25 - 1.5);
    }

    /// 左端ドラッグは頭を同じ量だけ動かし、終端は動かさないこと
    #[test]
    fn left_resize_moves_heads_and_keeps_ends() {
        let targets = vec![
            (
                0,
                Note {
                    duration: 1.0,
                    ..placed(1.0, 0)
                },
            ),
            (
                1,
                Note {
                    duration: 2.0,
                    ..placed(0.5, 1)
                },
            ),
        ];

        // 掴んだノートの頭 1.0 を 0.5 へ → 全員 -0.5 (音価は +0.5、終端は不変)
        let dx = resize_delta(&targets, -0.5, 0.25, true);
        assert_eq!(dx, -0.5);
        let ends: Vec<f32> = targets
            .iter()
            .map(|(_, n)| (n.start_tick + dx) + (n.duration - dx))
            .collect();
        assert_eq!(ends, vec![2.0, 2.5], "終端は動かないこと");

        // 左に振り切っても、いちばん頭が早いノートが 0 より前に出ない
        let dx = resize_delta(&targets, -5.0, 0.25, true);
        assert_eq!(dx, -0.5);

        // 右に振り切っても、いちばん短いノートが snap を下回らない
        let dx = resize_delta(&targets, 10.0, 0.25, true);
        assert_eq!(dx, 1.0 - 0.25);
    }

    /// 連符モードのスナップ幅: N 音でスナップ2つ分にちょうど収まること
    #[test]
    fn tuplet_fits_n_notes_in_two_snaps() {
        let mut state = EditorState::default();
        state.snap = 0.5; // 1/8 → 2つ分 = 四分音符1つ

        state.tuplet = 1;
        assert_eq!(state.snap_unit(), 0.5, "オフなら素通し");

        for n in [3u32, 4, 5, 6, 7] {
            state.tuplet = n;
            let span = state.snap_unit() * n as f32;
            assert!(
                (span - 1.0).abs() < 1e-6,
                "{n}連符 {n} 音が四分音符1つ分になること: {span}"
            );
        }

        // 代表値の確認 (8分3連 = 1/3拍、5連符 = 0.2拍)
        state.tuplet = 3;
        assert!((state.snap_unit() - 1.0 / 3.0).abs() < 1e-6);
        state.tuplet = 5;
        assert!((state.snap_unit() - 0.2).abs() < 1e-6);
    }

    /// コピーした控えは、貼り付けても値を保ったまま残ること。
    ///
    /// テキスト化を挟まなくなったので値の往復は自明だが、**何度でも貼れること**は
    /// 実装しだいで壊れる (控えを取り出したまま戻し忘れる)。
    #[test]
    fn copied_notes_can_be_pasted_more_than_once() {
        let mut state = EditorState::default();
        state.editor.notes = vec![placed(2.0, 1), placed(2.5, 3)];
        state.select_many(vec![0, 1]);

        let copied = state.copy_selection().expect("コピーできること");
        assert_eq!(copied.len(), 2);
        assert_eq!(copied[0].start_tick, 0.0, "先頭が 0 の相対位置になること");
        assert_eq!(copied[1].start_tick, 0.5);

        state.paste_notes(&copied, 4.0);
        state.paste_notes(&copied, 8.0);

        assert_eq!(state.editor.notes.len(), 6);
        assert_eq!(state.editor.notes[2].start_tick, 4.0);
        assert_eq!(state.editor.notes[4].start_tick, 8.0);
        assert_eq!(copied[0].start_tick, 0.0, "控えは貼っても変わらないこと");
    }

    /// 貼り付け時に、音階モードで使えない半音を丸めること
    #[test]
    fn paste_clamps_semitone_to_scale() {
        let mut state = EditorState::default(); // 12平均律 (半音 0..=11)
        let notes = vec![Note {
            semitone: 12, // B-P でコピーされた13音目
            ..placed(0.0, 0)
        }];
        state.paste_notes(&notes, 0.0);
        assert_eq!(state.editor.notes[0].semitone, 11);
    }

    /// 一括削除でインデックスがずれず、選ばれていないノートが残ること
    #[test]
    fn delete_selection_removes_only_selected() {
        let mut state = EditorState::default();
        state.editor.notes = vec![placed(0.0, 0), placed(1.0, 1), placed(2.0, 2)];
        state.select_many(vec![0, 2]);

        assert!(state.delete_selection());
        assert_eq!(state.editor.notes.len(), 1);
        assert_eq!(state.editor.notes[0].start_tick, 1.0);
        assert!(state.selection.is_empty());
    }

    /// クリックでの単体選択だけが last_note の追従対象になること
    #[test]
    fn only_direct_selection_tracks_last_note() {
        let mut state = EditorState::default();
        state.editor.notes = vec![placed(0.0, 0), placed(1.0, 1)];

        state.select_single(1);
        assert!(state.track_last_note);
        assert_eq!(state.selected, Some(1));

        // 範囲選択は1個でも追従しない (詳細編集はできるようにする)
        state.select_many(vec![0]);
        assert_eq!(state.selected, Some(0), "1個なら詳細編集の対象にはなること");
        assert!(!state.track_last_note);
    }

    /// ドラッグ中の連続した変更が1ステップにまとまり、アンドゥ/リドゥで往復すること
    #[test]
    fn undo_groups_a_drag_into_one_step() {
        let mut state = EditorState::default();
        state.editor.notes = vec![placed(1.0, 0)];
        state.history.end_frame(&state.editor); // ドラッグ開始前のフレーム境界

        // 1回のドラッグを2フレーム分
        for start in [1.5, 2.0] {
            state.editor.notes[0].start_tick = start;
            state.history.record(EditGroup::Move);
            state.history.end_frame(&state.editor);
        }
        state.history.end_group(); // ドラッグ終了

        assert!(state.history.can_undo());
        assert!(state.history.undo(&mut state.editor));
        assert_eq!(state.editor.notes[0].start_tick, 1.0, "ドラッグ全体が1回で戻ること");
        assert!(!state.history.can_undo());

        assert!(state.history.redo(&mut state.editor));
        assert_eq!(state.editor.notes[0].start_tick, 2.0);
    }

    /// 単発操作は毎回1ステップになり、新しい操作でリドゥが捨てられること
    #[test]
    fn undo_handles_discrete_edits() {
        let mut state = EditorState::default();
        state.history.end_frame(&state.editor);

        for start in [0.0, 1.0] {
            state.history.record(EditGroup::Once);
            state.editor.notes.push(placed(start, 0));
            state.history.end_frame(&state.editor);
        }
        assert_eq!(state.editor.notes.len(), 2);

        state.history.undo(&mut state.editor);
        assert_eq!(state.editor.notes.len(), 1);
        assert!(state.history.can_redo());

        // アンドゥ後に別の操作をしたらリドゥは無効になる
        state.history.record(EditGroup::Once);
        state.editor.notes.push(placed(5.0, 0));
        state.history.end_frame(&state.editor);
        assert!(!state.history.can_redo());

        state.history.undo(&mut state.editor);
        assert_eq!(state.editor.notes.len(), 1);
        state.history.undo(&mut state.editor);
        assert!(state.editor.notes.is_empty());
    }

    /// 音階モードの変更 (半音の丸め) もアンドゥで戻ること
    #[test]
    fn undo_restores_scale_mode() {
        let mut state = EditorState::default();
        state.editor.notes = vec![Note {
            semitone: 11,
            ..placed(0.0, 0)
        }];
        state.history.end_frame(&state.editor);

        state.history.record(EditGroup::Once);
        state.editor.scale = ScaleMode::BohlenPierce13;
        state.history.end_frame(&state.editor);

        state.history.undo(&mut state.editor);
        assert_eq!(state.editor.scale, ScaleMode::Equal12);
        assert_eq!(state.editor.notes[0].semitone, 11);
    }

    /// 音階モードでステップ数が変わっても、オクターブ内の相対位置で色が決まること
    #[test]
    fn note_fill_uses_scale_steps() {
        // 差が丸めに埋もれないよう、オクターブ内で高い位置を見る
        let eq = note_fill(&pitched(11, 4), ScaleMode::Equal12); // 11/12 ≈ 0.92
        let bp = note_fill(&pitched(11, 4), ScaleMode::BohlenPierce13); // 11/13 ≈ 0.85
        assert_ne!(eq, bp, "ステップ数の違いが色に反映されること");
    }

    #[test]
    fn no_edge_scroll_in_the_middle() {
        let delta = edge_scroll_delta(viewport(), Pos2::new(500.0, 200.0), true);
        assert_eq!(delta, egui::Vec2::ZERO);
    }

    /// 右端では先の内容を見せたいので x は負、左端では正になる
    #[test]
    fn edge_scroll_direction() {
        let right = edge_scroll_delta(viewport(), Pos2::new(995.0, 200.0), true);
        assert!(right.x < 0.0, "右端では x が負になること: {right:?}");
        assert_eq!(right.y, 0.0);

        let left = edge_scroll_delta(viewport(), Pos2::new(5.0, 200.0), true);
        assert!(left.x > 0.0, "左端では x が正になること: {left:?}");

        let bottom = edge_scroll_delta(viewport(), Pos2::new(500.0, 398.0), true);
        assert!(bottom.y < 0.0, "下端では y が負になること: {bottom:?}");
    }

    /// 端に近いほど速くなること
    #[test]
    fn edge_scroll_accelerates_toward_edge() {
        let shallow = edge_scroll_delta(viewport(), Pos2::new(960.0, 200.0), true);
        let deep = edge_scroll_delta(viewport(), Pos2::new(999.0, 200.0), true);
        assert!(
            deep.x < shallow.x,
            "端に近いほど速いこと: 浅 {shallow:?} / 深 {deep:?}"
        );
    }

    /// 音価変更 (Resize) では縦スクロールしない
    #[test]
    fn edge_scroll_horizontal_only_when_requested() {
        let delta = edge_scroll_delta(viewport(), Pos2::new(500.0, 398.0), false);
        assert_eq!(delta.y, 0.0);
    }
}
