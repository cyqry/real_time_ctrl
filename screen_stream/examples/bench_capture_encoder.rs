use std::time::{Duration, Instant};

use screen_stream::{
    codec::{h264::H264Encoder, RawBgraFrame, RawD3D11Frame},
    CaptureConfig, EncoderBackend, Result,
};
use windows_capture::{
    dxgi_duplication_api::{DxgiDuplicationApi, Error as DxgiError},
    monitor::Monitor,
};

const DEFAULT_FRAMES: u64 = 120;
const DEFAULT_WARMUP: u64 = 20;

#[derive(Debug, Clone)]
struct Options {
    frames: u64,
    warmup: u64,
    backends: Vec<BenchBackend>,
}

#[derive(Debug, Clone, Copy)]
enum BenchBackend {
    MfCpu,
    MfD3D11,
}

#[derive(Debug)]
struct BenchResult {
    backend: BenchBackend,
    submitted: u64,
    produced: u64,
    skipped: u64,
    keyframes: u64,
    payload_bytes: u64,
    avg: Duration,
    p50: Duration,
    p95: Duration,
    max: Duration,
}

fn main() -> Result<()> {
    let options = parse_options();

    println!(
        "bench_capture_encoder frames={} warmup={} backends={}",
        options.frames,
        options.warmup,
        options
            .backends
            .iter()
            .map(|backend| backend.name())
            .collect::<Vec<_>>()
            .join(",")
    );

    let mut results = Vec::new();
    for backend in &options.backends {
        results.push(run_backend(*backend, &options)?);
    }

    println!();
    println!(
        "{:<10} {:>9} {:>9} {:>9} {:>9} {:>10} {:>10} {:>10} {:>10} {:>11} {:>12}",
        "backend",
        "submitted",
        "produced",
        "skipped",
        "keyframes",
        "avg_ms",
        "p50_ms",
        "p95_ms",
        "max_ms",
        "payloadMiB",
        "avgKiB"
    );
    for result in &results {
        println!(
            "{:<10} {:>9} {:>9} {:>9} {:>9} {:>10.3} {:>10.3} {:>10.3} {:>10.3} {:>11.2} {:>12.1}",
            result.backend.name(),
            result.submitted,
            result.produced,
            result.skipped,
            result.keyframes,
            ms(result.avg),
            ms(result.p50),
            ms(result.p95),
            ms(result.max),
            result.payload_bytes as f64 / 1024.0 / 1024.0,
            avg_kib(result.payload_bytes, result.produced),
        );
    }

    if let (Some(cpu), Some(gpu)) = (
        results
            .iter()
            .find(|result| matches!(result.backend, BenchBackend::MfCpu)),
        results
            .iter()
            .find(|result| matches!(result.backend, BenchBackend::MfD3D11)),
    ) {
        println!(
            "\nsummary d3d11_vs_cpu: avg_ms_speedup={:.2}x",
            ms(cpu.avg) / ms(gpu.avg).max(f64::EPSILON),
        );
    }

    Ok(())
}

fn run_backend(backend: BenchBackend, options: &Options) -> Result<BenchResult> {
    let monitor = Monitor::primary().map_err(|err| {
        screen_stream::ScreenStreamError::InvalidFrame(format!("primary monitor not found: {err}"))
    })?;
    let mut duplication = DxgiDuplicationApi::new(monitor)?;
    let mut config = CaptureConfig::smooth();
    config.encoder_backend = EncoderBackend::MediaFoundation;
    config.debug_stats.enabled = false;
    let mut encoder = H264Encoder::new(config.encoder_config())?;
    let mut frame_buffer = Vec::new();
    let started = Instant::now();
    let mut seq = 0_u64;

    for _ in 0..options.warmup {
        let frame = next_frame(&mut duplication, config.acquire_timeout_ms)?;
        let _ = encode_frame(
            backend,
            &mut encoder,
            frame,
            &mut frame_buffer,
            seq,
            started.elapsed().as_micros() as u64,
            seq == 0,
        )?;
        seq = seq.wrapping_add(1);
    }

    let mut measured = Vec::with_capacity(options.frames as usize);
    let mut produced = 0_u64;
    let mut skipped = 0_u64;
    let mut keyframes = 0_u64;
    let mut payload_bytes = 0_u64;

    for _ in 0..options.frames {
        let frame = next_frame(&mut duplication, config.acquire_timeout_ms)?;
        let timestamp_us = started.elapsed().as_micros() as u64;
        let force_keyframe = seq == 0 || seq % config.gop_frames.max(1) as u64 == 0;
        let encode_started = Instant::now();
        let encoded = encode_frame(
            backend,
            &mut encoder,
            frame,
            &mut frame_buffer,
            seq,
            timestamp_us,
            force_keyframe,
        )?;
        measured.push(encode_started.elapsed());

        if let Some(encoded) = encoded {
            produced += 1;
            keyframes += u64::from(encoded.is_keyframe);
            payload_bytes += encoded.payload.len() as u64;
        } else {
            skipped += 1;
        }
        seq = seq.wrapping_add(1);
    }

    measured.sort_unstable();
    let avg = if measured.is_empty() {
        Duration::ZERO
    } else {
        measured.iter().copied().sum::<Duration>() / measured.len() as u32
    };

    Ok(BenchResult {
        backend,
        submitted: options.frames,
        produced,
        skipped,
        keyframes,
        payload_bytes,
        avg,
        p50: percentile(&measured, 50),
        p95: percentile(&measured, 95),
        max: measured.last().copied().unwrap_or_default(),
    })
}

