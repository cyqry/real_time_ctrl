use bytes::{Buf, BufMut, BytesMut};

use crate::error::{Result, ScreenStreamError};

const MAGIC: &[u8; 4] = b"RTSS";
const VERSION: u16 = 1;
const KIND_HELLO: u8 = 1;
const KIND_VIDEO: u8 = 2;
const FLAG_KEYFRAME: u8 = 0b0000_0001;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodecId {
    H264 = 1,
}

impl CodecId {
    fn from_u8(value: u8) -> Result<Self> {
        match value {
            1 => Ok(Self::H264),
            _ => Err(ScreenStreamError::InvalidPacket(format!(
                "unknown codec id {value}"
            ))),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamInfo {
    pub codec: CodecId,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate_bps: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncodedPacket {
    pub seq: u64,
    pub timestamp_us: u64,
    pub width: u32,
    pub height: u32,
    pub is_keyframe: bool,
    pub payload: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WirePacket {
    Hello(StreamInfo),
    Video(EncodedPacket),
}

pub fn encode_packet(packet: &WirePacket) -> Result<BytesMut> {
    let mut buf = BytesMut::new();
    buf.put_slice(MAGIC);
    buf.put_u16(VERSION);

    match packet {
        WirePacket::Hello(info) => {
            // Hello 包故意保持很小且固定宽度，方便浏览器、转发层和原生客户端
            // 在不依赖 serde 的情况下解析流元数据。
            buf.put_u8(KIND_HELLO);
            buf.put_u8(info.codec as u8);
            buf.put_u32(info.width);
            buf.put_u32(info.height);
            buf.put_u32(info.fps);
            buf.put_u32(info.bitrate_bps);
        }
        WirePacket::Video(frame) => {
            if frame.payload.len() > u32::MAX as usize {
                return Err(ScreenStreamError::PacketTooLarge {
                    len: frame.payload.len(),
                    max: u32::MAX as usize,
                });
            }

            // payload 是一个完整的 H.264 access unit。保持 access unit 不拆分，
            // 可以直接复用于原生解码、WebCodecs、MSE 或 RTP 打包，
            // 避免额外的解码再编码。
            buf.put_u8(KIND_VIDEO);
            buf.put_u64(frame.seq);
            buf.put_u64(frame.timestamp_us);
            buf.put_u32(frame.width);
            buf.put_u32(frame.height);
            buf.put_u8(if frame.is_keyframe { FLAG_KEYFRAME } else { 0 });
            buf.put_u32(frame.payload.len() as u32);
            buf.put_slice(&frame.payload);
        }
    }

    Ok(buf)
}

pub fn decode_packet(mut buf: BytesMut) -> Result<WirePacket> {
    if buf.remaining() < 7 {
        return Err(ScreenStreamError::InvalidPacket(
            "packet header too short".into(),
        ));
    }

    let mut magic = [0_u8; 4];
    buf.copy_to_slice(&mut magic);
    if &magic != MAGIC {
        return Err(ScreenStreamError::InvalidPacket("bad stream magic".into()));
    }

    let version = buf.get_u16();
    if version != VERSION {
        return Err(ScreenStreamError::UnsupportedVersion(version));
    }

    match buf.get_u8() {
        KIND_HELLO => decode_hello(buf),
        KIND_VIDEO => decode_video(buf),
        kind => Err(ScreenStreamError::InvalidPacket(format!(
            "unknown packet kind {kind}"
        ))),
    }
}

fn decode_hello(mut buf: BytesMut) -> Result<WirePacket> {
    if buf.remaining() != 17 {
        return Err(ScreenStreamError::InvalidPacket(format!(
            "hello payload has {} bytes, expected 17",
            buf.remaining()
        )));
    }

    Ok(WirePacket::Hello(StreamInfo {
        codec: CodecId::from_u8(buf.get_u8())?,
        width: buf.get_u32(),
        height: buf.get_u32(),
        fps: buf.get_u32(),
        bitrate_bps: buf.get_u32(),
    }))
}

fn decode_video(mut buf: BytesMut) -> Result<WirePacket> {
    if buf.remaining() < 29 {
        return Err(ScreenStreamError::InvalidPacket(
            "video payload too short".into(),
        ));
    }

    let seq = buf.get_u64();
    let timestamp_us = buf.get_u64();
    let width = buf.get_u32();
    let height = buf.get_u32();
    let flags = buf.get_u8();
    let payload_len = buf.get_u32() as usize;

    if buf.remaining() != payload_len {
        return Err(ScreenStreamError::InvalidPacket(format!(
            "video payload length mismatch: header={payload_len}, actual={}",
            buf.remaining()
        )));
    }

    Ok(WirePacket::Video(EncodedPacket {
        seq,
        timestamp_us,
        width,
        height,
        is_keyframe: flags & FLAG_KEYFRAME != 0,
        payload: buf.to_vec(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_hello() {
        let packet = WirePacket::Hello(StreamInfo {
            codec: CodecId::H264,
            width: 1920,
            height: 1080,
            fps: 30,
            bitrate_bps: 4_000_000,
        });
        let encoded = encode_packet(&packet).unwrap();
        assert_eq!(decode_packet(encoded).unwrap(), packet);
    }

    #[test]
    fn round_trips_video() {
        let packet = WirePacket::Video(EncodedPacket {
            seq: 7,
            timestamp_us: 123,
            width: 1280,
            height: 720,
            is_keyframe: true,
            payload: vec![1, 2, 3, 4],
        });
        let encoded = encode_packet(&packet).unwrap();
        assert_eq!(decode_packet(encoded).unwrap(), packet);
    }
}
