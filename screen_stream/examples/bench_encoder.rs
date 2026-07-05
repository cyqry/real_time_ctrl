use std::time::{Duration, Instant};

use screen_stream::{
    codec::{h264::H264Encoder, RawBgraFrame},
    CaptureConfig, EncoderBackend, Result,
};

const DEFAULT_WIDTH: u32 = 1280;
const DEFAULT_HEIGHT: u32 = 720;
const DEFAULT_FRAMES: u64 = 240;
const DEFAULT_WARMUP: u64 = 30;
const FRAME_POOL: usize = 16;

#[derive(Debug, Clone)]
struct BenchOptions {
    width: u32,
    height: u32,
    frames: u64,
    warmup: u64,
    backends: Vec<BenchBackend>,
}

#[derive(Debug, Clone, Copy)]
struct BenchBackend {
    name: &'static str,
    backend: EncoderBackend,
}

#[derive(Debug)]
struct BenchResult {
    name: &'static str,
    submitted: u64,
    produced: u64,
    skipped: u64,
    keyframes: u64,
    payload_bytes: u64,
    total: Duration,
    avg: Duration,
    p50: Duration,
    p95: Duration,
    max: Duration,
}

fn main() -> Result<()> {
    let options = parse_options();
    let frames = make_frame_pool(options.width as usize, options.height as usize);

    println!(
        "bench_encoder {}x{} frames={} warmup={} backends={}",
        options.width,
        options.height,
        options.frames,
        options.warmup,
        options
            .backends
            .iter()
            .map(|backend| backend.name)
            .collect::<Vec<_>>()
            .join(",")
    );

    let mut results = Vec::new();
    for backend in &options.backends {
        results.push(run_backend(backend, &options, &frames)?);
    }

    println!();
    println!(
        "{:<10} {:>9} {:>9} {:>9} {:>9} {:>10} {:>10} {:>10} {:>10} {:>10} {:>11} {:>12}",
        "backend",
        "submitted",
        "produced",
        "skipped",
        "keyframes",
        "avg_ms",
        "p50_ms",
        "p95_ms",
        "max_ms",
        "enc/s",
        "payloadMiB",
        "avgKiB"
    );
    for result in &results {
        println!(
            "{:<10} {:>9} {:>9} {:>9} {:>9} {:>10.3} {:>10.3} {:>10.3} {:>10.3} {:>10.1} {:>11.2} {:>12.1}",
            result.name,
            result.submitted,
            result.produced,
            result.skipped,
            result.keyframes,
            ms(result.avg),
            ms(result.p50),
            ms(result.p95),
            ms(result.max),
            result.produced as f64 / result.total.as_secs_f64().max(f64::EPSILON),
            result.payload_bytes as f64 / 1024.0 / 1024.0,
            avg_kib(result.payload_bytes, result.produced),
        );
    }

    if let (Some(cpu), Some(gpu)) = (
        results.iter().find(|result| result.name == "software"),
        results.iter().find(|result| result.name == "mf"),
    ) {
        println!(
            "\nsummary gpu_vs_cpu: avg_ms_speedup={:.2}x throughput_speedup={:.2}x",
            ms(cpu.avg) / ms(gpu.avg).max(f64::EPSILON),
            (gpu.produced as f64 / gpu.total.as_secs_f64().max(f64::EPSILON))
                / (cpu.produced as f64 / cpu.total.as_secs_f64().max(f64::EPSILON))
                    .max(f64::EPSILON),
        );
    }

    Ok(())
}

