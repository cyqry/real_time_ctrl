param(
    [ValidateRange(1024, 65535)]
    [int]$HttpPort = 19090,
    [string]$ReportPath = ''
)

# 只验收灰度命名任务缓存。不会构建、发布或操作已有 Kik；所有测试请求固定到本轮新实例。
# 依赖：既有 sshtool 的 ytycc 目标、远端 python3/sha256sum/systemctl，以及项目内 task_fixture.exe。
$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName System.Net.Http
$Root = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
. (Join-Path $PSScriptRoot 'kik_test_lock.ps1')
$RunId = [Guid]::NewGuid().ToString('N')
$RunDir = Join-Path $Root "target/public-task-cache/$RunId"
$TempDir = Join-Path $RunDir 'temp'
$DeployDir = Join-Path $Root 'deploy/gray'
$ArtifactDir = Join-Path $DeployDir 'artifacts'
$SshStateDir = Join-Path $DeployDir 'private/sshtool'
$PrimaryTask = "rtc_cache_$RunId"
$SecondaryTask = "${PrimaryTask}_other"
$TaskNames = @($PrimaryTask, $SecondaryTask)
# 主任务专门覆盖 Unicode、空格和大写扩展名；其他正常任务默认使用各自唯一的最终名称。
$PrimaryFileName = "任务 $RunId.EXE"
$RemoteTasksRoot = '/home/deploy/rust/gray/tasks'
$RemoteLogRoot = '/home/deploy/rust/gray/logs'
$Fixture = Join-Path $Root 'target/debug/examples/task_fixture.exe'
$Utf8 = [Text.UTF8Encoding]::new($false)
$script:Session = $null
$script:ApiToken = $null
$script:HttpClient = $null
$script:KikId = $null
$script:StepName = 'initialization'
$script:RequestNumber = 0
$script:Processes = [Collections.Generic.List[object]]::new()
$script:RemoteOwned = [Collections.Generic.List[string]]::new()
$script:CachePaths = [Collections.Generic.HashSet[string]]::new([StringComparer]::OrdinalIgnoreCase)
$script:AuditCounts = @{}
$script:TaskFileNames = @{}
$StartedUtc = [DateTime]::UtcNow
$Result = [ordered]@{
    schema_version = 1; channel = 'gray'; run_id = $RunId
    started_at = $StartedUtc.ToString('o'); success = $false
    steps = [Collections.Generic.List[object]]::new()
    artifacts = @(); cache_audit = [Collections.Generic.List[object]]::new()
    cleanup = [ordered]@{ local_processes = $false; remote_tasks = $false; ssh_session = $false }
}

