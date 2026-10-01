# 本地 E2E 的 Kik 使用独立源码副本，锁路径仍只从该副本的 common/config.json 编译。
# 此文件只提供测试辅助函数，不是发布入口，也不修改用户维护的正式配置。

function Assert-KikFixturePath {
    param([string]$Path, [string]$Parent)

    $full = [IO.Path]::GetFullPath($Path)
    $parentFull = [IO.Path]::GetFullPath($Parent).TrimEnd('\', '/')
    $prefix = $parentFull + [IO.Path]::DirectorySeparatorChar
    if (!$full.StartsWith($prefix, [StringComparison]::OrdinalIgnoreCase)) {
        throw "Kik 测试路径必须位于指定项目目录内: $full"
    }
    # 只检查字符串前缀无法阻止目录联接逃出项目；沿现存父目录逐个拒绝 reparse point。
    $current = $full
    while ($current.Length -ge $parentFull.Length) {
        if (Test-Path -LiteralPath $current) {
            $item = Get-Item -LiteralPath $current -Force
            if (($item.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) {
                throw "Kik 测试不接受符号链接或目录联接: $current"
            }
        }
        if ($current.Equals($parentFull, [StringComparison]::OrdinalIgnoreCase)) { break }
        $current = [IO.Path]::GetDirectoryName($current)
    }
    return $full
}

function Copy-KikFixtureSource {
    param([string]$Source, [string]$Destination, [string]$RepositoryRoot, [string]$WorkspaceRoot)

    [void](Assert-KikFixturePath $Source $RepositoryRoot)
    [void](Assert-KikFixturePath $Destination $WorkspaceRoot)
    $item = Get-Item -LiteralPath $Source -Force
    if ($item.PSIsContainer) {
        [void][IO.Directory]::CreateDirectory($Destination)
        foreach ($child in Get-ChildItem -LiteralPath $Source -Force) {
            # 只复制源文件，不把历史产物、部署身份或报告带入另一个工作区。
            if ($child.Name -in @('target', 'reports', 'deploy', '.git')) { continue }
            Copy-KikFixtureSource $child.FullName (Join-Path $Destination $child.Name) $RepositoryRoot $WorkspaceRoot
        }
    } else {
        [void][IO.Directory]::CreateDirectory([IO.Path]::GetDirectoryName($Destination))
        Copy-Item -LiteralPath $Source -Destination $Destination -ErrorAction Stop
    }
}

function New-KikTestFixture {
    param([string]$RepositoryRoot, [string]$FixtureRoot)

    $repository = (Resolve-Path -LiteralPath $RepositoryRoot).Path
    $targetRoot = Assert-KikFixturePath (Join-Path $repository 'target') $repository
    $fixture = Assert-KikFixturePath $FixtureRoot $targetRoot
    # 每轮创建新副本，避免已删除的源文件残留；编译缓存单独复用且始终留在 target 内。
    $workspace = Assert-KikFixturePath (Join-Path $fixture ('workspace-' + [Guid]::NewGuid().ToString('N'))) $targetRoot
    $cargoTarget = Assert-KikFixturePath (Join-Path $fixture 'cargo-target') $targetRoot
    [void][IO.Directory]::CreateDirectory($workspace)
    [void][IO.Directory]::CreateDirectory($cargoTarget)

    $sourceConfig = Assert-KikFixturePath (Join-Path $repository 'common/config.json') $repository
    $sourceConfigHash = (Get-FileHash -LiteralPath $sourceConfig -Algorithm SHA256).Hash.ToLowerInvariant()
    foreach ($file in @('Cargo.toml', 'Cargo.lock')) {
        Copy-KikFixtureSource (Join-Path $repository $file) (Join-Path $workspace $file) $repository $workspace
    }
    # 保留原 workspace 与 Cargo.lock，避免缩减成员导致 --locked 重新解析依赖。
    # 新增 workspace 成员时也应在这里登记；根 config 是 real_ctrl 的 include_str! 编译输入。
    foreach ($member in @('real_ctrl', 'ctrl_server', 'ctrl_kik', 'common', 'ctrl_common', 'start', 'screen_stream', 'string_obfuscation_macros')) {
        foreach ($name in @('Cargo.toml', 'build.rs', 'rust-toolchain.toml', 'src', 'examples', 'tests')) {
            $relative = Join-Path $member $name
            $source = Join-Path $repository $relative
            if (Test-Path -LiteralPath $source) {
                Copy-KikFixtureSource $source (Join-Path $workspace $relative) $repository $workspace
            }
        }
    }
    foreach ($relative in @('config', 'common/config.json', 'common/string_obfuscation_key.rs')) {
        Copy-KikFixtureSource (Join-Path $repository $relative) (Join-Path $workspace $relative) $repository $workspace
    }

    $fixtureConfig = Join-Path $workspace 'common/config.json'
    $config = Get-Content -LiteralPath $fixtureConfig -Raw -Encoding UTF8 | ConvertFrom-Json
    # 使用普通相对锁文件名，让同一个 fixture EXE 在两个专属工作目录形成两个测试实例。
    # 这是修改副本配置，不是为 Kik 增加任何构建或运行时覆盖入口。
    $config.strings.LOCK_FILE_PATH = 'ctrl_kik.lock'
    [IO.File]::WriteAllText($fixtureConfig, ($config | ConvertTo-Json -Depth 20), [Text.UTF8Encoding]::new($false))
    $sourceConfigHashAfter = (Get-FileHash -LiteralPath $sourceConfig -Algorithm SHA256).Hash.ToLowerInvariant()
    if ($sourceConfigHashAfter -ne $sourceConfigHash) { throw '创建 Kik 测试副本期间原 config.json 发生变化，请重新运行验收。' }

    return [pscustomobject]@{
        workspace = $workspace
        cargo_target = $cargoTarget
        config_path = $fixtureConfig
        config_sha256 = (Get-FileHash -LiteralPath $fixtureConfig -Algorithm SHA256).Hash.ToLowerInvariant()
        lock_file_path = 'ctrl_kik.lock'
        source_config_path = $sourceConfig
        source_config_sha256_before = $sourceConfigHash
        source_config_sha256_after = $sourceConfigHashAfter
        source_config_unchanged = $true
        purpose = 'local-e2e-fixture-only'
    }
}
