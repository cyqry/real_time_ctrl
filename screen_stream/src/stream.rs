use std::time::Instant;

use bytes::BytesMut;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::codec::h264::SoftwareH264Decoder;
use crate::codec::DecodedVideoFrame;
use crate::config::PlayerConfig;
use crate::error::{Result, ScreenStreamError};
use crate::stats::ReceiveDebugStats;
use crate::wire::{decode_packet, encode_packet, CodecId, WirePacket};

pub async fn write_packet<W>(writer: &mut W, packet: &WirePacket) -> Result<()>
where
    W: AsyncWrite + Unpin,
{
    write_packet_counted(writer, packet).await.map(|_| ())
}

pub async fn write_packet_counted<W>(writer: &mut W, packet: &WirePacket) -> Result<usize>
where
    W: AsyncWrite + Unpin,
{
    let body = encode_packet(packet)?;
    if body.len() > u32::MAX as usize {
        return Err(ScreenStreamError::PacketTooLarge {
            len: body.len(),
            max: u32::MAX as usize,
        });
    }

    // 外层 framing 保持简单，并兼容当前项目风格：
    // u32 大端长度 + 一个完整 packet body。
    writer.write_all(&(body.len() as u32).to_be_bytes()).await?;
    writer.write_all(&body).await?;
    writer.flush().await?;
    Ok(body.len() + 4)
}

pub async fn read_packet<R>(reader: &mut R, max_packet_size: usize) -> Result<Option<WirePacket>>
where
    R: AsyncRead + Unpin,
{
    read_packet_counted(reader, max_packet_size)
        .await
        .map(|packet| packet.map(|(packet, _)| packet))
}

pub async fn read_packet_counted<R>(
    reader: &mut R,
    max_packet_size: usize,
) -> Result<Option<(WirePacket, usize)>>
where
    R: AsyncRead + Unpin,
{
    let mut len_buf = [0_u8; 4];
    match reader.read_exact(&mut len_buf).await {
        Ok(_) => {}
        Err(err) if err.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(err) => return Err(err.into()),
    }

    let len = u32::from_be_bytes(len_buf) as usize;
    if len > max_packet_size {
        return Err(ScreenStreamError::PacketTooLarge {
            len,
            max: max_packet_size,
        });
    }

    let mut body = BytesMut::zeroed(len);
    reader.read_exact(&mut body).await?;
    Ok(Some((decode_packet(body)?, len + 4)))
}

pub async fn receive_decoded<R, F>(
    mut reader: R,
    config: PlayerConfig,
    mut on_frame: F,
) -> Result<()>
where
    R: AsyncRead + Unpin,
    F: FnMut(DecodedVideoFrame) -> Result<()>,
{
    let mut decoder = SoftwareH264Decoder::new()?;
    let mut stream_codec = None;
    let mut stats = ReceiveDebugStats::new(config.debug_stats.clone());
    let max_packet_size = config.max_packet_size;

    while let Some((packet, wire_bytes)) = read_packet_counted(&mut reader, max_packet_size).await?
    {
        stats.on_packet(&packet, wire_bytes);
        match packet {
            WirePacket::Hello(info) => {
                if info.codec != CodecId::H264 {
                    return Err(ScreenStreamError::InvalidPacket(format!(
                        "unsupported codec {:?}",
                        info.codec
                    )));
                }
                stream_codec = Some(info.codec);
            }
            WirePacket::Video(packet) => {
                // 视频包之前必须先收到 Hello，这样接收端可以在 payload 到达前
                // 配置原生解码器或浏览器适配层。
                if stream_codec.is_none() {
                    return Err(ScreenStreamError::InvalidPacket(
                        "received video before stream hello".into(),
                    ));
                }

                let decode_started = Instant::now();
                let decoded = decoder.decode_packet(&packet)?;
                let decode_elapsed = decode_started.elapsed();
                if let Some(frame) = decoded {
                    stats.on_decoded(frame.rgba.len(), decode_elapsed);
                    on_frame(frame)?;
                }
            }
        }
    }

    Ok(())
}

pub async fn play_from<R, F>(reader: R, config: PlayerConfig, on_frame: F) -> Result<()>
where
    R: AsyncRead + Unpin,
    F: FnMut(DecodedVideoFrame) -> Result<()>,
{
    receive_decoded(reader, config, on_frame).await
}