fn next_frame(
    duplication: &mut DxgiDuplicationApi,
    timeout_ms: u32,
) -> Result<windows_capture::dxgi_duplication_api::DxgiDuplicationFrame<'_>> {
    match duplication.acquire_next_frame(timeout_ms.max(1000)) {
        Ok(frame) => Ok(frame),
        Err(DxgiError::Timeout) => Err(screen_stream::ScreenStreamError::InvalidFrame(
            "timed out waiting for a DXGI frame".into(),
        )),
        Err(err) => Err(err.into()),
    }
}

fn encode_frame(
    backend: BenchBackend,
    encoder: &mut H264Encoder,
    mut frame: windows_capture::dxgi_duplication_api::DxgiDuplicationFrame<'_>,
    frame_buffer: &mut Vec<u8>,
    seq: u64,
    timestamp_us: u64,
    force_keyframe: bool,
) -> Result<Option<screen_stream::EncodedVideoFrame>> {
    match backend {
        BenchBackend::MfCpu => {
            let buffer = frame.buffer()?;
            let bgra = buffer.as_nopadding_buffer(frame_buffer);
            let raw = RawBgraFrame {
                data: bgra,
                width: buffer.width(),
                height: buffer.height(),
                timestamp_us,
            };
            encoder.encode_bgra(raw, seq, force_keyframe)
        }
        BenchBackend::MfD3D11 => {
            let raw = RawD3D11Frame {
                texture: frame.texture(),
                device: frame.device(),
                context: frame.device_context(),
                width: frame.width(),
                height: frame.height(),
                timestamp_us,
            };
            encoder.encode_d3d11(raw, seq, force_keyframe)
        }
    }
}

fn parse_options() -> Options {
    let mut options = Options {
        frames: DEFAULT_FRAMES,
        warmup: DEFAULT_WARMUP,
        backends: vec![BenchBackend::MfCpu, BenchBackend::MfD3D11],
    };

    for arg in std::env::args().skip(1) {
        if let Some(frames) = arg
            .strip_prefix("--frames=")
            .and_then(|value| value.parse().ok())
        {
            options.frames = frames;
        } else if let Some(warmup) = arg
            .strip_prefix("--warmup=")
            .and_then(|value| value.parse().ok())
        {
            options.warmup = warmup;
        } else if let Some(backends) = arg.strip_prefix("--backends=") {
            options.backends = parse_backends(backends);
        }
    }

    options.frames = options.frames.max(1);
    options
}

fn parse_backends(value: &str) -> Vec<BenchBackend> {
    let mut backends = Vec::new();
    for name in value.split(',') {
        match name.trim() {
            "cpu" | "mf-cpu" => backends.push(BenchBackend::MfCpu),
            "d3d11" | "gpu" | "zero-copy" | "mf-d3d11" => backends.push(BenchBackend::MfD3D11),
            _ => {}
        }
    }
    if backends.is_empty() {
        vec![BenchBackend::MfCpu, BenchBackend::MfD3D11]
    } else {
        backends
    }
}

impl BenchBackend {
    fn name(self) -> &'static str {
        match self {
            Self::MfCpu => "mf-cpu",
            Self::MfD3D11 => "mf-d3d11",
        }
    }
}

fn percentile(values: &[Duration], percentile: usize) -> Duration {
    if values.is_empty() {
        return Duration::ZERO;
    }
    let index = ((values.len() - 1) * percentile / 100).min(values.len() - 1);
    values[index]
}

fn ms(value: Duration) -> f64 {
    value.as_secs_f64() * 1_000.0
}

fn avg_kib(bytes: u64, count: u64) -> f64 {
    if count == 0 {
        0.0
    } else {
        bytes as f64 / count as f64 / 1024.0
    }
}
