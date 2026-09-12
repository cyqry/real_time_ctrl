param(
    [switch]$SkipToolchainInstall,
    [string]$ServerHost = "",
    [ValidateRange(0, 65535)]
    [int]$ServerPort = 0,
    [ValidateSet("default", "gray", "production")]
    [string]$BuildChannel = "default",
    [string]$NoiseServerPublicKey = ""
)

$ErrorActionPreference = "Stop"
$ScriptDir = Split-Path -Parent $MyInvocation.MyCommand.Path
$Root = (Resolve-Path (Join-Path $ScriptDir "..")).Path
$ToolchainManifest = Join-Path $Root "ctrl_kik\rust-toolchain.toml"
$HardeningConfigPath = Join-Path $ScriptDir "ctrl_kik_hardening.psd1"
$AuditScript = Join-Path $ScriptDir "audit_ctrl_kik_binary.ps1"
$TargetDir = Join-Path $Root "target\ctrl-kik-protected"
$Artifact = Join-Path $TargetDir "hardened\ctrl_kik.exe"
$ReceiptPath = Join-Path $TargetDir "build-receipt.json"

$ToolchainText = Get-Content -LiteralPath $ToolchainManifest -Raw -Encoding UTF8
$ToolchainMatch = [regex]::Match($ToolchainText, '(?m)^\s*channel\s*=\s*"([^"]+)"')
if (-not $ToolchainMatch.Success) {
    throw "ctrl_kik/rust-toolchain.toml 缺少 channel"
}
$Toolchain = $ToolchainMatch.Groups[1].Value
$HardeningConfig = Import-PowerShellDataFile -LiteralPath $HardeningConfigPath
if ($HardeningConfig.Toolchain -ne $Toolchain) {
    throw "ctrl_kik 工具链配置不一致: rust-toolchain=$Toolchain, hardening=$($HardeningConfig.Toolchain)"
}

$PreviousEnvironment = @{
    RUSTFLAGS = $env:RUSTFLAGS
    CARGO_ENCODED_RUSTFLAGS = $env:CARGO_ENCODED_RUSTFLAGS
    CARGO_TARGET_DIR = $env:CARGO_TARGET_DIR
    CARGO_PROFILE_HARDENED_PANIC = $env:CARGO_PROFILE_HARDENED_PANIC
    RTC_CTRL_KIK_BUILD_HOST = $env:RTC_CTRL_KIK_BUILD_HOST
    RTC_CTRL_KIK_BUILD_PORT = $env:RTC_CTRL_KIK_BUILD_PORT
    RTC_CTRL_KIK_BUILD_CHANNEL = $env:RTC_CTRL_KIK_BUILD_CHANNEL
    RTC_CTRL_KIK_NOISE_SERVER_PUBLIC_KEY = $env:RTC_CTRL_KIK_NOISE_SERVER_PUBLIC_KEY
}
$PreviousLocation = Get-Location

