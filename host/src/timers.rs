//! clap.timer-support 拡張のホスト側実装。
//! JUCE 系など、GUI の描画にタイマーを必要とするプラグインのために必要。
//! clack リポジトリの cpal サンプル (MIT OR Apache-2.0) から移植。

use clack_extensions::timer::{PluginTimer, TimerId};
use clack_host::prelude::PluginMainThreadHandle;
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::time::{Duration, Instant};

/// 登録されたタイマーの一覧を管理する (メインスレッド専用)
pub struct Timers {
    timers: RefCell<HashMap<TimerId, Timer>>,
    latest_id: Cell<u32>,
    smallest_duration: Cell<Option<Duration>>,
}

impl Timers {
    pub fn new() -> Self {
        Self {
            timers: RefCell::new(HashMap::new()),
            latest_id: Cell::new(0),
            smallest_duration: Cell::new(None),
        }
    }

    /// 期限が来たタイマーの ID を返す
    fn tick_all(&self) -> Vec<TimerId> {
        let mut timers = self.timers.borrow_mut();
        let now = Instant::now();

        timers
            .values_mut()
            .filter_map(move |t| t.tick(now).then_some(t.id))
            .collect()
    }

    /// 期限が来たタイマーすべてについて、プラグインの on_timer コールバックを呼ぶ
    pub fn tick_timers(&self, timer_ext: &PluginTimer, plugin: &mut PluginMainThreadHandle) {
        for triggered in self.tick_all() {
            timer_ext.on_timer(plugin, triggered);
        }
    }

    pub fn register_new(&self, interval: Duration) -> TimerId {
        // 仕様上の推奨は 30ms 以下。UI 更新を滑らかにするため下限 10ms とする
        const MIN_INTERVAL: Duration = Duration::from_millis(10);
        let interval = interval.max(MIN_INTERVAL);

        let latest_id = self.latest_id.get() + 1;
        self.latest_id.set(latest_id);
        let id = TimerId(latest_id);

        self.timers
            .borrow_mut()
            .insert(id, Timer::new(id, interval));

        match self.smallest_duration.get() {
            None => self.smallest_duration.set(Some(interval)),
            Some(smallest) if smallest > interval => self.smallest_duration.set(Some(interval)),
            _ => {}
        }

        id
    }

    /// タイマーを解除する。指定 ID が存在したかを返す。
    pub fn unregister(&self, id: TimerId) -> bool {
        let mut timers = self.timers.borrow_mut();
        if timers.remove(&id).is_some() {
            self.smallest_duration
                .set(timers.values().map(|t| t.interval).min());
            true
        } else {
            false
        }
    }
}

impl Default for Timers {
    fn default() -> Self {
        Self::new()
    }
}

struct Timer {
    id: TimerId,
    interval: Duration,
    last_triggered_at: Option<Instant>,
}

impl Timer {
    fn new(id: TimerId, interval: Duration) -> Self {
        Self {
            id,
            interval,
            last_triggered_at: None,
        }
    }

    fn tick(&mut self, now: Instant) -> bool {
        let triggered = if let Some(last_updated_at) = self.last_triggered_at {
            now.checked_duration_since(last_updated_at)
                .map(|since| since > self.interval)
                .unwrap_or(false)
        } else {
            true
        };

        if triggered {
            self.last_triggered_at = Some(now);
        }

        triggered
    }
}
