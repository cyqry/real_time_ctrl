use std::time::Instant;

use screen_stream::{
    codec::{
        h264::{H264Encoder, SoftwareH264Decoder},
        RawD3D11Frame,
    },
    CaptureConfig, EncoderBackend, Result,
};
use windows_capture::{
    dxgi_duplication_api::{DxgiDuplicationApi, Error as DxgiError},
    monitor::Monitor,
};

fn main() -> Result<()> {
    let target_decoded = std::env::args()
        .nth(1)
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(30)
        .max(1);

    let mut config = CaptureConfig::smooth();
    config.encoder_backend = EncoderBackend::MediaFoundation;
    config.debug_stats.enabled = false;

    let monitor = Monitor::primary().map_err(|err| {
        screen_stream::ScreenStreamError::InvalidFrame(format!("primary monitor not found: {err}"))
    })?;
    let mut duplication = DxgiDuplicationApi::new(monitor)?;
    let (encoded_width, encoded_height) =
        config.encoded_dimensions(duplication.width(), duplication.height())?;
    let mut encoder = H264Encoder::new(config.encoder_config())?;
    let mut decoder = SoftwareH264Decoder::new()?;

    let started = Instant::now();
    let mut seq = 0_u64;
    let mut produced = 0_u64;
    let mut decoded = 0_u64;
    let mut keyframes = 0_u64;
    let max_attempts = target_decoded.saturating_mul(8).max(120);

    while decoded < target_decoded && seq < max_attempts {
        let frame = match duplication.acquire_next_frame(config.acquire_timeout_ms) {
            Ok(frame) => frame,
            Err(DxgiError::Timeout) => continue,
            Err(err) => return Err(err.into()),
        };

        let raw = RawD3D11Frame {
            texture: frame.texture(),
            device: frame.device(),
            context: frame.device_context(),
            width: frame.width(),
            height: frame.height(),
            timestamp_us: started.elapsed().as_micros() as u64,
        };

        let Some(encoded) = encoder.encode_d3d11(raw, seq, seq == 0)? else {
            seq = seq.wrapping_add(1);
            continue;
        };

        produced += 1;
        keyframes += u64::from(encoded.is_keyframe);
        assert_eq!(encoded.width, encoded_width);
        assert_eq!(encoded.height, encoded_height);
        assert_eq!(encoded.seq, seq);
        assert!(!encoded.payload.is_empty());

        let packet = encoded.into();
        if let Some(frame) = decoder.decode_packet(&packet)? {
            assert_eq!(frame.seq, packet.seq);
            assert_eq!(frame.width, encoded_width);
            assert_eq!(frame.height, encoded_height);
            assert_eq!(
                frame.rgba.len(),
                encoded_width as usize * encoded_height as usize * 4
            );
            decoded += 1;
        }

        seq = seq.wrapping_add(1);
    }

    if decoded == 0 {
        return Err(screen_stream::ScreenStreamError::InvalidFrame(
            "zero-copy encoder produced no decodable frames".into(),
        ));
    }

    println!(
        "zero-copy probe ok source={}x{} encoded={}x{} submitted={} produced={} decoded={} keyframes={} elapsed_ms={:.1}",
        duplication.width(),
        duplication.height(),
        encoded_width,
        encoded_height,
        seq,
        produced,
        decoded,
        keyframes,
        started.elapsed().as_secs_f64() * 1000.0
    );

    Ok(())
}
