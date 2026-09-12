param(
    [Parameter(Mandatory = $true)]
    [string]$BinaryPath
)

$ErrorActionPreference = "Stop"

function Resolve-Dumpbin {
    $Command = Get-Command dumpbin.exe -ErrorAction SilentlyContinue
    if ($null -ne $Command) {
        return $Command.Source
    }

    $VsWhere = Join-Path ${env:ProgramFiles(x86)} "Microsoft Visual Studio\Installer\vswhere.exe"
    if (Test-Path -LiteralPath $VsWhere) {
        $InstallPath = & $VsWhere -latest -products * -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath
        if (-not [string]::IsNullOrWhiteSpace($InstallPath)) {
            $Candidate = Get-ChildItem -LiteralPath (Join-Path $InstallPath "VC\Tools\MSVC") -Directory |
                Sort-Object Name -Descending |
                ForEach-Object { Join-Path $_.FullName "bin\Hostx64\x64\dumpbin.exe" } |
                Where-Object { Test-Path -LiteralPath $_ } |
                Select-Object -First 1
            if ($null -ne $Candidate) {
                return $Candidate
            }
        }
    }

    throw "未找到 dumpbin.exe；请安装 Visual Studio C++ x64 构建工具"
}

function Add-RustNeedle(
    [Collections.Generic.HashSet[string]]$Needles,
    [string]$Value
) {
    if ($Value.Length -ge 4) {
        [void]$Needles.Add($Value)
    }
    # format! 常把模板拆成多个静态片段；同时保留完整值可覆盖 JSON、普通花括号文本等。
    foreach ($Segment in [regex]::Split($Value, '\{[^}]*\}')) {
        $Needle = $Segment.Trim()
        if ($Needle.Length -ge 4) {
            [void]$Needles.Add($Needle)
        }
    }
}

function Get-RustStringInventory(
    [string[]]$Roots,
    [Collections.Generic.HashSet[string]]$LogNeedles = $null
) {
    $Needles = [Collections.Generic.HashSet[string]]::new([StringComparer]::Ordinal)
    foreach ($Root in $Roots) {
        if (-not (Test-Path -LiteralPath $Root)) {
            continue
        }
        Get-ChildItem -LiteralPath $Root -Filter "*.rs" -File -Recurse |
            ForEach-Object {
                $Source = [IO.File]::ReadAllText($_.FullName, [Text.Encoding]::UTF8)

                # 先提取 raw string 的完整 body。普通字符串正则仍可能保守提取 raw body 内部片段，
                # 但 HashSet 会去重，且额外 needle 只会提高泄漏检测灵敏度。
                $RawPattern = '(?s)(?:br|r)(?<hash>#{0,16})"(?<body>.*?)"\k<hash>'
                foreach ($Match in [regex]::Matches($Source, $RawPattern)) {
                    Add-RustNeedle -Needles $Needles -Value $Match.Groups["body"].Value
                }

                # 宏内字面量也会成为 needle；若 hidden! 展开正确，它不会命中最终 PE。
                foreach ($Match in [regex]::Matches($Source, '"((?:\\.|[^"\\])*)"')) {
                    try {
                        $Value = [regex]::Unescape($Match.Groups[1].Value)
                    } catch {
                        continue
                    }
                    Add-RustNeedle -Needles $Needles -Value $Value
                }

                # 日志属于项目 needle 的子集，但保留专项集合以输出更准确的失败类型。
                if ($null -ne $LogNeedles) {
                    foreach ($Match in [regex]::Matches(
                        $Source,
                        'dev_debug!\s*\(\s*"([^"\r\n]*)"'
                    )) {
                        try {
                            $Format = [regex]::Unescape($Match.Groups[1].Value)
                        } catch {
                            continue
                        }
                        Add-RustNeedle -Needles $LogNeedles -Value $Format
                    }
                }
            }
    }
    return $Needles
}

