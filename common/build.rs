mod decrypted;

use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes256Gcm, Key, Nonce}; // Or `Aes128Gcm`
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::fs;
use std::path::PathBuf;

#[derive(Deserialize)]
struct Config {
    strings: std::collections::HashMap<String, String>,
}

fn main() {
    println!("cargo:rerun-if-changed=config.json");
    println!("cargo:rerun-if-changed=decrypted.rs");
    // 可公开、与机器身份无关的连接默认值统一在构建期提供；运行时环境变量仍可覆盖。
    println!("cargo:rustc-env=RTC_DEFAULT_TLS_PORT=9443");
    println!("cargo:rustc-env=RTC_DEFAULT_TLS_SERVER_NAME=real-ctrl-server");

    // 以下材料禁止通过 cargo:rustc-env 注入，否则会进入二进制：
    // REAL_CTRL_AUTH_SECRET / CTRL_SERVER_AUTH_SECRET / CTRL_SERVER_TLS_KEY /
    // REAL_CTRL_TLS_SERVER_SPKI_SHA256。它们必须由部署环境或受控秘密系统提供。
    // 读取配置文件
    let config: Config = {
        let config_path = PathBuf::from("config.json");
        let config_content = fs::read_to_string(config_path).expect("Unable to read config file");
        serde_json::from_str(&config_content).expect("Invalid JSON format")
    };

    // 生成加密字符串代码
    let mut generated_code = String::new();
    generated_code.push_str(include_str!("decrypted.rs"));

    generated_code.push('\n');
    let mut strings = config.strings.into_iter().collect::<Vec<_>>();
    strings.sort_by(|left, right| left.0.cmp(&right.0));
    for (name, value) in strings {
        let (encrypted, nonce) = encrypt(&name, &value);
        generated_code.push_str(&format!(
            r#"
pub fn {name}() -> String {{
    decrypt("{encrypted}", "{nonce}")
}}
"#,
            name = name.to_uppercase(),
            encrypted = encrypted
        ));
    }
    generated_code.push('\n');

    // 生成代码属于构建产物，只写入 Cargo OUT_DIR，避免普通构建修改受版本控制的源码。
    let dest_path = PathBuf::from(std::env::var_os("OUT_DIR").expect("OUT_DIR 未设置"))
        .join("encrypted_strings.rs");
    fs::create_dir_all(dest_path.parent().unwrap()).unwrap();
    fs::write(dest_path, generated_code).unwrap();
}

fn encrypt(name: &str, plain_text: &str) -> (String, String) {
    // 明确指定Key的类型为Aes256Gcm
    let key = Key::<Aes256Gcm>::from_slice(decrypted::KEY);
    let cipher = Aes256Gcm::new(key);

    // 每个字段使用确定且唯一的 nonce，使构建可复现并避免 GCM 固定 nonce 复用。
    // KEY 与解密逻辑同在客户端，本机制只提高静态字符串扫描成本，不是秘密存储。
    let digest = Sha256::digest(format!("real_time_ctrl.build.string.v1\0{name}").as_bytes());
    let nonce_bytes = &digest[..12];
    let nonce = Nonce::from_slice(nonce_bytes);
    let cipher_text = cipher
        .encrypt(nonce, plain_text.as_bytes())
        .expect("encryption failure!");

    (hex::encode(cipher_text), hex::encode(nonce_bytes))
}
