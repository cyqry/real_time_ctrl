use crate::ctrl_frame::Frame;
use crate::ctrl_resp::Resp::{Kik, Server};
use crate::ctrl_resp::{CmdResp, ServerResp, ServerSuccessResp};
use bytes::BytesMut;
use common::message::kik_resp::KikResp;
use common::protocol;
use common::protocol::ReqCmd;

pub fn ctrl_ping() -> BytesMut {
    common::protocol::transfer_encode_frame(Frame::Ping)
}

pub fn ctrl_pong() -> BytesMut {
    common::protocol::transfer_encode_frame(Frame::Pong)
}

pub fn ctrl_server_resp(id: String, resp: ServerResp) -> BytesMut {
    protocol::transfer_encode_frame( Frame::Resp(CmdResp::new(id, Server(resp))))
}

pub fn ctrl_server_resp_error(id: String, info: String) -> BytesMut {
    protocol::transfer_encode_frame(Frame::Resp(CmdResp::new(id, Server(ServerResp::Error(protocol::ErrCode::EXCEPTION as u8, info)))))
}

pub fn ctrl_server_resp_success(id: String, info: String) -> BytesMut {
    protocol::transfer_encode_frame(Frame::Resp(CmdResp::new(id, Server(ServerResp::Success(ServerSuccessResp::Info(info))))))
}

pub fn ctrl_cmd_req(req: ReqCmd) -> BytesMut {
    protocol::transfer_encode_frame(Frame::Cmd(req))
}

pub fn ctrl_kik_resp(id: String, resp: KikResp) -> BytesMut {
    protocol::transfer_encode_frame(Frame::Resp(CmdResp::new(id, Kik(resp))))
}
