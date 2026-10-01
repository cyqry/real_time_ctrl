//! 任务的 Windows 原生进程生命周期。同步先挂起创建，再加入仅用于终止的 Job，最后恢复主线程。
//!
//! 不设置 CPU/内存/进程数量限制。独立句柄列表避免并发启动时把其他任务的管道继承给子进程；
//! 输出用非阻塞 PeekNamedPipe 检查后读取，每轮工作量有界，超时始终由本地单调时钟负责。

use common::{hidden, task::TaskSpec};
use std::{
    ffi::OsStr,
    mem::{size_of, zeroed},
    os::windows::{
        ffi::OsStrExt,
        io::{AsRawHandle, FromRawHandle, OwnedHandle},
    },
    path::Path,
    ptr::{null, null_mut},
    time::Duration,
};
use tokio::time::{sleep, Instant};
use winapi::{
    shared::minwindef::{DWORD, FALSE, TRUE},
    um::{
        fileapi::ReadFile,
        handleapi::SetHandleInformation,
        jobapi2::{
            AssignProcessToJobObject, CreateJobObjectW, QueryInformationJobObject,
            SetInformationJobObject, TerminateJobObject,
        },
        minwinbase::SECURITY_ATTRIBUTES,
        namedpipeapi::{CreatePipe, PeekNamedPipe},
        processthreadsapi::{
            CreateProcessW, DeleteProcThreadAttributeList, GetExitCodeProcess,
            InitializeProcThreadAttributeList, ResumeThread, UpdateProcThreadAttribute,
            PROCESS_INFORMATION,
        },
        synchapi::WaitForSingleObject,
        winbase::{
            CREATE_NO_WINDOW, CREATE_SUSPENDED, EXTENDED_STARTUPINFO_PRESENT, HANDLE_FLAG_INHERIT,
            STARTF_USESTDHANDLES, STARTUPINFOEXW,
        },
        winnt::{
            JobObjectBasicAccountingInformation, JobObjectExtendedLimitInformation, HANDLE,
            JOBOBJECT_BASIC_ACCOUNTING_INFORMATION, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
            JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        },
    },
};

const OUTPUT_LIMIT: usize = 64 * 1024;
const HANDLE_LIST_ATTRIBUTE: usize = 0x0002_0002;

fn raw(handle: &OwnedHandle) -> HANDLE {
    handle.as_raw_handle().cast()
}

fn owned(handle: HANDLE) -> anyhow::Result<OwnedHandle> {
    if handle.is_null() || handle as isize == -1 {
        return Err(std::io::Error::last_os_error().into());
    }
    // SAFETY: 调用者只传入本次成功创建、尚未转移所有权的 Windows 句柄。
    Ok(unsafe { OwnedHandle::from_raw_handle(handle.cast()) })
}

/// Job 关闭时终止所有关联的普通子进程；失败路径也不会遗留挂起的进程。
struct Job(OwnedHandle);
impl Job {
    fn new() -> anyhow::Result<Self> {
        unsafe {
            let job = Self(owned(CreateJobObjectW(null_mut(), null()))?);
            let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = zeroed();
            limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            if SetInformationJobObject(
                raw(&job.0),
                JobObjectExtendedLimitInformation,
                (&mut limits as *mut JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
                size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as DWORD,
            ) == 0
            {
                return Err(std::io::Error::last_os_error().into());
            }
            Ok(job)
        }
    }
    fn terminate(&self) -> anyhow::Result<()> {
        if unsafe { TerminateJobObject(raw(&self.0), 1) } == 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        Ok(())
    }

    /// TerminateJobObject 只提交终止请求；确认 ActiveProcesses 为零后才允许回报已终止。
    async fn terminate_and_wait(&self) -> anyhow::Result<()> {
        self.terminate()?;
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let mut accounting: JOBOBJECT_BASIC_ACCOUNTING_INFORMATION = unsafe { zeroed() };
            if unsafe {
                QueryInformationJobObject(
                    raw(&self.0),
                    JobObjectBasicAccountingInformation,
                    (&mut accounting as *mut JOBOBJECT_BASIC_ACCOUNTING_INFORMATION).cast(),
                    size_of::<JOBOBJECT_BASIC_ACCOUNTING_INFORMATION>() as DWORD,
                    null_mut(),
                )
            } == 0
            {
                return Err(std::io::Error::last_os_error().into());
            }
            if accounting.ActiveProcesses == 0 {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(anyhow::Error::msg(hidden!("任务进程组终止结果未确认")));
            }
            sleep(Duration::from_millis(10)).await;
        }
    }
}