fn run_backend(
    backend: &BenchBackend,
    options: &BenchOptions,
    frames: &[Vec<u8>],
) -> Result<BenchResult> {
    let mut config = CaptureConfig::smooth().with_encode_bounds(options.width, options.height);
    config.encoder_backend = backend.backend;
    config.debug_stats.enabled = false;

    let mut encoder = H264Encoder::new(config.encoder_config())?;
    let mut measured = Vec::with_capacity(options.frames as usize);
    let mut produced = 0_u64;
    let mut skipped = 0_u64;
    let mut keyframes = 0_u64;
    let mut payload_bytes = 0_u64;

    // 正式计时前先预热编码器和硬件 MFT 事件路径。
    for i in 0..options.warmup {
        let frame = frame_at(frames, options, i);
        let _ = encoder.encode_bgra(frame, i, i == 0)?;
    }

    let total_started = Instant::now();
    for i in 0..options.frames {
        let frame = frame_at(frames, options, i + options.warmup);
        let started = Instant::now();
        let encoded = encoder.encode_bgra(frame, i, i == 0)?;
        measured.push(started.elapsed());

        if let Some(encoded) = encoded {
            produced += 1;
            keyframes += u64::from(encoded.is_keyframe);
            payload_bytes += encoded.payload.len() as u64;
        } else {
            skipped += 1;
        }
    }
    let total = total_started.elapsed();

    measured.sort_unstable();
    let avg = if measured.is_empty() {
        Duration::ZERO
    } else {
        measured.iter().copied().sum::<Duration>() / measured.len() as u32
    };

    Ok(BenchResult {
        name: backend.name,
        submitted: options.frames,
        produced,
        skipped,
        keyframes,
        payload_bytes,
        total,
        avg,
        p50: percentile(&measured, 50),
        p95: percentile(&measured, 95),
        max: measured.last().copied().unwrap_or_default(),
    })
}

fn frame_at<'a>(frames: &'a [Vec<u8>], options: &BenchOptions, index: u64) -> RawBgraFrame<'a> {
    RawBgraFrame {
        data: &frames[index as usize % frames.len()],
        width: options.width,
        height: options.height,
        timestamp_us: index * 16_666,
    }
}

fn make_frame_pool(width: usize, height: usize) -> Vec<Vec<u8>> {
    (0..FRAME_POOL)
        .map(|seed| {
            let mut bgra = vec![0_u8; width * height * 4];
            fill_test_frame(&mut bgra, width, height, seed as u8);
            bgra
        })
        .collect()
}

fn fill_test_frame(bgra: &mut [u8], width: usize, height: usize, seed: u8) {
    let bar_left = (seed as usize * 37) % width.max(1);
    for y in 0..height {
        for x in 0..width {
            let i = (y * width + x) * 4;
            let checker = (((x / 32) ^ (y / 32)) & 1) as u8 * 32;
            let moving = if x.abs_diff(bar_left) < 12 { 90 } else { 0 };
            bgra[i] = (x as u8).wrapping_add(seed).wrapping_add(checker);
            bgra[i + 1] = (y as u8).wrapping_add(seed / 2);
            bgra[i + 2] = 120_u8.wrapping_add(seed).wrapping_add(moving);
            bgra[i + 3] = 255;
        }
    }
}

fn parse_options() -> BenchOptions {
    let mut options = BenchOptions {
        width: DEFAULT_WIDTH,
        height: DEFAULT_HEIGHT,
        frames: DEFAULT_FRAMES,
        warmup: DEFAULT_WARMUP,
        backends: vec![
            BenchBackend {
                name: "software",
                backend: EncoderBackend::OpenH264,
            },
            BenchBackend {
                name: "mf",
                backend: EncoderBackend::MediaFoundation,
            },
        ],
    };

    for arg in std::env::args().skip(1) {
        if let Some((width, height)) = arg
            .strip_prefix("--size=")
            .and_then(parse_size)
            .or_else(|| parse_size(&arg))
        {
            options.width = width;
            options.height = height;
        } else if let Some(frames) = arg
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

    if options.width % 2 != 0 {
        options.width -= 1;
    }
    if options.height % 2 != 0 {
        options.height -= 1;
    }
    options.width = options.width.max(2);
    options.height = options.height.max(2);
    options.frames = options.frames.max(1);
    options
}

fn parse_size(value: &str) -> Option<(u32, u32)> {
    let (width, height) = value.split_once('x').or_else(|| value.split_once('X'))?;
    Some((width.parse().ok()?, height.parse().ok()?))
}

fn parse_backends(value: &str) -> Vec<BenchBackend> {
    let mut backends = Vec::new();
    for name in value.split(',') {
        match name.trim() {
            "software" | "cpu" | "openh264" => backends.push(BenchBackend {
                name: "software",
                backend: EncoderBackend::OpenH264,
            }),
            "mf" | "gpu" | "hardware" => backends.push(BenchBackend {
                name: "mf",
                backend: EncoderBackend::MediaFoundation,
            }),
            "auto" => backends.push(BenchBackend {
                name: "auto",
                backend: EncoderBackend::Auto,
            }),
            _ => {}
        }
    }
    if backends.is_empty() {
        parse_backends("software,mf")
    } else {
        backends
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
