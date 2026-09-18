# 发布选择与产物校验集中在此；只定义函数，加载本文件不会构建、联网或修改部署身份。
function Get-PublishPlan {
    param([switch]$BuildOnly, [switch]$DeployOnly, [switch]$SkipCtrlServer, [switch]$SkipCtrlKik, [switch]$SkipRealCtrl, [switch]$SkipPublicTests)
    if ($BuildOnly -and $DeployOnly) { throw 'BuildOnly 与 DeployOnly 不能同时使用。' }
    if ($DeployOnly -and ($SkipCtrlServer -or $SkipCtrlKik -or $SkipRealCtrl)) { throw 'DeployOnly 只发布已校验的服务端产物，不接受组件跳过参数。' }
    $selected = @()
    if (!$DeployOnly) {
        if (!$SkipCtrlServer) { $selected += 'ctrl_server' }
        if (!$SkipCtrlKik) { $selected += 'ctrl_kik' }
        if (!$SkipRealCtrl) { $selected += 'real_ctrl' }
        if (!$selected.Count) { throw '至少需要选择一个构建组件。' }
    }
    $deploy = !$BuildOnly -and ($DeployOnly -or !$SkipCtrlServer)
    [pscustomobject]@{
        build = $selected
        skipped = @(@('ctrl_server','ctrl_kik','real_ctrl') | Where-Object { $_ -notin $selected })
        deployServer = $deploy
        publicTests = $deploy -and !$SkipPublicTests
        mode = if ($DeployOnly) { 'deploy-only' } elseif ($BuildOnly) { 'build-only' } else { 'build-and-publish-selected' }
    }
}

function Get-ArtifactComponent {
    param([string]$Name)
    # 白名单同时防止清单路径穿越；身份文件永远不能成为可分发产物。
    if ($Name -eq 'ctrl_server') { return 'ctrl_server' }
    if ($Name -in @('ctrl_kik.exe','ctrl_kik.build-receipt.json')) { return 'ctrl_kik' }
    if ($Name -match '^real_ctrl(_local_server|_invoker_http_service)?\.(exe|pdb)$') { return 'real_ctrl' }
    throw "清单中出现未知产物或非法路径：$Name"
}

function Read-VerifiedArtifacts {
    param([string]$Directory, [string]$ManifestPath, [string]$Channel, [string[]]$Components)
    if (!(Test-Path -LiteralPath $ManifestPath)) {
        if (Test-Path -LiteralPath $Directory) {
            $untracked = @(Get-ChildItem -LiteralPath $Directory -File | Where-Object { $_.Extension -ne '.lock' })
            if ($untracked.Count) { throw '已有产物缺少可信清单，不能静默复用。请恢复清单或完整重建。' }
        }
        return
    }
    $manifest = Get-Content -LiteralPath $ManifestPath -Raw -Encoding UTF8 | ConvertFrom-Json
    $kikPort = if ($Channel -eq 'gray') { 9005 } else { 9002 }
    $tlsPort = if ($Channel -eq 'gray') { 9009 } else { 9007 }
    if ($manifest.channel -ne $Channel -or $manifest.kik_noise_port -ne $kikPort -or $manifest.control_tls_port -ne $tlsPort) { throw '已有产物清单的通道或端口不匹配。' }
    $seen = @{}
    foreach ($entry in $manifest.artifacts) {
        $component = Get-ArtifactComponent $entry.name
        if ($seen.ContainsKey($entry.name)) { throw '清单包含重复产物。' }
        $seen[$entry.name] = $true
        if ($component -notin $Components) { continue }
        $path = Join-Path $Directory $entry.name
        if (!(Test-Path -LiteralPath $path -PathType Leaf) -or (Get-Item -LiteralPath $path).Length -ne $entry.bytes -or (Get-FileHash -LiteralPath $path -Algorithm SHA256).Hash.ToLowerInvariant() -ne $entry.sha256) { throw "保留产物校验失败：$($entry.name)" }
        # schema 2 的 built_at 是整包时间；schema 3 逐组件保留最初构建时间，不伪装成本轮新包。
        $builtAt = $manifest.built_at
        if ($manifest.PSObject.Properties.Name -contains 'components') {
            $prior = $manifest.components.$component
            if ($prior -and $prior.built_at) { $builtAt = $prior.built_at }
        }
        [pscustomobject]@{ name=$entry.name; bytes=$entry.bytes; sha256=$entry.sha256; component=$component; built_at=$builtAt }
    }
}

