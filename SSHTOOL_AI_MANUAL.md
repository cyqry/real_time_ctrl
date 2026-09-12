# sshtool AI 命令行操作手册

本手册供自动化 Agent 直接执行。默认 `D:\Code\Rust\mybin` 已在 PATH，
`sshtool.exe` 指向本项目 release。命令可从任意工作目录运行；默认配置和服务状态固定
在 `D:\Code\Rust\ai\sshtool`，不再使用调用者当前目录。

如果使用过程中发现有不符合文档的bug，报告此bug。

## 不可违反的规则

1. 不读取、打印、搜索、复制或解析真实 `config.toml`、其他凭据文件、
   `.sshtool/service.json` 或私钥。
2. 不把密码放入参数、环境变量、stdin、批处理计划、源码、日志或答复。
3. `init` 的 stdout 是临时 session token；只保存在当前进程变量，不写文件、不输出
   到答复。
4. 主机密钥变化时立即停止，交由用户从可信渠道核验；不得自动更新或绕过。
5. 每个独立任务/目标尽量只 `init` 一次，复用 token，最后在 `finally` 中 `close`。
6. 只删除本任务明确创建的远程文件，不能扩大清理范围。

## 目标选择

安全列出服务器别名（不会输出地址、账号、密码、端口或指纹）：

```powershell
sshtool targets
# 稳定的机器可读形式
sshtool targets --json
```

用户指定目标时，原样使用该别名。若用户没指定：

- 配置存在 `default_target` 或仅有一个目标时，`init` 可不传 `--target`；
- 配置有多个目标且无默认值时，工具会拒绝并仅列出可用别名；
- 不应由 Agent 根据主机地址猜测目标，也不能打开配置判断。

目标名只用于新连接：

```powershell
$session = (sshtool --quiet --target live init).Trim()
```

token 已经绑定目标。后续调用只传 `--session`，不得再传 `--target`：

```powershell
sshtool --session $session probe
```

## 单目标最短可靠模板

```powershell
$session = $null
try {
    $session = (sshtool --quiet --target live init).Trim()
    if ($LASTEXITCODE -ne 0 -or [string]::IsNullOrWhiteSpace($session)) {
        throw "sshtool init failed"
    }

    sshtool --session $session probe
    if ($LASTEXITCODE -ne 0) { throw "SSH session is unavailable" }

    sshtool --session $session exec 'uname -a && id'
    if ($LASTEXITCODE -ne 0) { throw "remote command failed" }
}
finally {
    if (-not [string]::IsNullOrWhiteSpace($session)) {
        sshtool --quiet --session $session close
    }
}
```

`--quiet` 只隐藏本地进度和传输统计，不吞远程 stdout/stderr，不改变退出码。

## 多目标并行模板

为每个目标分别 `init`，变量名必须能看出归属。不同 token 可并行；同一 token 的操作
会在服务内串行：

```powershell
$liveSession = (sshtool --quiet --target live init).Trim()
$stageSession = (sshtool --quiet --target staging init).Trim()

try {
    sshtool --session $liveSession exec 'hostname'
    if ($LASTEXITCODE -ne 0) { throw "live command failed" }

    sshtool --session $stageSession exec 'hostname'
    if ($LASTEXITCODE -ne 0) { throw "staging command failed" }
}
finally {
    if ($liveSession) { sshtool --quiet --session $liveSession close }
    if ($stageSession) { sshtool --quiet --session $stageSession close }
}
```

不要用一个目标的 token 执行另一个目标的任务。工具会拒绝
`--target staging --session $liveSession ...`，而不是静默忽略目标。

## 命令速查

### 会话

```powershell
$session = (sshtool --quiet --target live init).Trim()
sshtool --session $session probe
sshtool --session $session close
```

- `init`：按需启动本地服务，选择目标并认证，stdout 仅输出 token。
- `probe`：在复用连接上执行最小远程命令验证可用性。
- `close`：关闭一条 SSH/SFTP 会话并立即使 token 失效。

### 执行

```powershell
sshtool --session $session exec 'systemctl status myapp --no-pager'
```

远程命令必须是一个完整参数。PowerShell 优先使用单引号，避免本地展开 `$变量`、
反引号或子表达式。本地退出码等于远程退出码（Windows 可见范围 0–255）。

文件作为 stdin：

```powershell
sshtool --session $session exec --stdin-file .\script.sh 'bash -s'
```

管道流式 stdin：

