use crate::protocol::BufSerializable;
use bytes::{Buf, BufMut, BytesMut};

//todo 增加Err帧
#[derive(Debug, Clone)]
pub enum Resp {
    Info(String),
    DataId(String),
}

impl BufSerializable for Resp {
    fn to_buf(&self) -> BytesMut {
        match self {
            Resp::Info(s) => {
                let mut bytes_mut = BytesMut::new();
                bytes_mut.put_u8(0);
                bytes_mut.put_slice(s.as_bytes());
                bytes_mut
            }
            Resp::DataId(id) => {
                let mut bytes_mut = BytesMut::new();
                bytes_mut.put_u8(1);
                bytes_mut.put_slice(id.as_bytes());
                bytes_mut
            }
        }
    }
    fn from_buf(mut bys: BytesMut) -> Option<Self> {
        let code = bys.get_u8();
        match code {
            0 => Some(Resp::Info(String::from_utf8(bys.to_vec()).ok()?)),
            1 => Some(Resp::DataId(String::from_utf8(bys.to_vec()).ok()?)),
            _ => None,
        }
    }
}
#[test]
fn test() {
    let bytes_mut = Resp::Info("wettw".to_string()).to_buf();
    println!("{:?}", Resp::from_buf(bytes_mut).unwrap());
}
