param(
    [int]$KikNoisePort = 9002,
    [int]$TlsPort = 19443,
    [int]$HttpPort = 9000
)

$ErrorActionPreference = "Stop"
$script:Processes = @()
Add-Type -AssemblyName System.Net.Http

$ScriptDir = Split-Path -Parent $MyInvocation.MyCommand.Path
$Root = (Resolve-Path (Join-Path $ScriptDir "..")).Path
$E2eDir = Join-Path $Root "target\e2e"
$CertDir = Join-Path $E2eDir "certs"
$LogDir = Join-Path $E2eDir "logs"
$ReportPath = Join-Path $E2eDir "e2e_report.json"
$ApiToken = "e2e-local-token"
$ControlAuthSecret = "e2e-control-auth-secret-0123456789abcdef"
$SecondAccountSecret = "e2e-second-account-secret-0123456789abcdef"
$SameAccountHttpPort = $HttpPort + 1
$SecondAccountHttpPort = $HttpPort + 3

New-Item -ItemType Directory -Force -Path $E2eDir, $CertDir, $LogDir | Out-Null
Set-Content -LiteralPath (Join-Path $E2eDir "ctrl_ls_marker.txt") -Encoding UTF8 -Value "real_time_ctrl e2e marker"

if ($KikNoisePort -ne 9002) {
    throw "ctrl_kik 只使用编译期 PORT()；除非通过构建变量重新编译，否则 E2E KikNoisePort 必须保持 9002。"
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

function New-E2eNoiseIdentity {
    $privatePem = Join-Path $CertDir "kik-noise-private.pem"
    $privateDer = Join-Path $CertDir "kik-noise-private.der"
    $publicDer = Join-Path $CertDir "kik-noise-public.der"

    & openssl genpkey -algorithm X25519 -out $privatePem 2>$null
    Assert-CommandOk $LASTEXITCODE "Failed to generate E2E Noise private key"
    & openssl pkey -in $privatePem -outform DER -out $privateDer 2>$null
    Assert-CommandOk $LASTEXITCODE "Failed to export E2E Noise private key"
    & openssl pkey -in $privatePem -pubout -outform DER -out $publicDer 2>$null
    Assert-CommandOk $LASTEXITCODE "Failed to export E2E Noise public key"

    $privateBytes = [IO.File]::ReadAllBytes($privateDer)
    $publicBytes = [IO.File]::ReadAllBytes($publicDer)
    if ($privateBytes.Count -lt 32 -or $publicBytes.Count -lt 32) {
        throw "OpenSSL X25519 DER output is unexpectedly short"
    }
    [pscustomobject]@{
        Private = [Convert]::ToBase64String([byte[]]$privateBytes[($privateBytes.Count - 32)..($privateBytes.Count - 1)])
        Public = [Convert]::ToBase64String([byte[]]$publicBytes[($publicBytes.Count - 32)..($publicBytes.Count - 1)])
    }
}

function Build-E2eBinaries {
    param([string]$NoisePublicKey)
    $previousKey = $env:RTC_CTRL_KIK_NOISE_SERVER_PUBLIC_KEY
    $previousHost = $env:RTC_CTRL_KIK_BUILD_HOST
    $previousPort = $env:RTC_CTRL_KIK_BUILD_PORT
    try {
        # 公钥只在构建期进入 ctrl_kik；运行时不读取环境变量，也不接收服务端机器信息。
        $env:RTC_CTRL_KIK_NOISE_SERVER_PUBLIC_KEY = $NoisePublicKey
        $env:RTC_CTRL_KIK_BUILD_HOST = "127.0.0.1"
        $env:RTC_CTRL_KIK_BUILD_PORT = "$KikNoisePort"
        & cargo build --locked -p ctrl_server -p ctrl_kik -p real_ctrl --bins
        Assert-CommandOk $LASTEXITCODE "Failed to build E2E binaries"
        & cargo build --locked -p real_ctrl --example pipe_concurrency_probe
        Assert-CommandOk $LASTEXITCODE "Failed to build named-pipe concurrency probe"
    } finally {
        $env:RTC_CTRL_KIK_NOISE_SERVER_PUBLIC_KEY = $previousKey
        $env:RTC_CTRL_KIK_BUILD_HOST = $previousHost
        $env:RTC_CTRL_KIK_BUILD_PORT = $previousPort
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

    # 重定向管道必须立刻异步排空；否则认证失败时的 backtrace 可能填满 stderr，
    # 让被测进程阻塞在退出路径，进而把安全拒绝误判为超时。
    $stdoutTask = $process.StandardOutput.ReadToEndAsync()
    $stderrTask = $process.StandardError.ReadToEndAsync()

    $entry = [pscustomobject]@{
        Name          = $Name
        Proc          = $process
        StdOut        = Join-Path $LogDir "$Name.stdout.log"
        StdErr        = Join-Path $LogDir "$Name.stderr.log"
        StdOutTask    = $stdoutTask
        StdErrTask    = $stderrTask
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
                $stdout = $entry.StdOutTask.Result
                $stderr = $entry.StdErrTask.Result
                Set-Content -LiteralPath $entry.StdOut -Encoding UTF8 -Value $stdout
                Set-Content -LiteralPath $entry.StdErr -Encoding UTF8 -Value $stderr
            }
        }
    }
}

function Assert-RealCtrlRejected {
    param(
        [string]$Name,
        [hashtable]$EnvMap
    )
    $entry = Start-E2eProcess `
        -Name $Name `
        -ExePath (Join-Path $Root "target\debug\real_ctrl.exe") `
        -EnvMap $EnvMap
    if (-not $entry.Proc.WaitForExit(10000)) {
        throw "$Name did not reject the unsafe connection within 10 seconds"
    }
    if ($entry.Proc.ExitCode -eq 0) {
        throw "$Name unexpectedly exited successfully"
    }
}

function Invoke-ApiCommand {
    param([hashtable]$Command, [string]$RequestId, [int]$Port = $HttpPort)
    $body = @{
        version    = 1
        request_id = $RequestId
        command    = $Command
    } | ConvertTo-Json -Depth 12 -Compress

    Invoke-RestMethod `
        -Method Post `
        -Uri "http://127.0.0.1:$Port/api/v1/commands" `
        -Headers @{ "Authorization" = "Bearer $ApiToken" } `
        -ContentType "application/json" `
        -Body $body
}

