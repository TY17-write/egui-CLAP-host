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
    /// パラメータの変更
    Param {
        offset: u32,
        id: u32,
        value: f64,
    },
}

/// 1ブロック分のイベント。
///
/// オーディオスレッドで使うので、容量を事前に確保して毎ブロック `clear` する
/// (確保が起きるのは容量を超えたときだけ)。
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
}
