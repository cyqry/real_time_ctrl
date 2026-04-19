use common::message::kik_cmd_resp_info;
use crate::context::Context;
use crate::input_command::{InputCommand, RemoteResp, RemoteSuccessResp};
use crate::{ctrl_executor, direct_executor, local_executor, server_executor};
use common::message::kik_cmd_resp_info::Ls;
use ctrl_common::cmd_resp_info::{KikInfoVo, SysNow};

pub async fn distribution(context: &Context, command: InputCommand) -> anyhow::Result<String> {
    match command {
        InputCommand::Sys(sys) => match server_executor::execute(context, sys).await? {
            RemoteResp::Success(RemoteSuccessResp::Info(info)) => Ok(info),
            RemoteResp::Success(RemoteSuccessResp::SysList(vec)) => Ok(format_sys_list(vec)),
            RemoteResp::Success(RemoteSuccessResp::Now(now)) => Ok(format_now(now)),
            RemoteResp::Error(code, info) => Err(anyhow::anyhow!(info)),
            _ => {
                unreachable!()
            }
        },
        InputCommand::Local(local) => local_executor::execute(context, local).await,
        InputCommand::Ctrl(ctrl) => match ctrl_executor::execute(context, ctrl, false).await? {
            RemoteResp::Success(RemoteSuccessResp::Info(info)) => Ok(info),
            RemoteResp::Success(RemoteSuccessResp::Ls(vec)) => Ok(format_ls(&vec)),
            RemoteResp::Error(code, info) => Err(anyhow::anyhow!(info)),
            _ => {
                unreachable!()
            }
        },
        InputCommand::Exec(cmd) => match direct_executor::execute(context, &cmd).await? {
            RemoteResp::Success(RemoteSuccessResp::Info(info)) => Ok(info),
            RemoteResp::Error(code, info) => Err(anyhow::anyhow!(info)),
            _ => {
                unreachable!()
            }
        },
    }
}

fn format_now(sys_now: SysNow) -> String {
    match sys_now {
        SysNow::Kik(kik) => {
            format!(
                "当前正在控制 {}-----{}",
                kik.name,
                kik.id
            )
        }
        SysNow::None => {
            "没有被控制的Kik".to_owned()
        }
        SysNow::NotOnline => {
            "当前被控Kik不在线".to_owned()
        }
    }

}

fn format_ls(
    data: &Vec<kik_cmd_resp_info::Ls>,
) -> String {
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
    for kik_cmd_resp_info::Ls { filename, is_file, size, created_date, modified_date } in data {
        if let Some(name) = filename {
            max_filename_len = max_filename_len.max(name.len());
        }
        let is_file_str = is_file_str(*is_file);
        max_is_file_len = max_is_file_len.max(is_file_str.len());

        let size_str = format!(
            "{}",
            match size.map(|size| { size / 1024 }) {
                None => {
                    "__".to_string()
                }
                Some(size) => {
                    size.to_string()
                }
            }
        ); // 转换到KB
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
    for kik_cmd_resp_info::Ls { filename, is_file, size, created_date, modified_date } in data {
        let filename_str = filename.as_ref().unwrap_or(&blank);
        let size_str = format!(
            "{}",
            match size.map(|size| { size / 1024 }) {
                None => {
                    "__".to_string()
                }
                Some(size) => {
                    size.to_string()
                }
            }
        ); // 转换到KB
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


pub async fn distribution_other(
    context: &Context,
    command: InputCommand,
) -> anyhow::Result<RemoteResp> {
    match command {
        InputCommand::Sys(sys) => server_executor::execute(context, sys).await,
        InputCommand::Ctrl(ctrl) => ctrl_executor::execute(context, ctrl, true).await,
        InputCommand::Exec(cmd) => direct_executor::execute(context, &cmd).await,
        _ => {
            unimplemented!("不支持的")
        }
    }
}

fn format_sys_list(kiks: Vec<KikInfoVo>) -> String {
    let mut info = String::new();
    for kik in kiks.iter() {
        info += format!("{}--->{}\n", kik.id, kik.name).as_str();
    }
    info
}

fn format_use(kik: KikInfoVo) -> String {
    format!("您正在控制 {}-----{}", kik.name, kik.id)
}

fn format_file_meta(
    data: &Vec<kik_cmd_resp_info::Ls>,
) -> String {
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
    for kik_cmd_resp_info::Ls { filename, is_file, size, created_date, modified_date } in data {
        if let Some(name) = filename {
            max_filename_len = max_filename_len.max(name.len());
        }
        let is_file_str = is_file_str(*is_file);
        max_is_file_len = max_is_file_len.max(is_file_str.len());

        let size_str = format!(
            "{}",
            match size.map(|size| { size / 1024 }) {
                None => {
                    "__".to_string()
                }
                Some(size) => {
                    size.to_string()
                }
            }
        ); // 转换到KB
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
    for kik_cmd_resp_info::Ls { filename, is_file, size, created_date, modified_date } in data {
        let filename_str = filename.as_ref().unwrap_or(&blank);
        let size_str = format!(
            "{}",
            match size.map(|size| { size / 1024 }) {
                None => {
                    "__".to_string()
                }
                Some(size) => {
                    size.to_string()
                }
            }
        ); // 转换到KB
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