function Save-SelectedArtifacts {
    param([string]$Directory, [string]$ManifestPath, [string]$Channel, [string]$BackupDirectory, [System.Collections.IDictionary]$Sources, [object[]]$Retained)
    $selected = @($Sources.Keys | ForEach-Object { Get-ArtifactComponent $_ } | Select-Object -Unique)
    $fullDirectory = [IO.Path]::GetFullPath($Directory)
    $repoPrefix = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..')).TrimEnd('\','/') + [IO.Path]::DirectorySeparatorChar
    if (!$fullDirectory.StartsWith($repoPrefix, [StringComparison]::OrdinalIgnoreCase) -or ![IO.Path]::GetFullPath($BackupDirectory).StartsWith($repoPrefix, [StringComparison]::OrdinalIgnoreCase)) { throw '产物与回退目录必须位于当前仓库。' }
    foreach ($source in $Sources.Values) { if (!(Test-Path -LiteralPath $source -PathType Leaf)) { throw "本轮构建缺少产物：$source" } }
    [IO.Directory]::CreateDirectory($Directory) | Out-Null
    [IO.Directory]::CreateDirectory($BackupDirectory) | Out-Null
    $oldSelected = @(Get-ChildItem -LiteralPath $Directory -File | Where-Object { $_.Extension -ne '.lock' } | Where-Object { (Get-ArtifactComponent $_.Name) -in $selected })
    foreach ($file in $oldSelected) {
        # 先验证文件没有被运行中的用户进程占用；不结束用户会话，不动被跳过的 EXE。
        $stream = [IO.File]::Open($file.FullName, 'Open', 'ReadWrite', 'None')
        $stream.Dispose()
        Copy-Item -LiteralPath $file.FullName -Destination (Join-Path $BackupDirectory $file.Name)
    }
    if (Test-Path -LiteralPath $ManifestPath) { Copy-Item -LiteralPath $ManifestPath -Destination (Join-Path $BackupDirectory 'manifest.json') }
    try {
        # 只清理选中组件。跳过组件保持原文件和摘要，避免其正在运行时也被全目录清理影响。
        foreach ($file in $oldSelected) { Remove-Item -LiteralPath $file.FullName -Force }
        foreach ($entry in $Sources.GetEnumerator()) { Copy-Item -LiteralPath $entry.Value -Destination (Join-Path $Directory $entry.Key) }
        $now = [DateTimeOffset]::UtcNow.ToString('o')
        $artifacts = @($Retained | ForEach-Object { [ordered]@{name=$_.name;bytes=$_.bytes;sha256=$_.sha256} })
        foreach ($name in $Sources.Keys) {
            $path = Join-Path $Directory $name
            $artifacts += [ordered]@{name=$name;bytes=(Get-Item -LiteralPath $path).Length;sha256=(Get-FileHash -LiteralPath $path).Hash.ToLowerInvariant()}
        }
        $components = [ordered]@{}
        foreach ($name in @('ctrl_server','ctrl_kik','real_ctrl')) {
            $previous = @($Retained | Where-Object component -eq $name)
            $components[$name] = [ordered]@{
                status = if ($name -in $selected) { 'built' } elseif ($previous.Count) { 'retained' } else { 'absent' }
                built_at = if ($name -in $selected) { $now } elseif ($previous.Count) { $previous[0].built_at } else { $null }
            }
        }
        $manifest = [ordered]@{schema_version=3;channel=$Channel;kik_noise_port=$(if($Channel -eq 'gray'){9005}else{9002});control_tls_port=$(if($Channel -eq 'gray'){9009}else{9007});target='x86_64-unknown-linux-musl';built_at=$now;components=$components;artifacts=$artifacts}
        [IO.File]::WriteAllText("$ManifestPath.tmp", ($manifest | ConvertTo-Json -Depth 8), [Text.UTF8Encoding]::new($false))
        Move-Item -LiteralPath "$ManifestPath.tmp" -Destination $ManifestPath -Force
    } catch {
        # 本地提交失败也还原选中组件；上一轮清单继续有效，跳过组件从未参与写入。
        foreach ($name in $Sources.Keys) { $path=Join-Path $Directory $name; if(Test-Path -LiteralPath $path){Remove-Item -LiteralPath $path -Force} }
        foreach ($file in $oldSelected) { Copy-Item -LiteralPath (Join-Path $BackupDirectory $file.Name) -Destination $file.FullName -Force }
        throw
    }
}
