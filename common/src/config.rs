use crate::auth_util;
use std::string::ToString;

#[derive(Clone)]
pub struct Config {
    pub id: Id,
    pub server_host: String,
    pub server_port: String,
}

#[derive(Clone)]
pub struct Id {
    pub username: String,
    pub password: String,
}

impl Id {
    pub fn encrypt(&self) -> String {
        auth_util::encrypt(self.username.as_str(), self.password.as_str())
    }
}

pub static DEFAULT_USER_NAME: &str = "user";
pub static DEFAULT_PASS_WARD: &str = "123456";
