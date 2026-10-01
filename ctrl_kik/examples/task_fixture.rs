//! 任务 E2E 专用程序：输出、退出码、超时、后代进程和异步标记均由测试参数控制。
//! 不访问网络；测试写入路径由仓库内的验收脚本提供，不进入正式发布产物。

use std::{
    io::{self, Write},
    process::{Command, Stdio},
    time::Duration,
};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        None => println!("task-fixture-default"),
        Some("identity") => {
            // 仅测试程序报告自己的真实路径，用于核验缓存复用及同名更新；生产 Kik 不暴露该诊断。
            println!("exe:{}", std::env::current_exe()?.display());
            for arg in args.iter().skip(1) {
                println!("arg:{arg}");
            }
        }
        Some("echo") => {
            for arg in args.iter().skip(1) {
                println!("arg:{arg}");
            }
        }
        Some("quiet") => {}
        Some("exit") => std::process::exit(7),
        Some("sleep") => {
            let millis: u64 = args[1].parse()?;
            if let Some(path) = args.get(2) {
                std::fs::write(path, std::process::id().to_string())?;
            }
            std::thread::sleep(Duration::from_millis(millis));
            if let Some(path) = args.get(2) {
                std::fs::write(format!("{path}.done"), b"done")?;
            }
            println!("slept:{millis}");
        }
        Some("barrier") => {
            // 先证明每个进程都在运行，再由测试统一放行；不能靠短暂 sleep 推断运行数上限。
            let release = std::path::Path::new(&args[1]);
            std::fs::write(&args[2], std::process::id().to_string())?;
            let deadline = std::time::Instant::now() + Duration::from_secs(75);
            while !release.exists() {
                if std::time::Instant::now() >= deadline {
                    return Err("fixture barrier timed out".into());
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            println!("barrier-released");
        }
        Some(mode @ ("child" | "child-exit")) => {
            let mut child = Command::new(std::env::current_exe()?)
                .args(["sleep", "300000", &args[1]])
                .stdin(Stdio::null())
                .stdout(Stdio::inherit())
                .stderr(Stdio::inherit())
                .spawn()?;
            if mode == "child" {
                let _ = child.wait()?;
            } else {
                // 子进程先写 PID，父进程再退出，用于证明正常完成也清理普通后代。
                let deadline = std::time::Instant::now() + Duration::from_secs(5);
                while !std::path::Path::new(&args[1]).exists() {
                    if std::time::Instant::now() >= deadline {
                        let _ = child.kill();
                        return Err("child did not start".into());
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
                println!("parent-finished");
            }
        }
        Some("flood") => {
            let stderr = std::thread::spawn(|| {
                let mut stream = io::stderr().lock();
                for _ in 0..512 {
                    stream.write_all(&[b'E'; 8192]).unwrap();
                }
            });
            let mut stream = io::stdout().lock();
            for _ in 0..512 {
                stream.write_all(&[b'O'; 8192])?;
            }
            stderr.join().unwrap();
        }
        Some(_) => return Err("unknown fixture mode".into()),
    }
    Ok(())
}
