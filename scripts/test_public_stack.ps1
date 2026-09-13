param(
    [ValidateSet("Gray", "Production")]
    [string]$Channel = "Gray",
    [int]$HttpPort = 9000,
    [int]$BigFileMiB = 32
)

$ErrorActionPreference = "Stop"
$ScriptDir = Split-Path -Parent $MyInvocation.MyCommand.Path
$Root = (Resolve-Path (Join-Path $ScriptDir "..")).Path
$ChannelName = $Channel.ToLowerInvariant()
$TargetAlias = "ytycc"
$ExpectedKikNoisePort = if ($Channel -eq "Gray") { 9005 } else { 9002 }
$ExpectedControlTlsPort = if ($Channel -eq "Gray") { 9009 } else { 9007 }
$RemoteDir = if ($Channel -eq "Gray") { "/home/deploy/rust/gray" } else { "/home/deploy/rust/ctrl_server" }
$DeployDir = Join-Path $Root "target\deploy\$ChannelName"
$ArtifactDir = Join-Path $DeployDir "artifacts"
$ReportDir = Join-Path $DeployDir "reports"
$RunDir = Join-Path $DeployDir "public-e2e"
$LogDir = Join-Path $RunDir "logs"
$EnvPath = Join-Path $DeployDir "real_ctrl.env.json"
$KikExe = Join-Path $ArtifactDir "ctrl_kik.exe"
$KikReceipt = Join-Path $ArtifactDir "ctrl_kik.build-receipt.json"
$RealCtrlExe = Join-Path $ArtifactDir "real_ctrl.exe"
$HttpExe = Join-Path $ArtifactDir "real_ctrl_invoker_http_service.exe"
$ReportPath = Join-Path $ReportDir "public-e2e.json"
$script:Processes = @()

function Test-PortFree {
    param([int]$Port)
    if (Get-NetTCPConnection -LocalPort $Port -State Listen -ErrorAction SilentlyContinue) {
        throw "本机 HTTP 验收端口 $Port 已被占用"
    }
}

function Wait-TcpPort {
    param([string]$HostName, [int]$Port, [int]$TimeoutSeconds)
    $deadline = (Get-Date).AddSeconds($TimeoutSeconds)
    while ((Get-Date) -lt $deadline) {
        $client = [Net.Sockets.TcpClient]::new()
        try {
            $task = $client.ConnectAsync($HostName, $Port)
            if ($task.Wait(500) -and $client.Connected) { return }
        } catch {
        } finally {
            $client.Dispose()
        }
        Start-Sleep -Milliseconds 250
    }
    throw "等待 ${HostName}:$Port 超时"
}

function Start-TestProcess {
    param([string]$Name, [string]$Path, [string]$WorkingDirectory, [hashtable]$Environment)
    if (-not (Test-Path -LiteralPath $Path)) { throw "缺少被测程序: $Path" }
    $info = [Diagnostics.ProcessStartInfo]::new()
    $info.FileName = $Path
    $info.WorkingDirectory = $WorkingDirectory
    $info.UseShellExecute = $false
    $info.CreateNoWindow = $true
    $info.RedirectStandardOutput = $true
    $info.RedirectStandardError = $true
    if ($Name -like "real-ctrl*" -or $Name -like "wrong-*") {
        # 验收进程必须从发布 EXE 的编译默认值启动；先移除调用发布脚本时可能继承的运行覆盖。
        foreach ($variable in @(
            "REAL_CTRL_SERVER_HOST",
            "REAL_CTRL_TLS_PORT",
            "REAL_CTRL_TLS_SERVER_NAME",
            "REAL_CTRL_TLS_CA_CERT",
            "REAL_CTRL_TLS_SERVER_SPKI_SHA256",
            "REAL_CTRL_AUTH_SECRET",
            "REAL_CTRL_API_TOKEN",
            "REAL_CTRL_API_ALLOW_EXEC",
            "REAL_CTRL_HTTP_LOCK_PATH",
            "REAL_CTRL_HTTP_BINDING",
            "REAL_CTRL_HTTP_PORT",
            "REAL_CTRL_ACCOUNT_ID",
            "REAL_CTRL_INSTANCE_ID"
        )) {
            [void]$info.Environment.Remove($variable)
        }
    }
    foreach ($item in $Environment.GetEnumerator()) {
        $info.Environment[$item.Key] = [string]$item.Value
    }
    $process = [Diagnostics.Process]::new()
    $process.StartInfo = $info
    if (-not $process.Start()) { throw "启动 $Name 失败" }
    $entry = [pscustomobject]@{
        Name = $Name
        Process = $process
        StdOutTask = $process.StandardOutput.ReadToEndAsync()
        StdErrTask = $process.StandardError.ReadToEndAsync()
    }
    $script:Processes += $entry
    $entry
}

