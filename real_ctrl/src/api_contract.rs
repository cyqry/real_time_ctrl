//! 本地 HTTP 与 Windows 命名管道共用的版本化 JSON 契约。
//!
//! 该契约面向调用者，独立于内部二进制线协议。所有输入先做版本、未知字段、长度和 NUL 检查，再转换为
//! `InputCommand`；内部错误只映射为稳定错误码，不把实现细节当作长期 API。

use crate::input_command::{InputCommand, InputCtrlCommand, RemoteResp, RemoteSuccessResp};
use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use serde::{Deserialize, Serialize};

pub const API_VERSION: u16 = 1;
pub const MAX_REQUEST_ID_BYTES: usize = 128;
pub const MAX_KIK_ID_BYTES: usize = 128;
pub const MAX_PATH_BYTES: usize = 32 * 1024;
pub const MAX_EXEC_COMMAND_BYTES: usize = 32 * 1024;
pub const MAX_API_BINARY_BYTES: usize = 48 * 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
/// 一次开放 API 调用的稳定信封。
pub struct ApiRequest {
    /// 调用者声明的契约版本，必须等于当前 `API_VERSION`。
    pub version: u16,
    /// 调用者自定义的追踪 ID；服务端只校验并原样回显，不把它当成内部命令 ID。
    #[serde(default)]
    pub request_id: Option<String>,
    pub command: ApiCommand,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
/// API 支持的命令及其显式参数。
///
/// `local_path` 始终属于运行 real_ctrl 的机器，`remote_path` 始终属于 Kik；两者不能互换。
pub enum ApiCommand {
    // 使用零字段结构体变体而不是 unit 变体：Serde 对内部标签 unit 变体会忽略额外字段，
    // 结构体变体才能让 deny_unknown_fields 在开放 API 边界真正 fail-closed。
    SysList {},
    SysNow {},
    SysHistory {
        #[serde(default)]
        kik_id: Option<String>,
    },
    SysUse {
        kik_id: String,
    },
    CtrlLs {
        path: String,
    },
    CtrlScreen {
        #[serde(default)]
        save_path: Option<String>,
    },
    CtrlGetFile {
        remote_path: String,
        #[serde(default)]
        local_path: Option<String>,
    },
    CtrlGetBigFile {
        remote_path: String,
        #[serde(default)]
        local_path: Option<String>,
    },
    CtrlSetFile {
        local_path: String,
        remote_path: String,
    },
    CtrlSetBigFile {
        local_path: String,
        remote_path: String,
    },
    Exec {
        command: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
/// 统一响应信封。正常情况下 `ok=true` 只带 `data`，失败只带 `error`。
pub struct ApiResponse {
    pub version: u16,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<ApiResponseData>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<ApiErrorBody>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
/// 成功结果的结构化类型，调用方应按 `kind` 分派而不是解析展示字符串。
pub enum ApiResponseData {
    Info {
        message: String,
    },
    Ls {
        entries: Vec<common::message::kik_cmd_resp_info::Ls>,
    },
    SysList {
        items: Vec<ctrl_common::cmd_resp_info::KikInfoVo>,
    },
    SysNow {
        value: ctrl_common::cmd_resp_info::SysNow,
    },
    SysHistory {
        items: Vec<ctrl_common::cmd_resp_info::KikPresenceVo>,
    },
    Binary {
        content_type: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        filename: Option<String>,
        base64: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
/// 对外稳定错误：`code` 供程序判断，`message` 只供人阅读。
pub struct ApiErrorBody {
    pub code: String,
    pub message: String,
}

impl ApiRequest {
    pub fn new(command: ApiCommand) -> Self {
        Self {
            version: API_VERSION,
            request_id: None,
            command,
        }
    }

    pub fn validate(&self) -> Result<(), ApiErrorBody> {
        if let Some(request_id) = &self.request_id {
            validate_text(request_id, "request_id", MAX_REQUEST_ID_BYTES)?;
        }
        self.command.validate()
    }
}

impl ApiResponse {
    pub fn success(request: &ApiRequest, data: ApiResponseData) -> Self {
        Self {
            version: API_VERSION,
            request_id: request.request_id.clone(),
            ok: true,
            data: Some(data),
            error: None,
        }
    }

    pub fn error(request_id: Option<String>, error: ApiErrorBody) -> Self {
        Self {
            version: API_VERSION,
            request_id,
            ok: false,
            data: None,
            error: Some(error),
        }
    }
}

impl ApiErrorBody {
    pub fn bad_request(message: impl Into<String>) -> Self {
        Self::new("bad_request", message)
    }

    pub fn forbidden(message: impl Into<String>) -> Self {
        Self::new("forbidden", message)
    }

    pub fn busy(message: impl Into<String>) -> Self {
        Self::new("busy", message)
    }

    pub fn unauthorized(message: impl Into<String>) -> Self {
        Self::new("unauthorized", message)
    }

    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new("not_found", message)
    }

    pub fn remote(code: u32, message: impl Into<String>) -> Self {
        Self::new(format!("remote_{code}"), message)
    }

    pub fn internal(message: impl Into<String>) -> Self {
        Self::new("internal", message)
    }

    pub fn payload_too_large(message: impl Into<String>) -> Self {
        Self::new("payload_too_large", message)
    }

    pub fn unsupported_version(version: u16) -> Self {
        Self::new(
            "unsupported_version",
            format!("不支持的 API 版本: {version}"),
        )
    }

    fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
        }
    }
}

impl ApiCommand {
    pub fn kind(&self) -> &'static str {
        match self {
            ApiCommand::SysList {} => "sys_list",
            ApiCommand::SysNow {} => "sys_now",
            ApiCommand::SysHistory { .. } => "sys_history",
            ApiCommand::SysUse { .. } => "sys_use",
            ApiCommand::CtrlLs { .. } => "ctrl_ls",
            ApiCommand::CtrlScreen { .. } => "ctrl_screen",
            ApiCommand::CtrlGetFile { .. } => "ctrl_get_file",
            ApiCommand::CtrlGetBigFile { .. } => "ctrl_get_big_file",
            ApiCommand::CtrlSetFile { .. } => "ctrl_set_file",
            ApiCommand::CtrlSetBigFile { .. } => "ctrl_set_big_file",
            ApiCommand::Exec { .. } => "exec",
        }
    }

    fn validate(&self) -> Result<(), ApiErrorBody> {
        match self {
            ApiCommand::SysList {} | ApiCommand::SysNow {} => Ok(()),
            ApiCommand::SysHistory { kik_id } => {
                if let Some(kik_id) = kik_id {
                    validate_text(kik_id, "kik_id", MAX_KIK_ID_BYTES)?;
                }
                Ok(())
            }
            ApiCommand::SysUse { kik_id } => validate_text(kik_id, "kik_id", MAX_KIK_ID_BYTES),
            ApiCommand::CtrlLs { path } => validate_path(path, "path", false),
            ApiCommand::CtrlScreen { save_path } => {
                if let Some(path) = save_path {
                    validate_path(path, "save_path", false)?;
                }
                Ok(())
            }
            ApiCommand::CtrlGetFile {
                remote_path,
                local_path,
            } => {
                validate_path(remote_path, "remote_path", false)?;
                if let Some(path) = local_path {
                    validate_path(path, "local_path", true)?;
                }
                Ok(())
            }
            ApiCommand::CtrlGetBigFile {
                remote_path,
                local_path,
            } => {
                validate_path(remote_path, "remote_path", false)?;
                let path = local_path.as_deref().ok_or_else(|| {
                    ApiErrorBody::bad_request(
                        "ctrl_get_big_file 必须提供 local_path，禁止把大文件聚合进 API 响应内存",
                    )
                })?;
                validate_path(path, "local_path", false)
            }
            ApiCommand::CtrlSetFile {
                local_path,
                remote_path,
            }
            | ApiCommand::CtrlSetBigFile {
                local_path,
                remote_path,
            } => {
                validate_path(local_path, "local_path", false)?;
                validate_path(remote_path, "remote_path", false)
            }
            ApiCommand::Exec { command } => {
                validate_text(command, "command", MAX_EXEC_COMMAND_BYTES)
            }
        }
    }

    pub fn into_input_command(self) -> InputCommand {
        match self {
            ApiCommand::SysList {} => InputCommand::Sys(common::command::SysCommand::List),
            ApiCommand::SysNow {} => InputCommand::Sys(common::command::SysCommand::Now),
            ApiCommand::SysHistory { kik_id } => {
                InputCommand::Sys(common::command::SysCommand::History(kik_id))
            }
            ApiCommand::SysUse { kik_id } => {
                InputCommand::Sys(common::command::SysCommand::Use(kik_id))
            }
            ApiCommand::CtrlLs { path } => InputCommand::Ctrl(InputCtrlCommand::Ls(path)),
            ApiCommand::CtrlScreen { save_path } => InputCommand::Ctrl(InputCtrlCommand::Screen(
                save_path.unwrap_or_else(|| "_".to_string()),
            )),
            ApiCommand::CtrlGetFile {
                remote_path,
                local_path,
            } => InputCommand::Ctrl(InputCtrlCommand::GetFile(
                remote_path,
                local_path.unwrap_or_default(),
            )),
            ApiCommand::CtrlGetBigFile {
                remote_path,
                local_path,
            } => InputCommand::Ctrl(InputCtrlCommand::GetBigFile(
                remote_path,
                local_path.unwrap_or_default(),
            )),
            ApiCommand::CtrlSetFile {
                local_path,
                remote_path,
            } => InputCommand::Ctrl(InputCtrlCommand::SetFile(local_path, remote_path)),
            ApiCommand::CtrlSetBigFile {
                local_path,
                remote_path,
            } => InputCommand::Ctrl(InputCtrlCommand::SetBigFile(local_path, remote_path)),
            ApiCommand::Exec { command } => InputCommand::Exec(command),
        }
    }
}

fn validate_path(value: &str, field: &str, allow_empty: bool) -> Result<(), ApiErrorBody> {
    if allow_empty && value.is_empty() {
        return Ok(());
    }
    validate_text(value, field, MAX_PATH_BYTES)
}

fn validate_text(value: &str, field: &str, max_bytes: usize) -> Result<(), ApiErrorBody> {
    if value.is_empty() || value.len() > max_bytes || value.contains('\0') {
        return Err(ApiErrorBody::bad_request(format!(
            "{field} 必须为 1..={max_bytes} bytes 且不能包含 NUL"
        )));
    }
    Ok(())
}

pub fn remote_resp_to_api_data(
    command: &InputCommand,
    response: RemoteResp,
) -> Result<ApiResponseData, ApiErrorBody> {
    match response {
        RemoteResp::Success(RemoteSuccessResp::Info(message)) => {
            Ok(ApiResponseData::Info { message })
        }
        RemoteResp::Success(RemoteSuccessResp::Ls(entries)) => Ok(ApiResponseData::Ls { entries }),
        RemoteResp::Success(RemoteSuccessResp::SysList(items)) => {
            Ok(ApiResponseData::SysList { items })
        }
        RemoteResp::Success(RemoteSuccessResp::Now(value)) => Ok(ApiResponseData::SysNow { value }),
        RemoteResp::Success(RemoteSuccessResp::History(items)) => {
            Ok(ApiResponseData::SysHistory { items })
        }
        RemoteResp::SuccessData(bytes) => {
            if bytes.len() > MAX_API_BINARY_BYTES {
                return Err(ApiErrorBody::payload_too_large(format!(
                    "二进制响应超过开放 API 上限 {} bytes，请改用受控文件传输流程",
                    MAX_API_BINARY_BYTES
                )));
            }
            let (content_type, filename) = binary_meta(command);
            Ok(ApiResponseData::Binary {
                content_type,
                filename,
                base64: STANDARD.encode(bytes),
            })
        }
        RemoteResp::Error(code, message) => Err(ApiErrorBody::remote(code, message)),
    }
}

fn binary_meta(command: &InputCommand) -> (String, Option<String>) {
    match command {
        InputCommand::Ctrl(InputCtrlCommand::Screen(_)) => {
            ("image/png".to_string(), Some("screen.png".to_string()))
        }
        _ => ("application/octet-stream".to_string(), None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn api_command_json_is_stable() {
        let json =
            r#"{"version":1,"request_id":"r1","command":{"kind":"ctrl_ls","path":"C:\\Temp"}}"#;
        let req: ApiRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.version, API_VERSION);
        assert_eq!(req.request_id.as_deref(), Some("r1"));
        match req.command {
            ApiCommand::CtrlLs { path } => assert_eq!(path, "C:\\Temp"),
            _ => panic!("命令类型解析错误"),
        }
    }

    #[test]
    fn binary_response_uses_base64() {
        let cmd = InputCommand::Ctrl(InputCtrlCommand::Screen("_".to_string()));
        let data = remote_resp_to_api_data(&cmd, RemoteResp::SuccessData(vec![1, 2, 3])).unwrap();
        match data {
            ApiResponseData::Binary {
                content_type,
                filename,
                base64,
            } => {
                assert_eq!(content_type, "image/png");
                assert_eq!(filename.as_deref(), Some("screen.png"));
                assert_eq!(base64, "AQID");
            }
            _ => panic!("响应类型错误"),
        }
    }

    #[test]
    fn api_request_rejects_oversized_or_nul_fields() {
        let oversized = ApiRequest::new(ApiCommand::SysUse {
            kik_id: "x".repeat(MAX_KIK_ID_BYTES + 1),
        });
        assert!(oversized.validate().is_err());

        let nul_path = ApiRequest::new(ApiCommand::CtrlLs {
            path: "C:\\Temp\0hidden".to_string(),
        });
        assert!(nul_path.validate().is_err());

        let mut nul_request_id = ApiRequest::new(ApiCommand::SysNow {});
        nul_request_id.request_id = Some("safe\0forged".to_string());
        assert!(nul_request_id.validate().is_err());
    }

    #[test]
    fn api_json_rejects_unknown_fields_in_envelope_and_command() {
        let envelope = r#"{"version":1,"command":{"kind":"sys_now"},"unexpected":true}"#;
        let command = r#"{"version":1,"command":{"kind":"ctrl_ls","path":"C:\\Temp","typo":true}}"#;
        let unit_like_command = r#"{"version":1,"command":{"kind":"sys_now","typo":true}}"#;

        assert!(serde_json::from_str::<ApiRequest>(envelope).is_err());
        assert!(serde_json::from_str::<ApiRequest>(command).is_err());
        assert!(serde_json::from_str::<ApiRequest>(unit_like_command).is_err());
    }

    #[test]
    fn sys_history_api_maps_optional_kik_id() {
        let all = ApiRequest::new(ApiCommand::SysHistory { kik_id: None });
        assert!(all.validate().is_ok());
        assert!(matches!(
            all.command.into_input_command(),
            InputCommand::Sys(common::command::SysCommand::History(None))
        ));

        let one = ApiRequest::new(ApiCommand::SysHistory {
            kik_id: Some("kik-1".to_string()),
        });
        assert!(one.validate().is_ok());
        assert!(matches!(
            one.command.into_input_command(),
            InputCommand::Sys(common::command::SysCommand::History(Some(id))) if id == "kik-1"
        ));
    }
}
