//! Cargo 构建脚本生成内容的模块入口。
//!
//! `encrypted_strings.rs` 位于 `OUT_DIR`，包含由配置生成的混淆字符串访问函数；不要手工编辑生成文件。

#[allow(non_snake_case)]
pub mod encrypted_strings {
    include!(concat!(env!("OUT_DIR"), "/encrypted_strings.rs"));
}
