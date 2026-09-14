use real_ctrl::api_contract::{ApiCommand, ApiRequest, ApiResponseData};
use real_ctrl::local_client::client::invoke_api;
use std::time::{Duration, Instant};
use tokio::task::JoinSet;

const CONCURRENT_REQUESTS: usize = 12;

/// 该探针只用于本地 E2E：每个任务创建独立命名管道连接，从真实 DACL listener 进入同一业务服务层。
/// 若管道服务或核心响应路由仍存在全局串行锁，12 个约两秒命令会明显超过八秒门限。
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let started = Instant::now();
    let mut tasks = JoinSet::new();
    for index in 0..CONCURRENT_REQUESTS {
        tasks.spawn(async move {
            let request_id = format!("pipe-concurrent-{index}");
            let mut request = ApiRequest::new(ApiCommand::Exec {
                command: format!("ping -n 3 127.0.0.1 >NUL && echo pipe-concurrent-{index}"),
            });
            request.request_id = Some(request_id.clone());
            let response = invoke_api(&request).await?;
            if !response.ok || response.request_id.as_deref() != Some(request_id.as_str()) {
                anyhow::bail!("管道响应状态或 request_id 不匹配: {request_id}");
            }
            match response.data {
                Some(ApiResponseData::Info { message }) if message.contains(&request_id) => Ok(()),
                _ => anyhow::bail!("管道响应内容不匹配: {request_id}"),
            }
        });
    }

    let mut completed = 0_usize;
    while let Some(result) = tasks.join_next().await {
        result??;
        completed += 1;
    }
    let elapsed = started.elapsed();
    if completed != CONCURRENT_REQUESTS || elapsed >= Duration::from_secs(8) {
        anyhow::bail!(
            "命名管道请求疑似串行或不完整: completed={completed}, elapsed_ms={}",
            elapsed.as_millis()
        );
    }

    println!(
        "{{\"completed\":{completed},\"elapsed_ms\":{}}}",
        elapsed.as_millis()
    );
    Ok(())
}