function Stop-TestProcesses {
    $entries = @($script:Processes)
    [array]::Reverse($entries)
    foreach ($entry in $entries) {
        try {
            if (-not $entry.Process.HasExited) {
                try { $entry.Process.Kill($true) } catch { $entry.Process.Kill() }
                $entry.Process.WaitForExit(5000) | Out-Null
            }
        } finally {
            [IO.File]::WriteAllText(
                (Join-Path $LogDir "$($entry.Name).stdout.log"),
                $entry.StdOutTask.Result,
                [Text.UTF8Encoding]::new($false)
            )
            [IO.File]::WriteAllText(
                (Join-Path $LogDir "$($entry.Name).stderr.log"),
                $entry.StdErrTask.Result,
                [Text.UTF8Encoding]::new($false)
            )
            $entry.Process.Dispose()
        }
    }
}

function Remove-TestPayloadFiles {
    if (-not (Test-Path -LiteralPath $RunDir)) { return }
    $fullRunDir = [IO.Path]::GetFullPath($RunDir)
    $deployPrefix = [IO.Path]::GetFullPath($DeployDir).TrimEnd('\', '/') + [IO.Path]::DirectorySeparatorChar
    if (-not $fullRunDir.StartsWith($deployPrefix, [StringComparison]::OrdinalIgnoreCase)) {
        throw "拒绝清理部署目录外的验收文件: $fullRunDir"
    }
    # 日志是验收证据，根目录中的上传、下载和 lock 文件只是可再生负载，不应长期占用空间。
    Get-ChildItem -LiteralPath $fullRunDir -File -Force -ErrorAction SilentlyContinue |
        Remove-Item -Force -ErrorAction SilentlyContinue
    # 直启验收刻意不注入 REAL_CTRL_HTTP_LOCK_PATH，因此清理构建默认值在系统临时目录产生的锁文件。
    # 这里只删除当前通道的固定文件名，不扫描或递归清理系统临时目录。
    $directRunLock = Join-Path ([IO.Path]::GetTempPath()) "real_ctrl-http-$ChannelName.lock"
    [IO.File]::Delete($directRunLock)
}

function Invoke-ApiCommand {
    param([hashtable]$Command, [string]$RequestId, [string]$Token)
    $body = @{
        version = 1
        request_id = $RequestId
        command = $Command
    } | ConvertTo-Json -Depth 12 -Compress
    Invoke-RestMethod -Method Post -Uri "http://127.0.0.1:$HttpPort/api/v1/commands" `
        -Headers @{ Authorization = "Bearer $Token" } -ContentType "application/json" -Body $body
}

function Convert-Environment {
    param($Object)
    $map = @{}
    foreach ($property in $Object.PSObject.Properties) {
        $map[$property.Name] = [string]$property.Value
    }
    $map
}

function Fill-RandomBytes {
    param([byte[]]$Bytes)
    # RandomNumberGenerator.Fill 仅存在于较新的 .NET；Create/GetBytes 同时兼容 PS 5.1 与 7。
    $rng = [Security.Cryptography.RandomNumberGenerator]::Create()
    try {
        $rng.GetBytes($Bytes)
    } finally {
        $rng.Dispose()
    }
}

function Test-WrongPinRejected {
    $wrong = @{}
    $wrong["REAL_CTRL_TLS_SERVER_SPKI_SHA256"] = "0" * 64
    $wrong["REAL_CTRL_HTTP_LOCK_PATH"] = Join-Path $RunDir "wrong-pin.lock"
    $probe = Start-TestProcess -Name "wrong-pin-probe" -Path $RealCtrlExe -WorkingDirectory $Root -Environment $wrong
    if (-not $probe.Process.WaitForExit(15000)) {
        throw "错误 SPKI pin 未在 15 秒内拒绝"
    }
    if ($probe.Process.ExitCode -eq 0) {
        throw "错误 SPKI pin 被意外接受"
    }
}

function Test-ControlRejectedOnKikPort {
    $isolated = @{}
    $isolated["REAL_CTRL_TLS_PORT"] = "$ExpectedKikNoisePort"
    $isolated["REAL_CTRL_HTTP_LOCK_PATH"] = Join-Path $RunDir "wrong-role-port.lock"
    $probe = Start-TestProcess -Name "wrong-role-port-probe" -Path $RealCtrlExe -WorkingDirectory $Root -Environment $isolated
    if (-not $probe.Process.WaitForExit(15000)) {
        throw "Kik Noise 端口未在 15 秒内拒绝 real_ctrl TLS"
    }
    if ($probe.Process.ExitCode -eq 0) {
        throw "Kik Noise 端口意外接受 real_ctrl TLS"
    }
}

function Measure-NetworkBaseline {
    $source = Join-Path $ReportDir "sftp-source-8m.bin"
    $download = Join-Path $ReportDir "sftp-download-8m.bin"
    $bytes = New-Object byte[] (8 * 1024 * 1024)
    Fill-RandomBytes $bytes
    [IO.File]::WriteAllBytes($source, $bytes)
    $remote = "$RemoteDir/sftp-public-e2e.bin"
    $session = $null
    try {
        $session = (sshtool --quiet --target $TargetAlias init).Trim()
        if ($LASTEXITCODE -ne 0 -or [string]::IsNullOrWhiteSpace($session)) {
            throw "SFTP 基线会话初始化失败"
        }
        $upload = [Diagnostics.Stopwatch]::StartNew()
        sshtool --quiet --session $session upload $source $remote
        if ($LASTEXITCODE -ne 0) { throw "SFTP 基线上传失败" }
        $upload.Stop()
        $downloadWatch = [Diagnostics.Stopwatch]::StartNew()
        sshtool --quiet --session $session download $remote $download
        if ($LASTEXITCODE -ne 0) { throw "SFTP 基线下载失败" }
        $downloadWatch.Stop()
        # 整条远端命令必须作为一个参数传给 sshtool；使用单引号模板避免 PowerShell
        # 提前展开远端的 $pid 与命令替换表达式。
        # sshtool 会经过一层本机参数解析；不要在传入的远端命令中使用正则竖线，
        # 否则 Windows PowerShell 5.1 可能在去引号后让 Bash 把后半段误判为管道命令。
        $resourceCommand = 'set -eu; rm -f "__REMOTE__"; pid=$(systemctl show -p MainPID real-time-ctrl-__CHANNEL__.service); pid=${pid#MainPID=}; test "$pid" -gt 0; grep -e "^VmRSS:" -e "^Threads:" "/proc/$pid/status"'
        $resourceCommand = $resourceCommand.Replace('__REMOTE__', $remote).Replace('__CHANNEL__', $ChannelName)
        $resource = sshtool --quiet --session $session exec $resourceCommand
        if ($LASTEXITCODE -ne 0) { throw "服务端资源快照失败" }
        if ((Get-FileHash $source).Hash -ne (Get-FileHash $download).Hash) {
            throw "SFTP 基线摘要不一致"
        }
        [ordered]@{
            size_mib = 8
            upload_mib_per_second = [Math]::Round(8 / [Math]::Max($upload.Elapsed.TotalSeconds, 0.001), 2)
            download_mib_per_second = [Math]::Round(8 / [Math]::Max($downloadWatch.Elapsed.TotalSeconds, 0.001), 2)
            server_resource = @($resource)
        }
    } finally {
        if ($session) {
            sshtool --quiet --session $session exec "rm -f '$remote'" | Out-Null
            sshtool --quiet --session $session close | Out-Null
        }
        Remove-Item -LiteralPath $source, $download -Force -ErrorAction SilentlyContinue
    }
}

[IO.Directory]::CreateDirectory($ReportDir) | Out-Null
[IO.Directory]::CreateDirectory($RunDir) | Out-Null
[IO.Directory]::CreateDirectory($LogDir) | Out-Null
Remove-TestPayloadFiles
if (-not (Test-Path -LiteralPath $EnvPath)) { throw "缺少部署环境文件: $EnvPath" }
if (-not (Test-Path -LiteralPath $KikReceipt)) { throw "缺少 ctrl_kik 构建回执: $KikReceipt" }
Test-PortFree $HttpPort

$environment = Convert-Environment (Get-Content -LiteralPath $EnvPath -Raw -Encoding UTF8 | ConvertFrom-Json)
$receipt = Get-Content -LiteralPath $KikReceipt -Raw -Encoding UTF8 | ConvertFrom-Json
$apiToken = $environment["REAL_CTRL_API_TOKEN"]
if ([int]$environment["REAL_CTRL_TLS_PORT"] -ne $ExpectedControlTlsPort) {
    throw "部署环境中的 real_ctrl TLS 端口与通道预设不一致"
}
if (-not $receipt.production_ready -or [int]$receipt.deployment.port -ne $ExpectedKikNoisePort) {
    throw "ctrl_kik 构建回执与当前 Kik Noise 端口不一致或未达到生产门禁"
}
$result = [ordered]@{
    schema_version = 2
    channel = $ChannelName
    kik_noise_port = $ExpectedKikNoisePort
    control_tls_port = $ExpectedControlTlsPort
    started_at = (Get-Date).ToUniversalTime().ToString("o")
    assertions = @()
    performance = [ordered]@{}
    success = $false
}

try {
    Wait-TcpPort $environment["REAL_CTRL_SERVER_HOST"] $ExpectedKikNoisePort 15
    Wait-TcpPort $environment["REAL_CTRL_SERVER_HOST"] $ExpectedControlTlsPort 15
    $result.assertions += "Kik Noise 与 real_ctrl TLS 双公网端口可达"

    # 先验证控制端确实执行 pin 校验，再启动长期 HTTP 实例。
    Test-WrongPinRejected
    $result.assertions += "错误 SPKI pin 被拒绝"

    Test-ControlRejectedOnKikPort
    $result.assertions += "Kik Noise 端口拒绝 real_ctrl TLS 角色"

    $http = Start-TestProcess -Name "real-ctrl-http" -Path $HttpExe -WorkingDirectory $Root -Environment @{}
    Wait-TcpPort "127.0.0.1" $HttpPort 30
    $health = Invoke-RestMethod -Method Get -Uri "http://127.0.0.1:$HttpPort/api/health"
    if ($health -ne "OK") { throw "HTTP health 返回异常" }
    $result.assertions += "real_ctrl EXE 无启动脚本/运行环境可直接连接，HTTP 健康检查通过"

    $unauthorized = $false
    try {
        $body = @{ version = 1; request_id = "no-token"; command = @{ kind = "sys_list" } } | ConvertTo-Json -Compress
        Invoke-RestMethod -Method Post -Uri "http://127.0.0.1:$HttpPort/api/v1/commands" `
            -ContentType "application/json" -Body $body | Out-Null
    } catch {
        $unauthorized = ([int]$_.Exception.Response.StatusCode -eq 401)
    }
    if (-not $unauthorized) { throw "HTTP API 未拒绝无 token 请求" }
    $result.assertions += "HTTP API token 门禁通过"

    $baseline = Invoke-ApiCommand @{ kind = "sys_list" } "baseline" $apiToken
    $baselineIds = @{}
    if ($baseline.ok -and $baseline.data.items) {
        foreach ($item in $baseline.data.items) { $baselineIds[[string]$item.id] = $true }
    }
    $beforeKikNow = Invoke-ApiCommand @{ kind = "sys_now" } "before-kik-now" $apiToken
    $hadCurrentKik = $beforeKikNow.ok -and $null -ne $beforeKikNow.data.value.Kik

    $kik = Start-TestProcess -Name "ctrl-kik" -Path $KikExe -WorkingDirectory $RunDir -Environment @{}
    $selected = $null
    for ($attempt = 0; $attempt -lt 40; $attempt++) {
        Start-Sleep -Milliseconds 500
        $list = Invoke-ApiCommand @{ kind = "sys_list" } "list-$attempt" $apiToken
        if ($list.ok) {
            $selected = @($list.data.items | Where-Object { -not $baselineIds.ContainsKey([string]$_.id) }) | Select-Object -First 1
            if ($selected) { break }
        }
    }
    if (-not $selected) { throw "公网服务未发现本次启动的 ctrl_kik" }
    $kikId = [string]$selected.id
    $result.assertions += "ctrl_kik 通过 Noise NK 接入公网服务"

    $history = Invoke-ApiCommand @{ kind = "sys_history"; kik_id = $kikId } "history-online" $apiToken
    if (-not ($history.ok -and @($history.data.items).Count -eq 1 -and $history.data.items[0].online)) {
        throw "sys_history 未返回当前在线 Kik"
    }
    $result.assertions += "sys_history 返回最近上线时间与在线状态"

    if (-not $hadCurrentKik) {
        $autoNow = Invoke-ApiCommand @{ kind = "sys_now" } "auto-now" $apiToken
        if (-not ($autoNow.ok -and [string]$autoNow.data.value.Kik.id -eq $kikId)) {
            throw "无当前目标时，新上线 Kik 未被自动选择"
        }
        $result.assertions += "无当前目标时自动选择新上线 Kik"
    }

    $use = Invoke-ApiCommand @{ kind = "sys_use"; kik_id = $kikId } "use" $apiToken
    if (-not $use.ok) { throw "sys_use 失败: $($use.error.message)" }
    $now = Invoke-ApiCommand @{ kind = "sys_now" } "now" $apiToken
    if (-not $now.ok) { throw "sys_now 失败: $($now.error.message)" }
    $result.assertions += "sys_list/sys_use/sys_now 通过"

    $missingRemote = Join-Path $RunDir "missing-download-source.bin"
    $missingLocal = Join-Path $RunDir "missing-download-target.bin"
    Remove-Item -LiteralPath $missingRemote, $missingLocal -Force -ErrorAction SilentlyContinue
    $missingDownload = Invoke-ApiCommand @{
        kind = "ctrl_get_file"
        remote_path = $missingRemote
        local_path = $missingLocal
    } "missing-download" $apiToken
    if ($missingDownload.ok -or $missingDownload.error.message -match "不匹配的数据关联 ID") {
        throw "被控端文件读取错误被错误映射成数据关联 ID 不匹配"
    }
    $result.assertions += "文件读取业务错误保留真实原因且释放数据路由"

    $listPath = Join-Path $RunDir "list-marker.txt"
    [IO.File]::WriteAllText($listPath, "public e2e", [Text.UTF8Encoding]::new($false))
    $ls = Invoke-ApiCommand @{ kind = "ctrl_ls"; path = $RunDir } "ls" $apiToken
    if (-not ($ls.ok -and @($ls.data.entries).Count -gt 0)) { throw "ctrl_ls 失败" }
    $result.assertions += "目录读取通过"

    $exec = Invoke-ApiCommand @{ kind = "exec"; command = "echo public-e2e-ok" } "exec" $apiToken
    if (-not ($exec.ok -and $exec.data.message -match "public-e2e-ok")) { throw "exec 失败" }
    $result.assertions += "Exec 双层授权与执行通过"

    $smallSource = Join-Path $RunDir "small-source.bin"
    $smallRemote = Join-Path $RunDir "small-remote.bin"
    $smallDownload = Join-Path $RunDir "small-download.bin"
    [IO.File]::WriteAllBytes($smallSource, [byte[]](0..255))
    $setSmall = Invoke-ApiCommand @{
        kind = "ctrl_set_file"; local_path = $smallSource; remote_path = $smallRemote
    } "set-small" $apiToken
    if (-not $setSmall.ok) { throw "ctrl_set_file 失败" }
    $getSmall = Invoke-ApiCommand @{
        kind = "ctrl_get_file"; remote_path = $smallRemote; local_path = $smallDownload
    } "get-small" $apiToken
    if (-not $getSmall.ok) { throw "ctrl_get_file 失败" }
    if ((Get-FileHash $smallSource).Hash -ne (Get-FileHash $smallDownload).Hash) {
        throw "小文件往返摘要不一致"
    }
    $result.assertions += "小文件上传下载通过"

    $bigSource = Join-Path $RunDir "big-source.bin"
    $bigRemote = Join-Path $RunDir "big-remote.bin"
    $bigDownload = Join-Path $RunDir "big-download.bin"
    Remove-Item -LiteralPath $bigRemote, $bigDownload -Force -ErrorAction SilentlyContinue
    $buffer = New-Object byte[] (1024 * 1024)
    Fill-RandomBytes $buffer
    $file = [IO.File]::Open($bigSource, [IO.FileMode]::Create, [IO.FileAccess]::Write, [IO.FileShare]::Read)
    try {
        for ($index = 0; $index -lt $BigFileMiB; $index++) { $file.Write($buffer, 0, $buffer.Length) }
        $file.Flush($true)
    } finally {
        $file.Dispose()
    }

    $watch = [Diagnostics.Stopwatch]::StartNew()
    $setBig = Invoke-ApiCommand @{
        kind = "ctrl_set_big_file"; local_path = $bigSource; remote_path = $bigRemote
    } "set-big" $apiToken
    $watch.Stop()
    if (-not $setBig.ok) { throw "ctrl_set_big_file 失败: $($setBig.error.message)" }
    $uploadSeconds = [Math]::Max($watch.Elapsed.TotalSeconds, 0.001)

    $watch.Restart()
    $getBig = Invoke-ApiCommand @{
        kind = "ctrl_get_big_file"; remote_path = $bigRemote; local_path = $bigDownload
    } "get-big" $apiToken
    $watch.Stop()
    if (-not $getBig.ok) { throw "ctrl_get_big_file 失败: $($getBig.error.message)" }
    $downloadSeconds = [Math]::Max($watch.Elapsed.TotalSeconds, 0.001)
    if ((Get-FileHash $bigSource).Hash -ne (Get-FileHash $bigDownload).Hash) {
        throw "大文件公网往返 SHA-256 不一致"
    }
    $result.performance.big_file_mib = $BigFileMiB
    $result.performance.upload_seconds = [Math]::Round($uploadSeconds, 3)
    $result.performance.upload_mib_per_second = [Math]::Round($BigFileMiB / $uploadSeconds, 2)
    $result.performance.download_seconds = [Math]::Round($downloadSeconds, 3)
    $result.performance.download_mib_per_second = [Math]::Round($BigFileMiB / $downloadSeconds, 2)
    $result.assertions += "$BigFileMiB MiB 多连接乱序分片往返 SHA-256 通过"

    $baseline = Measure-NetworkBaseline
    $result.performance.network_baseline = $baseline
    $limitingRate = [Math]::Min(
        [double]$baseline.upload_mib_per_second,
        [double]$baseline.download_mib_per_second
    )
    $applicationRate = [Math]::Min(
        [double]$result.performance.upload_mib_per_second,
        [double]$result.performance.download_mib_per_second
    )
    $result.performance.relay_efficiency_percent = [Math]::Round(100 * $applicationRate / $limitingRate, 1)
    $result.assertions += "SFTP 链路基线、摘要与服务端资源快照通过"

    # 公网环境同样验证完整下线事件，确保服务端只在主/数据连接均释放后更新时间。
    try { $kik.Process.Kill($true) } catch { $kik.Process.Kill() }
    $kik.Process.WaitForExit(5000) | Out-Null
    $offlineHistory = $null
    for ($attempt = 0; $attempt -lt 60; $attempt++) {
        Start-Sleep -Milliseconds 500
        $offlineHistory = Invoke-ApiCommand `
            @{ kind = "sys_history"; kik_id = $kikId } `
            "history-offline-$attempt" `
            $apiToken
        if ($offlineHistory.ok -and
            @($offlineHistory.data.items).Count -eq 1 -and
            -not $offlineHistory.data.items[0].online -and
            $null -ne $offlineHistory.data.items[0].recent_offline_unix_ms) {
            break
        }
    }
    if (-not ($offlineHistory.ok -and
        @($offlineHistory.data.items).Count -eq 1 -and
        -not $offlineHistory.data.items[0].online -and
        $null -ne $offlineHistory.data.items[0].recent_offline_unix_ms)) {
        throw "公网 sys_history 未观察到 Kik 下线事件"
    }
    $result.assertions += "公网 sys_history 返回最近下线时间"

    $result.success = $true
} catch {
    $result.error = $_.Exception.Message
    throw
} finally {
    Stop-TestProcesses
    Remove-TestPayloadFiles
    $result.completed_at = (Get-Date).ToUniversalTime().ToString("o")
    [IO.File]::WriteAllText($ReportPath, ($result | ConvertTo-Json -Depth 12), [Text.UTF8Encoding]::new($false))
}

$result | ConvertTo-Json -Depth 12
