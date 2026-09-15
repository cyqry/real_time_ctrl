param(
    # ctrl_kik 使用独立固定 nightly、build-std、日志剔除和二进制审计，禁止从本入口发布。
    [ValidateSet("real_ctrl", "ctrl_server")]
    [string[]]$Crate = @("real_ctrl", "ctrl_server")
)

$ErrorActionPreference = "Stop"
$ScriptDir = Split-Path -Parent $MyInvocation.MyCommand.Path
$Root = (Resolve-Path (Join-Path $ScriptDir "..")).Path
$PreviousRustFlags = $env:RUSTFLAGS
$CompiledDefaultsPath = Join-Path $Root "deploy\standalone\env\compiled_defaults.env.ps1"
$BuildVariableNames = @(
    "RTC_REAL_CTRL_BUILD_SERVER_HOST",
    "RTC_REAL_CTRL_BUILD_TLS_PORT",
    "RTC_REAL_CTRL_BUILD_TLS_SERVER_NAME",
    "RTC_REAL_CTRL_BUILD_TLS_CA_PEM_BASE64",
    "RTC_REAL_CTRL_BUILD_TLS_SPKI_SHA256",
    "RTC_REAL_CTRL_BUILD_HTTP_LOCK_PATH",
    "RTC_REAL_CTRL_BUILD_HTTP_BINDING",
    "RTC_REAL_CTRL_BUILD_HTTP_PORT",
    "RTC_REAL_CTRL_BUILD_ACCOUNT_ID",
    "RTC_REAL_CTRL_BUILD_INSTANCE_ID",
    "RTC_REAL_CTRL_BUILD_AUTH_SECRET",
    "RTC_REAL_CTRL_BUILD_API_TOKEN",
    "RTC_REAL_CTRL_BUILD_API_ALLOW_EXEC",
    "RTC_CTRL_SERVER_BUILD_BIND_HOST",
    "RTC_CTRL_SERVER_BUILD_KIK_PORT",
    "RTC_CTRL_SERVER_BUILD_TLS_PORT",
    "RTC_CTRL_SERVER_BUILD_TLS_CERT_PEM_BASE64",
    "RTC_CTRL_SERVER_BUILD_TLS_KEY_PEM_BASE64",
    "RTC_CTRL_SERVER_BUILD_AUTH_SECRET",
    "RTC_CTRL_SERVER_BUILD_ACCOUNTS_JSON_BASE64",
    "RTC_CTRL_SERVER_BUILD_KIK_NOISE_PRIVATE_KEY",
    "RTC_CTRL_SERVER_BUILD_ALLOW_EXEC"
)
$PreviousBuildVariables = @{}
foreach ($Name in $BuildVariableNames) {
    $PreviousBuildVariables[$Name] = [Environment]::GetEnvironmentVariable(
        $Name,
        [EnvironmentVariableTarget]::Process
    )
}

function Assert-PlaintextAbsent {
    param(
        [string]$Path,
        [string[]]$Values
    )

    $binary = [IO.File]::ReadAllBytes($Path)
    $binaryText = [Text.Encoding]::GetEncoding(28591).GetString($binary)
    foreach ($value in $Values) {
        if (-not [string]::IsNullOrEmpty($value) -and $binaryText.Contains($value)) {
            throw "发布产物包含可直接搜索的敏感构建值: $Path"
        }
    }
}

try {
    Set-Location $Root
    if (-not (Test-Path -LiteralPath $CompiledDefaultsPath -PathType Leaf)) {
        throw "缺少生产构建默认值。请先运行 scripts/generate_production_identity.ps1"
    }
    # 该文件由身份生成器创建并收紧 ACL，只在构建进程中加载；生成的 EXE 内仅保留加密值。
    . $CompiledDefaultsPath
    $AccountSecrets = @()
    if (-not [string]::IsNullOrWhiteSpace($env:RTC_CTRL_SERVER_BUILD_ACCOUNTS_JSON_BASE64)) {
        try {
            $AccountsJson = [Text.Encoding]::UTF8.GetString(
                [Convert]::FromBase64String($env:RTC_CTRL_SERVER_BUILD_ACCOUNTS_JSON_BASE64)
            )
            $AccountSecrets = @($AccountsJson | ConvertFrom-Json | ForEach-Object { $_.secret })
        } catch {
            throw "RTC_CTRL_SERVER_BUILD_ACCOUNTS_JSON_BASE64 不是有效的账号 JSON Base64"
        }
    }
    $CfgFlag = "-Ccontrol-flow-guard=yes"
    if ([string]::IsNullOrWhiteSpace($PreviousRustFlags)) {
        $env:RUSTFLAGS = $CfgFlag
    } elseif ($PreviousRustFlags -notmatch "control-flow-guard") {
        $env:RUSTFLAGS = "$PreviousRustFlags $CfgFlag"
    }

    # production 继承 hardened 的 LTO、单 codegen unit 与 panic 策略，同时保留 PDB 便于诊断。
    $CargoArgs = @("build", "--locked", "--profile", "production")
    foreach ($Name in $Crate) {
        $CargoArgs += @("-p", $Name)
    }
    & cargo @CargoArgs
    if ($LASTEXITCODE -ne 0) {
        throw "production 构建失败，cargo exit code: $LASTEXITCODE"
    }

    $Artifacts = foreach ($Name in $Crate) {
        if ($Name -eq "real_ctrl") {
            Join-Path $Root "target\production\real_ctrl.exe"
            Join-Path $Root "target\production\real_ctrl_local_server.exe"
            Join-Path $Root "target\production\real_ctrl_invoker_http_service.exe"
        } else {
            Join-Path $Root "target\production\$Name.exe"
        }
    }
    foreach ($Artifact in $Artifacts) {
        if (-not (Test-Path -LiteralPath $Artifact)) {
            throw "缺少 production 构建产物: $Artifact"
        }
    }

    foreach ($Artifact in $Artifacts) {
        if ((Split-Path $Artifact -Leaf) -eq "ctrl_server.exe") {
            $ServerSensitiveValues = @(
                $env:RTC_CTRL_SERVER_BUILD_AUTH_SECRET,
                $env:RTC_CTRL_SERVER_BUILD_KIK_NOISE_PRIVATE_KEY,
                $env:RTC_CTRL_SERVER_BUILD_TLS_KEY_PEM_BASE64,
                $env:RTC_CTRL_SERVER_BUILD_ACCOUNTS_JSON_BASE64
            )
            $ServerSensitiveValues += $AccountSecrets
            Assert-PlaintextAbsent -Path $Artifact -Values $ServerSensitiveValues
        } else {
            Assert-PlaintextAbsent -Path $Artifact -Values @(
                $env:RTC_REAL_CTRL_BUILD_AUTH_SECRET,
                $env:RTC_REAL_CTRL_BUILD_API_TOKEN,
                $env:RTC_REAL_CTRL_BUILD_TLS_CA_PEM_BASE64
            )
        }
    }

    [pscustomobject]@{
        profile = "production"
        control_flow_guard = $true
        debug_info_retained = $true
        embedded_plaintext_audit = "passed"
        artifacts = $Artifacts
    } | ConvertTo-Json -Depth 4
} finally {
    $env:RUSTFLAGS = $PreviousRustFlags
    foreach ($Entry in $PreviousBuildVariables.GetEnumerator()) {
        [Environment]::SetEnvironmentVariable(
            $Entry.Key,
            $Entry.Value,
            [EnvironmentVariableTarget]::Process
        )
    }
    Set-Location $Root
}
