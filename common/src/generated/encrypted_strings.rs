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

pub fn MACHINE_CODE_2() -> String {
    decrypt("dc1034e21e50f1c9053fb89201b4d8aee36b2c40654bac74c9")
}

pub fn START_ERROR_2() -> String {
    decrypt("9c5014d547f60f66d206f32f7873481b496071898e51229835cec2f79938e879ef1b4da5acc6a879342cfbc7e45faf01")
}

pub fn HOST() -> String {
    decrypt("96500cc4503cd3945a171c6eadc3de778eec19ddb10b027249")
}

pub fn START_ERROR_3() -> String {
    decrypt("9c5014d547fa3e4cd224fd226f4e440e696e6fa18e47019b1dcdc3e89e35f86dea3ab6a09b49736d812996324718b3bb1a3199")
}

pub fn FIX_SAVE_PATH() -> String {
    decrypt("ab1e5ae45271d89e18c704a0d1a5d9e42adf5b0c1f569b520159838c464748ca")
}

pub fn START_ERROR_1() -> String {
    decrypt("9c5014d547f72c53de31fb2f684f4936766d5882806f2a952ac6c2f79938e8796e0a74a8966a7522cfc92efd975851d9")
}

pub fn MACHINE_CODE_1() -> String {
    decrypt("ad1c31e21e54f5cc03abcb8e9166218ef49a5b6154a97a030f")
}

pub fn START_RUN_PATH() -> String {
    decrypt("ab1e5ad35661c4d453d807e99ab8c403edcc8c19711eeaeed09066893d2f76")
}

