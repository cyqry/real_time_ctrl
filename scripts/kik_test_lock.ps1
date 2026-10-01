# 自动化验收只允许在项目内创建文件。这里读取构建回执，不修改配置、锁文件或进程。
# 旧回执没有真实锁路径时必须先重建，不能继续猜测锁位于当前工作目录。

function Assert-KikTestPathInProject {
    param([string]$Path, [string]$ProjectRoot)

    $full = [IO.Path]::GetFullPath($Path)
    $root = [IO.Path]::GetFullPath($ProjectRoot).TrimEnd('\', '/')
    $prefix = $root + [IO.Path]::DirectorySeparatorChar
    if (-not $full.StartsWith($prefix, [StringComparison]::OrdinalIgnoreCase)) {
        throw "Kik 自动化验收路径超出项目目录，拒绝启动或操作：$full"
    }
    # 字符串前缀不能识别目录联接；逐层检查已存在的父目录，避免测试落到项目外。
    for ($cursor = $full; $cursor -and $cursor.Length -ge $root.Length; $cursor = [IO.Path]::GetDirectoryName($cursor)) {
        if (Test-Path -LiteralPath $cursor) {
            $item = Get-Item -LiteralPath $cursor -Force
            if (($item.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) {
                throw "Kik 自动化验收路径包含链接或重解析点，拒绝启动：$cursor"
            }
        }
    }
    return $full
}

function Assert-KikTestLockLocation {
    param(
        [string]$ProjectRoot,
        [string]$WorkingDirectory,
        [string]$BinaryPath,
        [string]$ReceiptPath
    )

    $working = Assert-KikTestPathInProject $WorkingDirectory $ProjectRoot
    $binary = Assert-KikTestPathInProject $BinaryPath $ProjectRoot
    $receiptFile = Assert-KikTestPathInProject $ReceiptPath $ProjectRoot
    if (-not (Test-Path -LiteralPath $binary -PathType Leaf) -or
        -not (Test-Path -LiteralPath $receiptFile -PathType Leaf)) {
        throw '缺少 Kik EXE 或配套 protected 构建回执，请先重新构建并归档产物。'
    }
    $receipt = Get-Content -LiteralPath $receiptFile -Raw -Encoding UTF8 | ConvertFrom-Json
    $deploymentProperty = $receipt.PSObject.Properties['deployment']
    $deployment = if ($null -ne $deploymentProperty) { $deploymentProperty.Value } else { $null }
    if ($null -eq $deployment -or
        $null -eq $deployment.PSObject.Properties['lock_file_path'] -or
        $null -eq $deployment.PSObject.Properties['lock_file_path_source'] -or
        $null -eq $deployment.PSObject.Properties['config_sha256']) {
        throw '旧 Kik 回执没有真实 LOCK_FILE_PATH 来源和配置摘要，无法安全确定测试写入位置；请先用当前 protected 脚本重新构建。'
    }
    $configured = $deployment.lock_file_path
    if ($configured -isnot [string] -or [string]::IsNullOrWhiteSpace($configured) -or
        $configured.Contains([char]0) -or $deployment.lock_file_path_source -ne 'common/config.json' -or
        [string]$deployment.config_sha256 -notmatch '^[0-9a-fA-F]{64}$') {
        throw 'Kik 回执中的 LOCK_FILE_PATH 或配置来源不完整，请重新构建；验收不会覆盖配置。'
    }
    $auditProperty = $receipt.PSObject.Properties['audit']
    $audit = if ($null -ne $auditProperty) { $auditProperty.Value } else { $null }
    if ($null -eq $audit -or $null -eq $audit.PSObject.Properties['sha256'] -or
        [string]$audit.sha256 -notmatch '^[0-9a-fA-F]{64}$' -or
        (Get-FileHash -LiteralPath $binary -Algorithm SHA256).Hash -ne $audit.sha256) {
        throw 'Kik EXE 与构建回执 SHA-256 不一致，拒绝以其他程序的锁配置启动验收。'
    }

    if ([IO.Path]::IsPathRooted($configured)) {
        # C:foo 和 \foo 会依赖进程当前盘符；拒绝这类不能由工作目录唯一确定的路径。
        if ($configured -notmatch '^[A-Za-z]:[\\/]' -and $configured -notmatch '^\\\\[^\\]+\\[^\\]+') {
            throw 'LOCK_FILE_PATH 使用了不完整的绝对路径，无法可靠预检；请在 config.json 中设置完整绝对路径或普通相对路径。'
        }
        $resolved = [IO.Path]::GetFullPath($configured)
    } else {
        $resolved = [IO.Path]::GetFullPath((Join-Path $working $configured))
    }
    # 不打开、不创建、不删除这个锁；本预检只防止自动化执行越过项目文件边界。
    try {
        $resolved = Assert-KikTestPathInProject $resolved $ProjectRoot
    } catch {
        throw "回执中的 LOCK_FILE_PATH 来自 common/config.json；自动验收不会覆盖它。$($_.Exception.Message) 请使用项目内独立源码副本的测试配置构建测试程序，或在配置位置按需手工验收。"
    }
    return [pscustomobject]@{
        configured_path = $configured
        resolved_path = $resolved
        source = 'common/config.json'
        config_sha256 = [string]$deployment.config_sha256
        artifact_sha256 = [string]$audit.sha256
    }
}
