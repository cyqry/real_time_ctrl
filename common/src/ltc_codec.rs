use bytes::{Buf, BytesMut};
use std::io::{Error, ErrorKind};
use tokio_util::codec::Decoder;

use crate::hidden;

/// 握手阶段只允许小型身份/角色帧，认证完成后再按通道类型放宽。
pub const INIT_MAX_FRAME_LENGTH: usize = 4 * 1024;
/// 控制通道建议上限。当前先暴露常量，后续按连接类型逐步启用。
pub const CONTROL_MAX_FRAME_LENGTH: usize = 1024 * 1024;
/// 大文件使用 4 MiB 分片；保留 64 MiB payload 空间承载截图和小文件，
/// 同时阻止异常端仅凭长度前缀诱导进程申请 1 GiB 连续内存。
pub const DATA_MAX_FRAME_LENGTH: usize = 64 * 1024 * 1024 + 64 * 1024;
/// 未识别连接在握手后会切换到明确上限；默认值仅供尚未分型的通用调用点使用。
pub const DEFAULT_MAX_FRAME_LENGTH: usize = DATA_MAX_FRAME_LENGTH;

pub struct LengthFieldBasedFrameDecoder {
    pub current_len: Option<usize>,
    max_frame_len: usize,
}

impl LengthFieldBasedFrameDecoder {
    pub fn new() -> Self {
        Self::new_with_max_frame_len(DEFAULT_MAX_FRAME_LENGTH)
    }

    pub fn new_with_max_frame_len(max_frame_len: usize) -> Self {
        LengthFieldBasedFrameDecoder {
            current_len: None,
            max_frame_len,
        }
    }

    pub fn max_frame_len(&self) -> usize {
        self.max_frame_len
    }

    pub fn set_max_frame_len(&mut self, max_frame_len: usize) {
        // 已经读到长度字段的半帧不能在中途改上限，避免同一帧按两套规则解释。
        if self.current_len.is_none() {
            self.max_frame_len = max_frame_len;
        }
    }

    fn validate_frame_len(&self, len: usize) -> Result<(), Error> {
        if len > self.max_frame_len {
            Err(Error::new(
                ErrorKind::InvalidData,
                hidden!(
                    "frame length ",
                    len,
                    " exceeds max frame length ",
                    self.max_frame_len
                ),
            ))
        } else {
            Ok(())
        }
    }
}

impl Default for LengthFieldBasedFrameDecoder {
    fn default() -> Self {
        Self::new()
    }
}

impl Decoder for LengthFieldBasedFrameDecoder {
    type Item = BytesMut;
    type Error = std::io::Error;

    fn decode(&mut self, src: &mut BytesMut) -> Result<Option<Self::Item>, Self::Error> {
        if self.current_len.is_none() {
            if src.len() < 4 {
                // 前 4 个字节是大端 body 长度，长度字段不完整时继续等待。
                return Ok(None);
            }

            let len = src.get_u32() as usize;
            self.validate_frame_len(len)?;
            self.current_len = Some(len);

            if src.len() < len {
                return Ok(None);
            }

            // 只切出当前完整 body，后续粘包数据留给下一轮 decode。
            let res = src.split_to(len);
            self.current_len = None;
            Ok(Some(res))
        } else {
            let Some(current_len) = self.current_len else {
                return Ok(None);
            };
            self.validate_frame_len(current_len)?;
            if src.len() < current_len {
                return Ok(None);
            }

            self.current_len = None;
            Ok(Some(src.split_to(current_len)))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::BufMut;

    #[test]
    fn waits_for_complete_header() {
        let mut decoder = LengthFieldBasedFrameDecoder::new_with_max_frame_len(8);
        let mut src = BytesMut::from(&[0, 0, 0][..]);

        let decoded = decoder.decode(&mut src).unwrap();

        assert!(decoded.is_none());
        assert_eq!(src.len(), 3);
    }

    #[test]
    fn waits_for_complete_body_after_reading_length() {
        let mut decoder = LengthFieldBasedFrameDecoder::new_with_max_frame_len(8);
        let mut src = BytesMut::new();
        src.put_u32(4);
        src.extend_from_slice(&[1, 2]);

        let decoded = decoder.decode(&mut src).unwrap();

        assert!(decoded.is_none());
        assert_eq!(decoder.current_len, Some(4));
        assert_eq!(src.as_ref(), &[1, 2]);
    }

    #[test]
    fn decodes_one_complete_frame_and_keeps_trailing_bytes() {
        let mut decoder = LengthFieldBasedFrameDecoder::new_with_max_frame_len(8);
        let mut src = BytesMut::new();
        src.put_u32(3);
        src.extend_from_slice(&[1, 2, 3, 9, 9]);

        let decoded = decoder.decode(&mut src).unwrap().unwrap();

        assert_eq!(decoded.as_ref(), &[1, 2, 3]);
        assert_eq!(src.as_ref(), &[9, 9]);
        assert_eq!(decoder.current_len, None);
    }

    #[test]
    fn rejects_frame_that_exceeds_configured_limit() {
        let mut decoder = LengthFieldBasedFrameDecoder::new_with_max_frame_len(3);
        let mut src = BytesMut::new();
        src.put_u32(4);
        src.extend_from_slice(&[1, 2, 3, 4]);

        let err = decoder.decode(&mut src).unwrap_err();

        assert_eq!(err.kind(), ErrorKind::InvalidData);
        assert_eq!(decoder.current_len, None);
    }

    #[test]
    fn updates_max_frame_len_between_frames() {
        let mut decoder = LengthFieldBasedFrameDecoder::new_with_max_frame_len(8);

        decoder.set_max_frame_len(4);

        assert_eq!(decoder.max_frame_len(), 4);
    }

    #[test]
    fn does_not_update_max_frame_len_in_the_middle_of_a_frame() {
        let mut decoder = LengthFieldBasedFrameDecoder::new_with_max_frame_len(8);
        let mut src = BytesMut::new();
        src.put_u32(6);
        src.extend_from_slice(&[1, 2]);

        assert!(decoder.decode(&mut src).unwrap().is_none());
        decoder.set_max_frame_len(4);

        assert_eq!(decoder.max_frame_len(), 8);
    }
}
