use std::time::{Duration, Instant};

use crate::wire::WirePacket;

#[derive(Debug, Clone)]
pub struct DebugStatsConfig {
    /// 仅 debug 构建默认开启。release 构建保持安静，除非调用方显式开启。
    pub enabled: bool,
    /// 各统计阶段两次日志输出之间的最小间隔。
    pub interval: Duration,
}

impl Default for DebugStatsConfig {
    fn default() -> Self {
        Self {
            enabled: cfg!(debug_assertions),
            interval: Duration::from_secs(1),
        }
    }
}

impl DebugStatsConfig {
    pub fn disabled() -> Self {
        Self {
            enabled: false,
            interval: Duration::from_secs(1),
        }
    }

    pub fn every(interval: Duration) -> Self {
        Self {
            enabled: true,
            interval,
        }
    }

    fn active(&self) -> bool {
        self.enabled && !self.interval.is_zero()
    }
}

#[derive(Default)]
struct DurationCounter {
    count: u64,
    total: Duration,
    max: Duration,
}

impl DurationCounter {
    fn add(&mut self, value: Duration) {
        self.count += 1;
        self.total += value;
        self.max = self.max.max(value);
    }

    fn avg_ms(&self) -> f64 {
        if self.count == 0 {
            0.0
        } else {
            self.total.as_secs_f64() * 1_000.0 / self.count as f64
        }
    }

    fn max_ms(&self) -> f64 {
        self.max.as_secs_f64() * 1_000.0
    }

    fn reset(&mut self) {
        *self = Self::default();
    }
}

pub(crate) struct CaptureDebugStats {
    config: DebugStatsConfig,
    last_report: Instant,
    acquired: u64,
    fps_limited: u64,
    encoded: u64,
    encoder_skipped: u64,
    queue_dropped: u64,
    keyframes: u64,
    payload_bytes: u64,
    encode_time: DurationCounter,
}

impl CaptureDebugStats {
    pub(crate) fn new(config: DebugStatsConfig) -> Self {
        Self {
            config,
            last_report: Instant::now(),
            acquired: 0,
            fps_limited: 0,
            encoded: 0,
            encoder_skipped: 0,
            queue_dropped: 0,
            keyframes: 0,
            payload_bytes: 0,
            encode_time: DurationCounter::default(),
        }
    }

    pub(crate) fn on_acquired(&mut self) {
        self.acquired += 1;
        self.maybe_report();
    }

    pub(crate) fn on_fps_limited(&mut self) {
        self.fps_limited += 1;
        self.maybe_report();
    }

    pub(crate) fn on_encoded(&mut self, payload_len: usize, is_keyframe: bool, elapsed: Duration) {
        self.encoded += 1;
        self.payload_bytes += payload_len as u64;
        if is_keyframe {
            self.keyframes += 1;
        }
        self.encode_time.add(elapsed);
        self.maybe_report();
    }

    pub(crate) fn on_encoder_skipped(&mut self, elapsed: Duration) {
        self.encoder_skipped += 1;
        self.encode_time.add(elapsed);
        self.maybe_report();
    }

    pub(crate) fn on_queue_dropped(&mut self) {
        self.queue_dropped += 1;
        self.maybe_report();
    }

    fn maybe_report(&mut self) {
        if !self.config.active() || self.last_report.elapsed() < self.config.interval {
            return;
        }

        let elapsed = self.last_report.elapsed();
        eprintln!(
            "[screen_stream capture] acquired={:.1}/s encoded={:.1}/s encoder_skip={:.1}/s fps_limited={:.1}/s queue_drop={:.1}/s payload={:.2}Mbps avg_packet={:.1}KiB keyframes={} encode_avg={:.2}ms encode_max={:.2}ms",
            rate(self.acquired, elapsed),
            rate(self.encoded, elapsed),
            rate(self.encoder_skipped, elapsed),
            rate(self.fps_limited, elapsed),
            rate(self.queue_dropped, elapsed),
            mbps(self.payload_bytes, elapsed),
            avg_kib(self.payload_bytes, self.encoded),
            self.keyframes,
            self.encode_time.avg_ms(),
            self.encode_time.max_ms(),
        );
        self.reset();
    }

