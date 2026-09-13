use crate::kik_info::KikInfo;
use crate::protocol::BufSerializable;
use bytes::{Buf, BufMut, BytesMut};

const MAX_AUTH_FIELD_BYTES: usize = 256;
const MAX_KIK_ID_BYTES: usize = 128;

const KIK_REQ: u8 = 0;
const KIK_ID: u8 = 1;
const KIK_DATA_REQ: u8 = 2;
const KIK_DATA_REPLY: u8 = 3;
const CTRL_AUTH_START: u8 = 4;
const CTRL_AUTH_CHALLENGE: u8 = 5;
const CTRL_AUTH_PROOF: u8 = 6;
const CTRL_AUTH_SESSION: u8 = 7;
const CTRL_DATA_SESSION_REQ: u8 = 8;
const CTRL_DATA_SESSION_REPLY: u8 = 9;

#[derive(Debug, Clone)]
pub enum InitFrame {
    // 被控端上线和数据连接帧。Kik 传输只允许 Noise NK。
    KikReq(KikInfo),
    KikId(String),
    KikDataConnReq(String),
    KikDataConn(bool),

    // TLS 内使用 nonce + HMAC，避免长期认证秘密直接过线。
    CtrlAuthStart {
        account_id: String,
        instance_id: String,
        client_nonce: String,
    },
    CtrlAuthChallenge(String),
    CtrlAuthProof {
        client_nonce: String,
        proof: String,
    },
    CtrlAuthSession(String),

    // 数据通道必须绑定已认证的控制会话。
    CtrlDataSessionReq {
        session_id: String,
        channel_nonce: String,
        proof: String,
    },
    CtrlDataSessionReply(bool),
}

impl BufSerializable for InitFrame {
    fn to_buf(&self) -> BytesMut {
        match self {
            InitFrame::KikReq(info) => {
                let info_buf = info.to_buf();
                let mut buf = BytesMut::with_capacity(1 + info_buf.len());
                buf.put_u8(KIK_REQ);
                buf.put(info_buf);
                buf
            }
            InitFrame::KikId(id) => single_string_frame(KIK_ID, id),
            InitFrame::KikDataConnReq(id) => single_string_frame(KIK_DATA_REQ, id),
            InitFrame::KikDataConn(success) => bool_frame(KIK_DATA_REPLY, *success),
            InitFrame::CtrlAuthStart {
                account_id,
                instance_id,
                client_nonce,
            } => multi_string_frame(CTRL_AUTH_START, &[account_id, instance_id, client_nonce]),
            InitFrame::CtrlAuthChallenge(server_nonce) => {
                single_string_frame(CTRL_AUTH_CHALLENGE, server_nonce)
            }
            InitFrame::CtrlAuthProof {
                client_nonce,
                proof,
            } => multi_string_frame(CTRL_AUTH_PROOF, &[client_nonce, proof]),
            InitFrame::CtrlAuthSession(session_id) => {
                single_string_frame(CTRL_AUTH_SESSION, session_id)
            }
            InitFrame::CtrlDataSessionReq {
                session_id,
                channel_nonce,
                proof,
            } => multi_string_frame(CTRL_DATA_SESSION_REQ, &[session_id, channel_nonce, proof]),
            InitFrame::CtrlDataSessionReply(success) => {
                bool_frame(CTRL_DATA_SESSION_REPLY, *success)
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
            KIK_REQ => Some(InitFrame::KikReq(KikInfo::from_buf(bys)?)),
            KIK_ID => Some(InitFrame::KikId(read_exact_string(
                &mut bys,
                MAX_KIK_ID_BYTES,
                false,
            )?)),
            KIK_DATA_REQ => Some(InitFrame::KikDataConnReq(read_exact_string(
                &mut bys,
                MAX_KIK_ID_BYTES,
                false,
            )?)),
            KIK_DATA_REPLY => Some(InitFrame::KikDataConn(read_bool(&mut bys)?)),
            CTRL_AUTH_START => {
                let account_id = read_one_string(&mut bys)?;
                let instance_id = read_one_string(&mut bys)?;
                let client_nonce = read_one_string(&mut bys)?;
                if bys.has_remaining() {
                    return None;
                }
                Some(InitFrame::CtrlAuthStart {
                    account_id,
                    instance_id,
                    client_nonce,
                })
            }
            CTRL_AUTH_CHALLENGE => Some(InitFrame::CtrlAuthChallenge(read_exact_string(
                &mut bys,
                MAX_AUTH_FIELD_BYTES,
                false,
            )?)),
            CTRL_AUTH_PROOF => {
                let client_nonce = read_one_string(&mut bys)?;
                let proof = read_one_string(&mut bys)?;
                if bys.has_remaining() {
                    return None;
                }
                Some(InitFrame::CtrlAuthProof {
                    client_nonce,
                    proof,
                })
            }
            CTRL_AUTH_SESSION => Some(InitFrame::CtrlAuthSession(read_exact_string(
                &mut bys,
                MAX_AUTH_FIELD_BYTES,
                false,
            )?)),
            CTRL_DATA_SESSION_REQ => {
                let session_id = read_one_string(&mut bys)?;
                let channel_nonce = read_one_string(&mut bys)?;
                let proof = read_one_string(&mut bys)?;
                if bys.has_remaining() {
                    return None;
                }
                Some(InitFrame::CtrlDataSessionReq {
                    session_id,
                    channel_nonce,
                    proof,
                })
            }
            CTRL_DATA_SESSION_REPLY => Some(InitFrame::CtrlDataSessionReply(read_bool(&mut bys)?)),
            _ => None,
        }
    }
}

fn bool_frame(code: u8, success: bool) -> BytesMut {
    let mut buf = BytesMut::with_capacity(2);
    buf.put_u8(code);
    buf.put_u8(if success { 1 } else { 0 });
    buf
}

fn single_string_frame(code: u8, value: &str) -> BytesMut {
    let mut buf = BytesMut::with_capacity(1 + 4 + value.len());
    buf.put_u8(code);
    write_string(&mut buf, value);
    buf
}

fn multi_string_frame(code: u8, values: &[&str]) -> BytesMut {
    let size = 1 + values.iter().map(|value| 4 + value.len()).sum::<usize>();
    let mut buf = BytesMut::with_capacity(size);
    buf.put_u8(code);
    for value in values {
        write_string(&mut buf, value);
    }
    buf
}

fn write_string(buf: &mut BytesMut, value: &str) {
    buf.put_u32(value.len() as u32);
    buf.put_slice(value.as_bytes());
}

fn read_bool(buf: &mut BytesMut) -> Option<bool> {
    if buf.remaining() != 1 {
        return None;
    }
    match buf.get_u8() {
        0 => Some(false),
        1 => Some(true),
        _ => None,
    }
}

fn read_one_string(buf: &mut BytesMut) -> Option<String> {
    if buf.remaining() < 4 {
        return None;
    }
    let len = buf.get_u32() as usize;
    if len == 0 || len > MAX_AUTH_FIELD_BYTES || buf.remaining() < len {
        return None;
    }
    String::from_utf8(buf.split_to(len).to_vec()).ok()
}

fn read_exact_string(buf: &mut BytesMut, max_bytes: usize, allow_empty: bool) -> Option<String> {
    if buf.remaining() < 4 {
        return None;
    }
    let len = buf.get_u32() as usize;
    if (!allow_empty && len == 0) || len > max_bytes || buf.remaining() != len {
        return None;
    }
    let value = String::from_utf8(buf.split_to(len).to_vec()).ok()?;
    if buf.has_remaining() {
        return None;
    }
    Some(value)
}

#[cfg(test)]
mod tests {
    use super::CTRL_AUTH_START;
    use crate::kik_info::KikInfo;
    use crate::message::init_frame::InitFrame;
    use crate::protocol::BufSerializable;
    use bytes::{BufMut, BytesMut};

