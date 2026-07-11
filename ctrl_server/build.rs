use std::env;

fn main() {
    let profile = env::var("PROFILE").unwrap_or_else(|_| "debug".to_string());
    let log_level = if profile == "debug" { "DEBUG" } else { "INFO" };
    println!("cargo:rustc-env=LOG_LEVEL={log_level}");
    println!("cargo:rustc-env=CTRL_SERVER_DEFAULT_BIND_HOST=0.0.0.0");
    println!("cargo:rustc-env=CTRL_SERVER_DEFAULT_PORT=9002");

    // 服务端认证 secret 和 TLS 私钥必须由部署环境提供，禁止编译进服务端可执行文件。
}
