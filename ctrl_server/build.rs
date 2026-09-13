use std::env;

fn main() {
    let profile = env::var("PROFILE").unwrap_or_else(|_| "debug".to_string());
    let profile_log_level = if profile == "debug" { "DEBUG" } else { "INFO" };

    emit_default(
        "LOG_LEVEL",
        "RTC_CTRL_SERVER_BUILD_LOG_LEVEL",
        profile_log_level,
        |value| matches!(value, "OFF" | "ERROR" | "WARN" | "INFO" | "DEBUG" | "TRACE"),
    );
    emit_default(
        "CTRL_SERVER_DEFAULT_BIND_HOST",
        "RTC_CTRL_SERVER_BUILD_BIND_HOST",
        "0.0.0.0",
        valid_bind_host,
    );
    emit_default(
        "CTRL_SERVER_DEFAULT_PORT",
        "RTC_CTRL_SERVER_BUILD_KIK_PORT",
        "9002",
        valid_port,
    );
    emit_default(
        "CTRL_SERVER_DEFAULT_TLS_PORT",
        "RTC_CTRL_SERVER_BUILD_TLS_PORT",
        "9007",
        valid_port,
    );
    emit_default(
        "CTRL_SERVER_DEFAULT_TLS_CERT_PEM_BASE64",
        "RTC_CTRL_SERVER_BUILD_TLS_CERT_PEM_BASE64",
        "",
        valid_base64,
    );
    emit_default(
        "CTRL_SERVER_DEFAULT_TLS_KEY_PEM_BASE64",
        "RTC_CTRL_SERVER_BUILD_TLS_KEY_PEM_BASE64",
        "",
        valid_base64,
    );
    emit_default(
        "CTRL_SERVER_DEFAULT_AUTH_SECRET",
        "RTC_CTRL_SERVER_BUILD_AUTH_SECRET",
        "development-control-secret-0000000000000000",
        |value| value.len() >= 32 && value.is_ascii(),
    );
    emit_default(
        "CTRL_SERVER_DEFAULT_ACCOUNTS_JSON_BASE64",
        "RTC_CTRL_SERVER_BUILD_ACCOUNTS_JSON_BASE64",
        "",
        valid_base64,
    );
    emit_default(
        "CTRL_SERVER_DEFAULT_KIK_NOISE_PRIVATE_KEY",
        "RTC_CTRL_SERVER_BUILD_KIK_NOISE_PRIVATE_KEY",
        "AQIDBAUGBwgJCgsMDQ4PEBESExQVFhcYGRobHB0eHyA=",
        valid_base64,
    );
    emit_default(
        "CTRL_SERVER_DEFAULT_ALLOW_EXEC",
        "RTC_CTRL_SERVER_BUILD_ALLOW_EXEC",
        "1",
        valid_bool,
    );
}

fn emit_default(
    rust_name: &str,
    build_override: &str,
    fallback: &str,
    validate: impl Fn(&str) -> bool,
) {
    println!("cargo:rerun-if-env-changed={build_override}");
    let value = env::var(build_override)
        .ok()
        .map(|value| value.trim().to_string())
        .unwrap_or_else(|| fallback.to_string());
    assert!(validate(&value), "构建期配置 {build_override} 的格式不合法");
    println!("cargo:rustc-env={rust_name}={value}");
}

fn valid_bind_host(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 253
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b':'))
}

fn valid_port(value: &str) -> bool {
    value.parse::<u16>().is_ok_and(|port| port != 0)
}

fn valid_base64(value: &str) -> bool {
    value.len() <= 32 * 1024
        && value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'/' | b'_' | b'-' | b'=')
        })
}

fn valid_bool(value: &str) -> bool {
    matches!(value, "0" | "1" | "false" | "true")
}
