param(
    [string]$ReportPath = ""
)

$ErrorActionPreference = "Stop"
$ScriptDir = Split-Path -Parent $MyInvocation.MyCommand.Path
$Root = (Resolve-Path (Join-Path $ScriptDir "..")).Path
if ([string]::IsNullOrWhiteSpace($ReportPath)) {
    $ReportPath = Join-Path $Root "reports\audit\production-dependencies.json"
}

# Cargo.lock 会记录 workspace 成员的依赖以及某些未启用的可选依赖，而 cargo-audit
# 不解析最终 feature 图。先逐一审计实际发布 crate 的活动 normal/build 依赖树，只有确认
# 下列包不进入任何发布产物后，才允许在整仓 lockfile 扫描中忽略对应 advisory。
$ShippingPackages = @(
    "common",
    "ctrl_common",
    "ctrl_kik",
    "ctrl_server",
    "real_ctrl",
    "string_obfuscation_macros"
)
$ExcludedAdvisories = [ordered]@{
    "RUSTSEC-2026-0258" = "h2 0.3.27 只由冻结且不发布的 start 引入；生产 h2 已升级至 0.4.16"
    "RUSTSEC-2026-0235" = "rkyv 0.7.46 是 Cargo.lock 中未启用的可选依赖，生产活动依赖树不包含它"
    "RUSTSEC-2025-0134" = "rustls-pemfile 1.0.4 只由冻结且不发布的 start 引入；生产 TLS 已迁移 rustls-pki-types"
}
$ForbiddenActivePackages = @(
    "h2 v0.3.27",
    "rkyv v0.7.46",
    "rustls-pemfile v1.0.4",
    "rustls-pemfile v2.2.0"
)

Push-Location $Root
try {
    $Trees = [ordered]@{}
    foreach ($package in $ShippingPackages) {
        $output = @(& cargo tree -p $package --edges normal,build --prefix none 2>&1 |
                ForEach-Object { $_.ToString() })
        if ($LASTEXITCODE -ne 0) {
            throw "读取 $package 的生产依赖树失败"
        }
        $tree = $output -join "`n"
        foreach ($forbidden in $ForbiddenActivePackages) {
            if ($tree.Contains($forbidden)) {
                throw "生产 crate $package 的活动依赖树仍包含 $forbidden"
            }
        }
        $Trees[$package] = "clean"
    }

    $auditArgs = @("audit", "-D", "warnings")
    foreach ($advisory in $ExcludedAdvisories.Keys) {
        $auditArgs += @("--ignore", $advisory)
    }
    # Windows PowerShell 5.1 会把原生命令写入 stderr 的普通进度行包装为
    # NativeCommandError；这里只暂停异常提升，随后仍以 cargo 的退出码作严格门禁。
    $previousErrorPreference = $ErrorActionPreference
    $ErrorActionPreference = "Continue"
    $auditOutput = @(& cargo @auditArgs 2>&1 | ForEach-Object { $_.ToString() })
    $auditExitCode = $LASTEXITCODE
    $ErrorActionPreference = $previousErrorPreference
    if ($auditExitCode -ne 0) {
        $auditOutput | ForEach-Object { Write-Output $_ }
        throw "生产依赖 RustSec 审计失败"
    }

    $report = [ordered]@{
        schema_version = 1
        checked_at = [DateTimeOffset]::UtcNow.ToString("o")
        shipping_packages = $ShippingPackages
        active_dependency_trees = $Trees
        excluded_lockfile_advisories = $ExcludedAdvisories
        rustsec_other_findings = "none"
        success = $true
    }
    $directory = Split-Path -Parent $ReportPath
    [IO.Directory]::CreateDirectory($directory) | Out-Null
    [IO.File]::WriteAllText(
        $ReportPath,
        ($report | ConvertTo-Json -Depth 8),
        [Text.UTF8Encoding]::new($false)
    )
    $report | ConvertTo-Json -Depth 8
} finally {
    Pop-Location
}
