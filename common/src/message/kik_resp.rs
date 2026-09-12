use crate::protocol;
use crate::protocol::BufSerializable;
use bytes::{Buf, BufMut, BytesMut};
#[derive(Clone, Debug)]
pub enum KikResp {
    Success(ClientSuccessResp),
    Error(u8, String),
}

#[derive(Clone, Debug)]
pub enum ClientSuccessResp {
    Info(String),
    DataId(String),
    /// 大文件下载的轻量元数据随控制响应返回，数据连接只承载 FilePart。
    BigFile {
        data_id: String,
        total: u64,
        hash: Vec<u8>,
    },
}

const SHA256_BYTES: usize = 32;

impl BufSerializable for ClientSuccessResp {
    fn to_buf(&self) -> BytesMut {
        let mut bytes = BytesMut::new();
        match self {
            ClientSuccessResp::Info(s) => {
                bytes.put_u8(0); // 变体标识
                bytes.put_u32(s.len() as u32); // 字符串长度前缀
                bytes.put_slice(s.as_bytes()); // 内容
            }
            ClientSuccessResp::DataId(id) => {
                bytes.put_u8(1);
                bytes.put_u32(id.len() as u32);
                bytes.put_slice(id.as_bytes());
            }
            ClientSuccessResp::BigFile {
                data_id,
                total,
                hash,
            } => {
                bytes.put_u8(2);
                bytes.put_u32(data_id.len() as u32);
                bytes.put_slice(data_id.as_bytes());
                bytes.put_u64(*total);
                bytes.put_u32(hash.len() as u32);
                bytes.put_slice(hash);
            }
        }
        bytes
    }

    fn from_buf(mut buf: BytesMut) -> Option<Self> {
        // 至少需要 1 字节的变体标识
        if buf.remaining() < 1 {
            return None;
        }
        let variant = buf.get_u8();
        match variant {
            0 => {
                if buf.remaining() < 4 {
                    return None;
                }
                let len = buf.get_u32() as usize;
                if len > crate::ltc_codec::CONTROL_MAX_FRAME_LENGTH || buf.remaining() != len {
                    return None;
                }
                // 只取出指定长度的字节，避免复制全部剩余数据
                let data = buf.split_to(len);
                let s = String::from_utf8(data.to_vec()).ok()?;
                Some(ClientSuccessResp::Info(s))
            }

            1 => {
                if buf.remaining() < 4 {
                    return None;
                }
                let len = buf.get_u32() as usize;
                if len == 0 || len > 128 || buf.remaining() != len {
                    return None;
                }
                // 只取出指定长度的字节，避免复制全部剩余数据
                let data = buf.split_to(len);
                let s = String::from_utf8(data.to_vec()).ok()?;
                Some(ClientSuccessResp::DataId(s))
            }
            2 => {
                if buf.remaining() < 4 {
                    return None;
                }
                let id_len = buf.get_u32() as usize;
                if id_len == 0 || id_len > 128 || buf.remaining() < id_len + 8 + 4 {
                    return None;
                }
                let data_id = String::from_utf8(buf.split_to(id_len).to_vec()).ok()?;
                let total = buf.get_u64();
                if total > crate::file_util::MAX_BIG_FILE_BYTES {
                    return None;
                }
                let hash_len = buf.get_u32() as usize;
                if hash_len != SHA256_BYTES || buf.remaining() != hash_len {
                    return None;
                }
                Some(ClientSuccessResp::BigFile {
                    data_id,
                    total,
                    hash: buf.to_vec(),
                })
            }
            _ => None, // 未知变体
        }
    }
}

impl BufSerializable for KikResp {
    fn to_buf(&self) -> BytesMut {
        let mut bytes = BytesMut::new();
        match self {
            KikResp::Success(resp) => {
                bytes.put_u8(0);
                bytes.put(resp.to_buf());
            }
            KikResp::Error(code, msg) => {
                bytes.put_u8(1);
                bytes.put_u8(*code);
                bytes.put_u32(msg.len() as u32);
                bytes.put_slice(msg.as_bytes());
            }
        }
        bytes
    }

    fn from_buf(mut buf: BytesMut) -> Option<Self> {
        if buf.remaining() < 1 {
            return None;
        }
        let variant = buf.get_u8();
        match variant {
            0 => {
                // Success 变体：剩余部分必须是完整的 ClientSuccessResp
                let inner = ClientSuccessResp::from_buf(buf)?;
                Some(KikResp::Success(inner))
            }
            1 => {
                // Error 变体：至少需要 1 字节错误码 + 4 字节长度
                if buf.remaining() < 5 {
                    return None;
                }
                let err_code = buf.get_u8();
                let len = buf.get_u32() as usize;
                if len > 64 * 1024 || buf.remaining() != len {
                    return None;
                }
                let data = buf.split_to(len);
                let err_msg = String::from_utf8(data.to_vec()).ok()?;
                Some(KikResp::Error(err_code, err_msg))
            }
            _ => None,
        }
    }
}

pub fn kik_success_info(info: String) -> KikResp {
    KikResp::Success(ClientSuccessResp::Info(info))
}

pub fn kik_success_data_id(id: String) -> KikResp {
    KikResp::Success(ClientSuccessResp::DataId(id))
}

pub fn kik_success_big_file(data_id: String, total: u64, hash: Vec<u8>) -> KikResp {
    KikResp::Success(ClientSuccessResp::BigFile {
        data_id,
        total,
        hash,
    })
}

pub fn kik_error(message: String) -> KikResp {
    KikResp::Error(protocol::ErrCode::EXCEPTION as u8, message)
}

#[test]
fn test() {
    let bytes_mut = KikResp::Success(ClientSuccessResp::Info("wettw".to_string())).to_buf();
    println!("{:?}", KikResp::from_buf(bytes_mut).unwrap());
    let bytes_mut = KikResp::Error(1, "wettw".to_string()).to_buf();
    println!("{:?}", KikResp::from_buf(bytes_mut).unwrap());
}

#[test]
fn big_file_metadata_round_trip() {
    let response = ClientSuccessResp::BigFile {
        data_id: "data-id".to_string(),
        total: 42,
        hash: vec![7; SHA256_BYTES],
    };

    match ClientSuccessResp::from_buf(response.to_buf()) {
        Some(ClientSuccessResp::BigFile {
            data_id,
            total,
            hash,
        }) => {
            assert_eq!(data_id, "data-id");
            assert_eq!(total, 42);
            assert_eq!(hash, vec![7; SHA256_BYTES]);
        }
        _ => panic!("unexpected response"),
    }
}

#[test]
fn big_file_metadata_rejects_invalid_bounds() {
    let oversized = ClientSuccessResp::BigFile {
        data_id: "data-id".to_string(),
        total: crate::file_util::MAX_BIG_FILE_BYTES + 1,
        hash: vec![7; SHA256_BYTES],
    };
    assert!(ClientSuccessResp::from_buf(oversized.to_buf()).is_none());

    let short_hash = ClientSuccessResp::BigFile {
        data_id: "data-id".to_string(),
        total: 42,
        hash: vec![7; SHA256_BYTES - 1],
    };
    assert!(ClientSuccessResp::from_buf(short_hash.to_buf()).is_none());

    let mut trailing =
        kik_success_big_file("data-id".to_string(), 42, vec![7; SHA256_BYTES]).to_buf();
    trailing.put_u8(0);
    assert!(KikResp::from_buf(trailing).is_none());
}
