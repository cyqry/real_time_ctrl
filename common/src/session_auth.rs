use hmac::{Hmac, Mac};
use rand::RngCore;
use sha2::Sha256;

type HmacSha256 = Hmac<Sha256>;

pub const CTRL_AUTH_V2_LABEL: &str = "real_ctrl.auth.v2";
pub const CTRL_DATA_V2_LABEL: &str = "real_ctrl.data.v2";

pub fn random_nonce_hex() -> String {
    let mut nonce = [0_u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut nonce);
    hex::encode(nonce)
}

pub fn hmac_sha256_hex(secret: &str, parts: &[&str]) -> String {
    let mut mac = HmacSha256::new_from_slice(secret.as_bytes())
        .expect("HMAC-SHA256 接受任意长度密钥");
    for part in parts {
        mac.update(part.as_bytes());
        mac.update(&[0]);
    }
    hex::encode(mac.finalize().into_bytes())
}

pub fn verify_hmac_sha256_hex(secret: &str, parts: &[&str], expected_hex: &str) -> bool {
    let Ok(expected) = hex::decode(expected_hex) else {
        return false;
    };
    let mut mac = HmacSha256::new_from_slice(secret.as_bytes())
        .expect("HMAC-SHA256 接受任意长度密钥");
    for part in parts {
        mac.update(part.as_bytes());
        mac.update(&[0]);
    }
    mac.verify_slice(&expected).is_ok()
}

pub fn ctrl_auth_proof(secret: &str, client_nonce: &str, server_nonce: &str) -> String {
    hmac_sha256_hex(secret, &[CTRL_AUTH_V2_LABEL, client_nonce, server_nonce])
}

pub fn verify_ctrl_auth_proof(
    secret: &str,
    client_nonce: &str,
    server_nonce: &str,
    proof: &str,
) -> bool {
    verify_hmac_sha256_hex(secret, &[CTRL_AUTH_V2_LABEL, client_nonce, server_nonce], proof)
}

pub fn ctrl_data_proof(secret: &str, session_id: &str, channel_nonce: &str) -> String {
    hmac_sha256_hex(secret, &[CTRL_DATA_V2_LABEL, session_id, channel_nonce])
}

pub fn verify_ctrl_data_proof(
    secret: &str,
    session_id: &str,
    channel_nonce: &str,
    proof: &str,
) -> bool {
    verify_hmac_sha256_hex(secret, &[CTRL_DATA_V2_LABEL, session_id, channel_nonce], proof)
}

#[cfg(test)]
mod tests {
    use super::{
        ctrl_auth_proof, ctrl_data_proof, random_nonce_hex, verify_ctrl_auth_proof,
        verify_ctrl_data_proof,
    };

    #[test]
    fn nonce_has_expected_hex_len() {
        let nonce = random_nonce_hex();
        assert_eq!(nonce.len(), 64);
        assert!(nonce.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn auth_proof_round_trip() {
        let proof = ctrl_auth_proof("secret", "client", "server");
        assert!(verify_ctrl_auth_proof("secret", "client", "server", &proof));
        assert!(!verify_ctrl_auth_proof("secret", "client2", "server", &proof));
    }

    #[test]
    fn data_proof_round_trip() {
        let proof = ctrl_data_proof("secret", "session", "channel");
        assert!(verify_ctrl_data_proof("secret", "session", "channel", &proof));
        assert!(!verify_ctrl_data_proof("secret", "session", "other", &proof));
    }
}
