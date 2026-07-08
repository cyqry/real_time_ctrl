param(
    [int]$PlainPort = 9002,
    [int]$TlsPort = 19443,
    [int]$HttpPort = 9000
)

$ErrorActionPreference = "Stop"
$script:Processes = @()

$ScriptDir = Split-Path -Parent $MyInvocation.MyCommand.Path
$Root = (Resolve-Path (Join-Path $ScriptDir "..")).Path
$E2eDir = Join-Path $Root "target\e2e"
$CertDir = Join-Path $E2eDir "certs"
$LogDir = Join-Path $E2eDir "logs"
$ReportPath = Join-Path $E2eDir "e2e_report.json"
$ApiToken = "e2e-local-token"

New-Item -ItemType Directory -Force -Path $E2eDir, $CertDir, $LogDir | Out-Null
Set-Content -LiteralPath (Join-Path $E2eDir "ctrl_ls_marker.txt") -Encoding UTF8 -Value "real_time_ctrl e2e marker"

if ($PlainPort -ne 9002) {
    throw "ctrl_kik 已恢复为只使用编译期 PORT()；除非修改 common/config.json 并重新构建，否则 E2E PlainPort 必须保持 9002。"
}

function Assert-CommandOk {
    param([int]$ExitCode, [string]$Message)
    if ($ExitCode -ne 0) {
        throw $Message
    }
}

function Test-PortFree {
    param([int]$Port)
    $used = Get-NetTCPConnection -LocalPort $Port -State Listen -ErrorAction SilentlyContinue
    if (-not $used) {
        $used = netstat -ano | Select-String -Pattern ("^\s*TCP\s+\S+:$Port\s+\S+\s+LISTENING\s")
    }
    if ($used) {
        throw "Port $Port is already in use."
    }
}

function Wait-TcpPort {
    param(
        [string]$HostName,
        [int]$Port,
        [int]$TimeoutSeconds
    )
    $deadline = (Get-Date).AddSeconds($TimeoutSeconds)
    while ((Get-Date) -lt $deadline) {
        $client = [System.Net.Sockets.TcpClient]::new()
        try {
            $task = $client.ConnectAsync($HostName, $Port)
            if ($task.Wait(500) -and $client.Connected) {
                return
            }
        } catch {
        } finally {
            $client.Dispose()
        }
        Start-Sleep -Milliseconds 300
    }
    throw "Timed out waiting for ${HostName}:$Port"
}

function Wait-TlsEndpoint {
    param(
        [string]$HostName,
        [int]$Port,
        [string]$ServerName,
        [string]$CaCert,
        [int]$TimeoutSeconds
    )
    $deadline = (Get-Date).AddSeconds($TimeoutSeconds)
    $outPath = Join-Path $LogDir "openssl_s_client.stdout.log"
    $errPath = Join-Path $LogDir "openssl_s_client.stderr.log"
    while ((Get-Date) -lt $deadline) {
        try {
            $oldErrorActionPreference = $ErrorActionPreference
            $ErrorActionPreference = "Continue"
            try {
                $tlsOutput = "" | & openssl s_client `
                    -connect "${HostName}:$Port" `
                    -servername $ServerName `
                    -CAfile $CaCert `
                    -verify_return_error `
                    -brief `
                    2>&1
                $tlsExitCode = $LASTEXITCODE
            } finally {
                $ErrorActionPreference = $oldErrorActionPreference
            }
            $tlsOutput | Set-Content -LiteralPath $outPath -Encoding UTF8
            Set-Content -LiteralPath $errPath -Encoding UTF8 -Value ""
            if ($tlsExitCode -eq 0) {
                return
            }
        } catch {
        }
        Start-Sleep -Milliseconds 300
    }
    throw "Timed out waiting for TLS ${HostName}:$Port"
}

