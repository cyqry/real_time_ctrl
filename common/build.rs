#[path = "string_obfuscation_key.rs"]
mod key_material;

use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes256Gcm, Nonce};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::fs;
use std::path::PathBuf;

#[derive(Deserialize)]
struct Config {
    strings: BTreeMap<String, String>,
}

fn main() {
    println!("cargo:rerun-if-changed=config.json");
    println!("cargo:rerun-if-changed=string_obfuscation_key.rs");
    println!("cargo:rerun-if-env-changed=RTC_CTRL_KIK_BUILD_HOST");
    println!("cargo:rerun-if-env-changed=RTC_CTRL_KIK_BUILD_PORT");
    println!("cargo:rerun-if-env-changed=RTC_CTRL_KIK_BUILD_CHANNEL");
    println!("cargo:rerun-if-env-changed=RTC_CTRL_KIK_NOISE_SERVER_PUBLIC_KEY");

    let config_content =
        fs::read_to_string("config.json").expect("无法读取 common/config.json 配置文件");
    let mut config: Config =
        serde_json::from_str(&config_content).expect("common/config.json 不是合法 JSON");
    apply_build_overrides(&mut config.strings);

    let mut generated_code = String::from(
        "use std::sync::OnceLock;\n\
         use crate::string_obfuscation::decrypt_literal;\n",
    );
    for (name, value) in config.strings {
        validate_identifier(&name);
        let (cipher_text, nonce) = encrypt_config_value(&name, &value);
        let cipher_text = byte_array_source(&cipher_text);
        let nonce = byte_array_source(&nonce);
        let function_name = name.to_ascii_uppercase();
        writeln!(
            generated_code,
            "\npub fn {function_name}() -> String {{\n\
             \x20   static VALUE: OnceLock<String> = OnceLock::new();\n\
             \x20   VALUE.get_or_init(|| decrypt_literal(&{cipher_text}, &{nonce})).clone()\n\
             }}",
        )
        .expect("向内存字符串写入生成代码不应失败");
    }

    // 生成代码只写入 Cargo OUT_DIR，普通构建不会污染受版本控制的源码目录。
    let out_dir = PathBuf::from(std::env::var_os("OUT_DIR").expect("Cargo 未设置 OUT_DIR"));
    fs::write(out_dir.join("encrypted_strings.rs"), generated_code)
        .expect("无法写入构建期字符串密文");
}

/// 发布脚本只通过构建期环境变量覆盖 ctrl_kik 的公开部署参数。Cargo 会追踪这些变量，
/// 因此不同灰度/正式构建不会错误复用旧缓存；认证秘密和服务端私钥绝不能进入这里。
fn apply_build_overrides(strings: &mut BTreeMap<String, String>) {
    apply_override(strings, "RTC_CTRL_KIK_BUILD_HOST", "HOST", |value| {
        !value.is_empty()
            && value.len() <= 253
            && value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b':'))
    });
    apply_override(strings, "RTC_CTRL_KIK_BUILD_PORT", "PORT", |value| {
        value.parse::<u16>().is_ok_and(|port| port != 0)
    });
    apply_override(
        strings,
        "RTC_CTRL_KIK_BUILD_CHANNEL",
        "BUILD_CHANNEL",
        |value| {
            !value.is_empty()
                && value.len() <= 32
                && value
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
        },
    );
    apply_override(
        strings,
        "RTC_CTRL_KIK_NOISE_SERVER_PUBLIC_KEY",
        "KIK_NOISE_SERVER_PUBLIC_KEY",
        |value| {
            (value.len() == 43 || value.len() == 44 || value.len() == 64)
                && value.bytes().all(|byte| {
                    byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'/' | b'_' | b'-' | b'=')
                })
        },
    );
}

fn apply_override(
    strings: &mut BTreeMap<String, String>,
    environment_name: &str,
    config_name: &str,
    validate: impl FnOnce(&str) -> bool,
) {
    let Some(value) = std::env::var(environment_name)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
    else {
        return;
    };
    assert!(
        validate(&value),
        "构建期部署参数 {environment_name} 的格式不合法"
    );
    strings.insert(config_name.to_string(), value);
}

fn validate_identifier(name: &str) {
    let valid = !name.is_empty()
        && name
            .bytes()
            .all(|byte| byte == b'_' || byte.is_ascii_uppercase() || byte.is_ascii_digit())
        && !name.as_bytes()[0].is_ascii_digit();
    assert!(valid, "配置字符串名称必须是大写 Rust 标识符: {name}");
}

fn encrypt_config_value(name: &str, plain_text: &str) -> (Vec<u8>, [u8; 12]) {
    let cipher = Aes256Gcm::new_from_slice(&key_material::STRING_OBFUSCATION_KEY)
        .expect("固定的字符串混淆密钥长度必须为 32 字节");

    // nonce 同时绑定字段名和字段值：配置修改后会自动产生不同 nonce，避免固定 key 下复用。
    // 确定性生成可保持可复现构建；该机制只阻止静态明文搜索，不承载秘密性保证。
    let mut nonce_hasher = Sha256::new();
    nonce_hasher.update(b"real_time_ctrl.config.string.v2\0");
    nonce_hasher.update(name.as_bytes());
    nonce_hasher.update([0]);
    nonce_hasher.update(plain_text.as_bytes());
    let digest = nonce_hasher.finalize();
    let mut nonce = [0_u8; 12];
    nonce.copy_from_slice(&digest[..12]);

    let cipher_text = cipher
        .encrypt(Nonce::from_slice(&nonce), plain_text.as_bytes())
        .expect("配置字符串的构建期加密不应失败");
    (cipher_text, nonce)
}

fn byte_array_source(bytes: &[u8]) -> String {
    let mut source = String::from("[");
    for (index, byte) in bytes.iter().enumerate() {
        if index != 0 {
            source.push(',');
        }
        write!(source, "{byte}").expect("向内存字符串写入字节不应失败");
    }
    source.push(']');
    source
}
