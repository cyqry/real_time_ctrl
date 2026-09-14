use bytes::{BufMut, BytesMut};
use common::command::Command;
use common::kik_info::KikInfo;
use common::ltc_codec::LengthFieldBasedFrameDecoder;
use common::message::dok::Dok;
use common::message::init_frame::InitFrame;
use common::message::kik_frame::KikFrame;
use common::message::kik_resp::KikResp;
use common::protocol::{BufSerializable, ReqCmd};
use tokio_util::codec::Decoder;

/// 固定种子的轻量模糊测试既能稳定复现失败，也不会把随机输入或产物写到仓库外。
/// 这里覆盖所有直接接触网络字节的共享协议入口，目标是证明畸形短帧只会被拒绝而不会 panic。
#[test]
fn network_parsers_reject_arbitrary_short_frames_without_panicking() {
    let mut state = 0xD1B5_4A32_D192_ED03_u64;
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

        let _ = InitFrame::from_buf(bytes.clone());
        let _ = KikInfo::from_buf(bytes.clone());
        let _ = KikFrame::from_buf(bytes.clone());
        let _ = KikResp::from_buf(bytes.clone());
        let _ = Dok::from_buf(bytes.clone());
        let _ = ReqCmd::from_buf(bytes.clone());
        let _ = Command::from_buf(bytes.clone());

        let mut decoder = LengthFieldBasedFrameDecoder::new_with_max_frame_len(1_024);
        let mut framed = bytes;
        let _ = decoder.decode(&mut framed);
    }
}

#[test]
fn length_decoder_rejects_declared_allocation_above_limit_before_body_arrives() {
    let mut decoder = LengthFieldBasedFrameDecoder::new_with_max_frame_len(4_096);
    let mut input = BytesMut::new();
    input.put_u32(4_097);

    let error = decoder.decode(&mut input).unwrap_err();

    assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    assert_eq!(decoder.current_len, None);
}

fn next_u64(state: &mut u64) -> u64 {
    // xorshift64* 只用于生成测试字节，不承担任何密码学用途。
    *state ^= *state >> 12;
    *state ^= *state << 25;
    *state ^= *state >> 27;
    state.wrapping_mul(0x2545_F491_4F6C_DD1D)
}
