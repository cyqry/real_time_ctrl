use crate::api_contract::{
    remote_resp_to_api_data, ApiErrorBody, ApiRequest, ApiResponse, API_VERSION,
};
use crate::context::Context;
use crate::dispatch;
use crate::input_command::{InputCommand, RemoteResp};

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

    pub async fn execute(&self, command: InputCommand) -> anyhow::Result<RemoteResp> {
        self.ensure_allowed(&command)?;
        // 统一入口先复用既有分发逻辑；后续鉴权、审计、限流都应该收敛到这里。
        dispatch::distribution_other(&self.context, command).await
    }

    pub async fn execute_request(&self, request: ApiRequest) -> ApiResponse {
        if request.version != API_VERSION {
            return ApiResponse::error(
                request.request_id,
                ApiErrorBody::unsupported_version(request.version),
            );
        }

        let command = request.command.clone().into_input_command();
        match self.execute(command.clone()).await {
            Ok(resp) => match remote_resp_to_api_data(&command, resp) {
                Ok(data) => ApiResponse::success(&request, data),
                Err(err) => ApiResponse::error(request.request_id, err),
            },
            Err(err) => {
                ApiResponse::error(request.request_id, ApiErrorBody::internal(err.to_string()))
            }
        }
    }

    fn ensure_allowed(&self, command: &InputCommand) -> anyhow::Result<()> {
        match command {
            InputCommand::Exec(_) if !self.policy.allow_exec => Err(anyhow::anyhow!(
                "开放 API 默认禁用 Exec，请显式设置 REAL_CTRL_API_ALLOW_EXEC=1"
            )),
            InputCommand::Local(_) => Err(anyhow::anyhow!("开放 API 不支持本地生命周期命令")),
            _ => Ok(()),
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
