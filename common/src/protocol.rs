use crate::command::Command;
use crate::message::frame::Frame;
use crate::message::resp::Resp;
use bytes::{Buf, BufMut, BytesMut};

pub trait BufSerializable {
    fn to_buf(&self) -> BytesMut;
    fn from_buf(bys: BytesMut) -> Option<Self>
    where
        Self: Sized;
}

//对应 ltc解码器 data长度 data内容的格式
pub fn transfer_encode(bts: BytesMut) -> BytesMut {
    if bts.len() > u32::MAX as usize {
        panic!("要传输的数据太大")
    }
    let mut bytes_mut = BytesMut::with_capacity(bts.len() + 4);
    bytes_mut.put_slice(&(bts.len() as u32).to_be_bytes());
    bytes_mut.put(bts);
    bytes_mut
}

pub fn transfer_b_encode(bts: &[u8], start: u32, end: u32) -> BytesMut {
    let len = end - start;
    if len > u32::MAX {
        panic!("要传输的数据太大")
    }
    let mut bytes_mut = BytesMut::with_capacity((len + 4) as usize);
    bytes_mut.put_slice(&len.to_be_bytes());
    bytes_mut.put_slice(&bts[start as usize..end as usize]);
    bytes_mut
}

pub fn frame_decode(bys: BytesMut) -> Option<Frame> {
    Frame::from_buf(bys)
}

pub fn resp(resp: Resp) -> BytesMut {
    transfer_encode(Frame::Resp(resp).to_buf())
}

pub fn cmd(cmd: Command) -> BytesMut {
    transfer_encode(Frame::Cmd(cmd).to_buf())
}

pub fn ping() -> BytesMut {
    transfer_encode(Frame::Ping.to_buf())
}

pub fn pong() -> BytesMut {
    transfer_encode(Frame::Pong.to_buf())
}