/// 匿名管道的写端只供子进程，读端绝不继承；缓冲区满后仍持续排空。
struct OutputPipe {
    reader: OwnedHandle,
    bytes: Vec<u8>,
    truncated: bool,
    ended: bool,
}
impl OutputPipe {
    fn new() -> anyhow::Result<(Self, OwnedHandle)> {
        unsafe {
            let mut reader = null_mut();
            let mut writer = null_mut();
            let mut attrs = SECURITY_ATTRIBUTES {
                nLength: size_of::<SECURITY_ATTRIBUTES>() as DWORD,
                lpSecurityDescriptor: null_mut(),
                bInheritHandle: TRUE,
            };
            if CreatePipe(&mut reader, &mut writer, &mut attrs, 0) == 0 {
                return Err(std::io::Error::last_os_error().into());
            }
            let reader = owned(reader)?;
            let writer = owned(writer)?;
            if SetHandleInformation(raw(&reader), HANDLE_FLAG_INHERIT, 0) == 0 {
                return Err(std::io::Error::last_os_error().into());
            }
            Ok((
                Self {
                    reader,
                    bytes: vec![],
                    truncated: false,
                    ended: false,
                },
                writer,
            ))
        }
    }

    fn drain(&mut self) -> anyhow::Result<()> {
        // 每次最多读 256 KiB，然后让出运行线程，持续输出不能饿死超时检查。
        for _ in 0..16 {
            if self.ended {
                break;
            }
            let mut available = 0;
            if unsafe {
                PeekNamedPipe(
                    raw(&self.reader),
                    null_mut(),
                    0,
                    null_mut(),
                    &mut available,
                    null_mut(),
                )
            } == 0
            {
                let error = std::io::Error::last_os_error();
                if error.raw_os_error() == Some(109) {
                    self.ended = true;
                    break;
                }
                return Err(error.into());
            }
            if available == 0 {
                break;
            }
            let mut buffer = [0_u8; 16 * 1024];
            let mut read = 0;
            // SAFETY: 只有本执行单元读取此管道；只读取 Peek 已确认存在的字节，不等待新输出。
            if unsafe {
                ReadFile(
                    raw(&self.reader),
                    buffer.as_mut_ptr().cast(),
                    available.min(buffer.len() as u32),
                    &mut read,
                    null_mut(),
                )
            } == 0
            {
                let error = std::io::Error::last_os_error();
                // 最后一个写端可在 Peek 和 Read 之间关闭，这是正常 EOF，不是任务失败。
                if error.raw_os_error() == Some(109) {
                    self.ended = true;
                    break;
                }
                return Err(error.into());
            }
            let keep = (OUTPUT_LIMIT - self.bytes.len()).min(read as usize);
            self.bytes.extend_from_slice(&buffer[..keep]);
            self.truncated |= keep < read as usize;
        }
        Ok(())
    }
}

