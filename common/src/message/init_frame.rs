use bytes::{Buf, BufMut, BytesMut};
use crate::kik_info::KikInfo;
use crate::protocol::BufSerializable;

#[derive(Debug, Clone)]
pub enum InitFrame {
    CtrlAuthReply(bool),
    CtrlAuthReq(String),
    //与AuthReq携带一样的身份信息，作为控制端接收数据的连接
    CtrlDataConnReq(String),
    CtrlDataConnAuthReply(bool),

    //被控端发起被控请求，可能是重连或者新建连接
    KikReq(KikInfo),
    //服务端为被控端分配id(id具有足够随机性,作为被控端的身份识别)
    KikId(String),
    //被控端数据连接请求(id具有足够随机性,作为被控端的身份识别)，当有数据连接，加入全连接(被控上线并初始化完成)map<String,Pool>
    KikDataConnReq(String),
    //被控端数据连接成功与否
    KikDataConn(bool),
}


impl BufSerializable for InitFrame {
    fn to_buf(&self) -> BytesMut {
        match self {
            InitFrame::CtrlAuthReply(success) => {
                let mut buf = BytesMut::with_capacity(2);
                buf.put_u8(0);
                buf.put_u8(if *success { 1 } else { 0 });
                buf
            }
            InitFrame::CtrlAuthReq(info) => {
                let mut buf = BytesMut::with_capacity(1 + info.len());
                buf.put_u8(1);
                buf.put(info.as_bytes());
                buf
            }
            InitFrame::CtrlDataConnReq(info) => {
                let mut buf = BytesMut::with_capacity(1 + info.len());
                buf.put_u8(2);
                buf.put(info.as_bytes());
                buf
            }
            InitFrame::CtrlDataConnAuthReply(success) => {
                let mut buf = BytesMut::with_capacity(2);
                buf.put_u8(3);
                buf.put_u8(if *success { 1 } else { 0 });
                buf
            }
            InitFrame::KikReq(info) => {
                let mut buf = BytesMut::with_capacity(1 + info.to_buf().len());
                buf.put_u8(4);
                buf.put(info.to_buf());
                buf
            }
            InitFrame::KikId(id) => {
                let mut buf = BytesMut::with_capacity(1 + id.len());
                buf.put_u8(5);
                buf.put(id.as_bytes());
                buf
            }
            InitFrame::KikDataConnReq(id) => {
                let mut buf = BytesMut::with_capacity(1 + id.len());
                buf.put_u8(6);
                buf.put(id.as_bytes());
                buf
            }
            InitFrame::KikDataConn(success) => {
                let mut buf = BytesMut::with_capacity(2);
                buf.put_u8(7);
                buf.put_u8(if *success { 1 } else { 0 });
                buf
            }
        }
    }

    fn from_buf(mut bys: BytesMut) -> Option<Self>
    where
        Self: Sized,
    {
        // 至少需要一个字节用于 code
        if bys.remaining() < 1 {
            return None;
        }
        let code = bys.get_u8();

        match code {
            0 => {
                // CtrlAuthReply: 1 字节布尔值，之后不能有额外数据
                if bys.remaining() < 1 {
                    return None;
                }
                let value = bys.get_u8() == 1;
                if bys.remaining() != 0 {
                    return None;
                }
                Some(InitFrame::CtrlAuthReply(value))
            }
            1 => {
                // CtrlAuthReq: 剩余全部字节作为字符串
                let data = bys.split_to(bys.remaining());
                let s = String::from_utf8(data.to_vec()).ok()?;
                Some(InitFrame::CtrlAuthReq(s))
            }
            2 => {
                // CtrlDataConnReq: 剩余全部字节作为字符串
                let data = bys.split_to(bys.remaining());
                let s = String::from_utf8(data.to_vec()).ok()?;
                Some(InitFrame::CtrlDataConnReq(s))
            }
            3 => {
                // CtrlDataConnAuthReply: 1 字节布尔值，之后不能有额外数据
                if bys.remaining() < 1 {
                    return None;
                }
                let value = bys.get_u8() == 1;
                if bys.remaining() != 0 {
                    return None;
                }
                Some(InitFrame::CtrlDataConnAuthReply(value))
            }
            4 => {
                // KikReq: 依赖 KikInfo::from_buf 解析，它会消费整个 BytesMut
                // 注意：如果 KikInfo::from_buf 没有消费完整缓冲区，剩余数据会被丢弃
                let frame = KikInfo::from_buf(bys)?;
                Some(InitFrame::KikReq(frame))
            }
            5 => {
                // KikId: 剩余全部字节作为字符串
                let data = bys.split_to(bys.remaining());
                let s = String::from_utf8(data.to_vec()).ok()?;
                Some(InitFrame::KikId(s))
            }
            6 => {
                // KikDataConnReq: 剩余全部字节作为字符串
                let data = bys.split_to(bys.remaining());
                let s = String::from_utf8(data.to_vec()).ok()?;
                Some(InitFrame::KikDataConnReq(s))
            }
            7 => {
                // KikDataConn: 1 字节布尔值，之后不能有额外数据
                if bys.remaining() < 1 {
                    return None;
                }
                let value = bys.get_u8() == 1;
                if bys.remaining() != 0 {
                    return None;
                }
                Some(InitFrame::KikDataConn(value))
            }
            _ => None,
        }
    }
}







