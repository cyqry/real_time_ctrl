use crate::input_command::{InputCommand, InputCtrlCommand, RemoteResp, RemoteSuccessResp};
use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use serde::{Deserialize, Serialize};

pub const API_VERSION: u16 = 1;

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
}
