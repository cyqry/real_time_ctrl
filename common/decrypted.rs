use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes256Gcm, Key, Nonce}; // Or `Aes128Gcm`
use hex;

pub(crate) const KEY: &[u8] = b"asuiojslkgr!sA#Jk@^*svojsl@SHK%J"; // AES-256 密钥需要 32 字节

fn decrypt(cipher_text: &str) -> String {
    // 明确指定Key的类型为Aes256Gcm
    let key = Key::<Aes256Gcm>::from_slice(KEY);
    let cipher = Aes256Gcm::new(key);
    let nonce = Nonce::from_slice(b"unique nonce"); // 96-bits; unique per message
    let cipher_text_bytes = hex::decode(cipher_text).unwrap();
    let plain_text = cipher
        .decrypt(nonce, cipher_text_bytes.as_ref())
        .unwrap();

    String::from_utf8(plain_text).unwrap()
}