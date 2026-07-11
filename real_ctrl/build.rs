use std::env;

fn main() {
    let profile = env::var("PROFILE").unwrap_or_else(|_| "debug".to_string());
    let log_level = if profile == "debug" { "DEBUG" } else { "INFO" };
    println!("cargo:rustc-env=LOG_LEVEL={log_level}");
    println!("cargo:rustc-env=REAL_CTRL_DEFAULT_SERVER_PORT=9002");
    println!(
        "cargo:rustc-env=REAL_CTRL_DEFAULT_HTTP_LOCK_PATH=target/runtime/real_ctrl_invoker_http_service.lock"
    );

    // 控制面 secret、CA 路径和 SPKI pin 不能写入此处；cargo:rustc-env 会把值编译进产物。
}