```powershell
Get-Content .\payload.json -Raw |
    sshtool --session $session exec --pipe-stdin 'cat > /tmp/payload.json'
```

工具执行非 PTY 命令，不提供交互终端。不要把不会结束的交互输入接到
`--pipe-stdin`。

### 上传和下载

```powershell
sshtool --session $session upload .\artifact.tar.gz /tmp/artifact.tar.gz
sshtool --session $session download /var/log/myapp.log .\downloads\myapp.log
```

- 上传源必须是普通文件；先写远程同目录随机临时文件，校验字节并同步，再发布。
- 基础 SFTP v3 不支持覆盖 rename 时，使用同目录备份/发布/回滚。
- 拒绝覆盖远程符号链接、目录或其他非普通文件。
- 下载先写本地同目录独占临时文件，校验 size、同步并原子替换；失败保留旧目标。
- 若错误提示“新文件已发布但备份清理失败”，不能直接重试；先核验并清理提示的精确
  备份路径。

### 批处理

```powershell
sshtool --session $session batch .\plan.local.json
```

计划是最大 4 MiB 的非空 JSON 数组：

```json
[
  {
    "op": "upload",
    "local": "artifact.tar.gz",
    "remote": "/tmp/artifact.tar.gz"
  },
  {
    "op": "exec",
    "command": "tar -xzf /tmp/artifact.tar.gz -C /opt/myapp",
    "allow_failure": false
  },
  {
    "op": "download",
    "remote": "/var/log/myapp.log",
    "local": "logs/myapp.log"
  }
]
```

本地相对路径以计划文件目录为基准。默认遇到错误或非零远程退出码停止；只有业务上
明确容许失败的检查命令才设置 `"allow_failure": true`。计划不得包含秘密或 token。

## 高频调用策略

- 一个有顺序依赖的流程：一次 `init`、多次操作、一次 `close`。
- 不同目标或真正独立的任务：各自 `init`，使用不同 token 并行。
- 同一 token 并发调用不会提高并行度，服务会串行化；合并为远程 shell 命令或
  `batch` 更高效。
- 大输出直接流式处理，不在 Agent 内存中完整缓存。
- 每个命令都检查 `$LASTEXITCODE`；不能把非零退出码当作“有输出所以成功”。

## 服务状态与恢复

```powershell
sshtool service status
sshtool service start
sshtool service stop
```

行为约定：

- 运行中：输出 `running pid=... endpoint=127.0.0.1:<随机端口>`，退出码 0。
- 未运行：输出 `stopped state_dir=D:\Code\Rust\ai\sshtool\.sshtool`，退出码 3。
- `service stop` 幂等；未运行时也返回成功。
- `init` 会自动启动服务；仅 `service start` 不会建立或恢复 SSH 会话。
- `service stop` 会使所有目标的 token 失效，除非确定没有其他任务，否则只关闭自己
  的 token。

服务或 SSH 断开后的处理：

| 现象 | Agent 行为 |
|---|---|
| token 不存在、关闭或空闲过期 | 丢弃 token，重新对原目标 `init` |
| SSH 连接断开或操作超时 | 不复用旧 token；重新 `init` |
| 本地服务未运行 | 直接重新 `init`，它会自动启动服务 |
| 本地服务状态存在但不可达 | 保留错误，尝试一次 `init`；仍失败则报告用户 |
| 主机密钥不匹配 | 立即停止并交由用户可信核验 |
| 密码认证失败 | 只报告脱敏错误，禁止读取配置排查 |

中断的非幂等远程命令（部署、数据库写入、重启等）结果可能不确定。重新执行前先用
只读命令核验远端状态。上传/下载最终目标采用失败安全发布，但异步中止可能留下带随机
后缀的临时文件，只能清理本任务能够精确确认的路径。

## `--direct` 恢复模式

```powershell
sshtool --direct --target live probe
sshtool --direct --target live exec 'uname -a'
```

`--direct` 每次重新握手认证，只在常驻服务不可用或用户明确要求一次性连接时使用。它
仍由二进制内部读取固定配置，Agent 不得打开配置。

## 任务结束检查

- 已确认使用了正确的目标别名和对应 token。
- 所有命令退出码已检查。
- 关键上传/下载按业务要求校验了哈希或结果。
- 仅清理本任务明确创建的远程临时文件。
- 每个 session 都执行了 `close`。
- 最终答复不含 token、地址、端口、账号、密码、指纹或配置内容。
