
mod decrypted;

use std::fs;
use std::path::PathBuf;
use serde::Deserialize;
use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes256Gcm, Key, Nonce}; // Or `Aes128Gcm`
use hex;

#[derive(Deserialize)]
struct Config {
    strings: std::collections::HashMap<String, String>,
}

fn main() {
    // 读取配置文件
    let config: Config = {
        let config_path = PathBuf::from("config.json");
        let config_content = fs::read_to_string(config_path).expect("Unable to read config file");
        serde_json::from_str(&config_content).expect("Invalid JSON format")
    };

    // 生成加密字符串代码
    let mut generated_code = String::new();
    generated_code.push_str(include_str!("decrypted.rs"));

    generated_code.push_str("\n");
    for (name, value) in config.strings {
        let encrypted = encrypt(&value);
        generated_code.push_str(
            &format!(
                r#"
pub fn {name}() -> String {{
    decrypt("{encrypted}")
}}
"#,
                name = name.to_uppercase(),
                encrypted = encrypted
            ));
    }
    generated_code.push_str("\n");

    // 指定输出目录为项目根目录下的 src/generated 目录
    let dest_path = PathBuf::from("src/generated/encrypted_strings.rs");
    fs::create_dir_all(dest_path.parent().unwrap()).unwrap();
    fs::write(dest_path, generated_code).unwrap();
}



fn encrypt(plain_text: &str) -> String {
    // 明确指定Key的类型为Aes256Gcm
    let key = Key::<Aes256Gcm>::from_slice(decrypted::KEY);
    let cipher = Aes256Gcm::new(key);

    let nonce = Nonce::from_slice(b"unique nonce"); // 96-bits; unique per message
    let cipher_text = cipher
        .encrypt(nonce, plain_text.as_bytes())
        .expect("encryption failure!");

    hex::encode(cipher_text)
}