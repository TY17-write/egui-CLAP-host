//! バックエンドに依らない、1ブロック分のイベント列。
//!
//! トランスポートと GUI はここへ書き、各バックエンド (CLAP / VST3) が
//! 自分の形式へ移す。CLAP と VST3 ではイベントの表し方が違うので、
//! 間に中立の形を挟んでおく。
//!
//! 時刻はすべて**ブロック内のサンプルオフセット**。

/// ブロック内の1イベント
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum BlockEvent {
    NoteOn {
        offset: u32,
        key: u8,
        /// 0.0..=1.0
        velocity: f64,
    },
    NoteOff {
        offset: u32,
        key: u8,
    },
    /// 鳴っている音を全部止める (停止・シーク・シーケンス差し替えのとき)。
    ///
    /// CLAP には NoteChoke があるが VST3 には相当するものが無いので、
    /// バックエンド側で表現を変える。
    ///
    /// **効かせた CC の解除もここで行う。** ペダルを踏んだまま止めると
    /// 踏みっぱなしで残るため、バックエンドが自分の出した CC を覚えておき、
    /// ここで解除値に戻す (音符の choke と同じ考え方)。
    Choke {
        offset: u32,
    },
    /// コントロールチェンジ (CC 段が出す)。
    ///
    /// VST3 は `IMidiMapping` 経由でパラメータへ、CLAP は生 MIDI へ、と
    /// 送り方が全く違うのでここでは番号と値だけを持つ。
    Cc {
        offset: u32,
        /// CC 番号 0..=127
        number: u8,
        /// 値 0..=127
        value: u8,
    },
    /// パラメータの変更。
    ///
    /// **これだけは宛先を持つ。** 音符と CC はトラックの全ノードへ配ってよい
    /// (受け取れないノードは自分で捨てる) が、パラメータは**どのノードのものか
    /// 決まっている**。チェーンに同じエフェクトを2段刺したとき、片方だけを
    /// 動かせないと困る。
    Param {
        offset: u32,
        /// チェーンの何段目に宛てたものか (0 が先頭)
        node: usize,
        id: u32,
        value: f64,
    },
}

impl BlockEvent {
    /// ブロック内のサンプルオフセット
    pub fn offset(&self) -> u32 {
        match self {
            BlockEvent::NoteOn { offset, .. }
            | BlockEvent::NoteOff { offset, .. }
            | BlockEvent::Choke { offset }
            | BlockEvent::Cc { offset, .. }
            | BlockEvent::Param { offset, .. } => *offset,
        }
    }
}

/// 1ブロック分のイベント。
///
/// オーディオスレッドで使うので、容量を事前に確保して毎ブロック `clear` する
/// (確保が起きるのは容量を超えたときだけ)。
///
/// **中身はオフセットの昇順に保つ。** CLAP も VST3 も時刻順で渡す前提なので、
/// 打ち込みを複数まとめるときは [`merge_tail`](Self::merge_tail) で溶かし込む。
#[derive(Debug)]
pub struct BlockEvents {
    events: Vec<BlockEvent>,
}

impl BlockEvents {
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            events: Vec::with_capacity(capacity),
        }
    }

    pub fn clear(&mut self) {
        self.events.clear();
    }

    pub fn push(&mut self, event: BlockEvent) {
        self.events.push(event);
    }

    pub fn is_empty(&self) -> bool {
        self.events.is_empty()
    }

    pub fn len(&self) -> usize {
        self.events.len()
    }

    pub fn iter(&self) -> std::slice::Iter<'_, BlockEvent> {
        self.events.iter()
    }

    /// 昇順の列 `add` をこの列へ溶かし込む。**この列も昇順であること**が前提。
    ///
    /// 打ち込みを複数受けるトラックで使う。1本ぶんずつ取り出した列はそれぞれ
    /// 昇順なので、これを繰り返せば全体が昇順に保たれる。
    ///
    /// **確保も一時領域も要らない** (オーディオスレッドで呼ぶ)。後ろから挿していき、
    /// 挿し込む位置は左へ進む一方なので、位置探しは全体で1往復に収まる。
    ///
    /// **同じオフセットでは元からあるほうが先に残る。** 同時刻の NoteOff と
    /// NoteOn が入れ替わると、同じ音を連打したときに切れない・出ないという形で
    /// 音に出る。並べ替えで済ませるとここが壊れる (`sort_unstable` は同着の順を
    /// 保たず、`sort` は確保する)。
    pub fn merge(&mut self, add: &BlockEvents) {
        if add.events.is_empty() {
            return;
        }
        if self.events.is_empty() {
            self.events.extend_from_slice(&add.events);
            return;
        }

        // 大きいほうから挿す。挿し込み位置は左へしか動かない
        let mut at = self.events.len();
        for event in add.events.iter().rev() {
            while at > 0 && self.events[at - 1].offset() > event.offset() {
                at -= 1;
            }
            // 同着の後ろへ入れるので、元からあるものが前に残る
            self.events.insert(at, *event);
        }
    }
}

