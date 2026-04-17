use crate::protocol::{BufSerializable};
use bytes::{Buf, BufMut, BytesMut};
use crate::protocol;
#[derive(Clone, Debug)]
pub enum KikResp {
    Success(ClientSuccessResp),
    Error(u8, String),
}

#[derive(Clone, Debug)]
pub enum ClientSuccessResp {
    Info(String),
    DataId(String),
}

impl BufSerializable for ClientSuccessResp {
    fn to_buf(&self) -> BytesMut {
        let mut bytes = BytesMut::new();
        match self {
            ClientSuccessResp::Info(s) => {
                bytes.put_u8(0);                     // 变体标识
                bytes.put_u32(s.len() as u32);       // 字符串长度前缀
                bytes.put_slice(s.as_bytes());       // 内容
            }
            ClientSuccessResp::DataId(id) => {
                bytes.put_u8(1);
                bytes.put_u32(id.len() as u32);
                bytes.put_slice(id.as_bytes());
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
                if buf.remaining() < len {
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
                if buf.remaining() < len {
                    return None;
                }
                // 只取出指定长度的字节，避免复制全部剩余数据
                let data = buf.split_to(len);
                let s = String::from_utf8(data.to_vec()).ok()?;
                Some(ClientSuccessResp::DataId(s))
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
                if buf.remaining() < len {
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
