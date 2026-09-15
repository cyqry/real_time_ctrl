[CmdletBinding()]
param(
    [ValidateNotNullOrEmpty()]
    [string]$ServerAddress = "ytycc.com",

    [ValidateNotNullOrEmpty()]
    [string]$TlsServerName = "ytycc.com",

    [ValidateRange(1, 65535)]
    [int]$KikNoisePort = 9002,

    [ValidateRange(1, 65535)]
    [int]$ControlTlsPort = 9007,

    [ValidateRange(1, 3650)]
    [int]$CertificateDays = 825,

    [switch]$Force
)

$ErrorActionPreference = "Stop"
Set-StrictMode -Version Latest

function Assert-LastExitCode {
    param(
        [int]$ExitCode,
        [string]$Message
    )

    if ($ExitCode -ne 0) {
        throw "$Message (exit code: $ExitCode)"
    }
}

function New-RandomToken {
    param([ValidateRange(32, 128)][int]$ByteCount = 48)

    $bytes = New-Object byte[] $ByteCount
    $rng = [System.Security.Cryptography.RandomNumberGenerator]::Create()
    try {
        $rng.GetBytes($bytes)
    } finally {
        $rng.Dispose()
    }

    # Base64URL 只包含环境变量、IDE 配置和 PowerShell 单引号字符串均可安全承载的字符。
    return [Convert]::ToBase64String($bytes).TrimEnd('=').Replace('+', '-').Replace('/', '_')
}

function ConvertTo-PowerShellSingleQuotedLiteral {
    param([string]$Value)
    return "'" + $Value.Replace("'", "''") + "'"
}

function Write-EnvironmentScript {
    param(
        [string]$Path,
        [System.Collections.Specialized.OrderedDictionary]$Values,
        [string]$Description
    )

    $lines = @(
        "# 此文件由 scripts/generate_production_identity.ps1 自动生成。",
        "# $Description",
        "# 包含生产秘密，不得提交版本库、发送到聊天工具或打包进安装程序。"
    )
    foreach ($entry in $Values.GetEnumerator()) {
        $literal = ConvertTo-PowerShellSingleQuotedLiteral ([string]$entry.Value)
        $lines += ('$env:{0} = {1}' -f $entry.Key, $literal)
    }
    $lines | Set-Content -LiteralPath $Path -Encoding UTF8
}

function Write-IdeaEnvironmentLine {
    param(
        [string]$Path,
        [System.Collections.Specialized.OrderedDictionary]$Values
    )

    $parts = foreach ($entry in $Values.GetEnumerator()) {
        "$($entry.Key)=$($entry.Value)"
    }
    ($parts -join ';') | Set-Content -LiteralPath $Path -Encoding UTF8
}

function Protect-SensitiveFile {
    param([string]$Path)

    # 使用 SID 避免 Windows 语言不同导致账户名解析失败；移除继承，只允许当前用户和 LocalSystem 访问。
    $currentUserSid = [System.Security.Principal.WindowsIdentity]::GetCurrent().User.Value
    $currentUserGrant = '*{0}:(F)' -f $currentUserSid
    & icacls.exe $Path /inheritance:r /grant:r $currentUserGrant "*S-1-5-18:(F)" | Out-Null
    Assert-LastExitCode $LASTEXITCODE "Failed to restrict sensitive file ACL: $Path"
}

function Get-SubjectAlternativeNameLines {
    param(
        [string]$Address,
        [string]$ServerName
    )

    $lines = @("DNS.1 = $ServerName")
    $parsedIp = $null
    if ([System.Net.IPAddress]::TryParse($Address, [ref]$parsedIp)) {
        $lines += "IP.1 = $Address"
    } elseif ($Address -ne $ServerName) {
        $lines += "DNS.2 = $Address"
    }
    return $lines
}

$repoRoot = [System.IO.Path]::GetFullPath((Join-Path $PSScriptRoot ".."))
$prodDir = Join-Path $repoRoot "deploy\standalone"
$certDir = Join-Path $prodDir "certs"
$envDir = Join-Path $prodDir "env"
$certPath = Join-Path $certDir "server.crt"
$keyPath = Join-Path $certDir "server.key"
$pubKeyPath = Join-Path $certDir "server.pubkey.pem"
$spkiPath = Join-Path $certDir "server.spki.der"
$noisePrivatePemPath = Join-Path $certDir "kik-noise-private.pem"
$noisePrivateDerPath = Join-Path $certDir "kik-noise-private.der"
$noisePublicDerPath = Join-Path $certDir "kik-noise-public.der"
$openSslConfigPath = Join-Path $certDir "openssl_real_ctrl.cnf"
$ctrlServerEnvPath = Join-Path $envDir "ctrl_server.env.ps1"
$realCtrlEnvPath = Join-Path $envDir "real_ctrl.env.ps1"
$allEnvPath = Join-Path $envDir "production.env.ps1"
$ideaCtrlServerPath = Join-Path $envDir "idea_ctrl_server.txt"
$ideaRealCtrlPath = Join-Path $envDir "idea_real_ctrl.txt"
$ctrlKikBuildEnvPath = Join-Path $envDir "ctrl_kik_build.env.ps1"
$compiledDefaultsPath = Join-Path $envDir "compiled_defaults.env.ps1"
$manifestPath = Join-Path $prodDir "identity_manifest.txt"
$readmePath = Join-Path $prodDir "README.txt"

