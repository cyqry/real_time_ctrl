param(
    [switch]$IncludePublic,
    [ValidateSet("Gray", "Production")]
    [string]$Channel = "Gray",
    [int]$PublicBigFileMiB = 32
)

$ErrorActionPreference = "Stop"
$ScriptDir = Split-Path -Parent $MyInvocation.MyCommand.Path
$Root = (Resolve-Path (Join-Path $ScriptDir "..")).Path
$ReportDir = Join-Path $Root "target\security-tests"
$ReportPath = Join-Path $ReportDir "security-concurrency-summary.json"
[IO.Directory]::CreateDirectory($ReportDir) | Out-Null

$result = [ordered]@{
    schema_version = 1
    started_at = [DateTimeOffset]::UtcNow.ToString("o")
    public_channel = if ($IncludePublic) { $Channel.ToLowerInvariant() } else { $null }
    stages = @()
    success = $false
}

function Invoke-TestStage {
    param([string]$Name, [scriptblock]$Action)
    $watch = [Diagnostics.Stopwatch]::StartNew()
    try {
        & $Action
        if ($LASTEXITCODE -notin @(0, $null)) {
            throw "$Name 退出码为 $LASTEXITCODE"
        }
        $watch.Stop()
        $script:result.stages += [ordered]@{
            name = $Name
            success = $true
            elapsed_ms = $watch.ElapsedMilliseconds
        }
    } catch {
        $watch.Stop()
        $script:result.stages += [ordered]@{
            name = $Name
            success = $false
            elapsed_ms = $watch.ElapsedMilliseconds
            error = $_.Exception.Message
        }
        throw
    }
}

Push-Location $Root
try {
    Invoke-TestStage "shipping-crate-tests" {
        cargo test --locked `
            -p common `
            -p ctrl_common `
            -p ctrl_server `
            -p real_ctrl `
            -p ctrl_kik `
            -p string_obfuscation_macros
    }
    Invoke-TestStage "strict-clippy" {
        cargo clippy --locked `
            -p common `
            -p ctrl_common `
            -p ctrl_server `
            -p real_ctrl `
            --all-targets `
            -- `
            -D warnings
    }
    Invoke-TestStage "production-dependency-audit" {
        & (Join-Path $ScriptDir "audit_production_dependencies.ps1") `
            -ReportPath (Join-Path $ReportDir "production-dependencies.json")
    }
    Invoke-TestStage "local-full-stack-security-concurrency" {
        & (Join-Path $ScriptDir "e2e_ctrl_stack.ps1")
    }
    if ($IncludePublic) {
        Invoke-TestStage "public-gray-bounded-security-concurrency" {
            & (Join-Path $ScriptDir "test_public_stack.ps1") `
                -Channel $Channel `
                -BigFileMiB $PublicBigFileMiB
        }
    }
    $result.success = $true
} catch {
    $result.error = $_.Exception.Message
    throw
} finally {
    Pop-Location
    $result.completed_at = [DateTimeOffset]::UtcNow.ToString("o")
    [IO.File]::WriteAllText(
        $ReportPath,
        ($result | ConvertTo-Json -Depth 10),
        [Text.UTF8Encoding]::new($false)
    )
}

$result | ConvertTo-Json -Depth 10
