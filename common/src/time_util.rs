use std::time::{Instant, Duration};

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

pub enum TimeUnit {
    Seconds,
    Milliseconds,
    Microseconds,
}