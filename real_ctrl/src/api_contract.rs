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
pub struct ApiRequest {
    pub version: u16,
    #[serde(default)]
    pub request_id: Option<String>,
    pub command: ApiCommand,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ApiCommand {
    SysList,
    SysNow,
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
    Binary {
        content_type: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        filename: Option<String>,
        base64: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
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
        if self
            .request_id
            .as_ref()
            .is_some_and(|value| value.is_empty() || value.len() > MAX_REQUEST_ID_BYTES)
        {
            return Err(ApiErrorBody::bad_request(format!(
                "request_id 必须为 1..={MAX_REQUEST_ID_BYTES} bytes"
            )));
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
            ApiCommand::SysList => "sys_list",
            ApiCommand::SysNow => "sys_now",
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
            ApiCommand::SysList | ApiCommand::SysNow => Ok(()),
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
            }
            | ApiCommand::CtrlGetBigFile {
                remote_path,
                local_path,
            } => {
                validate_path(remote_path, "remote_path", false)?;
                if let Some(path) = local_path {
                    validate_path(path, "local_path", true)?;
                }
                Ok(())
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
            ApiCommand::SysList => InputCommand::Sys(common::command::SysCommand::List),
            ApiCommand::SysNow => InputCommand::Sys(common::command::SysCommand::Now),
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
    }
}