$ResolvedBinary = (Resolve-Path -LiteralPath $BinaryPath).Path
$ScriptDir = Split-Path -Parent $MyInvocation.MyCommand.Path
$ProjectRoot = (Resolve-Path (Join-Path $ScriptDir "..")).Path
$HardeningConfig = Import-PowerShellDataFile -LiteralPath (
    Join-Path $ScriptDir "ctrl_kik_hardening.psd1"
)
$CargoHome = if (-not [string]::IsNullOrWhiteSpace($env:CARGO_HOME)) {
    [IO.Path]::GetFullPath($env:CARGO_HOME)
} else {
    Join-Path $env:USERPROFILE ".cargo"
}
$Bytes = [IO.File]::ReadAllBytes($ResolvedBinary)
# ISO-8859-1 保持 byte -> char 一一对应；正则直接扫描整块文本，避免创建并多次遍历
# 数千个 PowerShell 字符串对象。所有禁止模式本身均为 ASCII，因此不会损失检测能力。
$AsciiBinaryText = [Text.Encoding]::GetEncoding(28591).GetString($Bytes)
$Utf8BinaryText = [Text.UTF8Encoding]::new($false, $false).GetString($Bytes)
$Utf16Encoding = [Text.UnicodeEncoding]::new($false, $false, $false)
$Utf16BinaryTextEven = $Utf16Encoding.GetString($Bytes)
$Utf16BinaryTextOdd = if ($Bytes.Length -gt 1) {
    $Utf16Encoding.GetString($Bytes, 1, $Bytes.Length - 1)
} else {
    ""
}
$Dumpbin = Resolve-Dumpbin
# dumpbin 支持在一次调用中同时输出 PE headers 和 load config，减少一次进程启动。
$PeMetadata = (& $Dumpbin /headers /loadconfig $ResolvedBinary 2>&1 | Out-String)
if ($LASTEXITCODE -ne 0) {
    throw "dumpbin 无法读取 PE 元数据，exit code: $LASTEXITCODE"
}

# 这些内容属于源码、构建机、调试或日志痕迹，protected 产物必须为零。
$ForbiddenPatterns = [ordered]@{
    project_root_path      = [regex]::Escape($ProjectRoot)
    cargo_home_path        = [regex]::Escape($CargoHome)
    rust_source_file       = '(?i)\.rs(?:$|[^a-z0-9_])'
    cargo_registry_path    = '(?i)(cargo_repository|\.cargo[\\/]registry|registry[\\/]src)'
    project_source_path    = '(?i)(ctrl_kik|common)[\\/]src[\\/]'
    rust_compiler_path     = '(?i)([\\/]rust[\\/]deps[\\/]|rustc[-_][a-z0-9]|rustc[\\/])'
    panic_location         = '(?i)panicked at'
    pdb_reference          = '(?i)\.pdb(?:$|[^a-z0-9_])'
    absolute_build_path    = '(?i)[a-z]:[\\/](code|users|environment)[\\/]'
    cargo_manifest         = '(?i)CARGO_MANIFEST_DIR'
    application_module     = '(?i)(ctrl_kik|common)::[a-z_]'
    release_logger         = '(?i)(RUST_LOG_STYLE|env_logger)'
}

$ForbiddenHits = [ordered]@{}
foreach ($Entry in $ForbiddenPatterns.GetEnumerator()) {
    $Matches = @([regex]::Matches($AsciiBinaryText, $Entry.Value))
    if ($Matches.Count -gt 0) {
        $ForbiddenHits[$Entry.Key] = @(
            $Matches |
                ForEach-Object Value |
                Select-Object -Unique -First 5
        )
    }
}

# 日志 needle 与项目 needle 在同一次源码读取中收集，保留专项错误分类但不重复遍历文件。
$ClientSourceRoot = Join-Path $ProjectRoot "ctrl_kik\src"
$LogNeedles = [Collections.Generic.HashSet[string]]::new([StringComparer]::Ordinal)
$ProjectNeedles = Get-RustStringInventory -Roots @(
    (Join-Path $ProjectRoot "ctrl_kik\src"),
    (Join-Path $ProjectRoot "common\src")
) -LogNeedles $LogNeedles
$LogMessageHits = @(
    $LogNeedles |
        Where-Object {
            $Utf8BinaryText.Contains($_) -or
            $Utf16BinaryTextEven.Contains($_) -or
            $Utf16BinaryTextOdd.Contains($_)
        } |
        Sort-Object
)
if ($LogMessageHits.Count -gt 0) {
    $ForbiddenHits["release_log_message"] = @($LogMessageHits | Select-Object -First 10)
}

# 对 ctrl_kik 全部源码（包含 screen）及其 common 依赖做反向比对。只有在最终 PE 中仍能直接搜索到的
# 源码片段才会命中，因此 hidden! 的密文参数不会误报。
$AllProjectPlaintextHits = @(
    $ProjectNeedles |
        Where-Object {
            $Utf8BinaryText.Contains($_) -or
            $Utf16BinaryTextEven.Contains($_) -or
            $Utf16BinaryTextOdd.Contains($_)
        } |
        Sort-Object
)
$KnownRuntimeCollisionSet =
    [Collections.Generic.HashSet[string]]::new([StringComparer]::Ordinal)
