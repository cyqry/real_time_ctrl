use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::time::{Duration, Instant};

use tokio::io::{AsyncWrite, AsyncWriteExt};
use tokio::sync::mpsc;
use tokio::sync::mpsc::error::TrySendError;
use windows_capture::dxgi_duplication_api::{DxgiDuplicationApi, Error as DxgiError};
use windows_capture::monitor::Monitor;

use crate::codec::h264::H264Encoder;
use crate::codec::{RawBgraFrame, RawD3D11Frame};
use crate::config::CaptureConfig;
use crate::error::{Result, ScreenStreamError};
use crate::stats::{CaptureDebugStats, TransportDebugStats};
use crate::stream::write_packet_counted;
use crate::wire::{CodecId, StreamInfo, WirePacket};

pub async fn send_primary_screen<W>(mut writer: W, config: CaptureConfig) -> Result<()>
where
    W: AsyncWrite + Unpin,
{
    let (tx, mut rx) = mpsc::channel::<WirePacket>(config.queue_capacity.max(1));
    let capture_config = config.clone();
    let running = Arc::new(AtomicBool::new(true));
    let capture_running = Arc::clone(&running);
    let mut transport_stats = TransportDebugStats::new(config.debug_stats.clone(), "send");

    let capture_task =
        tokio::task::spawn_blocking(move || run_capture_loop(capture_config, tx, capture_running));

    while let Some(packet) = rx.recv().await {
        let write_started = Instant::now();
        match write_packet_counted(&mut writer, &packet).await {
            Ok(wire_bytes) => {
                transport_stats.on_sent(&packet, wire_bytes, write_started.elapsed());
            }
            Err(err) => {
                running.store(false, Ordering::Relaxed);
                let _ = writer.shutdown().await;
                capture_task.await??;
                return Err(err);
            }
        }
    }

    running.store(false, Ordering::Relaxed);
    capture_task.await??;
    Ok(())
}

