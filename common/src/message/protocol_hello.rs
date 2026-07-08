use crate::protocol::BufSerializable;
use bytes::{Buf, BufMut, BytesMut};

pub const PROTOCOL_HELLO_MAGIC: &[u8; 4] = b"RTHL";
pub const PROTOCOL_VERSION_V1: u16 = 1;
/// real_ctrl 可校验服务端身份的能力位。
pub const CAP_SERVER_IDENTITY_PIN: u32 = 1 << 0;
/// 数据通道可绑定到已认证控制会话的能力位。
pub const CAP_SESSION_BINDING: u32 = 1 << 1;
/// ctrl_kik 采用最小客户端模型的能力位。
pub const CAP_LIMITED_CTRL_KIK: u32 = 1 << 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProtocolRole {
    Ctrl,
    CtrlData,
    Kik,
    KikData,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProtocolHello {
    /// 协议版本用于兼容迁移，当前版本不强制替换旧 InitFrame。
    pub version: u16,
    /// 连接角色决定后续帧上限、鉴权策略和分派路径。
    pub role: ProtocolRole,
    /// 能力位只表达协议能力，不携带服务端机器信息。
    pub capabilities: u32,
}

impl ProtocolHello {
    pub fn new(role: ProtocolRole, capabilities: u32) -> Self {
        Self {
            version: PROTOCOL_VERSION_V1,
            role,
            capabilities,
        }
    }
}

impl ProtocolRole {
    fn to_code(self) -> u8 {
        match self {
            ProtocolRole::Ctrl => 0,
            ProtocolRole::CtrlData => 1,
            ProtocolRole::Kik => 2,
            ProtocolRole::KikData => 3,
        }
    }

    fn from_code(code: u8) -> Option<Self> {
        match code {
            0 => Some(ProtocolRole::Ctrl),
            1 => Some(ProtocolRole::CtrlData),
            2 => Some(ProtocolRole::Kik),
            3 => Some(ProtocolRole::KikData),
            _ => None,
        }
    }
}

impl BufSerializable for ProtocolHello {
    fn to_buf(&self) -> BytesMut {
        let mut buf = BytesMut::with_capacity(11);
        buf.put_slice(PROTOCOL_HELLO_MAGIC);
        buf.put_u16(self.version);
        buf.put_u8(self.role.to_code());
        buf.put_u32(self.capabilities);
        buf
    }

    fn from_buf(mut bys: BytesMut) -> Option<Self>
    where
        Self: Sized,
    {
        if bys.remaining() != 11 {
            return None;
        }

        let magic = bys.split_to(4);
        if magic.as_ref() != PROTOCOL_HELLO_MAGIC {
            return None;
        }

        let version = bys.get_u16();
        let role = ProtocolRole::from_code(bys.get_u8())?;
        let capabilities = bys.get_u32();
        Some(Self {
            version,
            role,
            capabilities,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn protocol_hello_round_trip() {
        let hello = ProtocolHello::new(
            ProtocolRole::Ctrl,
            CAP_SERVER_IDENTITY_PIN | CAP_SESSION_BINDING,
        );

        let decoded = ProtocolHello::from_buf(hello.to_buf()).unwrap();

        assert_eq!(hello, decoded);
    }

    #[test]
    fn protocol_hello_rejects_bad_magic() {
        let mut buf = ProtocolHello::new(ProtocolRole::Kik, CAP_LIMITED_CTRL_KIK).to_buf();
        buf[0] = b'X';

        assert!(ProtocolHello::from_buf(buf).is_none());
    }

    #[test]
    fn protocol_hello_rejects_unknown_role() {
        let mut buf = ProtocolHello::new(ProtocolRole::Kik, CAP_LIMITED_CTRL_KIK).to_buf();
        buf[6] = 99;

        assert!(ProtocolHello::from_buf(buf).is_none());
    }
}