function Assert-ProjectPath {
    param([string]$Path, [string]$Parent = $Root)
    $full = [IO.Path]::GetFullPath($Path)
    $prefix = [IO.Path]::GetFullPath($Parent).TrimEnd('\', '/') + [IO.Path]::DirectorySeparatorChar
    if (!$full.StartsWith($prefix, [StringComparison]::OrdinalIgnoreCase)) {
        throw '路径超出当前项目或本轮测试目录'
    }
    # 本轮目录的前缀约束之外，还须检查已有父目录是否是联接；此检查发生在首次创建前。
    return (Assert-KikTestPathInProject $full $Root)
}

function Get-SafeError {
    param([string]$Message)
    # 原生工具失败只返回步骤名/退出码，不复制 stderr；这里额外屏蔽已持有的两种会话秘密。
    foreach ($secret in @($script:ApiToken, $script:Session)) {
        if (![string]::IsNullOrEmpty($secret)) { $Message = $Message.Replace($secret, '[redacted]') }
    }
    return $Message
}

function Invoke-Step {
    param([string]$Name, [scriptblock]$Action)
    $script:StepName = $Name
    $watch = [Diagnostics.Stopwatch]::StartNew()
    $step = [ordered]@{ name = $Name; success = $false; elapsed_ms = 0 }
    try { & $Action; $step.success = $true }
    finally { $step.elapsed_ms = $watch.ElapsedMilliseconds; $Result.steps.Add($step) }
}

function ConvertTo-NativeArgument {
    param([string]$Value)
    # ProcessStartInfo 不经过 shell。兼容 Windows PowerShell 5.1 的 Arguments 属性，
    # 双引号前与参数末尾的反斜杠按 Windows argv 规则成对转义，不能用 JSON 转义代替。
    $escaped = [regex]::Replace($Value, '(\\*)"', '${1}${1}\"')
    $escaped = [regex]::Replace($escaped, '(\\+)$', '${1}${1}')
    return '"' + $escaped + '"'
}

function ConvertTo-PosixArgument {
    param([string]$Value)
    $quote = [string][char]39
    $escape = $quote + [char]34 + $quote + [char]34 + $quote
    return $quote + $Value.Replace($quote, $escape) + $quote
}

function Invoke-SshTool {
    param([string[]]$Arguments, [string]$Operation, [int]$TimeoutSeconds = 90)
    $info = [Diagnostics.ProcessStartInfo]::new()
    $info.FileName = $script:SshToolPath
    $info.WorkingDirectory = $RunDir
    $info.UseShellExecute = $false
    $info.CreateNoWindow = $true
    $info.RedirectStandardOutput = $true
    $info.RedirectStandardError = $true
    $info.StandardOutputEncoding = $Utf8
    $info.StandardErrorEncoding = $Utf8
    $info.Arguments = (@('--state-dir', $SshStateDir, '--quiet') + $Arguments |
        ForEach-Object { ConvertTo-NativeArgument $_ }) -join ' '
    foreach ($variable in @('TEMP', 'TMP', 'TMPDIR')) { $info.EnvironmentVariables[$variable] = $TempDir }
    $process = [Diagnostics.Process]::new()
    $process.StartInfo = $info
    try {
        if (!$process.Start()) { throw "SSH 操作无法启动：$Operation" }
        $stdout = $process.StandardOutput.ReadToEndAsync()
        $stderr = $process.StandardError.ReadToEndAsync()
        if (!$process.WaitForExit($TimeoutSeconds * 1000)) {
            try { $process.Kill($true) } catch { $process.Kill() }
            [void]$process.WaitForExit(5000)
            throw "SSH 操作超时：$Operation"
        }
        if ($process.ExitCode -ne 0) { throw "SSH 操作失败：$Operation，退出码 $($process.ExitCode)" }
        # 不输出命令行、会话、上传进度或 stderr。只有调用者明确需要的数据在内存中返回。
        [void]$stderr.GetAwaiter().GetResult()
        return $stdout.GetAwaiter().GetResult().Trim()
    } finally { $process.Dispose() }
}

function Invoke-Remote {
    param([string]$Command, [string]$Operation)
    if ([string]::IsNullOrWhiteSpace($script:Session)) { throw '尚未建立本轮 SSH 会话' }
    Invoke-SshTool -Arguments @('--session', $script:Session, 'exec', $Command) -Operation $Operation
}

function Get-RemoteTaskDirectory {
    param([string]$Name)
    if ($Name -notin $TaskNames -or $Name -notmatch '^rtc_cache_[0-9a-f]{32}(_other)?$') {
        throw '拒绝访问本轮任务目录白名单之外的路径'
    }
    return "$RemoteTasksRoot/$Name"
}

function Get-RemoteOwnerCheck {
    param([string]$Name)
    $directory = Get-RemoteTaskDirectory $Name
    if ($directory -notin $script:RemoteOwned) { throw '远端任务目录不属于本轮创建记录' }
    $marker = "$directory/.rtc-public-owner"
    # 每次写入前重新验证，不把第一次创建目录成功当成后续路径身份永远不变的保证。
    return ('test ! -L ' + (ConvertTo-PosixArgument $directory) + '; test -d ' + (ConvertTo-PosixArgument $directory) +
        '; test ! -L ' + (ConvertTo-PosixArgument $marker) + '; test -f ' + (ConvertTo-PosixArgument $marker) +
        '; test "$(cat ' + (ConvertTo-PosixArgument $marker) + ')" = ' + (ConvertTo-PosixArgument $RunId))
}

function Send-TaskFile {
    param([string]$Name, [ValidateSet('tool.exe', 'task.toml')][string]$FileName, [string]$Source)
    $sourcePath = Assert-ProjectPath $Source $RunDir
    $directory = Get-RemoteTaskDirectory $Name
    if ($directory -notin $script:RemoteOwned) { throw '远端任务目录不属于本轮创建记录' }
    $pending = "$directory/$FileName.upload"
    $destination = "$directory/$FileName"
    $destinationCheck = 'test ! -L ' + (ConvertTo-PosixArgument $destination) + '; if test -e ' +
        (ConvertTo-PosixArgument $destination) + '; then test -f ' + (ConvertTo-PosixArgument $destination) + '; fi'
    $beforeUpload = 'set -eu; ' + (Get-RemoteOwnerCheck $Name) + '; test ! -L ' + (ConvertTo-PosixArgument $pending) +
        '; test ! -e ' + (ConvertTo-PosixArgument $pending) + '; ' + $destinationCheck
    [void](Invoke-Remote $beforeUpload '上传前核对任务目录归属与非链接路径')
    [void](Invoke-SshTool -Arguments @('--session', $script:Session, 'upload', $sourcePath, $pending) -Operation '上传本轮任务文件')
    $expected = (Get-FileHash -LiteralPath $sourcePath -Algorithm SHA256).Hash.ToLowerInvariant()
    $actual = Invoke-Remote -Command ('sha256sum -- ' + (ConvertTo-PosixArgument $pending)) -Operation '验证远端上传摘要'
    if (($actual -split '\s+')[0] -ne $expected) { throw '远端上传内容 SHA-256 不一致' }
    $command = 'set -eu; ' + (Get-RemoteOwnerCheck $Name) + '; test ! -L ' + (ConvertTo-PosixArgument $pending) +
        '; test -f ' + (ConvertTo-PosixArgument $pending) + '; ' + $destinationCheck +
        '; chmod 0644 -- ' + (ConvertTo-PosixArgument $pending) +
        '; mv -f -- ' + (ConvertTo-PosixArgument $pending) + ' ' + (ConvertTo-PosixArgument $destination)
    [void](Invoke-Remote $command '提交本轮任务文件')
}

function Set-TaskConfiguration {
    param([string]$Name, [string[]]$Arguments, [ValidateSet('sync', 'async')][string]$Mode = 'sync',
        [ValidateSet('output', 'default')][string]$ResponseMode = 'output', [bool]$Enabled = $true,
        [AllowEmptyString()][string]$FileName, [switch]$UseBinaryName,
        [string]$BinaryPath = 'tool.exe', [switch]$OmitArguments,
        [string]$DefaultContent = 'public-task-cache-started')
    if (!$PSBoundParameters.ContainsKey('FileName')) {
        $FileName = if ($Name -eq $PrimaryTask) { $PrimaryFileName } else { "$Name.exe" }
    }
    $argumentText = ($Arguments | ForEach-Object { ConvertTo-Json -InputObject $_ -Compress }) -join ', '
    $text = "enabled = $($Enabled.ToString().ToLowerInvariant())`nbinary = " + (ConvertTo-Json -InputObject $BinaryPath -Compress) + "`nmode = '$Mode'`n"
    if (!$OmitArguments) { $text += "args = [$argumentText]`n" }
    # 缺省名称只在专门用例省略；其余配置更新仍保留同一个最终名称，以验证缓存不被重写。
    if (!$UseBinaryName) { $text += 'file_name = ' + (ConvertTo-Json -InputObject $FileName -Compress) + "`n" }
    if ($Mode -eq 'sync') { $text += "timeout_seconds = 15`n" }
    $text += "[response]`nmode = '$ResponseMode'`ndefault_content = " + (ConvertTo-Json -InputObject $DefaultContent -Compress) + "`n"
    if ($OmitArguments -and ($Mode -ne 'async' -or !$UseBinaryName -or $text -match '(?m)^\s*(args|file_name|timeout_seconds)\s*=')) {
        throw '最小异步配置用例意外包含应省略的字段或使用了其他模式'
    }
    $local = Join-Path $RunDir "$Name.task.toml"
    [IO.File]::WriteAllText($local, $text, $Utf8)
    Send-TaskFile $Name 'task.toml' $local
    $script:TaskFileNames[$Name] = if ($UseBinaryName) { 'tool.exe' } else { $FileName }
}

function Start-OwnedProcess {
    param([string]$Name, [string]$Executable, [string]$WorkingDirectory, [hashtable]$Environment)
    [void](Assert-ProjectPath $WorkingDirectory $RunDir)
    if ($Name -eq 'kik') {
        $startLock = Assert-KikTestLockLocation -ProjectRoot $Root -WorkingDirectory $WorkingDirectory `
            -BinaryPath $Executable -ReceiptPath (Join-Path $ArtifactDir 'ctrl_kik.build-receipt.json')
        if ($startLock.resolved_path -ne $Result.kik_lock.resolved_path) {
            throw '任务缓存预检后 Kik 锁配置发生变化，拒绝继续验收'
        }
    }
    $info = [Diagnostics.ProcessStartInfo]::new()
    $info.FileName = $Executable
    $info.WorkingDirectory = $WorkingDirectory
    $info.UseShellExecute = $false
    $info.CreateNoWindow = $true
    $info.RedirectStandardOutput = $true
    $info.RedirectStandardError = $true
    # 凭据、端点与公钥仍使用已审计 artifact 的编译默认值，只注入本轮本地隔离参数。
    foreach ($key in @($info.EnvironmentVariables.Keys)) {
        if ([string]$key -like 'REAL_CTRL_*' -or [string]$key -like 'RTC_CTRL_KIK_*') {
            $info.EnvironmentVariables.Remove([string]$key)
        }
    }
    foreach ($variable in @('TEMP', 'TMP', 'TMPDIR')) { $info.EnvironmentVariables[$variable] = $TempDir }
    foreach ($item in $Environment.GetEnumerator()) { $info.EnvironmentVariables[$item.Key] = [string]$item.Value }
    $process = [Diagnostics.Process]::new()
    $process.StartInfo = $info
    if (!$process.Start()) { $process.Dispose(); throw "无法启动本轮 $Name" }
    $entry = [pscustomobject]@{
        Name = $Name; Process = $process; StdOut = $process.StandardOutput.ReadToEndAsync(); StdErr = $process.StandardError.ReadToEndAsync()
    }
    $script:Processes.Add($entry)
    return $entry
}

function Invoke-Api {
    param([hashtable]$Command, [switch]$Remote)
    $script:RequestNumber++
    $id = "cache-$RunId-$($script:RequestNumber)"
    $body = @{ version = 1; request_id = $id; command = $Command }
    if ($Remote) {
        if (!$script:KikId) { throw '尚未确认本轮 Kik 的归属' }
        $body.target_kik_id = $script:KikId
    }
    $content = [Net.Http.StringContent]::new(($body | ConvertTo-Json -Depth 8 -Compress), $Utf8, 'application/json')
    $response = $null
    try {
        $response = $script:HttpClient.PostAsync("http://127.0.0.1:$HttpPort/api/v1/commands", $content).GetAwaiter().GetResult()
        if (!$response.IsSuccessStatusCode) { throw "本地 API 请求失败，HTTP $([int]$response.StatusCode)" }
        $reply = $response.Content.ReadAsStringAsync().GetAwaiter().GetResult() | ConvertFrom-Json
        if ($reply.request_id -ne $id) { throw 'API 响应关联 ID 不匹配' }
        return $reply
    } finally { if ($null -ne $response) { $response.Dispose() }; $content.Dispose() }
}

function Run-Task {
    param([string]$Name)
    [void](Get-RemoteTaskDirectory $Name)
    Invoke-Api -Command @{ kind = 'run_task'; task_name = $Name } -Remote
}

function Assert-TaskSuccess {
    param($Reply, [string]$Expected)
    if (!$Reply.ok -or $Reply.data.kind -ne 'info' -or !$Reply.data.message.Contains($Expected)) {
        # 这里只记录本轮受控 fixture 的结构化响应，便于区分业务错误和断言错误。
        $Result.task_failure = Get-SafeError ($Reply | ConvertTo-Json -Depth 8 -Compress)
        throw "任务未返回本步骤预期响应：$($Result.task_failure)"
    }
}

function Get-CachePath {
    param($Reply, [string]$Name = $PrimaryTask)
    Assert-TaskSuccess $Reply 'exe:'
    $match = [regex]::Match($Reply.data.message, '(?m)^exe:(.+)\r?$')
    if (!$match.Success) { throw '测试程序没有返回实际执行路径' }
    $path = $match.Groups[1].Value.TrimEnd("`r")
    if ($path.StartsWith('\\?\')) { $path = $path.Substring(4) }
    $full = Assert-ProjectPath $path $TempDir
    $prefix = [IO.Path]::GetFullPath($TempDir).TrimEnd('\', '/') + [IO.Path]::DirectorySeparatorChar
    $relative = $full.Substring($prefix.Length)
    if ($relative -notmatch '^[0-9a-f]{64}[\\/][^\\/]+$') { throw '任务执行路径不符合临时目录下直接建立部署目录的缓存规则' }
    if (!$script:TaskFileNames.ContainsKey($Name) -or ![string]::Equals([IO.Path]::GetFileName($full), $script:TaskFileNames[$Name], [StringComparison]::OrdinalIgnoreCase)) { throw '任务实际文件名不是配置名称或缺省二进制原名' }
    # 后续损坏注入与清理只允许操作已经由当前测试程序报告、且位于本轮目录的路径。
    [void]$script:CachePaths.Add($full)
    return $full
}

function Assert-CacheHash {
    param([string]$Cache, [string]$Source)
    [void](Assert-ProjectPath $Cache $TempDir)
    if ((Get-FileHash -LiteralPath $Cache).Hash -ne (Get-FileHash -LiteralPath $Source).Hash) {
        throw 'Kik 缓存与本轮服务端源程序的 SHA-256 不一致'
    }
}

function Read-TaskAudit {
    param([string]$Name)
    [void](Get-RemoteTaskDirectory $Name)
    # 仅远端读取最近两个滚动日志；只返回当前 task/Kik 的缓存计数，账号、请求、其他日志不出远端。
    $code = @'
import glob,json,re
paths=sorted(glob.glob("__LOG__/normal-ctrl_server.*.log"))[-2:]
if not paths: raise RuntimeError("missing task audit log")
needle="kik=__KIK__, task=__TASK__,"
rows=[]
for path in paths:
    with open(path,"r",encoding="utf-8",errors="replace") as stream:
        for line in stream:
            if needle not in line or "cache=" not in line: continue
            match=re.search(r"cache=(hit|miss|legacy), (transferred_bytes|expected_bytes)=(\d+)",line)
            if match:
                rows.append({"cache":match.group(1),match.group(2):int(match.group(3))})
                if len(rows)>64: raise RuntimeError("unexpected task audit volume")
print(json.dumps({"rows":rows},separators=(",",":")))
'@
    $code = $code.Replace('__LOG__', $RemoteLogRoot).Replace('__KIK__', $script:KikId).Replace('__TASK__', $Name)
    $json = Invoke-Remote ('PYTHONDONTWRITEBYTECODE=1 python3 -c ' + (ConvertTo-PosixArgument $code)) '读取本轮缓存审计'
    return @((ConvertFrom-Json $json).rows)
}

function Assert-CacheAudit {
    param([string]$Name, [ValidateSet('hit', 'miss')][string]$Cache, [int]$Count = 1, [long]$ExpectedBytes = 0)
    $before = if ($script:AuditCounts.ContainsKey($Name)) { [int]$script:AuditCounts[$Name] } else { 0 }
    $deadline = [DateTime]::UtcNow.AddSeconds(30)
    do {
        $rows = @(Read-TaskAudit $Name)
        if ($rows.Count -ge $before + $Count) { break }
        Start-Sleep -Milliseconds 250
    } while ([DateTime]::UtcNow -lt $deadline)
    if ($rows.Count -ne $before + $Count) { throw '本轮任务的服务端缓存审计条数不符合预期' }
    foreach ($row in @($rows | Select-Object -Skip $before)) {
        if ($row.cache -ne $Cache) { throw '服务端缓存命中/传输判定不符合预期' }
        if ($Cache -eq 'hit' -and $row.transferred_bytes -ne 0) { throw '缓存命中未证明零二进制传输' }
        if ($Cache -eq 'miss' -and $row.expected_bytes -ne $ExpectedBytes) { throw '缓存未命中声明的传输大小不符合源文件' }
        $Result.cache_audit.Add([ordered]@{ step = $script:StepName; task = $Name; cache = $Cache; expected_bytes = $row.expected_bytes; transferred_bytes = $row.transferred_bytes })
    }
    $script:AuditCounts[$Name] = $rows.Count
}

function Stop-OwnedProcesses {
    # 先停本轮 Kik，避免控制断开期间又创建测试子进程；不按名字结束用户的运行实例。
    $entries = @($script:Processes.ToArray())
    [array]::Reverse($entries)
    $errors = [Collections.Generic.List[string]]::new()
    foreach ($entry in $entries) {
        try {
            if (!$entry.Process.HasExited) {
                try { $entry.Process.Kill($true) } catch { $entry.Process.Kill() }
                if (!$entry.Process.WaitForExit(5000)) { throw '本轮测试进程未在清理时退出' }
            }
            # 持续排空过 stdout/stderr，但不落盘业务参数或可能含身份信息的进程输出。
            [void]$entry.StdOut.GetAwaiter().GetResult()
            [void]$entry.StdErr.GetAwaiter().GetResult()
        } catch { $errors.Add((Get-SafeError $_.Exception.Message)) }
        finally { $entry.Process.Dispose() }
    }
    foreach ($process in @(Get-Process -ErrorAction SilentlyContinue)) {
        $ownedPath = $false
        try {
            $path = $process.Path
            if ($path -and $path.StartsWith('\\?\')) { $path = $path.Substring(4) }
            if (!$path -or !$script:CachePaths.Contains($path)) { continue }
            $ownedPath = $true
            [void](Assert-ProjectPath $path $TempDir)
            if ($process.StartTime.ToUniversalTime() -lt $StartedUtc) { throw '缓存路径对应了不属于本轮的运行进程' }
            if (!$process.HasExited) {
                $process.Kill()
                if (!$process.WaitForExit(5000)) { throw '本轮缓存测试子进程未退出' }
            }
        } catch [System.ComponentModel.Win32Exception] {
            # 无权观察无关进程可以跳过；已经匹配本轮缓存路径后的检查/终止失败必须报告。
            if ($ownedPath) { $errors.Add((Get-SafeError $_.Exception.Message)) }
        } catch [InvalidOperationException] {
            # 观察阶段恰好退出可以跳过；归属确认后失败不能把 cleanup 错标为成功。
            if ($ownedPath) { $errors.Add((Get-SafeError $_.Exception.Message)) }
        } catch { $errors.Add((Get-SafeError $_.Exception.Message)) }
        finally { $process.Dispose() }
    }
    if ($errors.Count) { throw ($errors -join '; ') }
}

$failure = $null
try {
    if (!$ReportPath) { $ReportPath = Join-Path $DeployDir "reports/public-task-cache-$RunId.json" }
    elseif (![IO.Path]::IsPathRooted($ReportPath)) { $ReportPath = Join-Path $Root $ReportPath }
    $ReportPath = Assert-ProjectPath $ReportPath
    [IO.Directory]::CreateDirectory((Split-Path -Parent $ReportPath)) | Out-Null
    foreach ($directory in @($RunDir, $TempDir, (Join-Path $RunDir 'http'), (Join-Path $RunDir 'kik'), (Join-Path $RunDir 'proof'))) {
        [void](Assert-ProjectPath $directory)
        [IO.Directory]::CreateDirectory($directory) | Out-Null
    }
    $Result.work_directory = $RunDir

    Invoke-Step 'artifact-and-local-preflight' {
        # 必须先确认锁也留在项目内，之后才允许 SSH、任务上传与被测进程启动。
        $Result.kik_lock = Assert-KikTestLockLocation -ProjectRoot $Root -WorkingDirectory (Join-Path $RunDir 'kik') `
            -BinaryPath (Join-Path $ArtifactDir 'ctrl_kik.exe') -ReceiptPath (Join-Path $ArtifactDir 'ctrl_kik.build-receipt.json')
        $manifest = Get-Content -LiteralPath (Join-Path $DeployDir 'manifest.json') -Raw | ConvertFrom-Json
        if ($manifest.channel -ne 'gray' -or $manifest.kik_noise_port -ne 9005 -or $manifest.control_tls_port -ne 9009) { throw 'manifest 不是目标灰度通道' }
        foreach ($name in @('ctrl_kik.exe', 'ctrl_kik.build-receipt.json', 'real_ctrl_invoker_http_service.exe', 'ctrl_server')) {
            $entries = @($manifest.artifacts | Where-Object name -eq $name)
            $path = Join-Path $ArtifactDir $name
            if ($entries.Count -ne 1 -or !(Test-Path -LiteralPath $path -PathType Leaf)) { throw 'manifest 缺少唯一的所需产物记录' }
            $hash = (Get-FileHash -LiteralPath $path).Hash.ToLowerInvariant()
            if ($hash -ne $entries[0].sha256 -or (Get-Item -LiteralPath $path).Length -ne $entries[0].bytes) { throw '实际 artifact 与 manifest 不一致' }
            $Result.artifacts += [ordered]@{ name = $name; sha256 = $hash; bytes = $entries[0].bytes }
        }
        $receipt = Get-Content -LiteralPath (Join-Path $ArtifactDir 'ctrl_kik.build-receipt.json') -Raw | ConvertFrom-Json
        $kikHash = ($Result.artifacts | Where-Object name -eq 'ctrl_kik.exe').sha256
        if (!$receipt.production_ready -or $receipt.profile -ne 'hardened-protected' -or
            !$receipt.audit.passed -or !$receipt.audit.full_project_plaintext_clean -or !$receipt.audit.protected_scope_plaintext_clean -or
            $receipt.audit.sha256 -ne $kikHash -or $receipt.deployment.port -ne 9005 -or $receipt.deployment.channel -ne 'gray' -or
            $receipt.deployment.kik_transport -ne 'Noise_NK_25519_ChaChaPoly_BLAKE2s') { throw 'Kik protected 回执与当前灰度产物不一致' }
        if (!(Test-Path -LiteralPath $Fixture -PathType Leaf)) { throw '缺少项目内 task_fixture.exe，请先构建测试 example' }
        if (Get-NetTCPConnection -LocalPort $HttpPort -State Listen -ErrorAction SilentlyContinue) { throw '专属 HTTP 验收端口已占用' }
        $script:SshToolPath = (Get-Command sshtool.exe -ErrorAction Stop).Source
        [void](Assert-ProjectPath $SshStateDir)
        [IO.Directory]::CreateDirectory($SshStateDir) | Out-Null
        # 复用发布脚本的 DACL 收紧方式；Set-Acl 写回整个描述符可能额外要求审计权限。
        # 这里只修改项目内 SSH 状态目录的访问权限，不申请 SeSecurityPrivilege 或管理员权限。
        $sid = [Security.Principal.WindowsIdentity]::GetCurrent().User.Value
        & icacls.exe $SshStateDir /inheritance:r /grant:r "*${sid}:(OI)(CI)F" 2>$null | Out-Null
        if ($LASTEXITCODE -ne 0) { throw '收紧项目内 SSH 状态目录访问权限失败' }
        try { $environment = Get-Content -LiteralPath (Join-Path $DeployDir 'real_ctrl.env.json') -Raw | ConvertFrom-Json }
        catch { throw '部署环境 JSON 无法读取' }
        $script:ApiToken = [string]$environment.REAL_CTRL_API_TOKEN
        if (!$script:ApiToken -or $environment.REAL_CTRL_TLS_PORT -ne 9009) { throw '灰度 API 凭据或 TLS 端口配置不完整' }
        $environment = $null
        $script:HttpClient = [Net.Http.HttpClient]::new()
        $script:HttpClient.Timeout = [TimeSpan]::FromSeconds(180)
        $script:HttpClient.DefaultRequestHeaders.Authorization = [Net.Http.Headers.AuthenticationHeaderValue]::new('Bearer', $script:ApiToken)
    }

    Invoke-Step 'gray-server-and-ssh-preflight' {
        $script:Session = Invoke-SshTool -Arguments @('--target', 'ytycc', 'init') -Operation '建立验收 SSH 会话'
        if ([string]::IsNullOrWhiteSpace($script:Session) -or $script:Session.Length -gt 512 -or $script:Session -match '[\r\n]') { throw 'SSH 会话初始化返回无效结果' }
        $status = Invoke-Remote 'systemctl show real-time-ctrl-gray.service -p ActiveState -p SubState -p MainPID -p User' '确认灰度服务状态'
        $fields = @{}
        foreach ($line in ($status -split '\r?\n')) { if ($line -match '^([^=]+)=(.*)$') { $fields[$matches[1]] = $matches[2] } }
        if ($fields.ActiveState -ne 'active' -or $fields.SubState -ne 'running' -or $fields.MainPID -notmatch '^[1-9][0-9]*$' -or !$fields.User -or $fields.User -eq 'root') { throw '灰度服务状态或运行身份不符合要求' }
        $hashText = Invoke-Remote "sha256sum /proc/$($fields.MainPID)/exe" '核对运行中服务端摘要'
        $serverHash = ($hashText -split '\s+')[0]
        if ($serverHash -ne ($Result.artifacts | Where-Object name -eq 'ctrl_server').sha256) { throw '运行中灰度服务端不是本次归档产物' }
        $Result.server = [ordered]@{ pid = [int]$fields.MainPID; user = $fields.User; sha256 = $serverHash }
        foreach ($name in $TaskNames) {
            $directory = Get-RemoteTaskDirectory $name
            [void](Invoke-Remote ('test ! -e ' + (ConvertTo-PosixArgument $directory)) '确认本轮远端目录不存在')
            # 先登记再创建：网络中断使 mkdir 结果未知时，finally 仍会尝试窄范围清理。
            $script:RemoteOwned.Add($directory)
            $create = 'set -eu; mkdir -m 0755 -- ' + (ConvertTo-PosixArgument $directory) +
                "; printf '%s' " + (ConvertTo-PosixArgument $RunId) + ' > ' + (ConvertTo-PosixArgument "$directory/.rtc-public-owner")
            [void](Invoke-Remote $create '创建本轮独立任务目录与归属标记')
        }
    }

    Invoke-Step 'owned-controller-and-kik' {
        $script:HttpProcess = Start-OwnedProcess 'http' (Join-Path $ArtifactDir 'real_ctrl_invoker_http_service.exe') (Join-Path $RunDir 'http') @{
            REAL_CTRL_HTTP_PORT = "$HttpPort"; REAL_CTRL_HTTP_BINDING = '127.0.0.1';
            REAL_CTRL_HTTP_LOCK_PATH = (Join-Path $RunDir 'http/controller.lock'); REAL_CTRL_INSTANCE_ID = "public-cache-$RunId"
        }
        $deadline = [DateTime]::UtcNow.AddSeconds(40)
        do {
            if ($script:HttpProcess.Process.HasExited) { throw '本轮 HTTP 控制端启动失败' }
            try {
                $health = Invoke-RestMethod -Uri "http://127.0.0.1:$HttpPort/api/health" -TimeoutSec 2
                if ($health -eq 'OK') { break }
            } catch {}
            Start-Sleep -Milliseconds 250
        } while ([DateTime]::UtcNow -lt $deadline)
        if ($health -ne 'OK') { throw '本轮 HTTP 控制端健康检查超时' }
        $baseline = Invoke-Api @{ kind = 'sys_list' }
        if (!$baseline.ok) { throw '不能建立灰度在线列表基线' }
        $baselineIds = @($baseline.data.items | ForEach-Object { [string]$_.id })
        $proofName = "owned-$RunId.txt"
        [IO.File]::WriteAllText((Join-Path $RunDir "proof/$proofName"), $RunId, $Utf8)
        $script:KikProcess = Start-OwnedProcess 'kik' (Join-Path $ArtifactDir 'ctrl_kik.exe') (Join-Path $RunDir 'kik') @{}
        $deadline = [DateTime]::UtcNow.AddSeconds(45)
        $new = @()
        do {
            if ($script:KikProcess.Process.HasExited) { throw '本轮 Kik 启动失败' }
            $list = Invoke-Api @{ kind = 'sys_list' }
            if ($list.ok) {
                # 展示名只用于缩小验收候选，不能当授权或机器身份；随后仍须 nonce 目录与实际 TEMP 证明。
                $new = @($list.data.items | Where-Object {
                    $parts = ([string]$_.name) -split '\\'
                    [string]$_.id -notin $baselineIds -and $parts.Count -eq 3 -and
                        [string]::Equals($parts[1], [Environment]::MachineName, [StringComparison]::OrdinalIgnoreCase) -and
                        [string]::Equals($parts[2], [Environment]::UserName, [StringComparison]::OrdinalIgnoreCase)
                })
            }
            if ($new.Count -gt 1) { throw '发现多个新增 Kik，拒绝猜测测试目标' }
            if ($new.Count -eq 1) { break }
            Start-Sleep -Milliseconds 500
        } while ([DateTime]::UtcNow -lt $deadline)
        if ($new.Count -ne 1) { throw '灰度服务未发现唯一的本轮 Kik' }
        $script:KikId = [string]$new[0].id
        if ($script:KikId -notmatch '^[0-9a-fA-F-]{36}$') { throw '新增 Kik 标识格式异常' }
        # nonce 目录只能证明同机可见；还必须读取目标进程的 TEMP，排除同账户旧 Kik 重连。
        $proof = Invoke-Api -Command @{ kind = 'ctrl_ls'; path = (Join-Path $RunDir 'proof') } -Remote
        if (!$proof.ok -or !@($proof.data.entries | Where-Object filename -eq $proofName).Count) { throw '新增 Kik 未通过本地目录归属证明' }
        $processProof = Invoke-Api -Command @{ kind = 'exec'; command = 'echo %TEMP%' } -Remote
        if (!$processProof.ok -or $processProof.data.kind -ne 'info') { throw '新增 Kik 未返回进程临时目录证明' }
        $observedTemp = [IO.Path]::GetFullPath(([string]$processProof.data.message).Trim()).TrimEnd('\', '/')
        $expectedTemp = [IO.Path]::GetFullPath($TempDir).TrimEnd('\', '/')
        if (![string]::Equals($observedTemp, $expectedTemp, [StringComparison]::OrdinalIgnoreCase)) { throw '新增 Kik 的 TEMP 不属于本轮实例，拒绝下发任务' }
        $Result.kik_temp_verified = $true
        $Result.kik_id = $script:KikId
        $Result.kik_name = [string]$new[0].name
        $Result.http_port = $HttpPort
    }

    Invoke-Step 'missing-file-name-preserves-binary-basename' {
        $script:Source = Join-Path $RunDir 'tool.exe'
        Copy-Item -LiteralPath $Fixture -Destination $script:Source
        $Result.fixture = [ordered]@{ sha256 = (Get-FileHash -LiteralPath $script:Source).Hash.ToLowerInvariant(); bytes = (Get-Item -LiteralPath $script:Source).Length }
        Send-TaskFile $PrimaryTask 'tool.exe' $script:Source
        Set-TaskConfiguration $PrimaryTask @('identity', 'default-filename') -UseBinaryName
        $reply = Run-Task $PrimaryTask
        Assert-TaskSuccess $reply 'arg:default-filename'
        $script:DefaultCache = Get-CachePath $reply
        Assert-CacheHash $script:DefaultCache $script:Source
        Assert-CacheAudit $PrimaryTask 'miss' -ExpectedBytes (Get-Item -LiteralPath $script:Source).Length
        $Result.default_cache_path = $script:DefaultCache
    }

    Invoke-Step 'dot-prefixed-binary-paths-reuse-original-basename-cache' {
        # 只改变服务端配置写法；实际程序、最终名称与摘要保持相同，必须命中缓存且没有二进制传输。
        $stamp = [DateTime]::new(2020, 2, 3, 4, 5, 6, [DateTimeKind]::Utc)
        [IO.File]::SetLastWriteTimeUtc($script:DefaultCache, $stamp)
        foreach ($binaryPath in @('./tool.exe', '././tool.exe')) {
            Set-TaskConfiguration $PrimaryTask @('identity', 'dot-path') -UseBinaryName -BinaryPath $binaryPath
            $reply = Run-Task $PrimaryTask
            Assert-TaskSuccess $reply 'arg:dot-path'
            if ((Get-CachePath $reply) -ne $script:DefaultCache -or [IO.File]::GetLastWriteTimeUtc($script:DefaultCache) -ne $stamp) {
                throw '安全的点路径改变了缓存名称或重写了相同二进制'
            }
            Assert-CacheHash $script:DefaultCache $script:Source
            Assert-CacheAudit $PrimaryTask 'hit'
        }
    }

    Invoke-Step 'minimal-dot-path-asynchronous-config-with-omitted-optional-fields' {
        # 使用用户给出的最小字段集合：省略 args/file_name/timeout_seconds，异步只返回已启动响应。
        Set-TaskConfiguration $PrimaryTask -BinaryPath './tool.exe' -Mode async -ResponseMode default `
            -UseBinaryName -OmitArguments -DefaultContent '任务已启动'
        $reply = Run-Task $PrimaryTask
        Assert-TaskSuccess $reply '任务已启动'
        if ($reply.data.message -ne '任务已启动') { throw '最小异步配置没有返回完整且唯一的默认响应' }
        Assert-CacheHash $script:DefaultCache $script:Source
        Assert-CacheAudit $PrimaryTask 'hit'
    }

    Invoke-Step 'configured-unicode-space-file-name-and-hash' {
        Set-TaskConfiguration $PrimaryTask @('identity', 'first-configuration')
        $reply = Run-Task $PrimaryTask
        Assert-TaskSuccess $reply 'arg:first-configuration'
        $script:Cache = Get-CachePath $reply
        if ($script:Cache -eq $script:DefaultCache) { throw '显式 file_name 未改变原名缓存路径' }
        Assert-CacheHash $script:Cache $script:Source
        Assert-CacheAudit $PrimaryTask 'miss' -ExpectedBytes (Get-Item -LiteralPath $script:Source).Length
        $Result.cache_path = $script:Cache
        $script:CacheStamp = [DateTime]::new(2020, 1, 2, 3, 4, 5, [DateTimeKind]::Utc)
        [IO.File]::SetLastWriteTimeUtc($script:Cache, $script:CacheStamp)
        $Result.cache_hit_timestamp_utc = $script:CacheStamp.ToString('o')
    }

    Invoke-Step 'cache-hit-zero-transfer-and-unchanged-file' {
        $reply = Run-Task $PrimaryTask
        if ((Get-CachePath $reply) -ne $script:Cache -or [IO.File]::GetLastWriteTimeUtc($script:Cache) -ne $script:CacheStamp) { throw '缓存命中改变了文件路径或重写了文件' }
        Assert-CacheHash $script:Cache $script:Source
        Assert-CacheAudit $PrimaryTask 'hit'
    }

    Invoke-Step 'configuration-update-uses-cached-binary' {
        Set-TaskConfiguration $PrimaryTask @('identity', 'updated-configuration')
        $reply = Run-Task $PrimaryTask
        Assert-TaskSuccess $reply 'arg:updated-configuration'
        if ((Get-CachePath $reply) -ne $script:Cache -or [IO.File]::GetLastWriteTimeUtc($script:Cache) -ne $script:CacheStamp) { throw '仅配置更新不应重写缓存程序' }
        Assert-CacheAudit $PrimaryTask 'hit'
    }

    Invoke-Step 'binary-update-replaces-same-cache-name' {
        $stream = [IO.File]::Open($script:Source, 'Append', 'Write', 'None')
        try { $stream.WriteByte(71) } finally { $stream.Dispose() }
        Send-TaskFile $PrimaryTask 'tool.exe' $script:Source
        $Result.updated_fixture = [ordered]@{ sha256 = (Get-FileHash -LiteralPath $script:Source).Hash.ToLowerInvariant(); bytes = (Get-Item -LiteralPath $script:Source).Length }
        if ((Get-CachePath (Run-Task $PrimaryTask)) -ne $script:Cache) { throw '更新二进制改变了稳定任务路径' }
        Assert-CacheHash $script:Cache $script:Source
        Assert-CacheAudit $PrimaryTask 'miss' -ExpectedBytes (Get-Item -LiteralPath $script:Source).Length
    }

    Invoke-Step 'same-size-corruption-is-repaired' {
        [void](Assert-ProjectPath $script:Cache $TempDir)
        $length = (Get-Item -LiteralPath $script:Cache).Length
        $stream = [IO.File]::Open($script:Cache, 'Open', 'Write', 'None')
        try { [void]$stream.Seek(-1, [IO.SeekOrigin]::End); $stream.WriteByte(72) } finally { $stream.Dispose() }
        if ((Get-Item -LiteralPath $script:Cache).Length -ne $length) { throw '测试损坏操作意外改变了文件大小' }
        if ((Get-CachePath (Run-Task $PrimaryTask)) -ne $script:Cache) { throw '修复损坏缓存改变了稳定路径' }
        Assert-CacheHash $script:Cache $script:Source
        Assert-CacheAudit $PrimaryTask 'miss' -ExpectedBytes $length
    }

    Invoke-Step 'different-tasks-same-name-and-content-share-zero-transfer-cache' {
        Send-TaskFile $SecondaryTask 'tool.exe' $script:Source
        Set-TaskConfiguration $SecondaryTask @('identity', 'shared-task-configuration') -FileName $PrimaryFileName
        # 更新和损坏修复后重新标记时间，证明跨任务命中没有把同内容文件重新写入。
        [IO.File]::SetLastWriteTimeUtc($script:Cache, $script:CacheStamp)
        $reply = Run-Task $SecondaryTask
        Assert-TaskSuccess $reply 'arg:shared-task-configuration'
        $other = Get-CachePath $reply $SecondaryTask
        if ($other -ne $script:Cache -or [IO.File]::GetLastWriteTimeUtc($other) -ne $script:CacheStamp) { throw '不同任务的同名同内容程序没有复用未重写的缓存文件' }
        Assert-CacheHash $other $script:Source
        Assert-CacheAudit $SecondaryTask 'hit'
    }

    Invoke-Step 'different-final-names-have-independent-cache-files' {
        Set-TaskConfiguration $SecondaryTask @('identity', 'independent-task')
        $other = Get-CachePath (Run-Task $SecondaryTask) $SecondaryTask
        if ($other -eq $script:Cache) { throw '不同最终文件名意外共享了同一缓存文件' }
        Assert-CacheHash $other $script:Source
        Assert-CacheAudit $SecondaryTask 'miss' -ExpectedBytes (Get-Item -LiteralPath $script:Source).Length
    }

    Invoke-Step 'parent-traversal-remains-rejected-after-dot-normalization' {
        # 相邻目录中的源程序确实存在，若错误地折叠 ..，该任务就会成功，不能靠文件缺失蒙混通过。
        $invalidPaths = @("../$PrimaryTask/tool.exe", "./../$PrimaryTask/tool.exe", './tool.exe/..', './tool.exe/', './tool.exe/.', '.', './')
        for ($index = 0; $index -lt $invalidPaths.Count; $index++) {
            Set-TaskConfiguration $SecondaryTask @('identity', 'must-not-execute') -BinaryPath $invalidPaths[$index]
            if ((Run-Task $SecondaryTask).ok) { throw "非法 binary 路径用例 $index 意外执行成功" }
        }
        if (@(Read-TaskAudit $SecondaryTask).Count -ne $script:AuditCounts[$SecondaryTask]) { throw '非法 binary 路径仍进入了 Kik 缓存准备阶段' }
        $reply = Run-Task $PrimaryTask
        Assert-TaskSuccess $reply 'arg:updated-configuration'
        if ((Get-CachePath $reply) -ne $script:Cache) { throw '相邻路径配置错误影响了正常任务的缓存位置' }
        Assert-CacheHash $script:Cache $script:Source
        Assert-CacheAudit $PrimaryTask 'hit'
        Set-TaskConfiguration $SecondaryTask @('identity', 'independent-task')
    }

    Invoke-Step 'dot-normalization-does-not-follow-file-or-directory-symlinks' {
        $directory = Get-RemoteTaskDirectory $SecondaryTask
        $primaryDirectory = Get-RemoteTaskDirectory $PrimaryTask
        $fileLink = "$directory/link.exe"
        $directoryLink = "$directory/linked_dir"
        # 链接仅存在于本轮 UUID 任务中，目标也限定为本轮已上传的文件/目录；不借用生产路径做探针。
        $command = 'set -eu; ' + (Get-RemoteOwnerCheck $SecondaryTask) + '; ' + (Get-RemoteOwnerCheck $PrimaryTask) +
            '; test ! -L ' + (ConvertTo-PosixArgument $fileLink) + '; test ! -e ' + (ConvertTo-PosixArgument $fileLink) +
            '; test ! -L ' + (ConvertTo-PosixArgument $directoryLink) + '; test ! -e ' + (ConvertTo-PosixArgument $directoryLink) +
            '; ln -s -- ' + (ConvertTo-PosixArgument 'tool.exe') + ' ' + (ConvertTo-PosixArgument $fileLink) +
            '; ln -s -- ' + (ConvertTo-PosixArgument $primaryDirectory) + ' ' + (ConvertTo-PosixArgument $directoryLink)
        [void](Invoke-Remote $command '建立本轮受控的文件链接和目录链接负例')
        foreach ($binaryPath in @('./link.exe', './linked_dir/./tool.exe')) {
            Set-TaskConfiguration $SecondaryTask @('identity', 'must-not-execute') -BinaryPath $binaryPath
            if ((Run-Task $SecondaryTask).ok) { throw '点路径归一化意外允许通过符号链接执行程序' }
        }
        if (@(Read-TaskAudit $SecondaryTask).Count -ne $script:AuditCounts[$SecondaryTask]) { throw '符号链接路径仍进入了 Kik 缓存准备阶段' }
        Set-TaskConfiguration $SecondaryTask @('identity', 'independent-task')
        $reply = Run-Task $SecondaryTask
        Assert-TaskSuccess $reply 'arg:independent-task'
        Assert-CacheHash (Get-CachePath $reply $SecondaryTask) $script:Source
        Assert-CacheAudit $SecondaryTask 'hit'
    }

    Invoke-Step 'invalid-file-names-fail-without-affecting-neighbor' {
        # 公网仅用本轮自有任务验证代表性负例；本地验收覆盖更完整的设备名/尾点/尾空格矩阵。
        # 非法配置必须在服务端被拒绝，不能进入数据传输或退回 tool.exe/旧散列名执行。
        $invalidNames = @('../escape.exe', 'CON.exe', 'LONGFI~1.EXE', ('界' * 80 + '.exe'))
        for ($index = 0; $index -lt $invalidNames.Count; $index++) {
            Set-TaskConfiguration $SecondaryTask @('identity', 'must-not-execute') -FileName $invalidNames[$index]
            if ((Run-Task $SecondaryTask).ok) { throw "非法文件名用例 $index 意外执行成功" }
        }
        if (@(Read-TaskAudit $SecondaryTask).Count -ne $script:AuditCounts[$SecondaryTask]) { throw '非法文件名仍进入了 Kik 缓存准备阶段' }
        $reply = Run-Task $PrimaryTask
        Assert-TaskSuccess $reply 'arg:updated-configuration'
        if ((Get-CachePath $reply) -ne $script:Cache) { throw '相邻坏任务影响了有效任务的缓存路径' }
        Assert-CacheHash $script:Cache $script:Source
        Assert-CacheAudit $PrimaryTask 'hit'
        Set-TaskConfiguration $SecondaryTask @('identity', 'independent-task')
    }

    Invoke-Step 'disabled-task-cannot-execute-a-warm-cache' {
        $marker = Join-Path $RunDir 'disabled.pid'
        Set-TaskConfiguration $PrimaryTask @('sleep', '1000', $marker) -Enabled $false
        $reply = Run-Task $PrimaryTask
        if ($reply.ok -or !$reply.error.message.Contains('禁用')) { throw '已禁用任务没有被服务端拒绝' }
        Start-Sleep -Milliseconds 300
        if (Test-Path -LiteralPath $marker) { throw '已禁用任务仍然启动了进程' }
        if (@(Read-TaskAudit $PrimaryTask).Count -ne $script:AuditCounts[$PrimaryTask]) { throw '已禁用任务仍进入了 Kik 缓存准备阶段' }
    }

    Invoke-Step 'cached-synchronous-task-waits-for-completion' {
        Set-TaskConfiguration $PrimaryTask @('sleep', '1200')
        $watch = [Diagnostics.Stopwatch]::StartNew()
        Assert-TaskSuccess (Run-Task $PrimaryTask) 'slept:1200'
        if ($watch.ElapsedMilliseconds -lt 1000) { throw '同步任务在实际完成前返回了响应' }
        Assert-CacheAudit $PrimaryTask 'hit'
    }

    Invoke-Step 'cached-asynchronous-task-returns-before-exit' {
        $marker = Join-Path $RunDir 'async.pid'
        Set-TaskConfiguration $PrimaryTask @('sleep', '4000', $marker) -Mode async -ResponseMode default
        $watch = [Diagnostics.Stopwatch]::StartNew()
        Assert-TaskSuccess (Run-Task $PrimaryTask) 'public-task-cache-started'
        if ($watch.ElapsedMilliseconds -ge 3500 -or (Test-Path -LiteralPath "$marker.done")) { throw '异步启动响应未在任务退出前返回' }
        $Result.async_response_ms = $watch.ElapsedMilliseconds
        $deadline = [DateTime]::UtcNow.AddSeconds(10)
        while (!(Test-Path -LiteralPath "$marker.done")) {
            if ([DateTime]::UtcNow -ge $deadline) { throw '异步测试程序未完成' }
            Start-Sleep -Milliseconds 50
        }
        Assert-CacheAudit $PrimaryTask 'hit'
    }

    Invoke-Step 'four-cached-synchronous-invocations-overlap' {
        Set-TaskConfiguration $PrimaryTask @('sleep', '3000')
        $pending = [Collections.Generic.List[Threading.Tasks.Task[Net.Http.HttpResponseMessage]]]::new()
        $contents = [Collections.Generic.List[Net.Http.StringContent]]::new()
        $watch = [Diagnostics.Stopwatch]::StartNew()
        try {
            for ($index = 0; $index -lt 4; $index++) {
                $body = @{ version = 1; request_id = "cache-parallel-$RunId-$index"; target_kik_id = $script:KikId; command = @{ kind = 'run_task'; task_name = $PrimaryTask } }
                $content = [Net.Http.StringContent]::new(($body | ConvertTo-Json -Depth 6 -Compress), $Utf8, 'application/json')
                $contents.Add($content)
                $pending.Add($script:HttpClient.PostAsync("http://127.0.0.1:$HttpPort/api/v1/commands", $content))
            }
            if (![Threading.Tasks.Task]::WaitAll([Threading.Tasks.Task[]]$pending.ToArray(), 25000)) { throw '四路缓存并发请求超过等待上限' }
            for ($index = 0; $index -lt $pending.Count; $index++) {
                $response = $pending[$index].Result
                try {
                    if (!$response.IsSuccessStatusCode) { throw '缓存并发调用的 HTTP 状态异常' }
                    $reply = $response.Content.ReadAsStringAsync().GetAwaiter().GetResult() | ConvertFrom-Json
                    Assert-TaskSuccess $reply 'slept:3000'
                    if ($reply.request_id -ne "cache-parallel-$RunId-$index") { throw '缓存并发响应关联错误' }
                } finally { $response.Dispose() }
            }
            $Result.same_task_parallel_ms = $watch.ElapsedMilliseconds
            if ($watch.ElapsedMilliseconds -ge 9000) { throw '四路三秒任务耗时未能证明运行期并行' }
        } finally { foreach ($content in $contents) { $content.Dispose() } }
        Assert-CacheAudit $PrimaryTask 'hit' -Count 4
    }
    $Result.success = $true
} catch {
    $failure = $_
    $Result.failed_step = $script:StepName
    $Result.error = Get-SafeError $_.Exception.Message
} finally {
    try { Stop-OwnedProcesses; $Result.cleanup.local_processes = $true }
    catch { $Result.cleanup.local_error = Get-SafeError $_.Exception.Message; $Result.success = $false }
    if ($null -ne $script:HttpClient) { $script:HttpClient.Dispose() }
    $remoteCleanupErrors = [Collections.Generic.List[string]]::new()
    try {
        foreach ($directory in $script:RemoteOwned) {
            try {
            if ($directory -notin @($TaskNames | ForEach-Object { Get-RemoteTaskDirectory $_ })) { throw '拒绝清理未登记的远端目录' }
            # 禁止 rm -r、通配符或扫描 tasks。先核对 owner 标记，再删除确定文件；目录非空则保留。
            # rm -f 只移除登记的链接自身，不跟随 linked_dir 进入目标目录；真实目录被替换到此处时会拒绝删除。
            $files = @('tool.exe', 'tool.exe.upload', 'task.toml', 'task.toml.upload', 'link.exe', 'linked_dir', '.rtc-public-owner') |
                ForEach-Object { ConvertTo-PosixArgument "$directory/$_" }
            $command = 'set -eu; test ! -L ' + (ConvertTo-PosixArgument $directory) + '; if test -d ' + (ConvertTo-PosixArgument $directory) +
                '; then test ! -L ' + (ConvertTo-PosixArgument "$directory/.rtc-public-owner") + '; test "$(cat ' +
                (ConvertTo-PosixArgument "$directory/.rtc-public-owner") + ')" = ' + (ConvertTo-PosixArgument $RunId) +
                '; rm -f -- ' + ($files -join ' ') + '; rmdir -- ' + (ConvertTo-PosixArgument $directory) + '; fi'
            [void](Invoke-Remote $command '清理本轮确定的任务文件')
            } catch { $remoteCleanupErrors.Add((Get-SafeError $_.Exception.Message)) }
        }
        if ($remoteCleanupErrors.Count) { throw ($remoteCleanupErrors -join '; ') }
        $Result.cleanup.remote_tasks = $true
    } catch { $Result.cleanup.remote_error = Get-SafeError $_.Exception.Message; $Result.success = $false }
    if ($script:Session) {
        try { [void](Invoke-SshTool -Arguments @('--session', $script:Session, 'close') -Operation '关闭验收 SSH 会话'); $Result.cleanup.ssh_session = $true }
        catch { $Result.cleanup.ssh_error = Get-SafeError $_.Exception.Message; $Result.success = $false }
    } else { $Result.cleanup.ssh_session = $true }
    $script:ApiToken = $null
    $script:Session = $null
    $Result.completed_at = [DateTime]::UtcNow.ToString('o')
    if ($ReportPath) {
        try {
            $safeReport = Assert-ProjectPath $ReportPath
            [IO.File]::WriteAllText($safeReport, ($Result | ConvertTo-Json -Depth 12), $Utf8)
        } catch { $Result.success = $false; $failure = $_ }
    }
}

if (!$Result.success -or $null -ne $failure) { throw "灰度任务缓存验收失败，步骤：$script:StepName；报告：$ReportPath" }
$Result | ConvertTo-Json -Depth 12