function New-E2eCertificate {
    $OpenSslConfig = Join-Path $CertDir "openssl_real_ctrl.cnf"
    $CertPath = Join-Path $CertDir "server.crt"
    $KeyPath = Join-Path $CertDir "server.key"
    $PubKeyPath = Join-Path $CertDir "server.pubkey.pem"
    $SpkiPath = Join-Path $CertDir "server.spki.der"

    @"
[req]
default_bits = 2048
prompt = no
default_md = sha256
distinguished_name = dn
x509_extensions = v3_req

[dn]
CN = real-ctrl-server

[v3_req]
subjectAltName = @alt_names

[alt_names]
DNS.1 = real-ctrl-server
IP.1 = 127.0.0.1
"@ | Set-Content -LiteralPath $OpenSslConfig -Encoding ASCII

    $ReqOutPath = Join-Path $LogDir "openssl_req.stdout.log"
    $ReqErrPath = Join-Path $LogDir "openssl_req.stderr.log"
    $oldErrorActionPreference = $ErrorActionPreference
    $ErrorActionPreference = "Continue"
    try {
        $reqOutput = & openssl req -x509 -newkey rsa:2048 -nodes -days 7 `
            -keyout $KeyPath -out $CertPath -config $OpenSslConfig -extensions v3_req `
            2>&1
        $reqExitCode = $LASTEXITCODE
    } finally {
        $ErrorActionPreference = $oldErrorActionPreference
    }
    Set-Content -LiteralPath $ReqOutPath -Encoding UTF8 -Value ""
    $reqOutput | Set-Content -LiteralPath $ReqErrPath -Encoding UTF8
    Assert-CommandOk $reqExitCode "Failed to generate E2E TLS certificate"

    & openssl x509 -in $CertPath -pubkey -noout -out $PubKeyPath | Out-Null
    Assert-CommandOk $LASTEXITCODE "Failed to export E2E TLS public key"

    & openssl pkey -pubin -in $PubKeyPath -outform DER -out $SpkiPath | Out-Null
    Assert-CommandOk $LASTEXITCODE "Failed to export E2E SPKI DER"

    $hashOutput = & openssl dgst -sha256 $SpkiPath
    Assert-CommandOk $LASTEXITCODE "Failed to calculate E2E SPKI pin"
    $pin = ($hashOutput -replace '^.*=\s*', '').Trim().ToLowerInvariant()

    [pscustomobject]@{
        Cert = $CertPath
        Key  = $KeyPath
        Pin  = $pin
    }
}

function Start-E2eProcess {
    param(
        [string]$Name,
        [string]$ExePath,
        [hashtable]$EnvMap
    )
    if (-not (Test-Path -LiteralPath $ExePath)) {
        throw "Missing executable: $ExePath"
    }

    $psi = [System.Diagnostics.ProcessStartInfo]::new()
    $psi.FileName = $ExePath
    $psi.WorkingDirectory = $Root
    $psi.UseShellExecute = $false
    $psi.RedirectStandardOutput = $true
    $psi.RedirectStandardError = $true
    $psi.CreateNoWindow = $true

    foreach ($item in $EnvMap.GetEnumerator()) {
        $psi.Environment[$item.Key] = [string]$item.Value
    }

    $process = [System.Diagnostics.Process]::new()
    $process.StartInfo = $psi
    if (-not $process.Start()) {
        throw "Failed to start $Name"
    }

    $entry = [pscustomobject]@{
        Name   = $Name
        Proc   = $process
        StdOut = Join-Path $LogDir "$Name.stdout.log"
        StdErr = Join-Path $LogDir "$Name.stderr.log"
    }
    $script:Processes += $entry
    $entry
}

