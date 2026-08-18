//! アンドゥ / リドゥ履歴。
//!
//! 各操作の側で「変更前」を用意しなくて済むよう、**フレーム境界で控えを取る**
//! 作りにしてある。呼ぶ側は変更したことを [`History::record`] で伝えるだけでよい。

use crate::sequencer::{MidiEditor, Note, ScaleMode, TrackInfo};

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
pub(super) enum EditGroup {
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
pub(super) struct History {
    /// 今フレームの開始時点の状態
    baseline: Snapshot,
    undo: Vec<Snapshot>,
    redo: Vec<Snapshot>,
    /// まとめ中の操作区分
    group: Option<EditGroup>,
}

impl History {
    pub(super) fn new(editor: &MidiEditor) -> Self {
        Self {
            baseline: Snapshot::capture(editor),
            undo: Vec::new(),
            redo: Vec::new(),
            group: None,
        }
    }

    /// 変更が起きたことを記録する。ドラッグのように毎フレーム呼ばれても、
    /// 同じ区分が続く間は1ステップにまとまる。
    pub(super) fn record(&mut self, group: EditGroup) {
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
    pub(super) fn end_group(&mut self) {
        self.group = None;
    }

    pub(super) fn can_undo(&self) -> bool {
        !self.undo.is_empty()
    }

    pub(super) fn can_redo(&self) -> bool {
        !self.redo.is_empty()
    }

    pub(super) fn undo(&mut self, editor: &mut MidiEditor) -> bool {
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

    pub(super) fn redo(&mut self, editor: &mut MidiEditor) -> bool {
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
    pub(super) fn end_frame(&mut self, editor: &MidiEditor) {
        if self.baseline.notes != editor.notes
            || self.baseline.tracks != editor.tracks
            || self.baseline.scale != editor.scale
        {
            self.baseline = Snapshot::capture(editor);
        }
    }
}
