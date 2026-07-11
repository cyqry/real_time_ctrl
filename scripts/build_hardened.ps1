param(
    [ValidateSet("real_ctrl", "ctrl_server", "ctrl_kik")]
    [string[]]$Crate = @("real_ctrl", "ctrl_server", "ctrl_kik"),
    [string]$SignCertificateThumbprint = "",
    [string]$TimestampUrl = "http://timestamp.digicert.com"
)

$ErrorActionPreference = "Stop"
$ScriptDir = Split-Path -Parent $MyInvocation.MyCommand.Path
$Root = (Resolve-Path (Join-Path $ScriptDir "..")).Path
$PreviousRustFlags = $env:RUSTFLAGS

try {
    Set-Location $Root
    $CfgFlag = "-Ccontrol-flow-guard=yes"
    if ([string]::IsNullOrWhiteSpace($PreviousRustFlags)) {
        $env:RUSTFLAGS = $CfgFlag
    } elseif ($PreviousRustFlags -notmatch "control-flow-guard") {
        $env:RUSTFLAGS = "$PreviousRustFlags $CfgFlag"
    }

    $CargoArgs = @("build", "--locked", "--profile", "hardened")
    foreach ($Name in $Crate) {
        $CargoArgs += @("-p", $Name)
    }
    & cargo @CargoArgs
    if ($LASTEXITCODE -ne 0) {
        throw "hardened 构建失败，cargo exit code: $LASTEXITCODE"
    }

    $Artifacts = foreach ($Name in $Crate) {
        if ($Name -eq "real_ctrl") {
            Join-Path $Root "target\hardened\real_ctrl.exe"
            Join-Path $Root "target\hardened\real_ctrl_local_server.exe"
            Join-Path $Root "target\hardened\real_ctrl_invoker_http_service.exe"
        } else {
            Join-Path $Root "target\hardened\$Name.exe"
        }
    }
    foreach ($Artifact in $Artifacts) {
        if (-not (Test-Path -LiteralPath $Artifact)) {
            throw "缺少 hardened 构建产物: $Artifact"
        }
    }

    if (-not [string]::IsNullOrWhiteSpace($SignCertificateThumbprint)) {
        $SignTool = Get-Command signtool.exe -ErrorAction Stop
        foreach ($Artifact in $Artifacts) {
            & $SignTool.Source sign `
                /sha1 $SignCertificateThumbprint `
                /fd SHA256 `
                /tr $TimestampUrl `
                /td SHA256 `
                $Artifact
            if ($LASTEXITCODE -ne 0) {
                throw "代码签名失败: $Artifact"
            }
        }
    }

    [pscustomobject]@{
        profile = "hardened"
        control_flow_guard = $true
        signed = -not [string]::IsNullOrWhiteSpace($SignCertificateThumbprint)
        artifacts = $Artifacts
    } | ConvertTo-Json -Depth 4
} finally {
    $env:RUSTFLAGS = $PreviousRustFlags
    Set-Location $Root
}