function Stop-E2eProcesses {
    $entries = @($script:Processes)
    [array]::Reverse($entries)
    foreach ($entry in $entries) {
        $proc = $entry.Proc
        try {
            if ($proc -and -not $proc.HasExited) {
                try {
                    $proc.Kill($true)
                } catch {
                    $proc.Kill()
                }
                $proc.WaitForExit(5000) | Out-Null
            }
        } finally {
            if ($proc) {
                $stdout = $proc.StandardOutput.ReadToEnd()
                $stderr = $proc.StandardError.ReadToEnd()
                Set-Content -LiteralPath $entry.StdOut -Encoding UTF8 -Value $stdout
                Set-Content -LiteralPath $entry.StdErr -Encoding UTF8 -Value $stderr
            }
        }
    }
}

function Invoke-ApiCommand {
    param([hashtable]$Command, [string]$RequestId)
    $body = @{
        version    = 1
        request_id = $RequestId
        command    = $Command
    } | ConvertTo-Json -Depth 12 -Compress

    Invoke-RestMethod `
        -Method Post `
        -Uri "http://127.0.0.1:$HttpPort/api/v1/commands" `
        -Headers @{ "Authorization" = "Bearer $ApiToken" } `
        -ContentType "application/json" `
        -Body $body
}

$result = [ordered]@{
    root        = $Root
    e2e_dir     = $E2eDir
    plain_port  = $PlainPort
    tls_port    = $TlsPort
    http_port   = $HttpPort
    started     = @()
    assertions  = @()
    success     = $false
}

try {
    Test-PortFree -Port $PlainPort
    Test-PortFree -Port $TlsPort
    Test-PortFree -Port $HttpPort

    $cert = New-E2eCertificate
    $result.tls_pin = $cert.Pin

    $serverEnv = @{
        "CTRL_SERVER_BIND_HOST" = "127.0.0.1"
        "CTRL_SERVER_PORT" = "$PlainPort"
        "CTRL_SERVER_TLS_PORT" = "$TlsPort"
        "CTRL_SERVER_TLS_CERT" = $cert.Cert
        "CTRL_SERVER_TLS_KEY" = $cert.Key
        "CTRL_SERVER_E2E_TRACE_PATH" = (Join-Path $E2eDir "ctrl_server_trace.log")
        "RUST_BACKTRACE" = "1"
    }
    $kikEnv = @{}
    $realCtrlEnv = @{
        "REAL_CTRL_SERVER_HOST" = "127.0.0.1"
        "REAL_CTRL_SERVER_PORT" = "$PlainPort"
        "REAL_CTRL_TLS_PORT" = "$TlsPort"
        "REAL_CTRL_TLS_SERVER_NAME" = "real-ctrl-server"
        "REAL_CTRL_TLS_CA_CERT" = $cert.Cert
        "REAL_CTRL_TLS_SERVER_SPKI_SHA256" = $cert.Pin
        "REAL_CTRL_HTTP_LOCK_PATH" = (Join-Path $E2eDir "real_ctrl_http.lock")
        "REAL_CTRL_E2E_TRACE_PATH" = (Join-Path $E2eDir "real_ctrl_trace.log")
        "REAL_CTRL_API_TOKEN" = $ApiToken
        "RUST_BACKTRACE" = "1"
    }

    $server = Start-E2eProcess -Name "ctrl_server" -ExePath (Join-Path $Root "target\debug\ctrl_server.exe") -EnvMap $serverEnv
    $result.started += @{ name = "ctrl_server"; pid = $server.Proc.Id }
    Wait-TcpPort -HostName "127.0.0.1" -Port $PlainPort -TimeoutSeconds 20
    Wait-TlsEndpoint -HostName "127.0.0.1" -Port $TlsPort -ServerName "real-ctrl-server" -CaCert $cert.Cert -TimeoutSeconds 20
    $result.assertions += "ctrl_server plain/tls ports listening"

    $kik = Start-E2eProcess -Name "ctrl_kik" -ExePath (Join-Path $Root "target\debug\ctrl_kik.exe") -EnvMap $kikEnv
    $result.started += @{ name = "ctrl_kik"; pid = $kik.Proc.Id }
    Start-Sleep -Seconds 2

    $realCtrl = Start-E2eProcess -Name "real_ctrl_invoker_http_service" -ExePath (Join-Path $Root "target\debug\real_ctrl_invoker_http_service.exe") -EnvMap $realCtrlEnv
    $result.started += @{ name = "real_ctrl_invoker_http_service"; pid = $realCtrl.Proc.Id }
    Wait-TcpPort -HostName "127.0.0.1" -Port $HttpPort -TimeoutSeconds 30

    $health = Invoke-RestMethod -Uri "http://127.0.0.1:$HttpPort/api/health" -Method Get
    if ($health -ne "OK") {
        throw "Unexpected HTTP health response: $health"
    }
    $result.assertions += "real_ctrl http health ok"

    $unauthorizedOk = $false
    try {
        $body = @{ version = 1; request_id = "missing-token"; command = @{ kind = "sys_list" } } | ConvertTo-Json -Compress
        Invoke-RestMethod -Method Post -Uri "http://127.0.0.1:$HttpPort/api/v1/commands" -ContentType "application/json" -Body $body | Out-Null
    } catch {
        $status = $_.Exception.Response.StatusCode
        if ([int]$status -eq 401) {
            $unauthorizedOk = $true
        }
    }
    if (-not $unauthorizedOk) {
        throw "API request without token was not rejected with 401"
    }
    $result.assertions += "http api token required"

    $sysList = $null
    for ($i = 0; $i -lt 20; $i++) {
        $sysList = Invoke-ApiCommand -Command @{ kind = "sys_list" } -RequestId "sys-list-$i"
        if ($sysList.ok -and $sysList.data.kind -eq "sys_list" -and $sysList.data.items.Count -gt 0) {
            break
        }
        Start-Sleep -Seconds 1
    }
    if (-not ($sysList.ok -and $sysList.data.items.Count -gt 0)) {
        throw "sys_list did not find online ctrl_kik, last=$($sysList | ConvertTo-Json -Depth 12 -Compress)"
    }
    $kikId = [string]$sysList.data.items[0].id
    $result.kik_id = $kikId
    $result.assertions += "sys_list sees ctrl_kik"

    $sysUse = Invoke-ApiCommand -Command @{ kind = "sys_use"; kik_id = $kikId } -RequestId "sys-use"
    if (-not ($sysUse.ok)) {
        throw "sys_use failed: $($sysUse | ConvertTo-Json -Depth 12 -Compress)"
    }
    $result.assertions += "sys_use selected ctrl_kik"

    $sysNow = Invoke-ApiCommand -Command @{ kind = "sys_now" } -RequestId "sys-now"
    if (-not ($sysNow.ok)) {
        throw "sys_now failed: $($sysNow | ConvertTo-Json -Depth 12 -Compress)"
    }
    $result.assertions += "sys_now ok"

    $ls = Invoke-ApiCommand -Command @{ kind = "ctrl_ls"; path = $E2eDir } -RequestId "ctrl-ls"
    if (-not ($ls.ok -and $ls.data.kind -eq "ls" -and $ls.data.entries.Count -gt 0)) {
        throw "ctrl_ls failed: $($ls | ConvertTo-Json -Depth 12 -Compress)"
    }
    $result.ls_entry_count = $ls.data.entries.Count
    $result.assertions += "ctrl_ls through ctrl_server and ctrl_kik ok"

    $result.success = $true
    $result.completed_at = (Get-Date).ToString("o")
    $result | ConvertTo-Json -Depth 12 | Set-Content -LiteralPath $ReportPath -Encoding UTF8
    Write-Output ($result | ConvertTo-Json -Depth 12)
} catch {
    $result.success = $false
    $result.error = $_.Exception.Message
    $result.completed_at = (Get-Date).ToString("o")
    $result | ConvertTo-Json -Depth 12 | Set-Content -LiteralPath $ReportPath -Encoding UTF8
    throw
} finally {
    Stop-E2eProcesses
}
