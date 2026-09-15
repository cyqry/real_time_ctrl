param(
    [ValidateSet("Gray", "Production")]
    [string]$Channel = "Gray",
    [ValidateRange(1, 65535)]
    [int]$HttpPort = 9000
)

$ErrorActionPreference = "Stop"
Set-StrictMode -Version Latest

$scriptDir = Split-Path -Parent $MyInvocation.MyCommand.Path
$root = (Resolve-Path (Join-Path $scriptDir "..")).Path
$channelName = $Channel.ToLowerInvariant()
$artifactDir = (Resolve-Path (Join-Path $root "deploy\$channelName\artifacts")).Path
$workDir = Join-Path $root "target\direct-artifacts\$channelName"
$script:results = [Collections.Generic.List[object]]::new()

function Start-CleanProcess {
    param([string]$Name, [string]$Path)

    $info = [Diagnostics.ProcessStartInfo]::new()
    $info.FileName = $Path
    $info.WorkingDirectory = $workDir
    $info.UseShellExecute = $false
    $info.CreateNoWindow = $true
    # 保持 stdin 管道打开，避免交互式 real_ctrl 因无控制台立即读到 EOF。
    $info.RedirectStandardInput = $true
    $info.RedirectStandardOutput = $true
    $info.RedirectStandardError = $true
    foreach ($variable in @(
        "REAL_CTRL_SERVER_HOST",
        "REAL_CTRL_TLS_PORT",
        "REAL_CTRL_TLS_SERVER_NAME",
        "REAL_CTRL_TLS_CA_CERT",
        "REAL_CTRL_TLS_SERVER_SPKI_SHA256",
        "REAL_CTRL_AUTH_SECRET",
        "REAL_CTRL_API_TOKEN",
        "REAL_CTRL_API_ALLOW_EXEC",
        "REAL_CTRL_HTTP_LOCK_PATH"
    )) {
        [void]$info.Environment.Remove($variable)
    }

    $process = [Diagnostics.Process]::new()
    $process.StartInfo = $info
    if (-not $process.Start()) {
        throw "无法直接启动 $Name"
    }
    $process.BeginOutputReadLine()
    $process.BeginErrorReadLine()
    return $process
}

function Stop-SmokeProcess {
    param([Diagnostics.Process]$Process)

    if (-not $Process.HasExited) {
        try {
            $Process.Kill($true)
        } catch {
            $Process.Kill()
        }
        [void]$Process.WaitForExit(5000)
    }
    $Process.Dispose()
}

function Assert-StaysRunning {
    param([string]$Name)

    $path = Join-Path $artifactDir $Name
    if (-not (Test-Path -LiteralPath $path -PathType Leaf)) {
        throw "发布目录缺少 $Name"
    }
    $process = Start-CleanProcess -Name $Name -Path $path
    try {
        Start-Sleep -Seconds 4
        if ($process.HasExited) {
            throw "$Name 直接启动后过早退出，exit code: $($process.ExitCode)"
        }
        $script:results.Add([pscustomobject]@{
            executable = $Name
            direct_start = "passed"
            proof = "alive_after_4s"
        })
    } finally {
        Stop-SmokeProcess -Process $process
    }
    Start-Sleep -Seconds 1
}

[IO.Directory]::CreateDirectory($workDir) | Out-Null
try {
    $httpName = "real_ctrl_invoker_http_service.exe"
    $http = Start-CleanProcess -Name $httpName -Path (Join-Path $artifactDir $httpName)
    try {
        $deadline = (Get-Date).AddSeconds(20)
        $health = $null
        do {
            Start-Sleep -Milliseconds 250
            if ($http.HasExited) {
                throw "$httpName 直接启动后过早退出，exit code: $($http.ExitCode)"
            }
            try {
                $health = Invoke-RestMethod -Uri "http://127.0.0.1:$HttpPort/api/health" -TimeoutSec 1
            } catch {
                $health = $null
            }
        } while ($health -ne "OK" -and (Get-Date) -lt $deadline)
        if ($health -ne "OK") {
            throw "$httpName 直接启动后健康端点未就绪"
        }
        $script:results.Add([pscustomobject]@{
            executable = $httpName
            direct_start = "passed"
            proof = "health=OK"
        })
    } finally {
        Stop-SmokeProcess -Process $http
    }
    Start-Sleep -Seconds 1

    # 这里验证各入口都能无 sidecar 直启；完整的多控制实例并存由 e2e_ctrl_stack.ps1 覆盖。
    Assert-StaysRunning -Name "real_ctrl_local_server.exe"
    Assert-StaysRunning -Name "real_ctrl.exe"
    Assert-StaysRunning -Name "ctrl_kik.exe"
} finally {
    [IO.File]::Delete((Join-Path ([IO.Path]::GetTempPath()) "real_ctrl-http-$channelName.lock"))
    [IO.File]::Delete((Join-Path $workDir "ctrl_kik.lock"))
}

$script:results | ConvertTo-Json