/// STARTUPINFOEX 属性内存必须按指针对齐，且比 CreateProcess 调用活得更久。
struct Attributes {
    storage: Vec<usize>,
}
impl Attributes {
    fn new(handles: &mut [HANDLE]) -> anyhow::Result<Self> {
        unsafe {
            let mut bytes = 0;
            InitializeProcThreadAttributeList(null_mut(), 1, 0, &mut bytes);
            if bytes == 0 {
                return Err(std::io::Error::last_os_error().into());
            }
            let mut attrs = Self {
                storage: vec![0; bytes.div_ceil(size_of::<usize>())],
            };
            if InitializeProcThreadAttributeList(attrs.pointer(), 1, 0, &mut bytes) == 0 {
                // 未初始化的列表不能调用 Delete；释放普通内存即可。
                attrs.storage.clear();
                return Err(std::io::Error::last_os_error().into());
            }
            if UpdateProcThreadAttribute(
                attrs.pointer(),
                0,
                HANDLE_LIST_ATTRIBUTE,
                handles.as_mut_ptr().cast(),
                std::mem::size_of_val(handles),
                null_mut(),
                null_mut(),
            ) == 0
            {
                return Err(std::io::Error::last_os_error().into());
            }
            Ok(attrs)
        }
    }
    fn pointer(&mut self) -> winapi::um::processthreadsapi::LPPROC_THREAD_ATTRIBUTE_LIST {
        self.storage.as_mut_ptr().cast()
    }
}
impl Drop for Attributes {
    fn drop(&mut self) {
        if !self.storage.is_empty() {
            unsafe {
                DeleteProcThreadAttributeList(self.pointer());
            }
        }
    }
}

/// Windows CRT 的参数引用规则；不是 shell 转义，&、| 等保持普通参数字符。
fn quote_argument(arg: &OsStr) -> Vec<u16> {
    let mut result = vec![34];
    let mut slashes = 0;
    for unit in arg.encode_wide() {
        if unit == 92 {
            slashes += 1;
            continue;
        }
        result.extend(std::iter::repeat_n(
            92,
            if unit == 34 { slashes * 2 + 1 } else { slashes },
        ));
        slashes = 0;
        result.push(unit);
    }
    result.extend(std::iter::repeat_n(92, slashes * 2));
    result.push(34);
    result
}

pub struct ProcessOutput {
    pub code: u32,
    pub stdout: String,
    pub stderr: String,
    pub truncated: bool,
}

/// 一次原生创建获得的句柄集合；异步没有 Job，未捕获输出时也不创建读取管道。
struct StartedProcess {
    job: Option<Job>,
    process: OwnedHandle,
    stdout: Option<OutputPipe>,
    stderr: Option<OutputPipe>,
}

