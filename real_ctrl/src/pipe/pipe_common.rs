

// src/common.rs
#[derive(Debug)]
pub struct Request {
    pub method: String,
    pub args: Vec<u8>,   // 序列化后的参数，例如 (i32, i32) 的二进制表示
}

#[derive(Debug)]
pub struct Response {
    pub result: Vec<u8>,
    pub error: Option<String>,
}

// // 辅助函数：将参数对序列化为 Vec<u8>
// pub fn serialize_args<A: Serialize<S>>(args: &A) -> Vec<u8> {
//     serde_json::to_vec(args).unwrap()
// }
//
// // 辅助函数：从 Vec<u8> 反序列化参数
// pub fn deserialize_args<A: for<'de> Deserialize<'de>>(bytes: &[u8]) -> anyhow::Result<A> {
//     Ok(serde_json::from_slice(bytes)?)
// }