    fn reset(&mut self) {
        self.last_report = Instant::now();
        self.acquired = 0;
        self.fps_limited = 0;
        self.encoded = 0;
        self.encoder_skipped = 0;
        self.queue_dropped = 0;
        self.keyframes = 0;
        self.payload_bytes = 0;
        self.encode_time.reset();
    }
}

pub(crate) struct TransportDebugStats {
    config: DebugStatsConfig,
    label: &'static str,
    last_report: Instant,
    packets: u64,
    video_packets: u64,
    hello_packets: u64,
    keyframes: u64,
    wire_bytes: u64,
    write_time: DurationCounter,
}

impl TransportDebugStats {
    pub(crate) fn new(config: DebugStatsConfig, label: &'static str) -> Self {
        Self {
            config,
            label,
            last_report: Instant::now(),
            packets: 0,
            video_packets: 0,
            hello_packets: 0,
            keyframes: 0,
            wire_bytes: 0,
            write_time: DurationCounter::default(),
        }
    }

    pub(crate) fn on_sent(&mut self, packet: &WirePacket, wire_bytes: usize, elapsed: Duration) {
        self.packets += 1;
        self.wire_bytes += wire_bytes as u64;
        self.write_time.add(elapsed);
        match packet {
            WirePacket::Hello(_) => self.hello_packets += 1,
            WirePacket::Video(frame) => {
                self.video_packets += 1;
                if frame.is_keyframe {
                    self.keyframes += 1;
                }
            }
        }
        self.maybe_report();
    }

    fn maybe_report(&mut self) {
        if !self.config.active() || self.last_report.elapsed() < self.config.interval {
            return;
        }

        let elapsed = self.last_report.elapsed();
        eprintln!(
            "[screen_stream {}] wire={:.2}Mbps packets={:.1}/s video={:.1}/s hello={} keyframes={} avg_packet={:.1}KiB write_avg={:.2}ms write_max={:.2}ms",
            self.label,
            mbps(self.wire_bytes, elapsed),
            rate(self.packets, elapsed),
            rate(self.video_packets, elapsed),
            self.hello_packets,
            self.keyframes,
            avg_kib(self.wire_bytes, self.packets),
            self.write_time.avg_ms(),
            self.write_time.max_ms(),
        );
        self.reset();
    }

    fn reset(&mut self) {
        self.last_report = Instant::now();
        self.packets = 0;
        self.video_packets = 0;
        self.hello_packets = 0;
        self.keyframes = 0;
        self.wire_bytes = 0;
        self.write_time.reset();
    }
}

pub(crate) struct ReceiveDebugStats {
    config: DebugStatsConfig,
    last_report: Instant,
    packets: u64,
    video_packets: u64,
    hello_packets: u64,
    keyframes: u64,
    decoded_frames: u64,
    wire_bytes: u64,
    decoded_rgba_bytes: u64,
    seq_gaps: u64,
    out_of_order: u64,
    last_seq: Option<u64>,
    decode_time: DurationCounter,
}

impl ReceiveDebugStats {
    pub(crate) fn new(config: DebugStatsConfig) -> Self {
        Self {
            config,
            last_report: Instant::now(),
            packets: 0,
            video_packets: 0,
            hello_packets: 0,
            keyframes: 0,
            decoded_frames: 0,
            wire_bytes: 0,
            decoded_rgba_bytes: 0,
            seq_gaps: 0,
            out_of_order: 0,
            last_seq: None,
            decode_time: DurationCounter::default(),
        }
    }

    pub(crate) fn on_packet(&mut self, packet: &WirePacket, wire_bytes: usize) {
        self.packets += 1;
        self.wire_bytes += wire_bytes as u64;
        match packet {
            WirePacket::Hello(_) => self.hello_packets += 1,
            WirePacket::Video(frame) => {
                self.video_packets += 1;
                if frame.is_keyframe {
                    self.keyframes += 1;
                }
                if let Some(prev) = self.last_seq {
                    if frame.seq > prev {
                        self.seq_gaps += frame.seq.saturating_sub(prev).saturating_sub(1);
                    } else if frame.seq != prev {
                        self.out_of_order += 1;
                    }
                }
                self.last_seq = Some(frame.seq);
            }
        }
        self.maybe_report();
    }