mod test{
    use crate::kik_info::KikInfo;
    use crate::message::init_frame::InitFrame;
    use crate::protocol::BufSerializable;

    fn assert_round_trip(frame: InitFrame) {
        let bytes = frame.to_buf();
        let decoded = InitFrame::from_buf(bytes.clone()).expect("反序列化失败");
        // 由于 InitFrame 实现了 Debug 和 Clone，可以直接比较
        // 注意：对于包含 KikInfo 的变体，需要确保 KikInfo 本身也实现了 PartialEq
        // 这里我们使用格式化字符串进行比较，因为 InitFrame 没有自动派生 PartialEq
        // 或者我们可以比较重新序列化后的字节是否一致
        let re_encoded = decoded.to_buf();
        assert_eq!(bytes, re_encoded, "往返序列化后字节不一致");
        println!("ok")
    }

    #[test]
    fn test_ctrl_auth_reply_true() {
        assert_round_trip(InitFrame::CtrlAuthReply(true));
    }

    #[test]
    fn test_ctrl_auth_reply_false() {
        assert_round_trip(InitFrame::CtrlAuthReply(false));
    }

    #[test]
    fn test_ctrl_auth_req() {
        let info = "test_auth_info".to_string();
        assert_round_trip(InitFrame::CtrlAuthReq(info));
    }

    #[test]
    fn test_ctrl_auth_req_empty() {
        assert_round_trip(InitFrame::CtrlAuthReq(String::new()));
    }

    #[test]
    fn test_ctrl_data_conn_req() {
        let info = "ctrl_data_conn_identity".to_string();
        assert_round_trip(InitFrame::CtrlDataConnReq(info));
    }

    #[test]
    fn test_ctrl_data_conn_auth_reply_true() {
        assert_round_trip(InitFrame::CtrlDataConnAuthReply(true));
    }

    #[test]
    fn test_ctrl_data_conn_auth_reply_false() {
        assert_round_trip(InitFrame::CtrlDataConnAuthReply(false));
    }

    #[test]
    fn test_kik_req() {
        // 构造模拟的 KikInfo
        let kik_info = KikInfo{
            id: Some("kik_example_data".into()),
            name: "sfsdf".to_string()
        };
        // 注意：原始代码中 InitFrame::KikReq 使用 KikInfo 类型，但此处测试中我们替换为 MockKikInfo。
        // 由于实际类型不匹配，此测试需要在实际项目中进行调整。这里仅演示测试思路。
        // 为了能够编译，可以注释掉此测试，或在实际项目中编写。
        assert_round_trip(InitFrame::KikReq(kik_info));
    }

    #[test]
    fn test_kik_id() {
        let id = "random_id_12345".to_string();
        assert_round_trip(InitFrame::KikId(id));
    }

    #[test]
    fn test_kik_data_conn_req() {
        let id = "kik_data_conn_id".to_string();
        assert_round_trip(InitFrame::KikDataConnReq(id));
    }

    #[test]
    fn test_kik_data_conn_true() {
        assert_round_trip(InitFrame::KikDataConn(true));
    }

    #[test]
    fn test_kik_data_conn_false() {
        assert_round_trip(InitFrame::KikDataConn(false));
    }
}
