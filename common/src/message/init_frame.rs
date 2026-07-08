use crate::kik_info::KikInfo;
use crate::protocol::BufSerializable;
use bytes::{Buf, BufMut, BytesMut};

#[derive(Debug, Clone)]
pub enum InitFrame {
    CtrlAuthReply(bool),
    CtrlAuthReq(String),
    // 旧数据通道鉴权帧，保留给显式明文兼容模式。
    CtrlDataConnReq(String),
    CtrlDataConnAuthReply(bool),

    // 被控端上线和数据连接帧。ctrl_kik 默认仍保持最小客户端模型。
    KikReq(KikInfo),
    KikId(String),
    KikDataConnReq(String),
    KikDataConn(bool),

    // v2 控制端鉴权：TLS 内使用 nonce + HMAC，避免静态摘要直接过线。
    CtrlAuthStart(String),
    CtrlAuthChallenge(String),
    CtrlAuthProof {
        client_nonce: String,
        proof: String,
    },
    CtrlAuthSession(String),

    // v2 数据通道绑定：数据通道必须绑定已认证的控制会话。
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
            InitFrame::CtrlAuthReply(success) => bool_frame(0, *success),
            InitFrame::CtrlAuthReq(info) => legacy_string_frame(1, info),
            InitFrame::CtrlDataConnReq(info) => legacy_string_frame(2, info),
            InitFrame::CtrlDataConnAuthReply(success) => bool_frame(3, *success),
            InitFrame::KikReq(info) => {
                let info_buf = info.to_buf();
                let mut buf = BytesMut::with_capacity(1 + info_buf.len());
                buf.put_u8(4);
                buf.put(info_buf);
                buf
            }
            InitFrame::KikId(id) => legacy_string_frame(5, id),
            InitFrame::KikDataConnReq(id) => legacy_string_frame(6, id),
            InitFrame::KikDataConn(success) => bool_frame(7, *success),
            InitFrame::CtrlAuthStart(client_nonce) => single_string_frame(8, client_nonce),
            InitFrame::CtrlAuthChallenge(server_nonce) => single_string_frame(9, server_nonce),
            InitFrame::CtrlAuthProof {
                client_nonce,
                proof,
            } => multi_string_frame(10, &[client_nonce, proof]),
            InitFrame::CtrlAuthSession(session_id) => single_string_frame(11, session_id),
            InitFrame::CtrlDataSessionReq {
                session_id,
                channel_nonce,
                proof,
            } => multi_string_frame(12, &[session_id, channel_nonce, proof]),
            InitFrame::CtrlDataSessionReply(success) => bool_frame(13, *success),
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
            0 => Some(InitFrame::CtrlAuthReply(read_bool(&mut bys)?)),
            1 => Some(InitFrame::CtrlAuthReq(read_remaining_string(bys)?)),
            2 => Some(InitFrame::CtrlDataConnReq(read_remaining_string(bys)?)),
            3 => Some(InitFrame::CtrlDataConnAuthReply(read_bool(&mut bys)?)),
            4 => Some(InitFrame::KikReq(KikInfo::from_buf(bys)?)),
            5 => Some(InitFrame::KikId(read_remaining_string(bys)?)),
            6 => Some(InitFrame::KikDataConnReq(read_remaining_string(bys)?)),
            7 => Some(InitFrame::KikDataConn(read_bool(&mut bys)?)),
            8 => Some(InitFrame::CtrlAuthStart(read_exact_one_string(&mut bys)?)),
            9 => Some(InitFrame::CtrlAuthChallenge(read_exact_one_string(&mut bys)?)),
            10 => {
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
            11 => Some(InitFrame::CtrlAuthSession(read_exact_one_string(&mut bys)?)),
            12 => {
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
            13 => Some(InitFrame::CtrlDataSessionReply(read_bool(&mut bys)?)),
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

fn legacy_string_frame(code: u8, value: &str) -> BytesMut {
    let mut buf = BytesMut::with_capacity(1 + value.len());
    buf.put_u8(code);
    buf.put_slice(value.as_bytes());
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
    Some(buf.get_u8() == 1)
}

fn read_remaining_string(mut buf: BytesMut) -> Option<String> {
    String::from_utf8(buf.split_to(buf.remaining()).to_vec()).ok()
}

fn read_one_string(buf: &mut BytesMut) -> Option<String> {
    if buf.remaining() < 4 {
        return None;
    }
    let len = buf.get_u32() as usize;
    if buf.remaining() < len {
        return None;
    }
    String::from_utf8(buf.split_to(len).to_vec()).ok()
}

fn read_exact_one_string(buf: &mut BytesMut) -> Option<String> {
    let value = read_one_string(buf)?;
    if buf.has_remaining() {
        return None;
    }
    Some(value)
}

#[cfg(test)]
mod tests {
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
    fn old_ctrl_frames_round_trip() {
        assert_round_trip(InitFrame::CtrlAuthReply(true));
        assert_round_trip(InitFrame::CtrlAuthReply(false));
        assert_round_trip(InitFrame::CtrlAuthReq("test_auth_info".to_string()));
        assert_round_trip(InitFrame::CtrlAuthReq(String::new()));
        assert_round_trip(InitFrame::CtrlDataConnReq(
            "ctrl_data_conn_identity".to_string(),
        ));
        assert_round_trip(InitFrame::CtrlDataConnAuthReply(true));
        assert_round_trip(InitFrame::CtrlDataConnAuthReply(false));
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
    fn ctrl_auth_v2_frames_round_trip() {
        assert_round_trip(InitFrame::CtrlAuthStart("client_nonce".to_string()));
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
    fn ctrl_auth_v2_rejects_short_strings() {
        let mut buf = BytesMut::new();
        buf.put_u8(8);
        buf.put_u32(16);
        buf.put_slice(b"short");

        assert!(InitFrame::from_buf(buf).is_none());
    }

    #[test]
    fn ctrl_auth_v2_rejects_trailing_bytes() {
        let mut buf = InitFrame::CtrlAuthStart("client".to_string()).to_buf();
        buf.put_u8(99);

        assert!(InitFrame::from_buf(buf).is_none());
    }

    #[test]
    fn bool_frames_reject_trailing_bytes() {
        let mut buf = InitFrame::CtrlAuthReply(true).to_buf();
        buf.put_u8(99);

        assert!(InitFrame::from_buf(buf).is_none());
    }
}
