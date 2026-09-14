use bytes::BytesMut;
use common::protocol::BufSerializable;
use ctrl_common::ctrl_frame::Frame;
use ctrl_common::ctrl_resp::{CmdResp, Resp, ServerResp, ServerSuccessResp};

/// 控制端协议解析器必须把任意不可信字节收敛为 `None` 或合法结构，不能因短帧崩溃。
#[test]
fn control_protocol_parsers_are_total_for_arbitrary_short_frames() {
    let mut state = 0x83A9_267B_1B24_5D6F_u64;
    for round in 0..20_000_usize {
        let len = if round < 1_025 {
            round
        } else {
            (next_u64(&mut state) as usize) % 1_025
        };
        let mut raw = vec![0_u8; len];
        for byte in &mut raw {
            *byte = next_u64(&mut state) as u8;
        }
        let bytes = BytesMut::from(raw.as_slice());

        let _ = Frame::from_buf(bytes.clone());
        let _ = CmdResp::from_buf(bytes.clone());
        let _ = Resp::from_buf(bytes.clone());
        let _ = ServerResp::from_buf(bytes.clone());
        let _ = ServerSuccessResp::from_buf(bytes);
    }
}

fn next_u64(state: &mut u64) -> u64 {
    *state ^= *state >> 12;
    *state ^= *state << 25;
    *state ^= *state >> 27;
    state.wrapping_mul(0x2545_F491_4F6C_DD1D)
}