function Invoke-ParallelApiCommands {
    param(
        [hashtable[]]$Commands,
        [int]$Port = $HttpPort,
        [string]$RequestIdPrefix = "parallel",
        [int]$TimeoutMilliseconds = 20000
    )
    $client = [System.Net.Http.HttpClient]::new()
    $client.DefaultRequestHeaders.Authorization =
        [System.Net.Http.Headers.AuthenticationHeaderValue]::new("Bearer", $ApiToken)
    try {
        $tasks = [System.Collections.Generic.List[System.Threading.Tasks.Task[System.Net.Http.HttpResponseMessage]]]::new()
        for ($i = 0; $i -lt $Commands.Count; $i++) {
            $body = @{
                version = 1
                request_id = "$RequestIdPrefix-$i"
                command = $Commands[$i]
            } | ConvertTo-Json -Depth 12 -Compress
            $content = [System.Net.Http.StringContent]::new(
                $body,
                [System.Text.Encoding]::UTF8,
                "application/json"
            )
            $tasks.Add($client.PostAsync("http://127.0.0.1:$Port/api/v1/commands", $content))
        }
        if (-not [System.Threading.Tasks.Task]::WaitAll(
            [System.Threading.Tasks.Task[]]$tasks.ToArray(),
            $TimeoutMilliseconds
        )) {
            throw "Parallel HTTP commands timed out"
        }
        @($tasks | ForEach-Object {
            $response = $_.Result
            $body = $response.Content.ReadAsStringAsync().Result
            if (-not $response.IsSuccessStatusCode) {
                throw "Parallel HTTP command failed with $([int]$response.StatusCode): $body"
            }
            $body | ConvertFrom-Json
        })
    } finally {
        $client.Dispose()
    }
}

function Invoke-ParallelApiTargets {
    param([object[]]$Targets, [int]$TimeoutMilliseconds = 20000)
    $client = [System.Net.Http.HttpClient]::new()
    $client.DefaultRequestHeaders.Authorization =
        [System.Net.Http.Headers.AuthenticationHeaderValue]::new("Bearer", $ApiToken)
    $requests = [System.Collections.Generic.List[System.Net.Http.HttpRequestMessage]]::new()
    $tasks = [System.Collections.Generic.List[System.Threading.Tasks.Task[System.Net.Http.HttpResponseMessage]]]::new()
    try {
        foreach ($target in $Targets) {
            $body = @{
                version = 1
                request_id = [string]$target.RequestId
                command = $target.Command
            } | ConvertTo-Json -Depth 12 -Compress
            $request = [System.Net.Http.HttpRequestMessage]::new(
                [System.Net.Http.HttpMethod]::Post,
                "http://127.0.0.1:$([int]$target.Port)/api/v1/commands"
            )
            $request.Content = [System.Net.Http.StringContent]::new(
                $body,
                [System.Text.Encoding]::UTF8,
                "application/json"
            )
            $requests.Add($request)
            $tasks.Add($client.SendAsync($request))
        }
        if (-not [System.Threading.Tasks.Task]::WaitAll(
            [System.Threading.Tasks.Task[]]$tasks.ToArray(),
            $TimeoutMilliseconds
        )) {
            throw "Parallel multi-instance HTTP commands timed out"
        }
        @($tasks | ForEach-Object {
            $response = $_.Result
            try {
                $body = $response.Content.ReadAsStringAsync().Result
                if (-not $response.IsSuccessStatusCode) {
                    throw "Parallel multi-instance HTTP command failed with $([int]$response.StatusCode): $body"
                }
                $body | ConvertFrom-Json
            } finally {
                $response.Dispose()
            }
        })
    } finally {
        foreach ($request in $requests) {
            $request.Dispose()
        }
        $client.Dispose()
    }
}

function Invoke-RawHttpRequest {
    param(
        [string]$Method,
        [string]$Path,
        [string]$Body = $null,
        [hashtable]$Headers = @{},
        [string]$ContentType = "application/json",
        [int]$Port = $HttpPort,
        [switch]$AllowTransportRejection
    )
    $handler = [System.Net.Http.HttpClientHandler]::new()
    $handler.AllowAutoRedirect = $false
    $client = [System.Net.Http.HttpClient]::new($handler)
    $client.Timeout = [TimeSpan]::FromSeconds(15)
    $request = [System.Net.Http.HttpRequestMessage]::new(
        [System.Net.Http.HttpMethod]::new($Method),
        "http://127.0.0.1:$Port$Path"
    )
    try {
        foreach ($entry in $Headers.GetEnumerator()) {
            $request.Headers.TryAddWithoutValidation($entry.Key, [string]$entry.Value) | Out-Null
        }
        if ($null -ne $Body) {
            $request.Content = [System.Net.Http.StringContent]::new(
                $Body,
                [System.Text.Encoding]::UTF8,
                $ContentType
            )
        }
        try {
            $response = $client.SendAsync($request).GetAwaiter().GetResult()
            $responseBody = $response.Content.ReadAsStringAsync().GetAwaiter().GetResult()
            $responseHeaders = @{}
            foreach ($header in $response.Headers) {
                $responseHeaders[$header.Key.ToLowerInvariant()] = $header.Value -join ","
            }
            foreach ($header in $response.Content.Headers) {
                $responseHeaders[$header.Key.ToLowerInvariant()] = $header.Value -join ","
            }
            $rawResult = [pscustomobject]@{
                Status  = [int]$response.StatusCode
                Body    = $responseBody
                Headers = $responseHeaders
                RejectedByDisconnect = $false
            }
            $response.Dispose()
            $rawResult
        } catch {
            $cursor = $_.Exception
            $transportFailure = $false
            while ($cursor) {
                if ($cursor -is [System.Net.Http.HttpRequestException] -or
                    $cursor -is [System.Net.Sockets.SocketException]) {
                    $transportFailure = $true
                }
                $cursor = $cursor.InnerException
            }
            if (-not $AllowTransportRejection -or -not $transportFailure) {
                throw
            }
            [pscustomobject]@{
                Status = 0
                Body = ""
                Headers = @{}
                RejectedByDisconnect = $true
            }
        }
    } finally {
        $request.Dispose()
        $client.Dispose()
        $handler.Dispose()
    }
}

function Read-ExactBytes {
    param([System.IO.Stream]$Stream, [int]$Length)
    $buffer = New-Object byte[] $Length
    $offset = 0
    while ($offset -lt $Length) {
        $read = $Stream.Read($buffer, $offset, $Length - $offset)
        if ($read -le 0) {
            throw "Named pipe closed before a complete response was received"
        }
        $offset += $read
    }
    $buffer
}

