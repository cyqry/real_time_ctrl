use crate::api_contract::{
    remote_resp_to_api_data, ApiErrorBody, ApiRequest, ApiResponse, API_VERSION,
};
use crate::context::Context;
use crate::dispatch;
use crate::input_command::{InputCommand, RemoteResp};

#[derive(Debug, thiserror::Error)]
pub enum ApiServiceError {
    #[error("控制命令并发达到上限")]
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
                    ApiErrorBody::busy("控制命令并发达到上限，请稍后重试"),
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
        // 所有开放 API 入口最终进入同一分发点；有界 permit 持有到关联数据处理结束。
        dispatch::distribution_other(&self.context, command)
            .await
            .map_err(ApiServiceError::Execution)
    }

    fn ensure_allowed(&self, command: &InputCommand) -> Result<(), ApiServiceError> {
        match command {
            InputCommand::Exec(_) if !self.policy.allow_exec => Err(ApiServiceError::Forbidden(
                "开放 API 的当前策略禁止 Exec，可通过 REAL_CTRL_API_ALLOW_EXEC=1 覆盖".to_string(),
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
            Self::Busy => ApiErrorBody::busy("控制命令并发达到上限，请稍后重试"),
            Self::Forbidden(message) => ApiErrorBody::forbidden(message),
            Self::Execution(_) => ApiErrorBody::internal("命令执行失败"),
        }
    }
}

impl ApiPolicy {
    pub fn from_env() -> Self {
        Self {
            allow_exec: crate::runtime_config::api_allow_exec(),
        }
    }

    #[cfg(test)]
    pub fn allow_exec_for_test() -> Self {
        Self { allow_exec: true }
    }
}
