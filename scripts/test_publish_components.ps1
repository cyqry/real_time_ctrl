# 测试只写仓库 target/ 专用夹具，不构建业务程序、不读取部署身份、不连接服务器。
$ErrorActionPreference = 'Stop'
. (Join-Path $PSScriptRoot 'publish_components.ps1')
$root = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..'))
$work = Join-Path $root ('target/publish-component-tests/' + [Guid]::NewGuid().ToString('N'))
$artifacts = Join-Path $work 'artifacts'
$manifest = Join-Path $work 'manifest.json'
[IO.Directory]::CreateDirectory($artifacts) | Out-Null
$checks = [Collections.Generic.List[string]]::new()
function Expect-Failure([string]$Name,[scriptblock]$Action) {
    $failed = $false
    try { & $Action | Out-Null } catch { $failed=$true }
    if (!$failed) { throw "应拒绝却成功：$Name" }
    $checks.Add($Name)
}
foreach ($mask in 0..6) {
    $server = [bool]($mask -band 1); $kik=[bool]($mask -band 2); $ctrl=[bool]($mask -band 4)
    $plan = Get-PublishPlan -SkipCtrlServer:$server -SkipCtrlKik:$kik -SkipRealCtrl:$ctrl
    if ($plan.deployServer -eq $server -or (('ctrl_server' -in $plan.build) -eq $server) -or (('ctrl_kik' -in $plan.build) -eq $kik) -or (('real_ctrl' -in $plan.build) -eq $ctrl)) { throw "跳过组合不正确：$mask" }
    $checks.Add("selection-$mask")
}
Expect-Failure 'reject-all-skipped' { Get-PublishPlan -SkipCtrlServer -SkipCtrlKik -SkipRealCtrl }
Expect-Failure 'reject-conflicting-modes' { Get-PublishPlan -BuildOnly -DeployOnly }
Expect-Failure 'reject-deploy-only-skip' { Get-PublishPlan -DeployOnly -SkipCtrlServer }
if ((Get-PublishPlan -BuildOnly).deployServer -or (Get-PublishPlan -DeployOnly).build.Count) { throw '构建/部署模式不正确。' }
$checks.Add('build-only-and-deploy-only')

# 旧 Kik 是真实文件；部分发布过程中以禁止写入的共享读句柄持有它，证明不会删除/改写跳过组件。
$kikPath = Join-Path $artifacts 'ctrl_kik.exe'
[IO.File]::WriteAllText($kikPath,'old-fixture-kik')
$oldTime = '2026-01-01T00:00:00Z'
$oldManifest = [ordered]@{schema_version=2;channel='gray';kik_noise_port=9005;control_tls_port=9009;built_at=$oldTime;artifacts=@(@{name='ctrl_kik.exe';bytes=(Get-Item $kikPath).Length;sha256=(Get-FileHash $kikPath).Hash.ToLowerInvariant()})}
[IO.File]::WriteAllText($manifest,($oldManifest | ConvertTo-Json -Depth 8))
$retained = @(Read-VerifiedArtifacts -Directory $artifacts -ManifestPath $manifest -Channel gray -Components @('ctrl_kik'))
$newServer = Join-Path $work 'new-server'
[IO.File]::WriteAllText($newServer,'new-fixture-server')
$handle = [IO.File]::Open($kikPath,'Open','Read','Read')
try { Save-SelectedArtifacts -Directory $artifacts -ManifestPath $manifest -Channel gray -BackupDirectory (Join-Path $work 'backup') -Sources ([ordered]@{ctrl_server=$newServer}) -Retained $retained }
finally { $handle.Dispose() }
$updated = Get-Content -Raw $manifest | ConvertFrom-Json
if ($updated.components.ctrl_kik.status -ne 'retained' -or [DateTimeOffset]$updated.components.ctrl_kik.built_at -ne [DateTimeOffset]$oldTime -or $updated.components.ctrl_server.status -ne 'built' -or [IO.File]::ReadAllText($kikPath) -ne 'old-fixture-kik') { throw '部分提交错误地改变或重标记了保留产物。' }
$checks.Add('retain-open-skipped-binary-and-original-build-time')
$null = @(Read-VerifiedArtifacts -Directory $artifacts -ManifestPath $manifest -Channel gray -Components @('ctrl_server','ctrl_kik'))
$checks.Add('committed-manifest-hashes-match')
Expect-Failure 'reject-other-channel' { Read-VerifiedArtifacts -Directory $artifacts -ManifestPath $manifest -Channel production -Components @('ctrl_server') }
[IO.File]::AppendAllText($kikPath,'tamper')
Expect-Failure 'reject-tampered-retained-artifact' { Read-VerifiedArtifacts -Directory $artifacts -ManifestPath $manifest -Channel gray -Components @('ctrl_kik') }
Expect-Failure 'reject-manifest-path-traversal' { Get-ArtifactComponent '../identity/server.key' }
Expect-Failure 'reject-unknown-artifact' { Get-ArtifactComponent 'app.apk' }
[IO.Directory]::CreateDirectory((Join-Path $root 'reports')) | Out-Null
[ordered]@{success=$true;checks=@($checks);count=$checks.Count;fixture=$work} | ConvertTo-Json -Depth 5 | Set-Content -LiteralPath (Join-Path $root 'reports/publish-components-tests.json') -Encoding UTF8
"发布选择与产物回归通过：$($checks.Count) 项。"