$sensitiveOutputs = @(
    $keyPath,
    $noisePrivatePemPath,
    $noisePrivateDerPath,
    $ctrlServerEnvPath,
    $realCtrlEnvPath,
    $allEnvPath,
    $ideaCtrlServerPath,
    $ideaRealCtrlPath,
    $compiledDefaultsPath
)
$existingOutputs = @($sensitiveOutputs | Where-Object { Test-Path -LiteralPath $_ })
if ($existingOutputs.Count -gt 0 -and -not $Force) {
    throw "Production identity already exists. Use -Force only for an intentional rotation; old pins and secrets will stop working.`n$($existingOutputs -join "`n")"
}

$openSsl = Get-Command openssl -ErrorAction SilentlyContinue
if ($null -eq $openSsl) {
    throw "OpenSSL was not found. Install OpenSSL 3.x and add openssl.exe to PATH."
}

New-Item -ItemType Directory -Force -Path $certDir, $envDir | Out-Null

$sanLines = Get-SubjectAlternativeNameLines -Address $ServerAddress -ServerName $TlsServerName
$openSslConfig = @(
    "[req]",
    "default_bits = 3072",
    "prompt = no",
    "default_md = sha256",
    "distinguished_name = dn",
    "x509_extensions = v3_server",
    "",
    "[dn]",
    "CN = $TlsServerName",
    "",
    "[v3_server]",
    "basicConstraints = critical, CA:FALSE",
    "keyUsage = critical, digitalSignature, keyEncipherment",
    "extendedKeyUsage = serverAuth",
    "subjectKeyIdentifier = hash",
    "authorityKeyIdentifier = keyid,issuer",
    "subjectAltName = @alt_names",
    "",
    "[alt_names]"
) + $sanLines
$openSslConfig | Set-Content -LiteralPath $openSslConfigPath -Encoding ASCII

& $openSsl.Source req -x509 -newkey rsa:3072 -nodes -sha256 -days $CertificateDays `
    -keyout $keyPath -out $certPath -config $openSslConfigPath -extensions v3_server
Assert-LastExitCode $LASTEXITCODE "Failed to generate TLS certificate"

& $openSsl.Source genpkey -algorithm X25519 -out $noisePrivatePemPath
Assert-LastExitCode $LASTEXITCODE "Failed to generate Kik Noise private key"
& $openSsl.Source pkey -in $noisePrivatePemPath -outform DER -out $noisePrivateDerPath
Assert-LastExitCode $LASTEXITCODE "Failed to export Kik Noise private key"
& $openSsl.Source pkey -in $noisePrivatePemPath -pubout -outform DER -out $noisePublicDerPath
Assert-LastExitCode $LASTEXITCODE "Failed to export Kik Noise public key"

$noisePrivateBytes = [IO.File]::ReadAllBytes($noisePrivateDerPath)
$noisePublicBytes = [IO.File]::ReadAllBytes($noisePublicDerPath)
if ($noisePrivateBytes.Count -lt 32 -or $noisePublicBytes.Count -lt 32) {
    throw "OpenSSL X25519 DER output is unexpectedly short"
}
$noisePrivate = [Convert]::ToBase64String(
    [byte[]]$noisePrivateBytes[($noisePrivateBytes.Count - 32)..($noisePrivateBytes.Count - 1)]
)
$noisePublic = [Convert]::ToBase64String(
    [byte[]]$noisePublicBytes[($noisePublicBytes.Count - 32)..($noisePublicBytes.Count - 1)]
)

& $openSsl.Source x509 -in $certPath -pubkey -noout -out $pubKeyPath
Assert-LastExitCode $LASTEXITCODE "Failed to export TLS public key"
& $openSsl.Source pkey -pubin -in $pubKeyPath -outform DER -out $spkiPath
Assert-LastExitCode $LASTEXITCODE "Failed to export SPKI DER"
$hashOutput = & $openSsl.Source dgst -sha256 $spkiPath
Assert-LastExitCode $LASTEXITCODE "Failed to calculate SPKI SHA-256 pin"
$spkiPin = ($hashOutput -replace '^.*=\s*', '').Trim().ToLowerInvariant()
if ($spkiPin -notmatch '^[0-9a-f]{64}$') {
    throw "OpenSSL returned an invalid SPKI pin: $spkiPin"
}

$controlAuthSecret = New-RandomToken -ByteCount 48
$apiToken = New-RandomToken -ByteCount 48
# 运行锁可在异常退出后重建，不应与证书、构建输入一起长期保存。
$httpLockPath = Join-Path ([IO.Path]::GetTempPath()) "real_ctrl-http-production.lock"

$ctrlServerValues = [ordered]@{
    CTRL_SERVER_BIND_HOST = "0.0.0.0"
    CTRL_SERVER_PORT = [string]$KikNoisePort
    CTRL_SERVER_TLS_PORT = [string]$ControlTlsPort
    CTRL_SERVER_TLS_CERT = $certPath
    CTRL_SERVER_TLS_KEY = $keyPath
    CTRL_SERVER_KIK_NOISE_PRIVATE_KEY = $noisePrivate
    CTRL_SERVER_AUTH_SECRET = $controlAuthSecret
    CTRL_SERVER_ALLOW_EXEC = "1"
    RUST_BACKTRACE = "1"
}
$realCtrlValues = [ordered]@{
    REAL_CTRL_SERVER_HOST = $ServerAddress
    REAL_CTRL_TLS_PORT = [string]$ControlTlsPort
    REAL_CTRL_TLS_SERVER_NAME = $TlsServerName
    REAL_CTRL_TLS_CA_CERT = $certPath
    REAL_CTRL_TLS_SERVER_SPKI_SHA256 = $spkiPin
    REAL_CTRL_AUTH_SECRET = $controlAuthSecret
    REAL_CTRL_ACCOUNT_ID = "default"
    REAL_CTRL_API_TOKEN = $apiToken
    REAL_CTRL_API_ALLOW_EXEC = "1"
    REAL_CTRL_HTTP_LOCK_PATH = $httpLockPath
    RUST_BACKTRACE = "1"
}
$ctrlKikBuildValues = [ordered]@{
    RTC_CTRL_KIK_BUILD_HOST = $ServerAddress
    RTC_CTRL_KIK_BUILD_PORT = [string]$KikNoisePort
    RTC_CTRL_KIK_NOISE_SERVER_PUBLIC_KEY = $noisePublic
    RTC_CTRL_KIK_BUILD_CHANNEL = "production"
}
$compiledDefaultValues = [ordered]@{
    RTC_REAL_CTRL_BUILD_SERVER_HOST = $ServerAddress
    RTC_REAL_CTRL_BUILD_TLS_PORT = [string]$ControlTlsPort
    RTC_REAL_CTRL_BUILD_TLS_SERVER_NAME = $TlsServerName
    RTC_REAL_CTRL_BUILD_TLS_CA_PEM_BASE64 = [Convert]::ToBase64String(
        [IO.File]::ReadAllBytes($certPath)
    )
    RTC_REAL_CTRL_BUILD_TLS_SPKI_SHA256 = $spkiPin
    RTC_REAL_CTRL_BUILD_HTTP_LOCK_PATH = "real_ctrl-http-production.lock"
    RTC_REAL_CTRL_BUILD_HTTP_BINDING = "127.0.0.1"
    RTC_REAL_CTRL_BUILD_HTTP_PORT = "9000"
    RTC_REAL_CTRL_BUILD_ACCOUNT_ID = "default"
    RTC_REAL_CTRL_BUILD_INSTANCE_ID = ""
    RTC_REAL_CTRL_BUILD_AUTH_SECRET = $controlAuthSecret
    RTC_REAL_CTRL_BUILD_API_TOKEN = $apiToken
    RTC_REAL_CTRL_BUILD_API_ALLOW_EXEC = "1"
    RTC_CTRL_SERVER_BUILD_BIND_HOST = "0.0.0.0"
    RTC_CTRL_SERVER_BUILD_KIK_PORT = [string]$KikNoisePort
    RTC_CTRL_SERVER_BUILD_TLS_PORT = [string]$ControlTlsPort
    RTC_CTRL_SERVER_BUILD_TLS_CERT_PEM_BASE64 = [Convert]::ToBase64String(
        [IO.File]::ReadAllBytes($certPath)
    )
    RTC_CTRL_SERVER_BUILD_TLS_KEY_PEM_BASE64 = [Convert]::ToBase64String(
        [IO.File]::ReadAllBytes($keyPath)
    )
    RTC_CTRL_SERVER_BUILD_AUTH_SECRET = $controlAuthSecret
    RTC_CTRL_SERVER_BUILD_ACCOUNTS_JSON_BASE64 = ""
    RTC_CTRL_SERVER_BUILD_KIK_NOISE_PRIVATE_KEY = $noisePrivate
    RTC_CTRL_SERVER_BUILD_ALLOW_EXEC = "1"
}
$allValues = [ordered]@{}
foreach ($entry in $ctrlServerValues.GetEnumerator()) {
    $allValues[$entry.Key] = $entry.Value
}
foreach ($entry in $realCtrlValues.GetEnumerator()) {
    $allValues[$entry.Key] = $entry.Value
}
foreach ($entry in $ctrlKikBuildValues.GetEnumerator()) {
    $allValues[$entry.Key] = $entry.Value
}

Write-EnvironmentScript -Path $ctrlServerEnvPath -Values $ctrlServerValues -Description "Load this file for the ctrl_server process."
Write-EnvironmentScript -Path $realCtrlEnvPath -Values $realCtrlValues -Description "Load this file for real_ctrl or real_ctrl_invoker_http_service."
Write-EnvironmentScript -Path $ctrlKikBuildEnvPath -Values $ctrlKikBuildValues -Description "Load this file only while building ctrl_kik."
Write-EnvironmentScript -Path $compiledDefaultsPath -Values $compiledDefaultValues -Description "Load this file only while building direct-run production binaries."
Write-EnvironmentScript -Path $allEnvPath -Values $allValues -Description "Loads all values for local integration. Production services should use per-process files."
Write-IdeaEnvironmentLine -Path $ideaCtrlServerPath -Values $ctrlServerValues
Write-IdeaEnvironmentLine -Path $ideaRealCtrlPath -Values $realCtrlValues

$notBefore = (& $openSsl.Source x509 -in $certPath -noout -startdate) -replace '^notBefore=', ''
Assert-LastExitCode $LASTEXITCODE "Failed to read certificate start date"
$notAfter = (& $openSsl.Source x509 -in $certPath -noout -enddate) -replace '^notAfter=', ''
Assert-LastExitCode $LASTEXITCODE "Failed to read certificate expiration date"
$certFingerprint = (& $openSsl.Source x509 -in $certPath -noout -fingerprint -sha256) -replace '^sha256 Fingerprint=', ''
Assert-LastExitCode $LASTEXITCODE "Failed to read certificate fingerprint"

@(
    "real_time_ctrl production identity",
    "generated_at=$(Get-Date -Format o)",
    "server_address=$ServerAddress",
    "tls_server_name=$TlsServerName",
    "kik_noise_port=$KikNoisePort",
    "control_tls_port=$ControlTlsPort",
    "kik_noise_public_key=$noisePublic",
    "certificate_not_before=$notBefore",
    "certificate_not_after=$notAfter",
    "certificate_sha256=$certFingerprint",
    "spki_sha256=$spkiPin"
) | Set-Content -LiteralPath $manifestPath -Encoding UTF8

@(
    "Production identity has been generated.",
    "",
    "Production binaries embed these values as encrypted build defaults and can run directly.",
    "Runtime environment variables remain higher-priority overrides.",
    "",
    "Advanced/manual environment loading:",
    ". .\deploy\standalone\env\production.env.ps1",
    ". .\deploy\standalone\env\ctrl_server.env.ps1",
    ". .\deploy\standalone\env\real_ctrl.env.ps1",
    ". .\deploy\standalone\env\ctrl_kik_build.env.ps1  # build time only",
    ". .\deploy\standalone\env\compiled_defaults.env.ps1  # build time only",
    "",
    "IDEA environment fields:",
    "deploy\standalone\env\idea_ctrl_server.txt",
    "deploy\standalone\env\idea_real_ctrl.txt",
    "",
    "server.key and env contain secrets used as protected build inputs.",
    "Never commit, share or package the secret files.",
    "Rotation: powershell -NoProfile -ExecutionPolicy Bypass -File .\scripts\generate_production_identity.ps1 -Force"
) | Set-Content -LiteralPath $readmePath -Encoding UTF8

foreach ($path in $sensitiveOutputs) {
    Protect-SensitiveFile -Path $path
}

# 最终构建只需要证书、私钥 PEM 和受保护的环境文件；删除可再生中间件，减少私钥副本。
foreach ($intermediate in @(
    $noisePrivateDerPath,
    $noisePublicDerPath,
    $pubKeyPath,
    $spkiPath,
    $openSslConfigPath
)) {
    [IO.File]::Delete($intermediate)
}

[pscustomobject]@{
    success = $true
    output_dir = $prodDir
    certificate = $certPath
    private_key = $keyPath
    spki_sha256 = $spkiPin
    ctrl_server_env = $ctrlServerEnvPath
    real_ctrl_env = $realCtrlEnvPath
    ctrl_kik_build_env = $ctrlKikBuildEnvPath
    all_env = $allEnvPath
    idea_ctrl_server = $ideaCtrlServerPath
    idea_real_ctrl = $ideaRealCtrlPath
    certificate_not_after = $notAfter
    exec_management_enabled = $true
    compiled_defaults = $compiledDefaultsPath
} | ConvertTo-Json -Depth 3