function Invoke-PipePayload {
    param([byte[]]$Payload, [long]$DeclaredLength = -1)
    $pipe = [System.IO.Pipes.NamedPipeClientStream]::new(
        ".",
        "real_ctrl_service_pipe",
        [System.IO.Pipes.PipeDirection]::InOut,
        [System.IO.Pipes.PipeOptions]::None
    )
    try {
        $pipe.Connect(5000)
        $length = if ($DeclaredLength -ge 0) { [uint32]$DeclaredLength } else { [uint32]$Payload.Length }
        $lengthBytes = [BitConverter]::GetBytes($length)
        if ([BitConverter]::IsLittleEndian) {
            [Array]::Reverse($lengthBytes)
        }
        $pipe.Write($lengthBytes, 0, $lengthBytes.Length)
        if ($Payload.Length -gt 0) {
            $pipe.Write($Payload, 0, $Payload.Length)
        }
        $pipe.Flush()

        [byte[]]$responseLengthBytes = Read-ExactBytes -Stream $pipe -Length 4
        if ([BitConverter]::IsLittleEndian) {
            [Array]::Reverse($responseLengthBytes)
        }
        $responseLength = [BitConverter]::ToUInt32($responseLengthBytes, 0)
        if ($responseLength -gt 64MB) {
            throw "Named pipe returned an oversized test response: $responseLength bytes"
        }
        [byte[]]$responseBytes = Read-ExactBytes -Stream $pipe -Length ([int]$responseLength)
        [byte[]]$magic = [Text.Encoding]::ASCII.GetBytes("RTCAPI1`0")
        if ($responseBytes.Length -lt $magic.Length) {
            throw "Named pipe response is shorter than the API magic"
        }
        for ($i = 0; $i -lt $magic.Length; $i++) {
            if ($responseBytes[$i] -ne $magic[$i]) {
                throw "Named pipe response has an invalid API magic"
            }
        }
        $json = [Text.Encoding]::UTF8.GetString(
            $responseBytes,
            $magic.Length,
            $responseBytes.Length - $magic.Length
        )
        $json | ConvertFrom-Json
    } finally {
        $pipe.Dispose()
    }
}

function Send-RawTcpProbe {
    param([int]$Port, [byte[]]$Payload)
    $client = [System.Net.Sockets.TcpClient]::new()
    try {
        $connect = $client.ConnectAsync("127.0.0.1", $Port)
        if (-not $connect.Wait(2000) -or -not $client.Connected) {
            throw "Raw TCP probe could not connect to port $Port"
        }
        if ($Payload.Length -gt 0) {
            $stream = $client.GetStream()
            $stream.Write($Payload, 0, $Payload.Length)
            $stream.Flush()
        }
    } finally {
        $client.Dispose()
    }
}