// 原始 Windows 指针只在这个同步创建函数中使用，不跨越 await；返回值全部拥有句柄所有权。
fn start(path: &Path, spec: &TaskSpec, synchronous: bool) -> anyhow::Result<StartedProcess> {
    if !path
        .extension()
        .and_then(OsStr::to_str)
        .is_some_and(|extension| extension.eq_ignore_ascii_case(&hidden!("exe")))
    {
        return Err(anyhow::Error::msg(hidden!("任务程序只支持原生 .exe 文件")));
    }
    // Rust 文件操作可访问超过 MAX_PATH 的缓存，直接把普通 DOS 路径交给 CreateProcessW
    // 却仍可能得到 ERROR_PATH_NOT_FOUND。canonicalize 在 Windows 返回扩展长度绝对路径，
    // 仅用于明确的 lpApplicationName；argv[0]、参数引用和 TEMP 工作目录保持既有语义。
    // 调用者的已验证文件/目录句柄和准备锁仍持有到创建结束，转换期间不释放镜像身份保护。
    let application_path = std::fs::canonicalize(path)?;
    let job = if synchronous { Some(Job::new()?) } else { None };
    let null_file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(hidden!("NUL"))?;
    let null_handle = null_file.as_raw_handle() as HANDLE;
    if unsafe { SetHandleInformation(null_handle, HANDLE_FLAG_INHERIT, HANDLE_FLAG_INHERIT) } == 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    let (stdout, out_write) = if spec.output {
        let (r, w) = OutputPipe::new()?;
        (Some(r), Some(w))
    } else {
        (None, None)
    };
    let (stderr, err_write) = if spec.output {
        let (r, w) = OutputPipe::new()?;
        (Some(r), Some(w))
    } else {
        (None, None)
    };
    let mut handles = vec![null_handle];
    if let Some(h) = &out_write {
        handles.push(raw(h));
    }
    if let Some(h) = &err_write {
        handles.push(raw(h));
    }
    let mut attrs = Attributes::new(&mut handles)?;
    let mut command_line = quote_argument(path.as_os_str());
    for arg in &spec.args {
        command_line.push(32);
        command_line.extend(quote_argument(OsStr::new(arg)));
    }
    command_line.push(0);
    if command_line.len() > 32767 {
        return Err(anyhow::Error::msg(hidden!(
            "任务参数超过 Windows 命令行上限"
        )));
    }
    let application: Vec<u16> = application_path
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect();
    let directory: Vec<u16> = std::env::temp_dir()
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect();
    let (process, thread) = unsafe {
        let mut startup: STARTUPINFOEXW = zeroed();
        startup.StartupInfo.cb = size_of::<STARTUPINFOEXW>() as DWORD;
        startup.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
        startup.StartupInfo.hStdInput = null_handle;
        startup.StartupInfo.hStdOutput = out_write.as_ref().map(raw).unwrap_or(null_handle);
        startup.StartupInfo.hStdError = err_write.as_ref().map(raw).unwrap_or(null_handle);
        startup.lpAttributeList = attrs.pointer();
        let mut info: PROCESS_INFORMATION = zeroed();
        if CreateProcessW(
            application.as_ptr(),
            command_line.as_mut_ptr(),
            null_mut(),
            null_mut(),
            TRUE,
            CREATE_SUSPENDED | CREATE_NO_WINDOW | EXTENDED_STARTUPINFO_PRESENT,
            null_mut(),
            directory.as_ptr(),
            &mut startup.StartupInfo,
            &mut info,
        ) == 0
        {
            return Err(std::io::Error::last_os_error().into());
        }
        (owned(info.hProcess)?, owned(info.hThread)?)
    };
    // 本进程不保留写端，否则读端永远不能观察到 EOF。
    drop((out_write, err_write, null_file, attrs));
    unsafe {
        if let Some(job) = &job {
            if AssignProcessToJobObject(raw(&job.0), raw(&process)) == 0 {
                let error = std::io::Error::last_os_error();
                winapi::um::processthreadsapi::TerminateProcess(raw(&process), 1);
                return Err(error.into());
            }
        }
        if ResumeThread(raw(&thread)) == u32::MAX {
            let error = std::io::Error::last_os_error();
            winapi::um::processthreadsapi::TerminateProcess(raw(&process), 1);
            return Err(error.into());
        }
    }
    drop(thread);
    Ok(StartedProcess {
        job,
        process,
        stdout,
        stderr,
    })
}

/// 异步仅确认原生程序创建并恢复执行；关闭本地观察句柄不会终止进程，也不登记后台账本。
/// 明确提供 lpApplicationName；入口同时限定 EXE 后缀，防止 Windows 启动脚本解释器。
pub fn start_async(path: &Path, spec: &TaskSpec) -> anyhow::Result<()> {
    let _started = start(path, spec, false)?;
    Ok(())
}

/// guard 固定已校验的缓存文件和准备锁；只在进程成功创建后释放，不把同步运行期串行化。
pub async fn run_with_start_guard(
    path: &Path,
    spec: &TaskSpec,
    guard: impl Send,
) -> anyhow::Result<ProcessOutput> {
    let StartedProcess {
        job,
        process,
        mut stdout,
        mut stderr,
    } = start(path, spec, true)?;
    let job = job.ok_or_else(|| anyhow::Error::msg(hidden!("任务进程生命周期不可用")))?;
    drop(guard);
    let deadline = Instant::now() + Duration::from_secs(u64::from(spec.timeout_seconds));
    // 无论是正常退出、超时还是管道/等待失败，都走同一个收尾入口。
    // Job 的 kill-on-close 是异常取消时的兜底，正常错误路径必须等待进程组结束。
    let outcome = wait_for_exit(&process, &mut stdout, &mut stderr, deadline).await;
    if let Err(error) = job.terminate_and_wait().await {
        return Err(anyhow::Error::msg(hidden!("任务终止失败或未确认: ", error)));
    }
    let code = outcome?;
    finish_output(&mut stdout, &mut stderr).await?;
    let truncated = stdout.as_ref().is_some_and(|p| p.truncated || !p.ended)
        || stderr.as_ref().is_some_and(|p| p.truncated || !p.ended);
    Ok(ProcessOutput {
        code,
        stdout: output_text(stdout),
        stderr: output_text(stderr),
        truncated,
    })
}

