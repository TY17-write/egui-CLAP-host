//! 画面の寸法と、ズームの下限・上限。
//!
//! **複数のファイルから使う定数だけをここに置く。** 1箇所でしか使わないものは
//! 使う側のファイルに残してある (例: 段の帯の幅は [`gutter`](super::gutter))。

/// 1四分音符の横幅の既定値 (ピクセル)。
/// 実際の幅は `EditorState::ppq` (横ズームで変わる)
pub(super) const PPQ: f32 = 80.0;
/// 1四分音符の横幅の下限。小節線が潰れて拍が読めなくなるので、これ以上は縮めない
pub(super) const MIN_PPQ: f32 = 16.0;
/// 1四分音符の横幅の上限
pub(super) const MAX_PPQ: f32 = 480.0;
/// Ctrl+中ドラッグ 1px あたりの横ズーム倍率。
///
/// 画面の端から端まで (およそ 900px) 動かして 200 倍前後になる大きさ。
/// これより小さいと、下限から上限まで動かすのに何往復も必要になる。
pub(super) const PPQ_ZOOM_PER_PIXEL: f32 = 1.006;
/// 段の高さの既定値。実際の高さは `EditorState::row_h` (縦ズームで変わる)
pub(super) const ROW_H: f32 = 24.0;
/// 段の高さの下限。左のトラック欄のボタンが潰れるので、これ以上は縮めない
pub(super) const MIN_ROW_H: f32 = 12.0;
/// 段の高さの上限
pub(super) const MAX_ROW_H: f32 = 96.0;
/// Ctrl+ホイール1ノッチあたりの倍率 (段の高さ)
pub(super) const ROW_ZOOM_STEP: f32 = 1.15;
/// ルーラーの高さ
pub(super) const RULER_H: f32 = 22.0;

/// 左のトラック欄の幅。
///
/// **入れるものを数えて決めてある。** 名前の入力欄 + 切り替え4つ (M/S/W/V) +
/// 印が1行に収まる幅。ここを削ると名前が「ト…」になって用をなさなくなり、
/// 増やすとグリッドが狭くなる。項目を足すときは実際に見て確かめること。
pub(super) const GUTTER_W: f32 = 230.0;

/// 指定できるオクターブの範囲
pub(super) const MIN_OCTAVE: i32 = -2;
pub(super) const MAX_OCTAVE: i32 = 8;