fn run_capture_loop(
    config: CaptureConfig,
    tx: mpsc::Sender<WirePacket>,
    running: Arc<AtomicBool>,
) -> Result<()> {
    validate_capture_config(&config)?;

    let monitor = Monitor::primary().map_err(|err| {
        ScreenStreamError::InvalidFrame(format!("primary monitor not found: {err}"))
    })?;
    let mut duplication = DxgiDuplicationApi::new(monitor)?;
    let mut encoder = H264Encoder::new(config.encoder_config())?;
    let mut stats = CaptureDebugStats::new(config.debug_stats.clone());

    send_hello(&tx, &duplication, &config)?;

    let started_at = Instant::now();
    let min_frame_interval = Duration::from_secs_f64(1.0 / config.max_fps.max(1) as f64);
    let mut last_encoded_at = Instant::now() - min_frame_interval;
    let mut seq = 0_u64;
    let mut frame_buffer = Vec::new();
    let mut try_d3d11 = encoder.can_encode_d3d11();

    while running.load(Ordering::Relaxed) {
        let mut frame = match duplication.acquire_next_frame(config.acquire_timeout_ms) {
            Ok(frame) => {
                stats.on_acquired();
                frame
            }
            Err(DxgiError::Timeout) => continue,
            Err(DxgiError::AccessLost) => {
                // 显示模式变化、桌面切换或部分全屏切换会让 DXGI access 失效。
                // 这里重建 duplication，并强制下一帧为关键帧，让接收端能从
                // 干净的解码状态恢复。
                duplication = duplication.recreate()?;
                encoder.request_keyframe();
                send_hello(&tx, &duplication, &config)?;
                continue;
            }
            Err(err) => return Err(err.into()),
        };

        if last_encoded_at.elapsed() < min_frame_interval {
            stats.on_fps_limited();
            continue;
        }

        // 在执行昂贵的编码前先预留 writer 队列容量。如果传输端已经落后，
        // 直接跳过这帧原始画面可以省掉 CPU/GPU 编码开销，并把延迟控制在
        // 有界范围内，避免编码出马上会被丢弃的包。
        let permit = match tx.try_reserve() {
            Ok(permit) => permit,
            Err(TrySendError::Full(_)) => {
                stats.on_queue_dropped();
                continue;
            }
            Err(TrySendError::Closed(_)) => return Err(ScreenStreamError::ChannelClosed),
        };

        let timestamp_us = started_at.elapsed().as_micros() as u64;
        let force_keyframe =
            seq == 0 || (config.gop_frames > 0 && seq % config.gop_frames as u64 == 0);
        let encode_started = Instant::now();
        let encoded = if try_d3d11 {
            let raw = RawD3D11Frame {
                texture: frame.texture(),
                device: frame.device(),
                context: frame.device_context(),
                width: frame.width(),
                height: frame.height(),
                timestamp_us,
            };
            match encoder.encode_d3d11(raw, seq, force_keyframe) {
                Ok(encoded) => encoded,
                Err(err) => {
                    eprintln!(
                        "[screen_stream capture] D3D11 zero-copy encode unavailable, falling back to CPU input: {err}"
                    );
                    try_d3d11 = false;
                    encode_cpu_frame(
                        &mut encoder,
                        &mut frame,
                        &mut frame_buffer,
                        seq,
                        timestamp_us,
                        force_keyframe,
                    )?
                }
            }
        } else {
            encode_cpu_frame(
                &mut encoder,
                &mut frame,
                &mut frame_buffer,
                seq,
                timestamp_us,
                force_keyframe,
            )?
        };
        let encode_elapsed = encode_started.elapsed();
        let Some(encoded) = encoded else {
            stats.on_encoder_skipped(encode_elapsed);
            continue;
        };
        stats.on_encoded(encoded.payload.len(), encoded.is_keyframe, encode_elapsed);

        if encoded.payload.len() > config.max_packet_size {
            return Err(ScreenStreamError::PacketTooLarge {
                len: encoded.payload.len(),
                max: config.max_packet_size,
            });
        }

        let packet = WirePacket::Video(encoded.into());
        permit.send(packet);
        seq = seq.wrapping_add(1);
        last_encoded_at = Instant::now();
    }

    Ok(())
}

fn encode_cpu_frame(
    encoder: &mut H264Encoder,
    frame: &mut windows_capture::dxgi_duplication_api::DxgiDuplicationFrame<'_>,
    frame_buffer: &mut Vec<u8>,
    seq: u64,
    timestamp_us: u64,
    force_keyframe: bool,
) -> Result<Option<crate::codec::EncodedVideoFrame>> {
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

fn send_hello(
    tx: &mpsc::Sender<WirePacket>,
    duplication: &DxgiDuplicationApi,
    config: &CaptureConfig,
) -> Result<()> {
    let (width, height) = config.encoded_dimensions(duplication.width(), duplication.height())?;

    tx.blocking_send(WirePacket::Hello(StreamInfo {
        codec: CodecId::H264,
        width,
        height,
        fps: config.max_fps,
        bitrate_bps: config.bitrate_bps,
    }))
    .map_err(|_| ScreenStreamError::ChannelClosed)
}

fn validate_capture_config(config: &CaptureConfig) -> Result<()> {
    if config.max_fps == 0 {
        return Err(ScreenStreamError::InvalidFrame(
            "max_fps must be greater than zero".into(),
        ));
    }
    if config.bitrate_bps == 0 {
        return Err(ScreenStreamError::InvalidFrame(
            "bitrate_bps must be greater than zero".into(),
        ));
    }
    if config.qp_max > 51 || config.qp_min > config.qp_max {
        return Err(ScreenStreamError::InvalidFrame(format!(
            "invalid qp range {}..={}; expected 0 <= min <= max <= 51",
            config.qp_min, config.qp_max
        )));
    }
    Ok(())
}
