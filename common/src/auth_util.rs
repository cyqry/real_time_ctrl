use crypto::digest::Digest;
use crypto::md5::Md5;
use crypto::sha1::Sha1;
use crypto::sha2::Sha256;

pub fn sha1(input: &str) -> String {
    let mut hasher = Sha1::new();
    hasher.input_str(input);
    hasher.result_str()
}

pub fn md5(input: &str) -> String {
    let mut hasher = Md5::new();
    hasher.input_str(input);
    hasher.result_str()
}

pub fn sha256(input: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.input_str(input);
    hasher.result_str()
}

pub fn pre_token(username: &str, password: &str) -> String {
    sha1(username) + &sha1(password)
}

pub fn final_token(mut pre_token: &str) -> String {
    pre_token = pre_token.trim();
    if pre_token.len() == 0 {
        panic!("错误的参数");
    }
    if pre_token.len() > 5 {
        md5(&pre_token[0..5]) + &sha256(&pre_token[5..])
    } else {
        sha256(pre_token)
    }
}

pub fn encrypt(username: &str, password: &str) -> String {
    final_token(pre_token(username, password).as_str())
}