    fn assert_round_trip(frame: InitFrame) {
        let bytes = frame.to_buf();
        let decoded = InitFrame::from_buf(bytes.clone()).expect("反序列化失败");
        assert_eq!(bytes, decoded.to_buf(), "往返序列化后字节不一致");
    }

    #[test]
    fn kik_frames_round_trip() {
        assert_round_trip(InitFrame::KikReq(KikInfo {
            id: Some("kik_example_data".into()),
            name: "test".to_string(),
        }));
        assert_round_trip(InitFrame::KikId("random_id_12345".to_string()));
        assert_round_trip(InitFrame::KikDataConnReq("kik_data_conn_id".to_string()));
        assert_round_trip(InitFrame::KikDataConn(true));
        assert_round_trip(InitFrame::KikDataConn(false));
    }

    #[test]
    fn ctrl_auth_frames_round_trip() {
        assert_round_trip(InitFrame::CtrlAuthStart {
            account_id: "account".to_string(),
            instance_id: "instance".to_string(),
            client_nonce: "client_nonce".to_string(),
        });
        assert_round_trip(InitFrame::CtrlAuthChallenge("server_nonce".to_string()));
        assert_round_trip(InitFrame::CtrlAuthProof {
            client_nonce: "client_nonce".to_string(),
            proof: "proof".to_string(),
        });
        assert_round_trip(InitFrame::CtrlAuthSession("session_id".to_string()));
        assert_round_trip(InitFrame::CtrlDataSessionReq {
            session_id: "session_id".to_string(),
            channel_nonce: "channel_nonce".to_string(),
            proof: "proof".to_string(),
        });
        assert_round_trip(InitFrame::CtrlDataSessionReply(true));
        assert_round_trip(InitFrame::CtrlDataSessionReply(false));
    }

    #[test]
    fn ctrl_auth_rejects_short_strings() {
        let mut buf = BytesMut::new();
        buf.put_u8(CTRL_AUTH_START);
        buf.put_u32(16);
        buf.put_slice(b"short");

        assert!(InitFrame::from_buf(buf).is_none());
    }

    #[test]
    fn ctrl_auth_rejects_trailing_bytes() {
        let mut buf = InitFrame::CtrlAuthStart {
            account_id: "account".to_string(),
            instance_id: "instance".to_string(),
            client_nonce: "client".to_string(),
        }
        .to_buf();
        buf.put_u8(99);

        assert!(InitFrame::from_buf(buf).is_none());
    }

    #[test]
    fn bool_frames_reject_trailing_bytes() {
        let mut buf = InitFrame::CtrlDataSessionReply(true).to_buf();
        buf.put_u8(99);

        assert!(InitFrame::from_buf(buf).is_none());

        let mut invalid = InitFrame::CtrlDataSessionReply(false).to_buf();
        invalid[1] = 2;
        assert!(InitFrame::from_buf(invalid).is_none());
    }
}
