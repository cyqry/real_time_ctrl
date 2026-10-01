//! 控制台和开放 API 共用的业务命令分派。
//!
//! `distribution` 把结果格式化成人类可读文本；`distribution_other` 保留结构化 `RemoteResp` 给 HTTP/管道。
//! 两者最终调用相同 executor，避免三种入口出现不同的远程行为。

use crate::context::Context;
use crate::input_command::{InputCommand, RemoteResp, RemoteSuccessResp};
use crate::{ctrl_executor, direct_executor, local_executor, server_executor};
use common::message::kik_cmd_resp_info;
use ctrl_common::cmd_resp_info::{KikInfoVo, LocalNow};

/// CLI 分派入口：串行执行并把结构化结果格式化为终端文本。
pub async fn distribution(context: &Context, command: InputCommand) -> anyhow::Result<String> {
    // 必须早于上传文件读取/散列等 await；后续 local_use 只能影响下一条操作。
    let target = if command.requires_target() {
        context.snapshot_target(None)?
    } else {
        String::new()
    };
    match command {
        InputCommand::RunTask(name) => {
            match direct_executor::run_task(context, &name, &target).await? {
                RemoteResp::Success(RemoteSuccessResp::Info(info)) => Ok(info),
                RemoteResp::Error(_, info) => Err(anyhow::anyhow!(info)),
                _ => Err(anyhow::anyhow!("任务响应类型不匹配")),
            }
        }
        InputCommand::Sys(sys) => match server_executor::execute(context, sys).await? {
            RemoteResp::Success(RemoteSuccessResp::Info(info)) => Ok(info),
            RemoteResp::Success(RemoteSuccessResp::SysList(vec)) => Ok(format_sys_list(vec)),
            RemoteResp::Success(RemoteSuccessResp::History(records)) => {
                Ok(format_kik_history(records))
            }
            RemoteResp::Success(RemoteSuccessResp::Now(now)) => Ok(format_now(now)),
            RemoteResp::Error(_code, info) => Err(anyhow::anyhow!(info)),
            _ => Err(anyhow::anyhow!("系统命令响应类型不匹配")),
        },
        InputCommand::Local(local) => match local_executor::execute(context, local).await? {
            RemoteResp::Success(RemoteSuccessResp::Now(now)) => Ok(format_now(now)),
            RemoteResp::Success(RemoteSuccessResp::Info(info)) => Ok(info),
            _ => Err(anyhow::anyhow!("本地命令响应类型不匹配")),
        },
        InputCommand::Ctrl(ctrl) => {
            match ctrl_executor::execute(context, ctrl, false, &target).await? {
                RemoteResp::Success(RemoteSuccessResp::Info(info)) => Ok(info),
                RemoteResp::Success(RemoteSuccessResp::Ls(vec)) => Ok(format_ls(&vec)),
                RemoteResp::Error(_code, info) => Err(anyhow::anyhow!(info)),
                _ => Err(anyhow::anyhow!("控制命令响应类型不匹配")),
            }
        }
        InputCommand::Exec(cmd) => match direct_executor::execute(context, &cmd, &target).await? {
            RemoteResp::Success(RemoteSuccessResp::Info(info)) => Ok(info),
            RemoteResp::Error(_code, info) => Err(anyhow::anyhow!(info)),
            _ => Err(anyhow::anyhow!("Exec 响应类型不匹配")),
        },
    }
}

fn format_now(sys_now: LocalNow) -> String {
    match sys_now {
        LocalNow::Kik(kik) => {
            format!(
                "本地已选择 {}-----{}（选择快照，不代表实时在线）",
                kik.name, kik.id
            )
        }
        LocalNow::None => "没有被控制的Kik".to_owned(),
        LocalNow::NotOnline => "当前被控Kik不在线".to_owned(),
    }
}

