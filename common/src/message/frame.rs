use crate::command::Command;
use crate::kik_info::KikInfo;
use crate::message::frame::Frame::*;
use crate::message::resp::Resp;
use crate::protocol::BufSerializable;
use bytes::{Buf, BufMut, BytesMut};
use log::debug;
use std::io::Read;

#[derive(Debug)]
pub enum Frame {
    CmdExtra(Command, String),
    RespExtra(Resp, String),
    Cmd(Command),
    Resp(Resp),

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

    Data(String, BytesMut), //数据传输的data帧

    //todo 增加ping pong负载，如果存在负载则需响应，用于探活
    Ping,
    Pong,
}

impl BufSerializable for Frame {
    fn to_buf(&self) -> BytesMut {
        match self {
            CmdExtra(cmd, s) => {
                let mut bytes_mut = BytesMut::new();
                bytes_mut.put_u8(14);
                let cmd_buf = cmd.to_buf();
                let cmd_len = cmd_buf.len() as u32;
                bytes_mut.put_u32(cmd_len);
                bytes_mut.put(cmd_buf);
                bytes_mut.put_slice(s.as_bytes());
                bytes_mut
            }
            RespExtra(resp, s) => {
                let mut bytes_mut = BytesMut::new();
                bytes_mut.put_u8(13);
                let resp_buf = resp.to_buf();
                let resp_len = resp_buf.len() as u32;
                bytes_mut.put_u32(resp_len);
                bytes_mut.put(resp_buf);
                bytes_mut.put_slice(s.as_bytes());
                bytes_mut
            }
            Data(data_id, bys) => {
                let mut bytes_mut = BytesMut::with_capacity(bys.len() + 1);
                bytes_mut.put_u8(12);

                let id_bys = data_id.as_bytes();
                bytes_mut.put_u32(id_bys.len() as u32);
                bytes_mut.put_slice(id_bys);
                bytes_mut.put_slice(bys);
                bytes_mut
            }
            Cmd(command) => {
                let mut bytes_mut = BytesMut::new();
                bytes_mut.put_u8(11);
                bytes_mut.put(command.to_buf());
                bytes_mut
            }
            Resp(resp) => {
                let mut bytes_mut = BytesMut::new();
                bytes_mut.put_u8(10);
                bytes_mut.put(resp.to_buf());
                bytes_mut
            }
            CtrlAuthReq(s) => {
                let mut bytes_mut = BytesMut::with_capacity(1 + s.as_bytes().len());
                bytes_mut.put_u8(9);
                bytes_mut.put_slice(s.as_bytes());
                bytes_mut
            }

            CtrlAuthReply(b) => {
                let mut bytes_mut = BytesMut::with_capacity(2);
                bytes_mut.put_u8(8);
                bytes_mut.put_u8(if *b { 1 } else { 0 });
                bytes_mut
            }
            CtrlDataConnReq(s) => {
                let mut bytes_mut = BytesMut::with_capacity(1 + s.as_bytes().len());
                bytes_mut.put_u8(7);
                bytes_mut.put_slice(s.as_bytes());
                bytes_mut
            }
            CtrlDataConnAuthReply(b) => {
                let mut bytes_mut = BytesMut::with_capacity(2);
                bytes_mut.put_u8(6);
                bytes_mut.put_u8(if *b { 1 } else { 0 });
                bytes_mut
            }
            KikReq(kik) => {
                let mut bytes_mut = BytesMut::with_capacity(2);
                bytes_mut.put_u8(5);
                bytes_mut.put(kik.to_buf());
                bytes_mut
            }

            KikId(s) => {
                let mut bytes_mut = BytesMut::with_capacity(1 + s.as_bytes().len());
                bytes_mut.put_u8(4);
                bytes_mut.put_slice(s.as_bytes());
                bytes_mut
            }
            KikDataConnReq(s) => {
                let mut bytes_mut = BytesMut::with_capacity(1 + s.as_bytes().len());
                bytes_mut.put_u8(3);
                bytes_mut.put_slice(s.as_bytes());
                bytes_mut
            }
            KikDataConn(b) => {
                let mut bytes_mut = BytesMut::with_capacity(2);
                bytes_mut.put_u8(2);
                bytes_mut.put_u8(if *b { 1 } else { 0 });
                bytes_mut
            }

            Ping => {
                let mut bytes_mut = BytesMut::with_capacity(1);
                bytes_mut.put_u8(1);
                bytes_mut
            }
            Pong => {
                let mut bytes_mut = BytesMut::with_capacity(1);
                bytes_mut.put_u8(0);
                bytes_mut
            }
        }
    }
    fn from_buf(mut bys: BytesMut) -> Option<Self> {
        let code = bys.get_u8();
        match code {
            14 => {
                let len = bys.get_u32();
                let resp = Command::from_buf(bys.split_to(len as usize))?;
                Some(CmdExtra(resp, String::from_utf8(bys.to_vec()).ok()?))
            }
            13 => {
                let len = bys.get_u32();
                let resp = Resp::from_buf(bys.split_to(len as usize))?;
                Some(RespExtra(resp, String::from_utf8(bys.to_vec()).ok()?))
            }
            12 => {
                let id_len = bys.get_u32();
                let id = String::from_utf8(bys.split_to(id_len as usize).to_vec()).ok()?;
                Some(Data(id, bys))
            }
            11 => Some(Cmd(Command::from_buf(bys)?)),
            10 => Some(Resp(Resp::from_buf(bys)?)),
            9 => Some(CtrlAuthReq(String::from_utf8(bys.to_vec()).ok()?)),
            8 => {
                let verify = bys.get_u8();
                if ![0, 1].contains(&verify) {
                    return None;
                }
                Some(CtrlAuthReply(verify == 1))
            }
            7 => Some(CtrlDataConnReq(String::from_utf8(bys.to_vec()).ok()?)),
            6 => {
                let verify = bys.get_u8();
                if ![0, 1].contains(&verify) {
                    return None;
                }
                Some(CtrlDataConnAuthReply(verify == 1))
            }
            5 => Some(KikReq(KikInfo::from_buf(bys)?)),
            4 => Some(KikId(String::from_utf8(bys.to_vec()).ok()?)),
            3 => Some(KikDataConnReq(String::from_utf8(bys.to_vec()).ok()?)),
            2 => {
                let verify = bys.get_u8();
                if ![0, 1].contains(&verify) {
                    return None;
                }
                Some(KikDataConn(verify == 1))
            }
            1 => Some(Ping),
            0 => Some(Pong),
            _ => None,
        }
    }
}

#[test]
fn test() {
    let bytes_mut = Frame::RespExtra(
        Resp::Info("草了".to_string()),
        "werwrwrwerwrweerwr".to_string(),
    )
    .to_buf();
    println!("{:?}", Frame::from_buf(bytes_mut).unwrap());
}
