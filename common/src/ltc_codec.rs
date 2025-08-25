use bytes::{Buf, BytesMut};
use log::debug;
use tokio_util::codec::Decoder;

pub struct LengthFieldBasedFrameDecoder {
    pub current_len: Option<usize>,
}

impl LengthFieldBasedFrameDecoder {
    pub fn new() -> Self {
        LengthFieldBasedFrameDecoder { current_len: None }
    }
}

impl Decoder for LengthFieldBasedFrameDecoder {
    type Item = BytesMut;
    type Error = std::io::Error;

    fn decode(&mut self, src: &mut BytesMut) -> Result<Option<Self::Item>, Self::Error> {
        // debug!("src len: {}", src.len());
        if self.current_len.is_none() {
            // debug!("无current_len");
            if src.len() < 4 {
                // 这里的 4 代表长度字段占用的字节数
                return Ok(None);
            }
            //get_uint会改变src中剩余字节，即将前四个字节读掉，以大端的方式读取
            let len = src.get_uint(4) as usize; // 这里我们从字节流中读取长度字段
            self.current_len = Some(len);

            if src.len() < len {
                return Ok(None);
            }
            // 这里相当于将前len个字节读掉
            let res = src.split_to(len);
            self.current_len = None;
            Ok(Some(res))
        } else {
            // debug!("有current_len,为{}",self.current_len.unwrap());
            let current_len = self.current_len.unwrap();
            if src.len() < current_len {
                return Ok(None);
            }
            self.current_len = None;
            // debug!("删除current_len");
            Ok(Some(src.split_to(current_len)))
        }
    }
}