fn format_ls(data: &Vec<kik_cmd_resp_info::Ls>) -> String {
    let mut res = String::new();
    let file_name_header = "Filename";
    let is_file_header = "IsFile";
    let size_header = "Size(KB)";
    let create_date_header = "Created Date";
    let modified_date_header = "Modified Date";
    // 用于存储每列的最大宽度
    let mut max_filename_len = file_name_header.len();
    let mut max_is_file_len = is_file_header.len();
    let mut max_size_len = size_header.len();
    let mut max_created_date_len = create_date_header.len();
    let mut max_modified_date_len = modified_date_header.len();

    let is_file_str = |is_file: bool| -> &str {
        if is_file {
            "File"
        } else {
            "Directory"
        }
    };
    // 首先，找出每列的最大宽度
    for kik_cmd_resp_info::Ls {
        filename,
        is_file,
        size,
        created_date,
        modified_date,
    } in data
    {
        if let Some(name) = filename {
            max_filename_len = max_filename_len.max(name.len());
        }
        let is_file_str = is_file_str(*is_file);
        max_is_file_len = max_is_file_len.max(is_file_str.len());

        let size_str = size
            .map(|size| (size / 1024).to_string())
            .unwrap_or_else(|| "__".to_string());
        max_size_len = max_size_len.max(size_str.len());

        if let Some(date) = created_date {
            max_created_date_len = max_created_date_len.max(date.len());
        }

        if let Some(date) = modified_date {
            max_modified_date_len = max_modified_date_len.max(date.len());
        }
    }

    // 打印表头
    res += &format!(
        "{:<width$} | {:<width2$} | {:<width3$} | {:<width4$} | {:<width5$}\n",
        file_name_header,
        is_file_header,
        size_header,
        create_date_header,
        modified_date_header,
        width = max_filename_len,
        width2 = max_is_file_len,
        width3 = max_size_len,
        width4 = max_created_date_len,
        width5 = max_modified_date_len,
    );

    // 打印分隔线
    res += &format!(
        "{}-+-{}-+-{}-+-{}-+-{}\n",
        "-".repeat(max_filename_len),
        "-".repeat(max_is_file_len),
        "-".repeat(max_size_len),
        "-".repeat(max_created_date_len),
        "-".repeat(max_modified_date_len),
    );

    // 打印数据
    let blank = "".to_string();
    for kik_cmd_resp_info::Ls {
        filename,
        is_file,
        size,
        created_date,
        modified_date,
    } in data
    {
        let filename_str = filename.as_ref().unwrap_or(&blank);
        let size_str = size
            .map(|size| (size / 1024).to_string())
            .unwrap_or_else(|| "__".to_string());
        let created_date_str = created_date.as_ref().unwrap_or(&blank);
        let modified_date_str = modified_date.as_ref().unwrap_or(&blank);

        res += &format!(
            "{:<width$} | {:<width2$} | {:<width3$} | {:<width4$} | {:<width5$}\n",
            filename_str,
            is_file_str(*is_file),
            size_str,
            created_date_str,
            modified_date_str,
            width = max_filename_len,
            width2 = max_is_file_len,
            width3 = max_size_len,
            width4 = max_created_date_len,
            width5 = max_modified_date_len,
        );
    }
    res
}

/// 开放 API 分派入口：保留结构化响应和原始小二进制数据。
pub async fn distribution_other(
    context: &Context,
    command: InputCommand,
) -> anyhow::Result<RemoteResp> {
    distribution_other_targeted(context, command, None).await
}

/// API 的显式 target 只绑定本次操作，不改写其他调用者共享的本地选择。
pub async fn distribution_other_targeted(
    context: &Context,
    command: InputCommand,
    explicit_target: Option<&str>,
) -> anyhow::Result<RemoteResp> {
    let target = if command.requires_target() {
        context.snapshot_target(explicit_target)?
    } else {
        String::new()
    };
    match command {
        InputCommand::Sys(sys) => server_executor::execute(context, sys).await,
        InputCommand::Ctrl(ctrl) => ctrl_executor::execute(context, ctrl, true, &target).await,
        InputCommand::Exec(cmd) => direct_executor::execute(context, &cmd, &target).await,
        InputCommand::RunTask(name) => direct_executor::run_task(context, &name, &target).await,
        InputCommand::Local(common::command::LocalCommand::LocalExit) => {
            Err(anyhow::anyhow!("开放 API 不支持退出进程"))
        }
        InputCommand::Local(local) => local_executor::execute(context, local).await,
    }
}

fn format_sys_list(kiks: Vec<KikInfoVo>) -> String {
    let mut info = String::new();
    for kik in kiks.iter() {
        info += format!("{}--->{}\n", kik.id, kik.name).as_str();
    }
    info
}

fn format_kik_history(records: Vec<ctrl_common::cmd_resp_info::KikPresenceVo>) -> String {
    use chrono::{Local, TimeZone};

    let mut output = String::new();
    for record in records {
        let online_time = Local
            .timestamp_millis_opt(record.recent_online_unix_ms as i64)
            .single()
            .map(|time| time.format("%Y-%m-%d %H:%M:%S").to_string())
            .unwrap_or_else(|| "-".to_string());
        let offline_time = record
            .recent_offline_unix_ms
            .and_then(|time| Local.timestamp_millis_opt(time as i64).single())
            .map(|time| time.format("%Y-%m-%d %H:%M:%S").to_string())
            .unwrap_or_else(|| "-".to_string());
        output.push_str(&format!(
            "{} | {} | {} | online={} | recent_online={} | recent_offline={}\n",
            record.id, record.name, record.ip, record.online, online_time, offline_time
        ));
    }
    output
}
