use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes256Gcm, Key, Nonce}; // Or `Aes128Gcm`

pub(crate) const KEY: &[u8] = b"asuiojslkgr!sA#Jk@^*svojsl@SHK%J"; // AES-256 密钥需要 32 字节

#[allow(dead_code)]
fn decrypt(cipher_text: &str, nonce_hex: &str) -> String {
    // 明确指定Key的类型为Aes256Gcm
    let key = Key::<Aes256Gcm>::from_slice(KEY);
    let cipher = Aes256Gcm::new(key);
    let nonce_bytes = hex::decode(nonce_hex).expect("生成代码中的 nonce 必须合法");
    let nonce = Nonce::from_slice(&nonce_bytes);
    let cipher_text_bytes = hex::decode(cipher_text).unwrap();
    let plain_text = cipher.decrypt(nonce, cipher_text_bytes.as_ref()).unwrap();

    String::from_utf8(plain_text).unwrap()
}
