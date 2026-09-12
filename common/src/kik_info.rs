use crate::protocol::BufSerializable;
use bytes::{Buf, BufMut, BytesMut};
const MAX_KIK_ID_BYTES: usize = 128;
// 名称会进入管理面列表响应；限制为 128 字节可确保最坏 JSON 转义后仍受 1 MiB 控制帧约束。
const MAX_KIK_NAME_BYTES: usize = 128;
#[derive(Clone, Debug)]
pub struct KikInfo {
    pub id: Option<String>,
    pub name: String,
}

impl BufSerializable for KikInfo {
    fn to_buf(&self) -> BytesMut {
        let mut bytes_mut = BytesMut::new();
        match &self.id {
            None => {
                bytes_mut.put_u8(0);
                bytes_mut.put_slice(self.name.as_bytes());
                bytes_mut
            }
            Some(id) => {
                bytes_mut.put_u8(1);
                bytes_mut.put_u32(id.len() as u32);
                bytes_mut.put_slice(id.as_bytes());
                bytes_mut.put_slice(self.name.as_bytes());
                bytes_mut
            }
        }
    }

    fn from_buf(mut bys: BytesMut) -> Option<Self>
    where
        Self: Sized,
    {
        if bys.remaining() < 1 {
            return None;
        }
        let code = bys.get_u8();
        match code {
            0 => {
                if bys.is_empty() || bys.remaining() > MAX_KIK_NAME_BYTES {
                    return None;
                }
                Some(KikInfo {
                    id: None,
                    name: String::from_utf8(bys.to_vec()).ok()?,
                })
            }
            1 => {
                if bys.remaining() < 4 {
                    return None;
                }
                let id_len = bys.get_u32();
                if id_len == 0
                    || id_len as usize > MAX_KIK_ID_BYTES
                    || bys.remaining() < id_len as usize
                    || bys.remaining() == id_len as usize
                    || bys.remaining() - id_len as usize > MAX_KIK_NAME_BYTES
                {
                    return None;
                }
                Some(KikInfo {
                    id: Some(String::from_utf8(bys.split_to(id_len as usize).to_vec()).ok()?),
                    name: String::from_utf8(bys.to_vec()).ok()?,
                })
            }
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn malformed_kik_info_returns_none() {
        assert!(KikInfo::from_buf(BytesMut::new()).is_none());
        assert!(KikInfo::from_buf(BytesMut::from(&[1, 0, 0][..])).is_none());
        assert!(KikInfo::from_buf(BytesMut::from(&[0][..])).is_none());
        assert!(KikInfo::from_buf(BytesMut::from(&[1, 0, 0, 0, 0, b'x'][..])).is_none());
    }
}
