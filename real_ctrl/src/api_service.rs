use crate::api_contract::{
    remote_resp_to_api_data, ApiErrorBody, ApiRequest, ApiResponse, API_VERSION,
};
use crate::context::Context;
use crate::dispatch;
use crate::input_command::{InputCommand, RemoteResp};

#[derive(Debug, thiserror::Error)]
pub enum ApiServiceError {
    #[error("另一个控制命令正在执行")]
    Busy,
    #[error("{0}")]
    Forbidden(String),
    #[error(transparent)]
    Execution(#[from] anyhow::Error),
}

#[derive(Clone)]
pub struct RealCtrlApi {
    context: Context,
    policy: ApiPolicy,
}

#[derive(Clone)]
pub struct ApiPolicy {
    allow_exec: bool,
}

impl RealCtrlApi {
    pub fn new(context: Context) -> Self {
        Self {
            context,
            policy: ApiPolicy::from_env(),
        }
    }

    pub fn with_policy(context: Context, policy: ApiPolicy) -> Self {
        Self { context, policy }
    }

    pub async fn execute(&self, command: InputCommand) -> Result<RemoteResp, ApiServiceError> {
        self.ensure_allowed(&command)?;
        let _permit = self
            .context
            .try_acquire_command()
            .map_err(|_| ApiServiceError::Busy)?;
        self.execute_allowed(command).await
    }

    pub async fn execute_request(&self, request: ApiRequest) -> ApiResponse {
        if request.version != API_VERSION {
            return ApiResponse::error(
                request.request_id,
                ApiErrorBody::unsupported_version(request.version),
            );
        }

        if let Err(error) = request.validate() {
            return ApiResponse::error(request.request_id, error);
        }

        let command = request.command.clone().into_input_command();
        if let Err(error) = self.ensure_allowed(&command) {
            return ApiResponse::error(request.request_id, error.into_api_error());
        }
        let _permit = match self.context.try_acquire_command() {
            Ok(permit) => permit,
            Err(_) => {
                return ApiResponse::error(
                    request.request_id,
                    ApiErrorBody::busy("另一个控制命令正在执行，请稍后重试"),
                )
            }
        };
        log::info!(
            "开放 API 调用: request_id={:?}, command={}",
            request.request_id,
            request.command.kind()
        );
        match self.execute_allowed(command.clone()).await {
            Ok(resp) => match remote_resp_to_api_data(&command, resp) {
                Ok(data) => ApiResponse::success(&request, data),
                Err(err) => ApiResponse::error(request.request_id, err),
            },
            Err(err) => {
                log::error!(
                    "开放 API 执行失败: request_id={:?}, command={}, error={}",
                    request.request_id,
                    request.command.kind(),
                    err
                );
                ApiResponse::error(request.request_id, ApiErrorBody::internal("命令执行失败"))
            }
        }
    }

    async fn execute_allowed(&self, command: InputCommand) -> Result<RemoteResp, ApiServiceError> {
        // 所有协议入口最终都进入这个分发点；并发门禁的 permit 由上层持有到数据处理结束。
        dispatch::distribution_other(&self.context, command)
            .await
            .map_err(ApiServiceError::Execution)
    }

    fn ensure_allowed(&self, command: &InputCommand) -> Result<(), ApiServiceError> {
        match command {
            InputCommand::Exec(_) if !self.policy.allow_exec => Err(ApiServiceError::Forbidden(
                "开放 API 默认禁用 Exec，请显式设置 REAL_CTRL_API_ALLOW_EXEC=1".to_string(),
            )),
            InputCommand::Local(_) => Err(ApiServiceError::Forbidden(
                "开放 API 不支持本地生命周期命令".to_string(),
            )),
            _ => Ok(()),
        }
    }
}

impl ApiServiceError {
    fn into_api_error(self) -> ApiErrorBody {
        match self {
            Self::Busy => ApiErrorBody::busy("另一个控制命令正在执行，请稍后重试"),
            Self::Forbidden(message) => ApiErrorBody::forbidden(message),
            Self::Execution(_) => ApiErrorBody::internal("命令执行失败"),
        }
    }
}

impl ApiPolicy {
    pub fn from_env() -> Self {
        Self {
            allow_exec: env_flag("REAL_CTRL_API_ALLOW_EXEC"),
        }
    }

    #[cfg(test)]
    pub fn allow_exec_for_test() -> Self {
        Self { allow_exec: true }
    }
}

fn env_flag(name: &str) -> bool {
    std::env::var(name)
        .map(|v| {
            matches!(
                v.as_str(),
                "1" | "true" | "TRUE" | "yes" | "YES" | "on" | "ON"
            )
        })
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_flag_accepts_enabled_values() {
        std::env::set_var("REAL_CTRL_API_TEST_FLAG", "true");
        assert!(env_flag("REAL_CTRL_API_TEST_FLAG"));
        std::env::remove_var("REAL_CTRL_API_TEST_FLAG");
    }
}