async fn wait_for_exit(
    process: &OwnedHandle,
    stdout: &mut Option<OutputPipe>,
    stderr: &mut Option<OutputPipe>,
    deadline: Instant,
) -> anyhow::Result<u32> {
    loop {
        if let Some(pipe) = stdout {
            pipe.drain()?;
        }
        if let Some(pipe) = stderr {
            pipe.drain()?;
        }
        match unsafe { WaitForSingleObject(raw(process), 0) } {
            0 => break,
            258 => {}
            _ => return Err(std::io::Error::last_os_error().into()),
        }
        if Instant::now() >= deadline {
            // 外层收到此错误后仍要先等待 Job 内所有进程退出，才会把错误发往服务端。
            return Err(anyhow::Error::msg(hidden!(
                "task_timeout: 同步任务超时，已终止"
            )));
        }
        sleep(Duration::from_millis(10)).await;
    }
    let mut code = 0;
    if unsafe { GetExitCodeProcess(raw(process), &mut code) } == FALSE {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(code)
}

async fn finish_output(
    stdout: &mut Option<OutputPipe>,
    stderr: &mut Option<OutputPipe>,
) -> anyhow::Result<()> {
    let finish = Instant::now() + Duration::from_secs(2);
    loop {
        if let Some(pipe) = stdout {
            pipe.drain()?;
        }
        if let Some(pipe) = stderr {
            pipe.drain()?;
        }
        if stdout.as_ref().is_none_or(|p| p.ended) && stderr.as_ref().is_none_or(|p| p.ended) {
            break;
        }
        if Instant::now() >= finish {
            break;
        }
        sleep(Duration::from_millis(10)).await;
    }
    Ok(())
}

fn output_text(pipe: Option<OutputPipe>) -> String {
    let Some(pipe) = pipe else {
        return String::new();
    };
    match String::from_utf8(pipe.bytes) {
        Ok(s) => s,
        // 上限可能刚好切在 UTF-8 字符中间；只有尾部不完整时保留前面的 UTF-8，避免整段误解码。
        Err(e) if e.utf8_error().error_len().is_none() => {
            String::from_utf8_lossy(e.as_bytes()).into_owned()
        }
        Err(e) => encoding_rs::GBK.decode(e.as_bytes()).0.into_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::task_cache::{
        testing::{child_spec, publish, Root},
        CacheEntry, VerifiedProgram,
    };

    /// 基目录由 Root::new 固定在仓库 target；嵌套 Root 保留普通 DOS 路径输入，
    /// 不能预先加扩展前缀掩盖原生启动入口的缺陷。外层 Root 回收本轮完整 UUID 树。
    async fn long_cached_child() -> (Root, Root, CacheEntry, VerifiedProgram) {
        let owner = Root::new();
        // 即使仓库位于很短的路径，也让最终镜像路径超过 300 个 UTF-16 单元。
        let root = Root(owner.0.join("long-cache-image with 空格".repeat(4)));
        std::fs::create_dir_all(&root.0).unwrap();
        let entry = root
            .acquire(format!(
                "{}.exe",
                "long-path-named-task-image-regression-".repeat(2)
            ))
            .await
            .unwrap();
        let bytes = std::fs::read(std::env::current_exe().unwrap()).unwrap();
        let program = publish(&entry, &bytes).await;
        assert!(program.path().as_os_str().encode_wide().count() > 300);
        assert!(
            !matches!(program.path().components().next(), Some(std::path::Component::Prefix(prefix)) if prefix.kind().is_verbatim())
        );
        (owner, root, entry, program)
    }

    #[tokio::test]
    async fn synchronous_cached_image_over_max_path_starts_with_verified_file_guard() {
        let (_owner, _root, entry, program) = long_cached_child().await;
        let path = program.path().to_path_buf();
        let expected = std::fs::canonicalize(&path).unwrap();
        let output = run_with_start_guard(&path, &child_spec(), (program, entry))
            .await
            .unwrap();
        assert_eq!(output.code, 0);
        let actual = output
            .stdout
            .lines()
            .find_map(|line| line.strip_prefix("fixture-image="))
            .expect("long-cache child did not report its actual native image");
        assert_eq!(std::fs::canonicalize(actual).unwrap(), expected);
        assert!(!output.truncated);
    }

    /// 测试观察句柄不改变异步的无 Job 语义；断言失败也只终止本轮直接创建的原生进程，
    /// 不能按进程名扫描或误杀其他实例。完成后再释放缓存目录，避免残留测试镜像。
    struct OwnedAsyncTestProcess(StartedProcess);
    impl Drop for OwnedAsyncTestProcess {
        fn drop(&mut self) {
            unsafe {
                if WaitForSingleObject(raw(&self.0.process), 0) != 0 {
                    winapi::um::processthreadsapi::TerminateProcess(raw(&self.0.process), 1);
                    WaitForSingleObject(raw(&self.0.process), 5000);
                }
            }
        }
    }

    #[tokio::test]
    async fn asynchronous_cached_image_over_max_path_starts_without_a_job() {
        let (_owner, _root, entry, program) = long_cached_child().await;
        let mut spec = child_spec();
        spec.asynchronous = true;
        spec.output = false;
        spec.timeout_seconds = 0;
        assert!(spec.valid());
        // 与 start_async 共用同一入口和参数；仅在测试保留观察句柄，验证子程序真实执行成功。
        let mut started = OwnedAsyncTestProcess(start(program.path(), &spec, false).unwrap());
        assert!(started.0.job.is_none());
        drop((program, entry));
        let StartedProcess {
            process,
            stdout,
            stderr,
            ..
        } = &mut started.0;
        let code = wait_for_exit(
            process,
            stdout,
            stderr,
            Instant::now() + Duration::from_secs(10),
        )
        .await
        .unwrap();
        assert_eq!(code, 0);
    }

    #[test]
    fn windows_argument_quoting_preserves_slashes_quotes_and_empty() {
        for (arg, expected) in [
            ("", "\"\""),
            ("a b", "\"a b\""),
            ("a\\", "\"a\\\\\""),
            ("a\"b", "\"a\\\"b\""),
        ] {
            assert_eq!(
                String::from_utf16(&quote_argument(OsStr::new(arg))).unwrap(),
                expected
            );
        }
    }

    #[tokio::test]
    async fn output_pipe_drains_beyond_retained_limit_and_observes_eof() {
        use std::io::Write;
        let (mut reader, writer) = OutputPipe::new().unwrap();
        // 单个管道容量有限，写入方必须并行。总输出大于保留上限，验证截断后仍持续排空。
        let writer = tokio::task::spawn_blocking(move || {
            let mut writer = std::fs::File::from(writer);
            for _ in 0..64 {
                writer.write_all(&[b'X'; 8192]).unwrap();
            }
        });
        let deadline = Instant::now() + Duration::from_secs(10);
        while !reader.ended {
            reader.drain().unwrap();
            assert!(
                Instant::now() < deadline,
                "pipe did not finish after writer exit"
            );
            tokio::task::yield_now().await;
        }
        writer.await.unwrap();
        assert_eq!(reader.bytes, vec![b'X'; OUTPUT_LIMIT]);
        assert!(reader.truncated);
    }

    #[test]
    fn truncated_utf8_tail_does_not_redecode_the_whole_output_as_gbk() {
        let (mut pipe, writer) = OutputPipe::new().unwrap();
        drop(writer);
        pipe.bytes = "正常输出".as_bytes().to_vec();
        pipe.bytes.extend_from_slice(&[0xe4, 0xb8]);
        let text = output_text(Some(pipe));
        assert_eq!(text, "正常输出\u{fffd}");
    }
}