    pub(crate) fn on_decoded(&mut self, rgba_bytes: usize, elapsed: Duration) {
        self.decoded_frames += 1;
        self.decoded_rgba_bytes += rgba_bytes as u64;
        self.decode_time.add(elapsed);
        self.maybe_report();
    }

    fn maybe_report(&mut self) {
        if !self.config.active() || self.last_report.elapsed() < self.config.interval {
            return;
        }

        let elapsed = self.last_report.elapsed();
        eprintln!(
            "[screen_stream receive] wire={:.2}Mbps packets={:.1}/s video={:.1}/s decoded={:.1}/s hello={} keyframes={} seq_gap={} out_of_order={} rgba={:.2}MB/s decode_avg={:.2}ms decode_max={:.2}ms",
            mbps(self.wire_bytes, elapsed),
            rate(self.packets, elapsed),
            rate(self.video_packets, elapsed),
            rate(self.decoded_frames, elapsed),
            self.hello_packets,
            self.keyframes,
            self.seq_gaps,
            self.out_of_order,
            megabytes_per_sec(self.decoded_rgba_bytes, elapsed),
            self.decode_time.avg_ms(),
            self.decode_time.max_ms(),
        );
        self.reset();
    }

    fn reset(&mut self) {
        self.last_report = Instant::now();
        self.packets = 0;
        self.video_packets = 0;
        self.hello_packets = 0;
        self.keyframes = 0;
        self.decoded_frames = 0;
        self.wire_bytes = 0;
        self.decoded_rgba_bytes = 0;
        self.seq_gaps = 0;
        self.out_of_order = 0;
        self.decode_time.reset();
    }
}

pub(crate) struct RenderDebugStats {
    config: DebugStatsConfig,
    last_report: Instant,
    paints: u64,
    presented: u64,
    duplicate_paints: u64,
    empty_paints: u64,
    last_seq: Option<u64>,
}

impl RenderDebugStats {
    pub(crate) fn new(config: DebugStatsConfig) -> Self {
        Self {
            config,
            last_report: Instant::now(),
            paints: 0,
            presented: 0,
            duplicate_paints: 0,
            empty_paints: 0,
            last_seq: None,
        }
    }

    pub(crate) fn on_paint(&mut self, seq: Option<u64>) {
        self.paints += 1;
        match seq {
            Some(seq) if Some(seq) == self.last_seq => self.duplicate_paints += 1,
            Some(seq) => {
                self.presented += 1;
                self.last_seq = Some(seq);
            }
            None => self.empty_paints += 1,
        }
        self.maybe_report();
    }

    fn maybe_report(&mut self) {
        if !self.config.active() || self.last_report.elapsed() < self.config.interval {
            return;
        }

        let elapsed = self.last_report.elapsed();
        eprintln!(
            "[screen_stream render] presented={:.1}/s paints={:.1}/s duplicate_paints={} empty_paints={}",
            rate(self.presented, elapsed),
            rate(self.paints, elapsed),
            self.duplicate_paints,
            self.empty_paints,
        );
        self.reset();
    }

    fn reset(&mut self) {
        self.last_report = Instant::now();
        self.paints = 0;
        self.presented = 0;
        self.duplicate_paints = 0;
        self.empty_paints = 0;
    }
}

fn rate(count: u64, elapsed: Duration) -> f64 {
    count as f64 / elapsed.as_secs_f64().max(f64::EPSILON)
}

fn mbps(bytes: u64, elapsed: Duration) -> f64 {
    bytes as f64 * 8.0 / elapsed.as_secs_f64().max(f64::EPSILON) / 1_000_000.0
}

fn megabytes_per_sec(bytes: u64, elapsed: Duration) -> f64 {
    bytes as f64 / elapsed.as_secs_f64().max(f64::EPSILON) / 1_000_000.0
}

fn avg_kib(bytes: u64, count: u64) -> f64 {
    if count == 0 {
        0.0
    } else {
        bytes as f64 / count as f64 / 1024.0
    }
}