foreach ($Collision in $HardeningConfig.KnownRuntimeStringCollisions) {
    [void]$KnownRuntimeCollisionSet.Add([string]$Collision)
}
$RuntimeCollisionHits = @(
    $AllProjectPlaintextHits | Where-Object { $KnownRuntimeCollisionSet.Contains($_) }
)
$ProjectPlaintextHits = @(
    $AllProjectPlaintextHits | Where-Object { -not $KnownRuntimeCollisionSet.Contains($_) }
)
if ($ProjectPlaintextHits.Count -gt 0) {
    $ForbiddenHits["project_plaintext"] = @($ProjectPlaintextHits | Select-Object -First 20)
}

# 配置值是第一类受保护字符串，单独做全量检查，避免它们因未被普通源码扫描覆盖而回归。
$ConfigPath = Join-Path $ProjectRoot "common\config.json"
$Config = Get-Content -LiteralPath $ConfigPath -Raw -Encoding UTF8 | ConvertFrom-Json
$ConfigPlaintextHits = @(
    $Config.strings.psobject.Properties |
        ForEach-Object { [string]$_.Value } |
        Where-Object {
            $_.Length -ge 4 -and
            (
                $Utf8BinaryText.Contains($_) -or
                $Utf16BinaryTextEven.Contains($_) -or
                $Utf16BinaryTextOdd.Contains($_)
            )
        } |
        Sort-Object -Unique
)
if ($ConfigPlaintextHits.Count -gt 0) {
    $ForbiddenHits["config_plaintext"] = @($ConfigPlaintextHits | Select-Object -First 20)
}

# 标准库/运行时自身仍可能包含这些公开指纹。它们不暴露源码路径，但持续计数可防止版本升级扩大可见面。
$AdvisoryPatterns = [ordered]@{
    rust_runtime = '(?i)(RUST_BACKTRACE|RustBacktraceMutex)'
    tokio_runtime = '(?i)(tokio|TOKIO_)'
    application_name = '(?i)ctrl_kik'
}
$AdvisoryCounts = [ordered]@{}
foreach ($Entry in $AdvisoryPatterns.GetEnumerator()) {
    # 这里统计实际出现次数；相比“命中该模式的字符串段数量”更稳定，也无需物化字符串表。
    $AdvisoryCounts[$Entry.Key] = [regex]::Matches($AsciiBinaryText, $Entry.Value).Count
}

$Mitigations = [ordered]@{
    high_entropy_aslr = $PeMetadata -match 'High Entropy Virtual Addresses'
    dynamic_base      = $PeMetadata -match 'Dynamic base'
    nx_compatible     = $PeMetadata -match 'NX compatible'
    cfg_header        = $PeMetadata -match 'Control Flow Guard'
    cfg_instrumented  = $PeMetadata -match 'CF instrumented'
    cfg_fid_table     = $PeMetadata -match 'FID table present'
    cet_compatible    = $PeMetadata -match 'CET compatible'
    no_codeview_pdb   = $PeMetadata -notmatch 'Format:\s+RSDS'
    no_rwx_section    = $PeMetadata -notmatch '(?im)^\s*Execute Read Write\s*$'
}
$MissingMitigations = @($Mitigations.GetEnumerator() | Where-Object { -not $_.Value } | ForEach-Object Key)

$Sha256 = [Security.Cryptography.SHA256]::Create()
try {
    $BinaryHash = ([BitConverter]::ToString($Sha256.ComputeHash($Bytes))).Replace("-", "").ToLowerInvariant()
} finally {
    $Sha256.Dispose()
}
$Result = [ordered]@{
    binary = $ResolvedBinary
    size_bytes = $Bytes.Length
    sha256 = $BinaryHash
    source_and_debug_trace_clean = $ForbiddenHits.Count -eq 0
    protected_scope_plaintext_clean = ($ProjectPlaintextHits.Count -eq 0) -and
        ($ConfigPlaintextHits.Count -eq 0)
    full_project_plaintext_clean = ($ProjectPlaintextHits.Count -eq 0) -and
        ($ConfigPlaintextHits.Count -eq 0)
    forbidden_hits = $ForbiddenHits
    runtime_string_collision_hits = @($RuntimeCollisionHits | Select-Object -First 20)
    advisory_fingerprint_counts = $AdvisoryCounts
    mitigations = $Mitigations
    passed = ($ForbiddenHits.Count -eq 0) -and ($MissingMitigations.Count -eq 0)
}

$Json = $Result | ConvertTo-Json -Depth 8
Write-Output $Json

$Failures = [Collections.Generic.List[string]]::new()
if ($ForbiddenHits.Count -gt 0) {
    $Failures.Add("发现禁止的源码/调试/日志痕迹: $($ForbiddenHits.Keys -join ', ')")
}
if ($MissingMitigations.Count -gt 0) {
    $Failures.Add("缺少 PE 缓解属性: $($MissingMitigations -join ', ')")
}
if ($Failures.Count -gt 0) {
    throw ($Failures -join '; ')
}
