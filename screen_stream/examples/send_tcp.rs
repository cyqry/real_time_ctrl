use screen_stream::{send_primary_screen, CaptureConfig, EncoderBackend, Result};
use tokio::net::TcpStream;

#[tokio::main]
async fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let addr = args.next().unwrap_or_else(|| "127.0.0.1:7007".to_string());
    let preset = args.next().unwrap_or_else(|| "balanced".to_string());
    let mut config = capture_config_from_preset(&preset);

    for arg in args {
        apply_arg(&mut config, &arg);
    }

    println!(
        "connecting to {addr}; preset={preset}; encoder={:?}; fps={} bitrate={}bps max={}x{} complexity={:?} qp={}..={} stats={}",
        config.encoder_backend,
        config.max_fps,
        config.bitrate_bps,
        format_bound(config.max_encode_width),
        format_bound(config.max_encode_height),
        config.encoder_complexity,
        config.qp_min,
        config.qp_max,
        config.debug_stats.enabled
    );

    let stream = TcpStream::connect(addr).await?;
    stream.set_nodelay(true)?;
    send_primary_screen(stream, config).await
}

fn capture_config_from_preset(value: &str) -> CaptureConfig {
    match value {
        "smooth" | "low-latency" | "latency" => CaptureConfig::smooth(),
        "quality" | "high-quality" | "hq" => CaptureConfig::high_quality(),
        "bandwidth" | "low-bandwidth" | "bw" => CaptureConfig::bandwidth_saver(),
        "native" | "source" | "full" => CaptureConfig::balanced().native_resolution(),
        _ => CaptureConfig::balanced(),
    }
}

fn apply_arg(config: &mut CaptureConfig, arg: &str) {
    match arg {
        "--stats" | "stats" | "--stat" | "stat" => config.debug_stats.enabled = true,
        "--no-stats" | "no-stats" => config.debug_stats.enabled = false,
        "--native" | "native" => {
            config.max_encode_width = None;
            config.max_encode_height = None;
        }
        "--software" | "software" => config.encoder_backend = EncoderBackend::OpenH264,
        "--mf" | "mf" | "hardware" => config.encoder_backend = EncoderBackend::MediaFoundation,
        "--auto" | "auto" => config.encoder_backend = EncoderBackend::Auto,
        _ => {
            if let Some(bounds) = arg
                .strip_prefix("--max=")
                .or_else(|| arg.strip_prefix("max="))
            {
                if let Some((width, height)) = parse_bounds(bounds) {
                    config.max_encode_width = Some(width);
                    config.max_encode_height = Some(height);
                }
            } else if let Some(encoder) = arg
                .strip_prefix("--encoder=")
                .or_else(|| arg.strip_prefix("encoder="))
            {
                match encoder {
                    "auto" => config.encoder_backend = EncoderBackend::Auto,
                    "mf" | "media-foundation" | "hardware" => {
                        config.encoder_backend = EncoderBackend::MediaFoundation;
                    }
                    "software" | "openh264" => config.encoder_backend = EncoderBackend::OpenH264,
                    _ => {}
                }
            }
        }
    }
}

fn parse_bounds(value: &str) -> Option<(u32, u32)> {
    let (width, height) = value.split_once('x').or_else(|| value.split_once('X'))?;
    Some((width.parse().ok()?, height.parse().ok()?))
}

fn format_bound(value: Option<u32>) -> String {
    value
        .map(|value| value.to_string())
        .unwrap_or_else(|| "native".to_string())
}