try {
    Set-Location $Root
    # 任何失败都必须让旧 receipt 失效，避免上一次的 production_ready=true
    # 被误认为本次源码对应的验收结果。产物也先删除，禁止构建失败后误取旧 PE。
    if (Test-Path -LiteralPath $ReceiptPath) {
        Remove-Item -LiteralPath $ReceiptPath -Force
    }
    if (Test-Path -LiteralPath $Artifact) {
        Remove-Item -LiteralPath $Artifact -Force
    }
    $Rustup = Get-Command rustup.exe -ErrorAction Stop
    $Installed = (& $Rustup.Source toolchain list) -join "`n"
    $ResolvedToolchain = $Toolchain
    if (-not ($Installed -match "(?m)^$([regex]::Escape($Toolchain))(-x86_64-pc-windows-msvc)?(\s|$)")) {
        # 开发机可能已有同一不可变 rustc 提交的 nightly 别名。只有提交 hash 完全相等才允许复用，
        # 避免重复下载；CI/生产环境仍优先安装日期固定工具链。
        $CompatibleAlias = $null
        foreach ($Alias in $HardeningConfig.CompatibleLocalAliases) {
            if ($Installed -notmatch "(?m)^$([regex]::Escape($Alias))(-x86_64-pc-windows-msvc)?(\s|$)") {
                continue
            }
            $AliasVersion = (& rustc "+$Alias" -Vv | Out-String)
            if ($AliasVersion -match "(?m)^commit-hash:\s*$([regex]::Escape($HardeningConfig.ExpectedCommit))\s*$") {
                $CompatibleAlias = $Alias
                break
            }
        }
        if ($null -ne $CompatibleAlias) {
            $ResolvedToolchain = $CompatibleAlias
        } elseif ($SkipToolchainInstall) {
            throw "缺少固定工具链 $Toolchain，且没有提交 hash 相同的本地别名"
        } else {
            & $Rustup.Source toolchain install $Toolchain --profile minimal --component rust-src --component llvm-tools --no-self-update
            if ($LASTEXITCODE -ne 0) {
                throw "安装固定 ctrl_kik 工具链失败: $Toolchain"
            }
        }
    }

    # 组件安装在全局 Rust 工具链缓存，符合项目的依赖缓存边界。
    & $Rustup.Source component add rust-src llvm-tools --toolchain $ResolvedToolchain
    if ($LASTEXITCODE -ne 0) {
        throw "固定工具链缺少 rust-src/llvm-tools: $ResolvedToolchain"
    }

    $RustcVersion = (& rustc "+$ResolvedToolchain" -Vv | Out-String).Trim()
    if ($RustcVersion -notmatch "(?m)^commit-hash:\s*$([regex]::Escape($HardeningConfig.ExpectedCommit))\s*$") {
        throw "ctrl_kik rustc 提交不匹配，期望 $($HardeningConfig.ExpectedCommit)"
    }
    $CargoHome = if (-not [string]::IsNullOrWhiteSpace($env:CARGO_HOME)) {
        [IO.Path]::GetFullPath($env:CARGO_HOME)
    } else {
        Join-Path $env:USERPROFILE ".cargo"
    }
    $RustSysroot = (& rustc "+$ResolvedToolchain" --print sysroot).Trim()

    # 使用编码 flags 避免工作区或 Cargo 路径包含空格时被错误拆词。
    $Flags = @(
        "-Ccontrol-flow-guard=yes",
        "-Zlocation-detail=none",
        "--remap-path-prefix=$Root=.",
        "--remap-path-prefix=$CargoHome=.",
        "--remap-path-prefix=$RustSysroot=.",
        "-Clink-arg=/DEBUG:NONE",
        "-Clink-arg=/CETCOMPAT",
        "-Clink-arg=/Brepro"
    )
    $env:RUSTFLAGS = $null
    $env:CARGO_ENCODED_RUSTFLAGS = $Flags -join [char]0x1f
    $env:CARGO_TARGET_DIR = $TargetDir
    $env:CARGO_PROFILE_HARDENED_PANIC = "immediate-abort"
    $env:RTC_CTRL_KIK_BUILD_HOST = if ([string]::IsNullOrWhiteSpace($ServerHost)) { $null } else { $ServerHost.Trim() }
    $env:RTC_CTRL_KIK_BUILD_PORT = if ($ServerPort -eq 0) { $null } else { "$ServerPort" }
    $env:RTC_CTRL_KIK_BUILD_CHANNEL = $BuildChannel
    $env:RTC_CTRL_KIK_NOISE_SERVER_PUBLIC_KEY = if ([string]::IsNullOrWhiteSpace($NoiseServerPublicKey)) { $null } else { $NoiseServerPublicKey.Trim() }

    if ($BuildChannel -ne "default" -and [string]::IsNullOrWhiteSpace($NoiseServerPublicKey)) {
        throw "灰度/正式 ctrl_kik 构建必须提供 Noise 服务端公钥"
    }

    $CargoArgs = @(
        "+$ResolvedToolchain",
        "-Z", "panic-immediate-abort",
        "-Z", "build-std=std,panic_abort",
        "-Z", "build-std-features=",
        "build",
        "--locked",
        "--package", "ctrl_kik",
        "--profile", "hardened",
        "--no-default-features"
    )
    & cargo @CargoArgs
    if ($LASTEXITCODE -ne 0) {
        throw "ctrl_kik protected 构建失败，cargo exit code: $LASTEXITCODE"
    }
    if (-not (Test-Path -LiteralPath $Artifact)) {
        throw "缺少 protected 构建产物: $Artifact"
    }

    # 防逆向验收只依赖可验证的内容最小化和 PE 缓解属性，不把代码签名误当作混淆能力。
    $FinalAuditJson = & $AuditScript -BinaryPath $Artifact
    $FinalAudit = $FinalAuditJson | ConvertFrom-Json

    $Receipt = [ordered]@{
        schema_version = 4
        crate = "ctrl_kik"
        profile = "hardened-protected"
        toolchain = $Toolchain
        resolved_toolchain = $ResolvedToolchain
        rustc = $RustcVersion
        cargo_target_dir = $TargetDir
        artifact = $Artifact
        deployment = [ordered]@{
            host = if ([string]::IsNullOrWhiteSpace($ServerHost)) { "config-default" } else { $ServerHost.Trim() }
            port = if ($ServerPort -eq 0) { "config-default" } else { $ServerPort }
            channel = $BuildChannel
            kik_transport = "Noise_NK_25519_ChaChaPoly_BLAKE2s"
        }
        production_ready = $FinalAudit.passed -and $FinalAudit.full_project_plaintext_clean
        audit = $FinalAudit
    }
    [IO.Directory]::CreateDirectory($TargetDir) | Out-Null
    [IO.File]::WriteAllText(
        $ReceiptPath,
        ($Receipt | ConvertTo-Json -Depth 10),
        [Text.UTF8Encoding]::new($false)
    )
    $Receipt | ConvertTo-Json -Depth 10
} finally {
    $env:RUSTFLAGS = $PreviousEnvironment.RUSTFLAGS
    $env:CARGO_ENCODED_RUSTFLAGS = $PreviousEnvironment.CARGO_ENCODED_RUSTFLAGS
    $env:CARGO_TARGET_DIR = $PreviousEnvironment.CARGO_TARGET_DIR
    $env:CARGO_PROFILE_HARDENED_PANIC = $PreviousEnvironment.CARGO_PROFILE_HARDENED_PANIC
    $env:RTC_CTRL_KIK_BUILD_HOST = $PreviousEnvironment.RTC_CTRL_KIK_BUILD_HOST
    $env:RTC_CTRL_KIK_BUILD_PORT = $PreviousEnvironment.RTC_CTRL_KIK_BUILD_PORT
    $env:RTC_CTRL_KIK_BUILD_CHANNEL = $PreviousEnvironment.RTC_CTRL_KIK_BUILD_CHANNEL
    $env:RTC_CTRL_KIK_NOISE_SERVER_PUBLIC_KEY = $PreviousEnvironment.RTC_CTRL_KIK_NOISE_SERVER_PUBLIC_KEY
    Set-Location $PreviousLocation
}
