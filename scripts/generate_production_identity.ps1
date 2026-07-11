[CmdletBinding()]
param(
    [ValidateNotNullOrEmpty()]
    [string]$ServerAddress = "127.0.0.1",

    [ValidateNotNullOrEmpty()]
    [string]$TlsServerName = "real-ctrl-server",

    [ValidateRange(1, 65535)]
    [int]$PlainPort = 9002,

    [ValidateRange(1, 65535)]
    [int]$TlsPort = 9443,

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
$prodDir = Join-Path $repoRoot "target\prod"
$certDir = Join-Path $prodDir "certs"
$envDir = Join-Path $prodDir "env"
$runtimeDir = Join-Path $prodDir "runtime"
$certPath = Join-Path $certDir "server.crt"
$keyPath = Join-Path $certDir "server.key"
$pubKeyPath = Join-Path $certDir "server.pubkey.pem"
$spkiPath = Join-Path $certDir "server.spki.der"
$openSslConfigPath = Join-Path $certDir "openssl_real_ctrl.cnf"
$ctrlServerEnvPath = Join-Path $envDir "ctrl_server.env.ps1"
$realCtrlEnvPath = Join-Path $envDir "real_ctrl.env.ps1"
$allEnvPath = Join-Path $envDir "production.env.ps1"
$ideaCtrlServerPath = Join-Path $envDir "idea_ctrl_server.txt"
$ideaRealCtrlPath = Join-Path $envDir "idea_real_ctrl.txt"
$manifestPath = Join-Path $prodDir "identity_manifest.txt"
$readmePath = Join-Path $prodDir "README.txt"

$sensitiveOutputs = @(
    $keyPath,
    $ctrlServerEnvPath,
    $realCtrlEnvPath,
    $allEnvPath,
    $ideaCtrlServerPath,
    $ideaRealCtrlPath
)
$existingOutputs = @($sensitiveOutputs | Where-Object { Test-Path -LiteralPath $_ })
if ($existingOutputs.Count -gt 0 -and -not $Force) {
    throw "Production identity already exists. Use -Force only for an intentional rotation; old pins and secrets will stop working.`n$($existingOutputs -join "`n")"
}

$openSsl = Get-Command openssl -ErrorAction SilentlyContinue
if ($null -eq $openSsl) {
    throw "OpenSSL was not found. Install OpenSSL 3.x and add openssl.exe to PATH."
}

New-Item -ItemType Directory -Force -Path $certDir, $envDir, $runtimeDir | Out-Null

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
$httpLockPath = Join-Path $runtimeDir "real_ctrl_invoker_http_service.lock"

$ctrlServerValues = [ordered]@{
    CTRL_SERVER_BIND_HOST = "0.0.0.0"
    CTRL_SERVER_PORT = [string]$PlainPort
    CTRL_SERVER_TLS_PORT = [string]$TlsPort
    CTRL_SERVER_TLS_CERT = $certPath
    CTRL_SERVER_TLS_KEY = $keyPath
    CTRL_SERVER_AUTH_SECRET = $controlAuthSecret
    CTRL_SERVER_ALLOW_EXEC = "1"
    RUST_BACKTRACE = "1"
}
$realCtrlValues = [ordered]@{
    REAL_CTRL_SERVER_HOST = $ServerAddress
    REAL_CTRL_SERVER_PORT = [string]$PlainPort
    REAL_CTRL_TLS_PORT = [string]$TlsPort
    REAL_CTRL_TLS_SERVER_NAME = $TlsServerName
    REAL_CTRL_TLS_CA_CERT = $certPath
    REAL_CTRL_TLS_SERVER_SPKI_SHA256 = $spkiPin
    REAL_CTRL_AUTH_SECRET = $controlAuthSecret
    REAL_CTRL_API_TOKEN = $apiToken
    REAL_CTRL_API_ALLOW_EXEC = "1"
    REAL_CTRL_HTTP_LOCK_PATH = $httpLockPath
    RUST_BACKTRACE = "1"
}
$allValues = [ordered]@{}
foreach ($entry in $ctrlServerValues.GetEnumerator()) {
    $allValues[$entry.Key] = $entry.Value
}
foreach ($entry in $realCtrlValues.GetEnumerator()) {
    $allValues[$entry.Key] = $entry.Value
}

Write-EnvironmentScript -Path $ctrlServerEnvPath -Values $ctrlServerValues -Description "Load this file for the ctrl_server process."
Write-EnvironmentScript -Path $realCtrlEnvPath -Values $realCtrlValues -Description "Load this file for real_ctrl or real_ctrl_invoker_http_service."
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
    "plain_port=$PlainPort",
    "tls_port=$TlsPort",
    "certificate_not_before=$notBefore",
    "certificate_not_after=$notAfter",
    "certificate_sha256=$certFingerprint",
    "spki_sha256=$spkiPin"
) | Set-Content -LiteralPath $manifestPath -Encoding UTF8

@(
    "Production identity has been generated.",
    "",
    "Load all variables into the current PowerShell session:",
    ". .\target\prod\env\production.env.ps1",
    "",
    "For separate processes, load one file in each terminal:",
    ". .\target\prod\env\ctrl_server.env.ps1",
    ". .\target\prod\env\real_ctrl.env.ps1",
    "",
    "IDEA environment fields:",
    "target\prod\env\idea_ctrl_server.txt",
    "target\prod\env\idea_real_ctrl.txt",
    "",
    "server.key and the env directory contain secrets. Never commit, share or package them.",
    "Rotation: powershell -NoProfile -ExecutionPolicy Bypass -File .\scripts\generate_production_identity.ps1 -Force"
) | Set-Content -LiteralPath $readmePath -Encoding UTF8

foreach ($path in $sensitiveOutputs) {
    Protect-SensitiveFile -Path $path
}

[pscustomobject]@{
    success = $true
    output_dir = $prodDir
    certificate = $certPath
    private_key = $keyPath
    spki_sha256 = $spkiPin
    ctrl_server_env = $ctrlServerEnvPath
    real_ctrl_env = $realCtrlEnvPath
    all_env = $allEnvPath
    idea_ctrl_server = $ideaCtrlServerPath
    idea_real_ctrl = $ideaRealCtrlPath
    certificate_not_after = $notAfter
    exec_management_enabled = $true
} | ConvertTo-Json -Depth 3