function Assert-KikSaturationDoesNotBlockTls {
    param(
        [int]$KikPort,
        [int]$ControlPort,
        [string]$ServerName,
        [string]$CaCert
    )
    $clients = [Collections.Generic.List[Net.Sockets.TcpClient]]::new()
    try {
        # 520 略高于 Kik 端口的 512 硬上限。连接不发送握手并仅保持到 TLS 探测结束，
        # 在本地受控环境复现慢连接饱和而不会触达公网。
        for ($index = 0; $index -lt 520; $index++) {
            $client = [Net.Sockets.TcpClient]::new()
            $task = $client.ConnectAsync("127.0.0.1", $KikPort)
            if ($task.Wait(2000) -and $client.Connected) {
                $clients.Add($client)
            } else {
                $client.Dispose()
            }
        }
        if ($clients.Count -lt 500) {
            throw "Could not establish enough local Kik saturation probes: $($clients.Count)"
        }
        Start-Sleep -Milliseconds 250
        Wait-TlsEndpoint `
            -HostName "127.0.0.1" `
            -Port $ControlPort `
            -ServerName $ServerName `
            -CaCert $CaCert `
            -TimeoutSeconds 10
        $clients.Count
    } finally {
        foreach ($client in $clients) {
            $client.Dispose()
        }
        Start-Sleep -Milliseconds 250
    }
}

$result = [ordered]@{
    root        = $Root
    e2e_dir     = $E2eDir
    kik_noise_port = $KikNoisePort
    tls_port    = $TlsPort
    http_port   = $HttpPort
    started     = @()
    assertions  = @()
    success     = $false
}

try {
    Test-PortFree -Port $KikNoisePort
    Test-PortFree -Port $TlsPort
    Test-PortFree -Port $HttpPort
    Test-PortFree -Port $SameAccountHttpPort
    Test-PortFree -Port $SecondAccountHttpPort

    $cert = New-E2eCertificate
    $noise = New-E2eNoiseIdentity
    Build-E2eBinaries -NoisePublicKey $noise.Public
    $result.tls_pin = $cert.Pin

    $accountsJson = @(
        @{
            account_id = "tenant_a"
            secret = $ControlAuthSecret
            allowed_kiks = @("*")
            max_instances = 8
            max_commands_per_instance = 16
        },
        @{
            account_id = "tenant_b"
            secret = $SecondAccountSecret
            allowed_kiks = @("*")
            max_instances = 8
            max_commands_per_instance = 16
        }
    ) | ConvertTo-Json -Depth 8 -Compress
    $accountsBase64 = [Convert]::ToBase64String([Text.Encoding]::UTF8.GetBytes($accountsJson))

    $serverEnv = @{
        "CTRL_SERVER_BIND_HOST" = "127.0.0.1"
        "CTRL_SERVER_PORT" = "$KikNoisePort"
        "CTRL_SERVER_TLS_PORT" = "$TlsPort"
        "CTRL_SERVER_TLS_CERT" = $cert.Cert
        "CTRL_SERVER_TLS_KEY" = $cert.Key
        "CTRL_SERVER_KIK_NOISE_PRIVATE_KEY" = $noise.Private
        "CTRL_SERVER_E2E_TRACE_PATH" = (Join-Path $E2eDir "ctrl_server_trace.log")
        "CTRL_SERVER_AUTH_SECRET" = $ControlAuthSecret
        "CTRL_SERVER_ACCOUNTS_JSON_BASE64" = $accountsBase64
        "RUST_BACKTRACE" = "1"
    }
    $kikEnv = @{ "LOG" = "DEBUG" }
    $realCtrlEnv = @{
        "REAL_CTRL_SERVER_HOST" = "127.0.0.1"
        "REAL_CTRL_TLS_PORT" = "$TlsPort"
        "REAL_CTRL_TLS_SERVER_NAME" = "real-ctrl-server"
        "REAL_CTRL_TLS_CA_CERT" = $cert.Cert
        "REAL_CTRL_TLS_SERVER_SPKI_SHA256" = $cert.Pin
        "REAL_CTRL_HTTP_LOCK_PATH" = (Join-Path $E2eDir "real_ctrl_http.lock")
        "REAL_CTRL_E2E_TRACE_PATH" = (Join-Path $E2eDir "real_ctrl_trace.log")
        "REAL_CTRL_API_TOKEN" = $ApiToken
        "REAL_CTRL_AUTH_SECRET" = $ControlAuthSecret
        "REAL_CTRL_ACCOUNT_ID" = "tenant_a"
        "REAL_CTRL_INSTANCE_ID" = "tenant-a-primary"
        "RUST_BACKTRACE" = "1"
    }

    $server = Start-E2eProcess -Name "ctrl_server" -ExePath (Join-Path $Root "target\debug\ctrl_server.exe") -EnvMap $serverEnv
    $result.started += @{ name = "ctrl_server"; pid = $server.Proc.Id }
    Wait-TcpPort -HostName "127.0.0.1" -Port $KikNoisePort -TimeoutSeconds 20
    Wait-TlsEndpoint -HostName "127.0.0.1" -Port $TlsPort -ServerName "real-ctrl-server" -CaCert $cert.Cert -TimeoutSeconds 20
    $result.assertions += "ctrl_server Kik Noise/TLS dedicated ports listening"

    # 在任何合法客户端接入前发送明文、随机和截断握手。探针连接均立即关闭，
    # 用于验证协议识别失败可回收，不制造持续的慢连接拒绝服务。
    [byte[]]$plainHttp = [Text.Encoding]::ASCII.GetBytes("GET / HTTP/1.1`r`nHost: localhost`r`n`r`n")
    [byte[]]$invalidNoise = @(0x7F, 0xFF, 0xFF, 0xFF, 0x52, 0x54, 0x43, 0x54)
    for ($i = 0; $i -lt 32; $i++) {
        Send-RawTcpProbe -Port $TlsPort -Payload $plainHttp
        Send-RawTcpProbe -Port $KikNoisePort -Payload $invalidNoise
    }
    Start-Sleep -Milliseconds 500
    if ($server.Proc.HasExited) {
        throw "ctrl_server exited after malformed transport probes"
    }
    Wait-TlsEndpoint -HostName "127.0.0.1" -Port $TlsPort -ServerName "real-ctrl-server" -CaCert $cert.Cert -TimeoutSeconds 10
    $result.assertions += "malformed plaintext and Noise probes are rejected without service loss"

    # 控制面明确只启用 TLS 1.3；即使证书可信，TLS 1.2 也不能协商成功。
    $oldErrorActionPreference = $ErrorActionPreference
    $ErrorActionPreference = "Continue"
    try {
        $tls12Output = "" | & openssl s_client `
            -tls1_2 `
            -connect "127.0.0.1:$TlsPort" `
            -servername "real-ctrl-server" `
            -CAfile $cert.Cert `
            -verify_return_error `
            -brief `
            2>&1
        $tls12ExitCode = $LASTEXITCODE
    } finally {
        $ErrorActionPreference = $oldErrorActionPreference
    }
    $tls12Output | Set-Content -LiteralPath (Join-Path $LogDir "openssl_tls12_rejection.log") -Encoding UTF8
    if ($tls12ExitCode -eq 0) {
        throw "TLS 1.2 unexpectedly negotiated on the TLS 1.3-only control port"
    }
    $result.assertions += "control port rejects TLS 1.2"

    $saturationConnections = Assert-KikSaturationDoesNotBlockTls `
        -KikPort $KikNoisePort `
        -ControlPort $TlsPort `
        -ServerName "real-ctrl-server" `
        -CaCert $cert.Cert
    if ($server.Proc.HasExited) {
        throw "ctrl_server exited during Kik connection saturation"
    }
    $result.kik_saturation_connections = $saturationConnections
    $result.assertions += "Kik connection saturation cannot consume TLS management permits"

    $wrongPinEnv = $realCtrlEnv.Clone()
    $wrongPinEnv["REAL_CTRL_TLS_SERVER_SPKI_SHA256"] = "0" * 64
    $wrongPinEnv["REAL_CTRL_E2E_TRACE_PATH"] = (Join-Path $E2eDir "wrong_pin_trace.log")
    Assert-RealCtrlRejected -Name "real_ctrl_wrong_pin_probe" -EnvMap $wrongPinEnv
    $result.assertions += "pinned TLS rejects wrong server SPKI pin"

    $wrongRoleEnv = $realCtrlEnv.Clone()
    $wrongRoleEnv["REAL_CTRL_TLS_PORT"] = "$KikNoisePort"
    $wrongRoleEnv["REAL_CTRL_E2E_TRACE_PATH"] = (Join-Path $E2eDir "wrong_role_trace.log")
    Assert-RealCtrlRejected -Name "real_ctrl_wrong_role_probe" -EnvMap $wrongRoleEnv
    $result.assertions += "Kik Noise port rejects real_ctrl TLS role"

    $wrongAccountEnv = $realCtrlEnv.Clone()
    $wrongAccountEnv["REAL_CTRL_ACCOUNT_ID"] = "tenant_b"
    $wrongAccountEnv["REAL_CTRL_INSTANCE_ID"] = "wrong-account-secret-probe"
    $wrongAccountEnv["REAL_CTRL_AUTH_SECRET"] = $ControlAuthSecret
    $wrongAccountEnv["REAL_CTRL_E2E_TRACE_PATH"] = (Join-Path $E2eDir "wrong_account_trace.log")
    Assert-RealCtrlRejected -Name "real_ctrl_wrong_account_secret_probe" -EnvMap $wrongAccountEnv
    $result.assertions += "account identity is cryptographically bound to its own secret"

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

    $validListBody = @{
        version = 1
        request_id = "http-security-valid"
        command = @{ kind = "sys_list" }
    } | ConvertTo-Json -Depth 8 -Compress
    $wrongToken = Invoke-RawHttpRequest `
        -Method "POST" `
        -Path "/api/v1/commands" `
        -Body $validListBody `
        -Headers @{ Authorization = "Bearer definitely-wrong" }
    $wrongScheme = Invoke-RawHttpRequest `
        -Method "POST" `
        -Path "/api/v1/commands" `
        -Body $validListBody `
        -Headers @{ Authorization = "Basic Zm9vOmJhcg==" }
    $headerToken = Invoke-RawHttpRequest `
        -Method "POST" `
        -Path "/api/v1/commands" `
        -Body $validListBody `
        -Headers @{ "X-Real-Ctrl-Token" = $ApiToken }
    if ($wrongToken.Status -ne 401 -or $wrongScheme.Status -ne 401 -or $headerToken.Status -ne 200) {
        throw "HTTP token variants did not fail closed: wrong=$($wrongToken.Status), scheme=$($wrongScheme.Status), header=$($headerToken.Status)"
    }
    $unauthorizedMalformed = Invoke-RawHttpRequest `
        -Method "POST" `
        -Path "/api/v1/commands" `
        -Body '{"version":1,"command":'
    if ($unauthorizedMalformed.Status -ne 401) {
        throw "HTTP authentication did not run before JSON body parsing"
    }
    $result.assertions += "wrong token and auth scheme are rejected while the dedicated token header works"

    $authorizedHeaders = @{ Authorization = "Bearer $ApiToken" }
    $malformedJson = Invoke-RawHttpRequest `
        -Method "POST" `
        -Path "/api/v1/commands" `
        -Body '{"version":1,"command":' `
        -Headers $authorizedHeaders
    $unknownKind = Invoke-RawHttpRequest `
        -Method "POST" `
        -Path "/api/v1/commands" `
        -Body '{"version":1,"request_id":"unknown-kind","command":{"kind":"not_a_command"}}' `
        -Headers $authorizedHeaders
    $unknownField = Invoke-RawHttpRequest `
        -Method "POST" `
        -Path "/api/v1/commands" `
        -Body '{"version":1,"request_id":"unknown-field","command":{"kind":"sys_now","typo":true}}' `
        -Headers $authorizedHeaders
    $wrongMethod = Invoke-RawHttpRequest `
        -Method "GET" `
        -Path "/api/v1/commands" `
        -Headers $authorizedHeaders
    foreach ($probe in @($malformedJson, $unknownKind, $unknownField, $wrongMethod)) {
        if ($probe.Status -lt 400 -or $probe.Status -ge 500) {
            throw "Malformed HTTP input did not produce a bounded 4xx response: $($probe.Status) $($probe.Body)"
        }
    }

    $unsupportedVersionBody = @{
        version = 65535
        request_id = "unsupported-version"
        command = @{ kind = "sys_now" }
    } | ConvertTo-Json -Compress
    $unsupportedVersion = Invoke-RawHttpRequest `
        -Method "POST" `
        -Path "/api/v1/commands" `
        -Body $unsupportedVersionBody `
        -Headers $authorizedHeaders
    $unsupportedVersionEnvelope = $unsupportedVersion.Body | ConvertFrom-Json
    if ($unsupportedVersion.Status -ne 200 -or
        $unsupportedVersionEnvelope.ok -or
        $unsupportedVersionEnvelope.error.code -ne "unsupported_version") {
        throw "Unsupported API version did not return the stable error envelope: $($unsupportedVersion.Body)"
    }

    $nulRequestId = Invoke-RawHttpRequest `
        -Method "POST" `
        -Path "/api/v1/commands" `
        -Body '{"version":1,"request_id":"safe\u0000forged","command":{"kind":"sys_now"}}' `
        -Headers $authorizedHeaders
    $nulEnvelope = $nulRequestId.Body | ConvertFrom-Json
    if ($nulRequestId.Status -ne 200 -or $nulEnvelope.ok -or $nulEnvelope.error.code -ne "bad_request") {
        throw "NUL request_id did not fail validation: $($nulRequestId.Body)"
    }

    $oversizedBody = '{"version":1,"request_id":"oversized","command":{"kind":"sys_now"},"padding":"' +
        ("x" * (1MB + 4096)) + '"}'
    $oversized = Invoke-RawHttpRequest `
        -Method "POST" `
        -Path "/api/v1/commands" `
        -Body $oversizedBody `
        -Headers $authorizedHeaders `
        -AllowTransportRejection
    if ($oversized.Status -ne 413 -and -not $oversized.RejectedByDisconnect) {
        throw "HTTP payload above 1 MiB was not rejected by status or transport close"
    }

    $evilOrigin = "https://attacker.invalid"
    $cors = Invoke-RawHttpRequest `
        -Method "OPTIONS" `
        -Path "/api/v1/commands" `
        -Headers @{
            Origin = $evilOrigin
            "Access-Control-Request-Method" = "POST"
        }
    if ($cors.Headers["access-control-allow-origin"] -eq $evilOrigin) {
        throw "CORS reflected an untrusted Origin"
    }
    $healthAfterHttpProbes = Invoke-RawHttpRequest -Method "GET" -Path "/api/health"
    if ($healthAfterHttpProbes.Status -ne 200 -or $healthAfterHttpProbes.Body -ne "OK") {
        throw "HTTP service did not recover after malformed input probes"
    }
    $result.assertions += "malformed JSON, schema drift, NUL, oversized body, wrong method and hostile CORS fail closed"

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

    $autoNow = Invoke-ApiCommand -Command @{ kind = "sys_now" } -RequestId "sys-now-auto"
    if (-not ($autoNow.ok -and
        $autoNow.data.kind -eq "sys_now" -and
        [string]$autoNow.data.value.Kik.id -eq $kikId)) {
        throw "First online Kik was not selected automatically: $($autoNow | ConvertTo-Json -Depth 12 -Compress)"
    }
    $result.assertions += "first accessible online Kik is selected automatically"

    $history = Invoke-ApiCommand -Command @{ kind = "sys_history"; kik_id = $kikId } -RequestId "sys-history"
    if (-not ($history.ok -and $history.data.kind -eq "sys_history" -and $history.data.items.Count -eq 1 -and $history.data.items[0].online)) {
        throw "sys_history did not return the online Kik: $($history | ConvertTo-Json -Depth 12 -Compress)"
    }
    $result.assertions += "sys_history returns recent Kik online state"

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

    # 命名管道与 HTTP 共用 ApiRequest/ApiResponse 契约。畸形 magic 和声明超限长度必须
    # 返回结构化错误；随后同一管道入口仍应能执行合法请求。
    [byte[]]$invalidPipePayload = [Text.Encoding]::UTF8.GetBytes('{"version":1}')
    $invalidPipe = Invoke-PipePayload -Payload $invalidPipePayload
    if ($invalidPipe.ok -or $invalidPipe.error.code -ne "bad_request") {
        throw "Named pipe accepted a request without RTCAPI1 magic"
    }
    $oversizedPipe = Invoke-PipePayload -Payload ([byte[]]@()) -DeclaredLength (1MB + 1)
    if ($oversizedPipe.ok -or $oversizedPipe.error.code -ne "payload_too_large") {
        throw "Named pipe did not reject an oversized declared request"
    }
    $pipeJson = @{
        version = 1
        request_id = "pipe-valid-after-attacks"
        command = @{ kind = "sys_now" }
    } | ConvertTo-Json -Depth 8 -Compress
    [byte[]]$validPipePayload = [Text.Encoding]::UTF8.GetBytes("RTCAPI1`0$pipeJson")
    $validPipe = Invoke-PipePayload -Payload $validPipePayload
    if (-not $validPipe.ok -or $validPipe.request_id -ne "pipe-valid-after-attacks") {
        throw "Named pipe did not recover after malformed requests"
    }
    $result.assertions += "named pipe rejects bad magic and oversized frames, then recovers"

    $pipeConcurrency = Start-E2eProcess `
        -Name "pipe_concurrency_probe" `
        -ExePath (Join-Path $Root "target\debug\examples\pipe_concurrency_probe.exe") `
        -EnvMap @{}
    if (-not $pipeConcurrency.Proc.WaitForExit(12000)) {
        throw "Named-pipe concurrency probe timed out"
    }
    if ($pipeConcurrency.Proc.ExitCode -ne 0) {
        throw "Named-pipe concurrency probe failed"
    }
    $pipeConcurrencyResult = $pipeConcurrency.StdOutTask.Result.Trim() | ConvertFrom-Json
    if ($pipeConcurrencyResult.completed -ne 12 -or $pipeConcurrencyResult.elapsed_ms -ge 8000) {
        throw "Named-pipe concurrency probe returned an invalid report"
    }
    $result.parallel_pipe_commands = [int]$pipeConcurrencyResult.completed
    $result.parallel_pipe_ms = [int]$pipeConcurrencyResult.elapsed_ms
    $result.assertions += "twelve named-pipe clients execute concurrently without response crossover"

    $sameAccountEnv = $realCtrlEnv.Clone()
    $sameAccountEnv["REAL_CTRL_HTTP_PORT"] = "$SameAccountHttpPort"
    $sameAccountEnv["REAL_CTRL_HTTP_LOCK_PATH"] = (Join-Path $E2eDir "real_ctrl_http_same_account.lock")
    $sameAccountEnv["REAL_CTRL_INSTANCE_ID"] = "tenant-a-secondary"
    $sameAccount = Start-E2eProcess `
        -Name "real_ctrl_same_account_instance" `
        -ExePath (Join-Path $Root "target\debug\real_ctrl_invoker_http_service.exe") `
        -EnvMap $sameAccountEnv
    $result.started += @{ name = "real_ctrl_same_account_instance"; pid = $sameAccount.Proc.Id }
    Wait-TcpPort -HostName "127.0.0.1" -Port $SameAccountHttpPort -TimeoutSeconds 30
    $sameList = Invoke-ApiCommand @{ kind = "sys_list" } "same-account-list" $SameAccountHttpPort
    $sameAutoNow = Invoke-ApiCommand @{ kind = "sys_now" } "same-account-auto-now" $SameAccountHttpPort
    $sameUse = Invoke-ApiCommand @{ kind = "sys_use"; kik_id = $kikId } "same-account-use" $SameAccountHttpPort
    $primaryStillAlive = Invoke-ApiCommand @{ kind = "sys_now" } "primary-after-secondary" $HttpPort
    if (-not ($sameList.ok -and
        $sameAutoNow.ok -and
        [string]$sameAutoNow.data.value.Kik.id -eq $kikId -and
        $sameUse.ok -and
        $primaryStillAlive.ok)) {
        throw "Same-account control instances did not remain independently active"
    }
    $result.assertions += "same account instances independently auto-select an accessible Kik"

    $secondAccountEnv = $realCtrlEnv.Clone()
    $secondAccountEnv["REAL_CTRL_HTTP_PORT"] = "$SecondAccountHttpPort"
    $secondAccountEnv["REAL_CTRL_HTTP_LOCK_PATH"] = (Join-Path $E2eDir "real_ctrl_http_second_account.lock")
    $secondAccountEnv["REAL_CTRL_ACCOUNT_ID"] = "tenant_b"
    $secondAccountEnv["REAL_CTRL_INSTANCE_ID"] = "tenant-b-primary"
    $secondAccountEnv["REAL_CTRL_AUTH_SECRET"] = $SecondAccountSecret
    $secondAccount = Start-E2eProcess `
        -Name "real_ctrl_second_account_instance" `
        -ExePath (Join-Path $Root "target\debug\real_ctrl_invoker_http_service.exe") `
        -EnvMap $secondAccountEnv
    $result.started += @{ name = "real_ctrl_second_account_instance"; pid = $secondAccount.Proc.Id }
    Wait-TcpPort -HostName "127.0.0.1" -Port $SecondAccountHttpPort -TimeoutSeconds 30
    $secondList = Invoke-ApiCommand @{ kind = "sys_list" } "second-account-list" $SecondAccountHttpPort
    $secondAutoNow = Invoke-ApiCommand @{ kind = "sys_now" } "second-account-auto-now" $SecondAccountHttpPort
    $secondUse = Invoke-ApiCommand @{ kind = "sys_use"; kik_id = $kikId } "second-account-use" $SecondAccountHttpPort
    $secondNow = Invoke-ApiCommand @{ kind = "sys_now" } "second-account-now" $SecondAccountHttpPort
    if (-not ($secondList.ok -and
        $secondAutoNow.ok -and
        [string]$secondAutoNow.data.value.Kik.id -eq $kikId -and
        $secondUse.ok -and
        $secondNow.ok)) {
        throw "Second control account could not establish an independent instance"
    }
    $result.assertions += "multiple accounts independently auto-select an authorized Kik"

    # 三个真实控制实例同时运行耗时命令，覆盖同账号多实例和多账号多实例的服务端隔离。
    # 12 个请求均小于 Kik 级 16 许可；若任何层仍是全局串行，耗时会接近 24 秒。
    $multiInstanceTargets = @()
    foreach ($target in @(
        @{ Prefix = "tenant-a-primary"; Port = $HttpPort },
        @{ Prefix = "tenant-a-secondary"; Port = $SameAccountHttpPort },
        @{ Prefix = "tenant-b-primary"; Port = $SecondAccountHttpPort }
    )) {
        for ($i = 0; $i -lt 4; $i++) {
            $requestId = "$($target.Prefix)-concurrent-$i"
            $multiInstanceTargets += [pscustomobject]@{
                Port = $target.Port
                RequestId = $requestId
                Command = @{
                    kind = "exec"
                    command = "ping -n 3 127.0.0.1 >NUL && echo $requestId"
                }
            }
        }
    }
    $multiInstanceWatch = [System.Diagnostics.Stopwatch]::StartNew()
    $multiInstanceResponses = Invoke-ParallelApiTargets -Targets $multiInstanceTargets
    $multiInstanceWatch.Stop()
    for ($i = 0; $i -lt $multiInstanceTargets.Count; $i++) {
        $expected = [string]$multiInstanceTargets[$i].RequestId
        $actual = $multiInstanceResponses[$i]
        if (-not $actual.ok -or $actual.request_id -ne $expected -or $actual.data.message -notmatch [regex]::Escape($expected)) {
            throw "Multi-instance response isolation failed for $expected"
        }
    }
    if ($multiInstanceWatch.ElapsedMilliseconds -ge 8000) {
        throw "Multi-account/instance commands appear serialized: $($multiInstanceWatch.ElapsedMilliseconds) ms"
    }
    $result.multi_instance_parallel_commands = $multiInstanceTargets.Count
    $result.multi_instance_parallel_ms = $multiInstanceWatch.ElapsedMilliseconds
    $result.assertions += "same-account and cross-account instances execute concurrently without response crossover"

    $missingRemote = Join-Path $E2eDir "missing-download-source.bin"
    $missingLocal = Join-Path $E2eDir "missing-download-target.bin"
    Remove-Item -LiteralPath $missingRemote, $missingLocal -Force -ErrorAction SilentlyContinue
    $missingDownload = Invoke-ApiCommand -Command @{
        kind = "ctrl_get_file"
        remote_path = $missingRemote
        local_path = $missingLocal
    } -RequestId "missing-download"
    if ($missingDownload.ok -or
        $missingDownload.error.message -match "不匹配的数据关联 ID") {
        throw "Kik file read error was incorrectly reported as a data-id mismatch"
    }
    $result.assertions += "download business errors preserve the real Kik error instead of an ID mismatch"

    $ls = Invoke-ApiCommand -Command @{ kind = "ctrl_ls"; path = $E2eDir } -RequestId "ctrl-ls"
    if (-not ($ls.ok -and $ls.data.kind -eq "ls" -and $ls.data.entries.Count -gt 0)) {
        throw "ctrl_ls failed: $($ls | ConvertTo-Json -Depth 12 -Compress)"
    }
    $result.ls_entry_count = $ls.data.entries.Count
    $result.assertions += "ctrl_ls through ctrl_server and ctrl_kik ok"

    # 六个各等待约两秒的命令若串行至少需要约十二秒；阈值给慢 CI 留出余量，
    # 同时能稳定识别旧的全局单命令门禁。
    $parallelCommands = @(0..5 | ForEach-Object {
        @{ kind = "exec"; command = "ping -n 3 127.0.0.1 >NUL && echo parallel-ok" }
    })
    $parallelWatch = [System.Diagnostics.Stopwatch]::StartNew()
    $parallelResponses = Invoke-ParallelApiCommands -Commands $parallelCommands
    $parallelWatch.Stop()
    if ($parallelResponses.Count -ne 6 -or
        @($parallelResponses | Where-Object { -not $_.ok -or $_.data.message -notmatch "parallel-ok" }).Count -ne 0) {
        throw "Parallel HTTP command responses were incomplete or mismatched"
    }
    for ($i = 0; $i -lt $parallelResponses.Count; $i++) {
        if ($parallelResponses[$i].request_id -ne "parallel-$i") {
            throw "Parallel HTTP response request_id crossed over at index $i"
        }
    }
    if ($parallelWatch.ElapsedMilliseconds -ge 8000) {
        throw "HTTP commands appear serialized: $($parallelWatch.ElapsedMilliseconds) ms"
    }
    $result.parallel_http_commands = 6
    $result.parallel_http_ms = $parallelWatch.ElapsedMilliseconds
    $result.assertions += "six HTTP commands execute independently in parallel"

    # 突发数同时超过本地 command gate 32 和服务端单实例/Kik 16。系统应快速拒绝超额请求，
    # 而不是无界排队、超时或把响应投递给错误的调用方。
    $burstCommands = @(0..47 | ForEach-Object {
        @{ kind = "exec"; command = "ping -n 4 127.0.0.1 >NUL && echo burst-ok" }
    })
    $burstWatch = [System.Diagnostics.Stopwatch]::StartNew()
    $burstResponses = Invoke-ParallelApiCommands `
        -Commands $burstCommands `
        -RequestIdPrefix "burst" `
        -TimeoutMilliseconds 15000
    $burstWatch.Stop()
    if ($burstResponses.Count -ne 48) {
        throw "Burst test returned $($burstResponses.Count) of 48 responses"
    }
    for ($i = 0; $i -lt $burstResponses.Count; $i++) {
        if ($burstResponses[$i].request_id -ne "burst-$i") {
            throw "Burst response request_id crossed over at index $i"
        }
    }
    $burstSucceeded = @($burstResponses | Where-Object { $_.ok -and $_.data.message -match "burst-ok" }).Count
    $burstRejected = @($burstResponses | Where-Object { -not $_.ok -and $null -ne $_.error.code }).Count
    if ($burstSucceeded -lt 8 -or $burstSucceeded -gt 16 -or $burstSucceeded + $burstRejected -ne 48) {
        throw "Burst quota result is outside the expected bounded behavior: success=$burstSucceeded rejected=$burstRejected"
    }
    if ($burstWatch.ElapsedMilliseconds -ge 10000) {
        throw "Burst requests were queued instead of bounded: $($burstWatch.ElapsedMilliseconds) ms"
    }
    $afterBurst = Invoke-ApiCommand -Command @{ kind = "sys_now" } -RequestId "after-burst"
    if (-not $afterBurst.ok) {
        throw "HTTP control path did not recover after quota saturation"
    }
    $result.burst_commands = 48
    $result.burst_succeeded = $burstSucceeded
    $result.burst_rejected = $burstRejected
    $result.burst_ms = $burstWatch.ElapsedMilliseconds
    $result.assertions += "48-request burst is bounded by permits and recovers without correlation loss"

    # 分批发出 100 个轻量命令，检查长期使用中的许可归还和 request_id 路由。
    $stabilityCompleted = 0
    for ($batch = 0; $batch -lt 10; $batch++) {
        $batchResponses = Invoke-ParallelApiCommands `
            -Commands @(0..9 | ForEach-Object { @{ kind = "sys_now" } }) `
            -RequestIdPrefix "stability-$batch" `
            -TimeoutMilliseconds 10000
        for ($i = 0; $i -lt $batchResponses.Count; $i++) {
            if (-not $batchResponses[$i].ok -or
                $batchResponses[$i].request_id -ne "stability-$batch-$i") {
                throw "Stability command failed or crossed responses at batch=$batch index=$i"
            }
            $stabilityCompleted++
        }
    }
    if ($stabilityCompleted -ne 100) {
        throw "Stability loop completed only $stabilityCompleted commands"
    }
    $result.stability_commands = $stabilityCompleted
    $result.assertions += "100-command stability loop releases permits and preserves request correlation"

    # 六个独立文件并行上传再并行下载，验证数据连接轮转、内部 wire ID 改写和乱序落盘隔离。
    $smallFileDir = Join-Path $E2eDir "parallel-files"
    New-Item -ItemType Directory -Force -Path $smallFileDir | Out-Null
    $smallFiles = @()
    for ($i = 0; $i -lt 6; $i++) {
        $source = Join-Path $smallFileDir "source-$i.bin"
        $remote = Join-Path $smallFileDir "remote-$i.bin"
        $download = Join-Path $smallFileDir "download-$i.bin"
        [byte[]]$content = New-Object byte[] (256KB)
        [System.Random]::new(7000 + $i).NextBytes($content)
        [IO.File]::WriteAllBytes($source, $content)
        Remove-Item -LiteralPath $remote, $download -Force -ErrorAction SilentlyContinue
        $smallFiles += [pscustomobject]@{ Source = $source; Remote = $remote; Download = $download }
    }
    $uploadResponses = Invoke-ParallelApiCommands `
        -Commands @($smallFiles | ForEach-Object {
            @{ kind = "ctrl_set_file"; local_path = $_.Source; remote_path = $_.Remote }
        }) `
        -RequestIdPrefix "parallel-upload" `
        -TimeoutMilliseconds 20000
    if (@($uploadResponses | Where-Object { -not $_.ok }).Count -ne 0) {
        throw "One or more parallel small-file uploads failed"
    }
    $downloadResponses = Invoke-ParallelApiCommands `
        -Commands @($smallFiles | ForEach-Object {
            @{ kind = "ctrl_get_file"; remote_path = $_.Remote; local_path = $_.Download }
        }) `
        -RequestIdPrefix "parallel-download" `
        -TimeoutMilliseconds 20000
    if (@($downloadResponses | Where-Object { -not $_.ok }).Count -ne 0) {
        throw "One or more parallel small-file downloads failed"
    }
    foreach ($fileSet in $smallFiles) {
        $sourceHash = (Get-FileHash -Algorithm SHA256 -LiteralPath $fileSet.Source).Hash
        $remoteHash = (Get-FileHash -Algorithm SHA256 -LiteralPath $fileSet.Remote).Hash
        $downloadHash = (Get-FileHash -Algorithm SHA256 -LiteralPath $fileSet.Download).Hash
        if ($sourceHash -ne $remoteHash -or $sourceHash -ne $downloadHash) {
            throw "Parallel file SHA-256 mismatch for $($fileSet.Source)"
        }
    }
    $result.parallel_file_roundtrips = $smallFiles.Count
    $result.assertions += "six concurrent file roundtrips preserve per-request SHA-256"

    # 12 MiB 覆盖多个 4 MiB 分片，同时保持 CI 运行时间可控。上传和下载都必须经过
    # real_ctrl -> ctrl_server -> ctrl_kik 数据通道，不能用同机文件存在替代协议验收。
    $bigSourcePath = Join-Path $E2eDir "big_source.bin"
    $bigRemotePath = Join-Path $E2eDir "big_remote.bin"
    $bigDownloadedPath = Join-Path $E2eDir "big_downloaded.bin"
    $buffer = New-Object byte[] (1024 * 1024)
    $random = [System.Random]::new(20260712)
    $random.NextBytes($buffer)
    $file = [System.IO.File]::Open($bigSourcePath, [System.IO.FileMode]::Create, [System.IO.FileAccess]::Write, [System.IO.FileShare]::Read)
    try {
        for ($i = 0; $i -lt 12; $i++) {
            $file.Write($buffer, 0, $buffer.Length)
        }
        $file.Flush($true)
    } finally {
        $file.Dispose()
    }

    $uploadWatch = [System.Diagnostics.Stopwatch]::StartNew()
    $setBig = Invoke-ApiCommand -Command @{
        kind = "ctrl_set_big_file"
        local_path = $bigSourcePath
        remote_path = $bigRemotePath
    } -RequestId "ctrl-set-big-file"
    $uploadWatch.Stop()
    if (-not $setBig.ok) {
        throw "ctrl_set_big_file failed: $($setBig | ConvertTo-Json -Depth 12 -Compress)"
    }

    $downloadWatch = [System.Diagnostics.Stopwatch]::StartNew()
    $getBig = Invoke-ApiCommand -Command @{
        kind = "ctrl_get_big_file"
        remote_path = $bigRemotePath
        local_path = $bigDownloadedPath
    } -RequestId "ctrl-get-big-file"
    $downloadWatch.Stop()
    if (-not $getBig.ok) {
        throw "ctrl_get_big_file failed: $($getBig | ConvertTo-Json -Depth 12 -Compress)"
    }
    $sourceHash = (Get-FileHash -Algorithm SHA256 -LiteralPath $bigSourcePath).Hash
    $remoteHash = (Get-FileHash -Algorithm SHA256 -LiteralPath $bigRemotePath).Hash
    $downloadedHash = (Get-FileHash -Algorithm SHA256 -LiteralPath $bigDownloadedPath).Hash
    if ($sourceHash -ne $remoteHash -or $sourceHash -ne $downloadedHash) {
        throw "Big file SHA-256 mismatch after upload/download"
    }
    $result.big_file_bytes = (Get-Item -LiteralPath $bigSourcePath).Length
    $result.big_file_upload_ms = $uploadWatch.ElapsedMilliseconds
    $result.big_file_download_ms = $downloadWatch.ElapsedMilliseconds
    $result.assertions += "12 MiB chunked upload/download preserves SHA-256"

    # 主连接和数据连接都退出后才应记为下线；轮询验证真实清理链路而非直接调用状态方法。
    try { $kik.Proc.Kill($true) } catch { $kik.Proc.Kill() }
    $kik.Proc.WaitForExit(5000) | Out-Null
    $offlineHistory = $null
    for ($i = 0; $i -lt 40; $i++) {
        Start-Sleep -Milliseconds 250
        $offlineHistory = Invoke-ApiCommand `
            -Command @{ kind = "sys_history"; kik_id = $kikId } `
            -RequestId "sys-history-offline-$i"
        if ($offlineHistory.ok -and
            $offlineHistory.data.items.Count -eq 1 -and
            -not $offlineHistory.data.items[0].online -and
            $null -ne $offlineHistory.data.items[0].recent_offline_unix_ms) {
            break
        }
    }
    if (-not ($offlineHistory.ok -and
        $offlineHistory.data.items.Count -eq 1 -and
        -not $offlineHistory.data.items[0].online -and
        $null -ne $offlineHistory.data.items[0].recent_offline_unix_ms)) {
        throw "sys_history did not observe Kik offline transition: $($offlineHistory | ConvertTo-Json -Depth 12 -Compress)"
    }
    $result.assertions += "sys_history records the real Kik offline transition"

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
