
use crate::protocol::BufSerializable;
use bytes::{BytesMut, BufMut, Buf};

pub enum Dok {
    FilePart(u64, u64, Vec<u8>),
    Err(ErrCode),
}

#[derive(Clone)]
pub enum ErrCode {
    ReadError = 1,
    WriteError = 2,
}

impl ErrCode {
    fn from_u8(value: u8) -> Option<Self> {
        match value {
            1 => Some(ErrCode::ReadError),
            2 => Some(ErrCode::WriteError),
            _ => None,
        }
    }
}


impl BufSerializable for Dok {
    fn to_buf(&self) -> BytesMut {
        match self {
            Dok::FilePart(start, end, data) => {
                let total_len = 1 + 8 + 8 + 4 + data.len();
                let mut buf = BytesMut::with_capacity(total_len);

                // 变体标识：0 表示 FilePart
                buf.put_u8(0);

                // 写入两个 u64
                buf.put_u64(*start);
                buf.put_u64(*end);

                // 写入数据长度和数据
                buf.put_u32(data.len() as u32);
                buf.put_slice(data);

                buf
            }
            Dok::Err(err_code) => {
                let mut buf = BytesMut::with_capacity(2);

                // 变体标识：1 表示 Err
                buf.put_u8(1);

                // 写入错误码
                buf.put_u8((*err_code).clone() as u8);

                buf
            }
        }
    }

    fn from_buf(mut bys: BytesMut) -> Option<Self> {

        if bys.remaining() < 1 {
            return None;
        }

        let variant = bys.get_u8();

        match variant {
            0 => {
                // FilePart: 需要读取 2个u64 + 1个u32长度 + 实际数据
                if bys.remaining() < 8 + 8 + 4 {
                    return None;
                }

                let start = bys.get_u64();
                let end = bys.get_u64();
                let data_len = bys.get_u32() as usize;

                // 检查是否有足够的数据
                if bys.remaining() < data_len {
                    return None;
                }

                let data = bys[..data_len].to_vec();
                bys.advance(data_len);

                Some(Dok::FilePart(start, end, data))
            }
            1 => {

                if bys.remaining() < 1 {
                    return None;
                }

                let err_code_byte = bys.get_u8();
                ErrCode::from_u8(err_code_byte)
                    .map(Dok::Err)
            }
            _ => None,
        }
    }
}

// 单元测试
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_file_part_serialization() {
        let data = vec![1, 2, 3, 4, 5];
        let dok = Dok::FilePart(100, 200, data.clone());

        let buf = dok.to_buf();
        let deserialized = Dok::from_buf(buf);

        assert!(deserialized.is_some());
        if let Dok::FilePart(start, end, data2) = deserialized.unwrap() {
            assert_eq!(start, 100);
            assert_eq!(end, 200);
            assert_eq!(data2, data);
        } else {
            panic!("Expected FilePart variant");
        }
    }

    #[test]
    fn test_err_serialization() {
        let dok = Dok::Err(ErrCode::ReadError);

        let buf = dok.to_buf();
        let deserialized = Dok::from_buf(buf);

        assert!(deserialized.is_some());
        match deserialized.unwrap() {
            Dok::Err(ErrCode::ReadError) => assert!(true),
            _ => panic!("Expected ReadError variant"),
        }
    }

    #[test]
    fn test_incomplete_buffer() {
        // 测试不完整的缓冲区
        let mut buf = BytesMut::new();
        buf.put_u8(0); // FilePart 变体
        buf.put_u64(100); // 只有第一个u64，缺少其他数据

        let result = Dok::from_buf(buf);
        assert!(result.is_none());
    }

    #[test]
    fn test_invalid_variant() {
        // 测试无效的变体标识
        let mut buf = BytesMut::new();
        buf.put_u8(99); // 无效的变体标识

        let result = Dok::from_buf(buf);
        assert!(result.is_none());
    }

    #[test]
    fn test_data_length_mismatch() {
        // 测试数据长度与实际数据不匹配的情况
        let mut buf = BytesMut::new();
        buf.put_u8(0); // FilePart 变体
        buf.put_u64(100);
        buf.put_u64(200);
        buf.put_u32(10); // 声明有10个字节
        buf.put_u32(1234); // 但只有4个字节

        let result = Dok::from_buf(buf);
        assert!(result.is_none());
    }

    #[test]
    fn test_empty_buffer() {
        // 测试空缓冲区
        let buf = BytesMut::new();
        let result = Dok::from_buf(buf);
        assert!(result.is_none());
    }
}