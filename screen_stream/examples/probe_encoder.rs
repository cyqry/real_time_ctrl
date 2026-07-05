use screen_stream::{
    codec::{h264::H264Encoder, RawBgraFrame},
    CaptureConfig, EncoderBackend, Result,
};
use std::time::Instant;

fn main() -> Result<()> {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    let backend = args.first().cloned().unwrap_or_else(|| "auto".to_string());
    let frame_count = args
        .get(1)
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(60);
    let mut config = CaptureConfig::smooth().with_encode_bounds(1280, 720);
    config.debug_stats.enabled = false;
    config.encoder_backend = match backend.as_str() {
        "mf" | "hardware" | "media-foundation" => EncoderBackend::MediaFoundation,
        "software" | "openh264" => EncoderBackend::OpenH264,
        _ => EncoderBackend::Auto,
    };

    let mut encoder = H264Encoder::new(config.encoder_config())?;
    let width = 1280_u32;
    let height = 720_u32;
    let mut bgra = vec![0_u8; width as usize * height as usize * 4];
    let started = Instant::now();
    let mut produced = 0_u64;
    let mut keyframes = 0_u64;
    let mut payload_bytes = 0_u64;

    for frame_index in 0..frame_count {
        fill_test_frame(
            &mut bgra,
            width as usize,
            height as usize,
            frame_index as u8,
        );
        let frame = RawBgraFrame {
            data: &bgra,
            width,
            height,
            timestamp_us: frame_index * 16_666,
        };
        if let Some(encoded) = encoder.encode_bgra(frame, frame_index, frame_index == 0)? {
            produced += 1;
            keyframes += u64::from(encoded.is_keyframe);
            payload_bytes += encoded.payload.len() as u64;
            if produced <= 3 {
                println!(
                    "encoded frame seq={} key={} {}x{} payload={} bytes",
                    encoded.seq,
                    encoded.is_keyframe,
                    encoded.width,
                    encoded.height,
                    encoded.payload.len()
                );
            }
        }
    }

    let elapsed = started.elapsed().as_secs_f64().max(f64::EPSILON);
    println!(
        "probe done backend={backend} produced={produced}/{frame_count} keyframes={keyframes} payload={:.2}MiB throughput={:.1} encoded/s",
        payload_bytes as f64 / 1024.0 / 1024.0,
        produced as f64 / elapsed,
    );
    Ok(())
}

fn fill_test_frame(bgra: &mut [u8], width: usize, height: usize, offset: u8) {
    for y in 0..height {
        for x in 0..width {
            let i = (y * width + x) * 4;
            bgra[i] = (x as u8).wrapping_add(offset);
            bgra[i + 1] = (y as u8).wrapping_add(offset);
            bgra[i + 2] = 180_u8.wrapping_add(offset);
            bgra[i + 3] = 255;
        }
    }
}