impl<'a> IntoIterator for &'a BlockEvents {
    type Item = &'a BlockEvent;
    type IntoIter = std::slice::Iter<'a, BlockEvent>;

    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// clear しても確保済みの容量は残ること (オーディオスレッドで確保しないため)
    #[test]
    fn clearing_keeps_the_capacity() {
        let mut events = BlockEvents::with_capacity(64);
        for offset in 0..64 {
            events.push(BlockEvent::NoteOn {
                offset,
                key: 60,
                velocity: 1.0,
            });
        }
        assert_eq!(events.len(), 64);

        events.clear();
        assert!(events.is_empty());
        assert!(
            events.events.capacity() >= 64,
            "容量が減らないこと (減ると次のブロックで確保が起きる)"
        );
    }

    fn on(offset: u32) -> BlockEvent {
        BlockEvent::NoteOn {
            offset,
            key: 60,
            velocity: 1.0,
        }
    }

    fn off(offset: u32) -> BlockEvent {
        BlockEvent::NoteOff { offset, key: 60 }
    }

    fn offsets(events: &BlockEvents) -> Vec<u32> {
        events.iter().map(|event| event.offset()).collect()
    }

    /// オフセットだけを並べた列を作る
    fn run(offsets: &[u32]) -> BlockEvents {
        let mut events = BlockEvents::with_capacity(64);
        for offset in offsets {
            events.push(on(*offset));
        }
        events
    }

    /// 2本の昇順の列が、1本の昇順の列になること
    #[test]
    fn merging_interleaves_by_offset() {
        let mut events = run(&[0, 30, 90]);
        events.merge(&run(&[10, 20, 100]));

        assert_eq!(offsets(&events), vec![0, 10, 20, 30, 90, 100]);
    }

    /// **同じオフセットでは元からあるほうが先に残ること。**
    ///
    /// 同時刻の NoteOff と NoteOn が入れ替わると、連打した音が切れない・
    /// 出ないという形で音に出る。並べ替えでは守れない性質。
    #[test]
    fn events_at_the_same_offset_keep_their_order() {
        let mut events = BlockEvents::with_capacity(16);
        // 元の列: 同じ位置で切ってから鳴らす (連打)
        events.push(off(48));
        events.push(on(48));

        // 溶かし込む側: 同じ位置に別の打ち込みの音
        let mut add = BlockEvents::with_capacity(16);
        add.push(on(48));
        events.merge(&add);

        let kinds: Vec<&str> = events
            .iter()
            .map(|event| match event {
                BlockEvent::NoteOff { .. } => "off",
                _ => "on",
            })
            .collect();
        assert_eq!(kinds, vec!["off", "on", "on"], "元の off→on が保たれること");
    }

    /// 溶かし込む側が全部前に来る場合と、全部後ろに来る場合
    #[test]
    fn merging_handles_runs_that_do_not_overlap() {
        let mut front = run(&[500, 600]);
        front.merge(&run(&[100, 200]));
        assert_eq!(offsets(&front), vec![100, 200, 500, 600]);

        let mut back = run(&[100, 200]);
        back.merge(&run(&[500, 600]));
        assert_eq!(offsets(&back), vec![100, 200, 500, 600]);
    }

    /// 片方が空なら何も動かないこと
    #[test]
    fn merging_with_an_empty_side_changes_nothing() {
        let mut events = run(&[10, 20]);
        events.merge(&BlockEvents::with_capacity(4));
        assert_eq!(offsets(&events), vec![10, 20]);

        let mut empty = BlockEvents::with_capacity(4);
        empty.merge(&run(&[10, 20]));
        assert_eq!(offsets(&empty), vec![10, 20]);
    }

    /// マージで確保が起きないこと (オーディオスレッドで呼ぶため)
    #[test]
    fn merging_does_not_allocate() {
        let mut events = BlockEvents::with_capacity(64);
        for offset in 0..16 {
            events.push(on(offset * 4));
        }
        let mut add = BlockEvents::with_capacity(64);
        for offset in 0..16 {
            add.push(on(offset * 4 + 1));
        }
        let before = events.events.capacity();

        events.merge(&add);

        assert_eq!(events.events.capacity(), before, "容量が動かないこと");
        assert_eq!(events.len(), 32);
        assert!(
            offsets(&events).windows(2).all(|pair| pair[0] <= pair[1]),
            "昇順であること"
        );
    }
}
