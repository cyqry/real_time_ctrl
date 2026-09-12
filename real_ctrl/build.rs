use std::env;

fn main() {
    let profile = env::var("PROFILE").unwrap_or_else(|_| "debug".to_string());
    let profile_log_level = if profile == "debug" { "DEBUG" } else { "INFO" };

    emit_default(
        "LOG_LEVEL",
        "RTC_REAL_CTRL_BUILD_LOG_LEVEL",
        profile_log_level,
        |value| matches!(value, "OFF" | "ERROR" | "WARN" | "INFO" | "DEBUG" | "TRACE"),
    );
    emit_default(
        "REAL_CTRL_DEFAULT_SERVER_HOST",
        "RTC_REAL_CTRL_BUILD_SERVER_HOST",
        "ytycc.com",
        valid_host,
    );
    emit_default(
        "REAL_CTRL_DEFAULT_TLS_PORT",
        "RTC_REAL_CTRL_BUILD_TLS_PORT",
        "9007",
        valid_port,
    );
    emit_default(
        "REAL_CTRL_DEFAULT_TLS_SERVER_NAME",
        "RTC_REAL_CTRL_BUILD_TLS_SERVER_NAME",
        "ytycc.com",
        valid_host,
    );
    emit_default(
        "REAL_CTRL_DEFAULT_TLS_CA_PEM_BASE64",
        "RTC_REAL_CTRL_BUILD_TLS_CA_PEM_BASE64",
        "",
        valid_base64,
    );
    emit_default(
        "REAL_CTRL_DEFAULT_TLS_SPKI_SHA256",
        "RTC_REAL_CTRL_BUILD_TLS_SPKI_SHA256",
        "",
        |value| value.is_empty() || valid_sha256(value),
    );
    emit_default(
        "REAL_CTRL_DEFAULT_HTTP_LOCK_PATH",
        "RTC_REAL_CTRL_BUILD_HTTP_LOCK_PATH",
        "real_ctrl-http.lock",
        valid_path,
    );
    emit_default(
        "REAL_CTRL_DEFAULT_AUTH_SECRET",
        "RTC_REAL_CTRL_BUILD_AUTH_SECRET",
        "development-control-secret-0000000000000000",
        |value| value.len() >= 32 && value.is_ascii(),
    );
    emit_default(
        "REAL_CTRL_DEFAULT_API_TOKEN",
        "RTC_REAL_CTRL_BUILD_API_TOKEN",
        "development-local-api-token-000000000000000",
        |value| value.len() >= 32 && value.is_ascii(),
    );
    emit_default(
        "REAL_CTRL_DEFAULT_API_ALLOW_EXEC",
        "RTC_REAL_CTRL_BUILD_API_ALLOW_EXEC",
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

fn valid_host(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 253
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b':'))
}

fn valid_port(value: &str) -> bool {
    value.parse::<u16>().is_ok_and(|port| port != 0)
}

fn valid_path(value: &str) -> bool {
    !value.is_empty() && value.len() <= 1024 && !value.contains(['\r', '\n', '\0'])
}

fn valid_sha256(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
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
