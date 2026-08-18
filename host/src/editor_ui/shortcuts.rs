//! キーボードショートカットと、再生の開始・停止。

use super::history::EditGroup;
use super::state::EditorState;
use super::EditorCommand;
use eframe::egui;

/// クリップボードが空のときにだけ置く印。
///
/// 中身に意味は無く、**Ctrl+V を発火させるためだけ**のもの。他所へ貼られても
/// 何が起きたか分かる文言にしてある。
const CLIPBOARD_MARKER: &str = "egui-clap-host: ノートをコピーしました";

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

/// 再生を始める。`from` を渡すとその位置へ移動してから再生する。
/// 停止したときに戻れるよう、開始時点の再生ヘッド位置を覚えておく。
pub(super) fn start_playback(
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
pub(super) fn stop_playback(state: &mut EditorState, commands: &mut Vec<EditorCommand>) {
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
pub(super) fn shortcuts(
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
