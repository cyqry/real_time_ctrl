//! Kik 注册阶段使用的最小身份信息。
//!
//! ID 用于重连和路由，名称用于展示；两者都不是安全凭据。Kik 的强安全边界是固定服务端公钥的
//! Noise 链路，控制权限则由服务端账号 ACL 决定。

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
                let name = String::from_utf8(bys.to_vec()).ok()?;
                if !valid_kik_name(&name) {
                    return None;
                }
                Some(KikInfo { id: None, name })
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
                let id = String::from_utf8(bys.split_to(id_len as usize).to_vec()).ok()?;
                let name = String::from_utf8(bys.to_vec()).ok()?;
                if !valid_kik_name(&name) {
                    return None;
                }
                Some(KikInfo { id: Some(id), name })
            }
            _ => None,
        }
    }
}

/// Kik 名称会进入服务端日志和管理面 JSON；禁止控制字符可阻断换行伪造日志，
/// 同时保留中文、空格等正常 Windows 计算机名显示所需字符。
fn valid_kik_name(name: &str) -> bool {
    !name.is_empty() && name.len() <= MAX_KIK_NAME_BYTES && !name.chars().any(char::is_control)
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

    #[test]
    fn kik_name_rejects_log_control_characters() {
        for name in ["forged\nentry", "forged\rentry", "forged\tentry", "\0"] {
            assert!(KikInfo::from_buf(
                KikInfo {
                    id: None,
                    name: name.to_string(),
                }
                .to_buf()
            )
            .is_none());
        }

        let valid = KikInfo {
            id: None,
            name: "生产 被控端-01".to_string(),
        };
        assert!(KikInfo::from_buf(valid.to_buf()).is_some());
    }
}
