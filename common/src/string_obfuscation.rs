//! 构建期字符串混淆的运行时解密支撑。
//!
//! 该能力只降低发布二进制被静态搜索的可读性，密钥最终仍在进程内，不能用于保存认证秘密或替代 TLS/Noise。

#[path = "../string_obfuscation_key.rs"]
mod key_material;

use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes256Gcm, Nonce};
use std::fmt::{Debug, Display, Formatter};
use std::sync::OnceLock;

static STRING_CIPHER: OnceLock<Aes256Gcm> = OnceLock::new();

/// 在 `hidden!` 动态参数中按 `Debug` 语义输出值，同时避免调用点引入 `format_args!` 宏。
pub struct DebugValue<'a, T: Debug + ?Sized>(&'a T);

impl<T: Debug + ?Sized> Display for DebugValue<'_, T> {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        Debug::fmt(self.0, formatter)
    }
}

pub fn debug<T: Debug + ?Sized>(value: &T) -> DebugValue<'_, T> {
    DebugValue(value)
}

/// 解密构建期生成的字符串片段。
///
/// 密文经过 AES-GCM 完整性校验；失败意味着生成代码、链接产物或内存已经损坏。这里不返回
/// 带明文错误信息的 `Result`，既避免污染发布产物，也避免调用方误把损坏后的值继续用于协议。
#[cold]
#[inline(never)]
pub fn decrypt_literal(cipher_text: &[u8], nonce: &[u8; 12]) -> String {
    // key schedule 全进程只初始化一次；明文本身仍按调用创建，不在全局缓存中长期驻留。
    let cipher = STRING_CIPHER.get_or_init(|| {
        Aes256Gcm::new_from_slice(&key_material::STRING_OBFUSCATION_KEY)
            .unwrap_or_else(|_| std::process::abort())
    });
    let plain_text = match cipher.decrypt(Nonce::from_slice(nonce), cipher_text) {
        Ok(plain_text) => plain_text,
        Err(_) => std::process::abort(),
    };
    match String::from_utf8(plain_text) {
        Ok(value) => value,
        Err(_) => std::process::abort(),
    }
}

#[cfg(test)]
mod tests {
    use super::decrypt_literal;
    use aes_gcm::aead::{Aead, KeyInit};
    use aes_gcm::{Aes256Gcm, Nonce};

    #[test]
    fn decrypts_generated_cipher_text() {
        let nonce = [7_u8; 12];
        let cipher =
            Aes256Gcm::new_from_slice(&super::key_material::STRING_OBFUSCATION_KEY).unwrap();
        let encrypted = cipher
            .encrypt(Nonce::from_slice(&nonce), b"runtime text".as_slice())
            .unwrap();

        assert_eq!(decrypt_literal(&encrypted, &nonce), "runtime text");
    }

    #[test]
    fn hidden_macro_joins_static_and_dynamic_parts() {
        let code = 42;
        assert_eq!(
            crate::hidden!("错误码=", code, ", 状态=", "测试状态"),
            "错误码=42, 状态=测试状态"
        );
    }

    #[test]
    fn hidden_macro_encrypts_compile_time_environment_values() {
        assert_eq!(crate::hidden!(env!("CARGO_PKG_NAME")), "common");
    }
}
