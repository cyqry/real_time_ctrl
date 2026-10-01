//! HTTP 与命名管道共用的稳定业务服务层。
//!
//! 适配器先验证 API 契约和能力策略，再取得进程级有界并发许可，最后进入统一分派。许可覆盖控制响应和
//! 关联数据处理的完整生命周期，防止大文件命令脱离并发统计。

use crate::api_contract::{
    remote_resp_to_api_data, ApiErrorBody, ApiRequest, ApiResponse, API_VERSION,
};
use crate::context::Context;
use crate::dispatch;
use crate::input_command::{InputCommand, RemoteResp};

#[derive(Debug, thiserror::Error)]
/// 服务层内部错误；进入 HTTP/管道响应前还会映射为稳定 `ApiErrorBody`。
pub enum ApiServiceError {
    #[error("控制命令并发达到上限")]
    Busy,
    #[error("{0}")]
    Forbidden(String),
    #[error(transparent)]
    Execution(#[from] anyhow::Error),
}

#[derive(Clone)]
/// 所有本地入口共用的命令服务门面。
pub struct RealCtrlApi {
    context: Context,
    policy: ApiPolicy,
}

#[derive(Clone)]
/// 本地入口能力策略；Exec 与命名任务均属于远程执行能力，共用开关。
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
        self.execute_allowed(command, None).await
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
        match self
            .execute_allowed(command.clone(), request.target_kik_id.as_deref())
            .await
        {
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
                ApiResponse::error(request.request_id, err.into_api_error())
            }
        }
    }

    async fn execute_allowed(
        &self,
        command: InputCommand,
        explicit_target: Option<&str>,
    ) -> Result<RemoteResp, ApiServiceError> {
        // 所有开放 API 入口最终进入同一分发点；有界 permit 持有到关联数据处理结束。
        dispatch::distribution_other_targeted(&self.context, command, explicit_target)
            .await
            .map_err(ApiServiceError::Execution)
    }

    fn ensure_allowed(&self, command: &InputCommand) -> Result<(), ApiServiceError> {
        self.policy.ensure_allowed(command)
    }
}

impl ApiServiceError {
    fn into_api_error(self) -> ApiErrorBody {
        match self {
            Self::Busy => ApiErrorBody::busy("控制命令并发达到上限，请稍后重试"),
            Self::Forbidden(message) => ApiErrorBody::forbidden(message),
            Self::Execution(error) if error.is::<crate::local_target::NoTargetSelected>() => {
                ApiErrorBody {
                    code: "no_target".into(),
                    message: error.to_string(),
                }
            }
            Self::Execution(_) => ApiErrorBody::internal("命令执行失败"),
        }
    }
}

impl ApiPolicy {
    fn ensure_allowed(&self, command: &InputCommand) -> Result<(), ApiServiceError> {
        match command {
            InputCommand::Exec(_) | InputCommand::RunTask(_) if !self.allow_exec => {
                Err(ApiServiceError::Forbidden(
                    "开放 API 的当前策略禁止 Exec 和任务执行，可通过 REAL_CTRL_API_ALLOW_EXEC=1 覆盖"
                        .to_string(),
                ))
            }
            InputCommand::Local(common::command::LocalCommand::LocalExit) => Err(ApiServiceError::Forbidden(
                "开放 API 不支持本地生命周期命令".to_string(),
            )),
            _ => Ok(()),
        }
    }

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn task_and_exec_share_the_open_api_execution_policy() {
        let commands = [
            InputCommand::Exec("echo test".into()),
            InputCommand::RunTask("collect-info".into()),
        ];
        let disabled = ApiPolicy { allow_exec: false };
        let enabled = ApiPolicy::allow_exec_for_test();
        for command in commands {
            let error = disabled
                .ensure_allowed(&command)
                .unwrap_err()
                .into_api_error();
            assert_eq!(error.code, "forbidden");
            assert!(enabled.ensure_allowed(&command).is_ok());
        }
        assert!(disabled
            .ensure_allowed(&InputCommand::Sys(common::command::SysCommand::List))
            .is_ok());
        for command in [
            common::command::LocalCommand::LocalNow,
            common::command::LocalCommand::LocalUse("A".into()),
        ] {
            assert!(disabled
                .ensure_allowed(&InputCommand::Local(command))
                .is_ok());
        }
        assert!(enabled
            .ensure_allowed(&InputCommand::Local(
                common::command::LocalCommand::LocalExit
            ))
            .is_err());
    }
}
