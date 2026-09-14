//! 简单的耗时测量工具。
//!
//! `Timer` 使用单调时钟 `Instant`，只适合统计持续时间，不参与线上时间戳或最近上下线时间计算。

use std::time::Instant;

pub struct Timer {
    start: Instant,
}

impl Timer {
    pub fn new() -> Timer {
        Timer {
            start: Instant::now(),
        }
    }

    pub fn elapsed(&self, unit: TimeUnit) -> f64 {
        let elapsed = self.start.elapsed();
        match unit {
            TimeUnit::Seconds => elapsed.as_secs_f64(),
            TimeUnit::Milliseconds => elapsed.as_millis() as f64,
            TimeUnit::Microseconds => elapsed.as_micros() as f64,
        }
    }

    pub fn reset(&mut self) {
        self.start = Instant::now();
    }
}

impl Default for Timer {
    fn default() -> Self {
        Self::new()
    }
}

pub enum TimeUnit {
    Seconds,
    Milliseconds,
    Microseconds,
}
