# 由 e2e_ctrl_stack.ps1 在原链路通过后调用。测试程序、任务配置和临时文件都位于仓库 target。
# 不导出环境秘密；只使用上层已有的本地 API 函数，结束时仅清理本轮 UUID 目录。
# 二进制缓存保留在 target/e2e/task-temp 供检查，可在测试进程结束后清理，不属于部署产物。

function Invoke-TaskExecutionChecks {
    param([string]$RepositoryRoot, [string]$WorkRoot, $SecondaryController, [int]$SecondaryPort)
    $taskPrefix = 'e2e_' + [Guid]::NewGuid().ToString('N')
    $taskCatalogRoot = Join-Path $RepositoryRoot 'target/debug/tasks'
    $taskRunRoot = Join-Path $WorkRoot $taskPrefix
    $fixture = Join-Path $RepositoryRoot 'target/debug/examples/task_fixture.exe'
    [IO.Directory]::CreateDirectory($taskRunRoot) | Out-Null
    $created = [Collections.Generic.List[string]]::new()
    $checks = [Collections.Generic.List[string]]::new()
    $taskFileNames = @{}
    $originalFailure = $null
    $taskReport = [ordered]@{ started_at = (Get-Date).ToString('o'); success = $false; checks = @(); metrics = @{} }

    function New-TaskFixture {
        param([string]$Suffix, [string[]]$Arguments = @(), [string]$Mode = 'sync',
            [string]$ResponseMode = 'output', [int]$TimeoutSeconds = 10, [switch]$NoConfig,
            [AllowEmptyString()][string]$FileName, [switch]$UseBinaryName,
            [string]$BinaryPath = 'tool.exe', [switch]$OmitArguments,
            [string]$DefaultContent = 'fixture-default-response')
        $name = "${taskPrefix}_$Suffix"
        $directory = Join-Path $taskCatalogRoot $name
        [IO.Directory]::CreateDirectory($directory) | Out-Null
        $created.Add($directory)
        Copy-Item -LiteralPath $fixture -Destination (Join-Path $directory 'tool.exe')
        # 普通用例各自使用唯一文件名，避免新的“同名共享缓存”规则使无关用例互相覆盖。
        # 只有专门验证缺省名称或共享缓存的用例才省略/复用 file_name。
        if (!$PSBoundParameters.ContainsKey('FileName')) { $FileName = "$name.exe" }
        $taskFileNames[$name] = if ($NoConfig -or $UseBinaryName) { 'tool.exe' } else { $FileName }
        if (!$NoConfig) {
            # JSON 字符串的转义是 TOML 基本字符串可接受的子集；实际配置保持真实换行。
            $argumentText = ($Arguments | ForEach-Object { ConvertTo-Json -InputObject $_ -Compress }) -join ', '
            $text = "enabled = true`nbinary = " + (ConvertTo-Json -InputObject $BinaryPath -Compress) + "`nmode = '$Mode'`n"
            if (!$OmitArguments) { $text += "args = [$argumentText]`n" }
            if (!$UseBinaryName) { $text += 'file_name = ' + (ConvertTo-Json -InputObject $FileName -Compress) + "`n" }
            if ($Mode -eq 'sync') { $text += "timeout_seconds = $TimeoutSeconds`n" }
            $text += "[response]`nmode = '$ResponseMode'`ndefault_content = " + (ConvertTo-Json -InputObject $DefaultContent -Compress) + "`n"
            [IO.File]::WriteAllText((Join-Path $directory 'task.toml'), $text, [Text.UTF8Encoding]::new($false))
        }
        return $name
    }

    function Run-TaskFixture {
        param([string]$Name)
        Invoke-ApiCommand -Command @{kind='run_task'; task_name=$Name} -RequestId ([Guid]::NewGuid().ToString('N'))
    }

    function Assert-TaskSuccess {
        param($Response, [string]$Expected)
        if (!$Response.ok -or $Response.data.kind -ne 'info' -or !$Response.data.message.Contains($Expected)) {
            throw "Task response mismatch: $($Response | ConvertTo-Json -Depth 6 -Compress)"
        }
    }

    function Get-TaskCachePath {
        param($Response, [string]$Name)
        Assert-TaskSuccess $Response 'exe:'
        $match = [regex]::Match($Response.data.message, '(?m)^exe:(.+)\r?$')
        if (!$match.Success) { throw 'Task fixture did not report its executable path' }
        $path = $match.Groups[1].Value.TrimEnd("`r")
        # current_exe 可能返回 Windows 扩展路径前缀，先归一化再校验测试文件所有权。
        if ($path.StartsWith('\\?\')) { $path = $path.Substring(4) }
        $full = [IO.Path]::GetFullPath($path)
        $allowed = [IO.Path]::GetFullPath((Join-Path $WorkRoot 'task-temp')) + [IO.Path]::DirectorySeparatorChar
        if (!$full.StartsWith($allowed, [StringComparison]::OrdinalIgnoreCase)) { throw 'Task cache escaped the repository test temporary directory' }
        # 检查完整缓存层级和本用例要求的叶名称，不能只看到 .exe 就相信它属于任务缓存。
        $relative = $full.Substring($allowed.Length)
        if ($relative -notmatch '^[0-9a-f]{64}[\\/][^\\/]+$') { throw 'Task executable is outside the direct temporary deployment cache layout' }
        if (!$taskFileNames.ContainsKey($Name) -or ![string]::Equals([IO.Path]::GetFileName($full), $taskFileNames[$Name], [StringComparison]::OrdinalIgnoreCase)) { throw 'Task filename did not match the configured or default binary basename' }
        return $full
    }

    function Add-TaskFixtureTrailer {
        param([string]$Name, [byte]$Value)
        # PE 末尾附加测试字节不改变程序行为，但真实改变大小和 SHA-256，模拟手动更新服务端二进制。
        $path = Join-Path $taskCatalogRoot "$Name/tool.exe"
        $stream = [IO.File]::Open($path, [IO.FileMode]::Append, [IO.FileAccess]::Write, [IO.FileShare]::None)
        try { $stream.WriteByte($Value) } finally { $stream.Dispose() }
    }

    function Invoke-TaskCacheChecks {
        $defaultName = New-TaskFixture -Suffix cache_default_name -Arguments @('identity') -UseBinaryName
        [void](Get-TaskCachePath (Run-TaskFixture $defaultName) $defaultName)
        $checks.Add('missing file_name preserves the server binary basename tool.exe')

        $sharedFileName = "任务 $taskPrefix.EXE"
        $name = New-TaskFixture -Suffix cache -Arguments @('identity','first-configuration') -FileName $sharedFileName
        $first = Run-TaskFixture $name
        Assert-TaskSuccess $first 'arg:first-configuration'
        $cache = Get-TaskCachePath $first $name
        $source = Join-Path $taskCatalogRoot "$name/tool.exe"
        if ((Get-FileHash -LiteralPath $cache).Hash -ne (Get-FileHash -LiteralPath $source).Hash) { throw 'First transfer cache does not match the server binary' }
        $stamp = [DateTime]::new(2020,1,2,3,4,5,[DateTimeKind]::Utc)
        [IO.File]::SetLastWriteTimeUtc($cache, $stamp)
        $second = Run-TaskFixture $name
        if ((Get-TaskCachePath $second $name) -ne $cache -or [IO.File]::GetLastWriteTimeUtc($cache) -ne $stamp) { throw 'Cache hit replaced the executable or changed its filename' }
        $checks.Add('configured Unicode and space-containing file_name is preserved; a hash hit keeps the file without rewriting it')

        $config = Join-Path $taskCatalogRoot "$name/task.toml"
        [IO.File]::WriteAllText($config, ([IO.File]::ReadAllText($config).Replace('first-configuration','updated-configuration')))
        $reply = Run-TaskFixture $name
        Assert-TaskSuccess $reply 'arg:updated-configuration'
        if ((Get-TaskCachePath $reply $name) -ne $cache -or [IO.File]::GetLastWriteTimeUtc($cache) -ne $stamp) { throw 'A configuration-only change rewrote the binary cache' }
        $checks.Add('a binary cache hit still executes the newly loaded task configuration')

        Add-TaskFixtureTrailer $name 71
        $reply = Run-TaskFixture $name
        if ((Get-TaskCachePath $reply $name) -ne $cache -or (Get-FileHash -LiteralPath $cache).Hash -ne (Get-FileHash -LiteralPath $source).Hash) { throw 'Updated server binary did not safely replace the same stable cache path' }
        # 改坏最后的测试尾字节，文件大小保持不变；必须比较摘要，不能只比较存在性或大小。
        $stream = [IO.File]::Open($cache, [IO.FileMode]::Open, [IO.FileAccess]::Write, [IO.FileShare]::None)
        try { [void]$stream.Seek(-1, [IO.SeekOrigin]::End); $stream.WriteByte(72) } finally { $stream.Dispose() }
        $reply = Run-TaskFixture $name
        if ((Get-TaskCachePath $reply $name) -ne $cache -or (Get-FileHash -LiteralPath $cache).Hash -ne (Get-FileHash -LiteralPath $source).Hash) { throw 'Same-size damaged cache was used without replacement' }
        $checks.Add('server binary updates and same-size cache corruption trigger a verified replacement at the same filename')

        $other = New-TaskFixture -Suffix cache_other -Arguments @('identity','shared-task-configuration') -FileName $sharedFileName
        # 主任务已经更新过尾字节；共享用例必须复制更新后的内容，否则应当发生替换而非命中。
        Copy-Item -LiteralPath $source -Destination (Join-Path $taskCatalogRoot "$other/tool.exe") -Force
        [IO.File]::SetLastWriteTimeUtc($cache, $stamp)
        $sharedReply = Run-TaskFixture $other
        Assert-TaskSuccess $sharedReply 'arg:shared-task-configuration'
        if ((Get-TaskCachePath $sharedReply $other) -ne $cache -or [IO.File]::GetLastWriteTimeUtc($cache) -ne $stamp) { throw 'Two tasks with identical final names and content did not reuse the unchanged cache file' }
        $separate = New-TaskFixture -Suffix cache_separate -Arguments @('identity')
        Copy-Item -LiteralPath $source -Destination (Join-Path $taskCatalogRoot "$separate/tool.exe") -Force
        if ((Get-TaskCachePath (Run-TaskFixture $separate) $separate) -eq $cache) { throw 'Different configured final names unexpectedly shared one cache file' }
        $checks.Add('different tasks share unchanged bytes at the same final filename and remain separate at different final filenames')

        # 非法名字必须由服务端拒绝，不能退回旧散列名或直接执行 tool.exe；坏配置不拖累相邻任务。
        $invalidNames = @('', '../escape.exe', 'C:\escape.exe', 'file.exe:stream', 'CON.exe', 'COM¹.exe', 'LPT².exe', 'trailing.exe ', 'trailing.exe.', 'LONGFI~1.EXE', ('界' * 80 + '.exe'))
        for ($index = 0; $index -lt $invalidNames.Count; $index++) {
            $invalid = New-TaskFixture -Suffix "invalid_name_$index" -Arguments @('identity') -FileName $invalidNames[$index]
            if ((Run-TaskFixture $invalid).ok) { throw "Invalid file_name case $index unexpectedly executed" }
        }
        Assert-TaskSuccess (Run-TaskFixture $other) 'arg:shared-task-configuration'
        $checks.Add('invalid path, device, stream, suffix, short alias and oversized UTF-8 filenames fail independently')

        [IO.File]::WriteAllText($config, ([IO.File]::ReadAllText($config).Replace('enabled = true','enabled = false')))
        if ((Run-TaskFixture $name).ok) { throw 'A warm binary cache bypassed the disabled task configuration' }
        $checks.Add('a shared warm cache does not bypass the disabled task configuration')

        # 同一任务的四次同步运行应重叠；准备阶段的互斥锁不能覆盖整个三秒执行期。
        $parallel = New-TaskFixture -Suffix cache_parallel -Arguments @('sleep','3000') -TimeoutSeconds 15
        Assert-TaskSuccess (Run-TaskFixture $parallel) 'slept:3000'
        $client = [Net.Http.HttpClient]::new()
        $client.DefaultRequestHeaders.Authorization = [Net.Http.Headers.AuthenticationHeaderValue]::new('Bearer', $ApiToken)
        $contents = [Collections.Generic.List[Net.Http.StringContent]]::new()
        $pending = [Collections.Generic.List[Threading.Tasks.Task[Net.Http.HttpResponseMessage]]]::new()
        try {
            $watch = [Diagnostics.Stopwatch]::StartNew()
            for ($index = 0; $index -lt 4; $index++) {
                $body = @{version=1;request_id="${taskPrefix}-cached-parallel-$index";command=@{kind='run_task';task_name=$parallel}} | ConvertTo-Json -Depth 5 -Compress
                $content = [Net.Http.StringContent]::new($body, [Text.Encoding]::UTF8, 'application/json')
                $contents.Add($content)
                $pending.Add($client.PostAsync("http://127.0.0.1:$HttpPort/api/v1/commands", $content))
            }
            if (![Threading.Tasks.Task]::WaitAll([Threading.Tasks.Task[]]$pending.ToArray(), 20000)) { throw 'Same-task cached concurrent responses timed out' }
            foreach ($request in $pending) {
                $response = $request.Result
                try { Assert-TaskSuccess ($response.Content.ReadAsStringAsync().Result | ConvertFrom-Json) 'slept:3000' }
                finally { $response.Dispose() }
            }
            if ($watch.Elapsed.TotalSeconds -ge 9) { throw 'The cache lock serialized same-task execution instead of only preparation' }
            $taskReport.metrics.same_task_cached_parallel_ms = $watch.ElapsedMilliseconds
            $checks.Add('four cached invocations of the same task execute concurrently without holding the preparation lock until exit')
        } finally {
            foreach ($content in $contents) { $content.Dispose() }
            $client.Dispose()
        }
    }

    function Invoke-TaskBinaryPathChecks {
        # 点组件只改变配置写法，不改变实际源文件和缓存叶名称；每次都走真实服务端解析与 Kik 执行。
        $plain = New-TaskFixture -Suffix binary_plain -Arguments @('identity') -UseBinaryName
        $cache = Get-TaskCachePath (Run-TaskFixture $plain) $plain
        $stamp = [DateTime]::new(2020, 2, 3, 4, 5, 6, [DateTimeKind]::Utc)
        [IO.File]::SetLastWriteTimeUtc($cache, $stamp)
        $paths = @('./tool.exe', '././tool.exe', '././bin/./tool.exe')
        for ($index = 0; $index -lt $paths.Count; $index++) {
            $name = New-TaskFixture -Suffix "binary_dot_$index" -Arguments @('identity') -UseBinaryName -BinaryPath $paths[$index]
            if ($paths[$index].Contains('bin/')) {
                $nestedDirectory = Join-Path $taskCatalogRoot "$name/bin"
                [IO.Directory]::CreateDirectory($nestedDirectory) | Out-Null
                Copy-Item -LiteralPath $fixture -Destination (Join-Path $nestedDirectory 'tool.exe')
            }
            $reply = Run-TaskFixture $name
            if ((Get-TaskCachePath $reply $name) -ne $cache -or [IO.File]::GetLastWriteTimeUtc($cache) -ne $stamp) {
                throw 'Safe dot components changed the binary basename or rewrote an identical cache entry'
            }
            if ((Get-FileHash -LiteralPath $cache).Hash -ne (Get-FileHash -LiteralPath $fixture).Hash) { throw 'Dot-path task cache does not match its source binary' }
        }
        $checks.Add('leading, repeated and nested dot components execute and reuse the unchanged original-basename cache')

        # 复现用户的最小异步配置：args、file_name 和 timeout_seconds 都不写，由服务端应用默认参数。
        $minimal = New-TaskFixture -Suffix binary_dot_async -BinaryPath './tool.exe' -Mode async -ResponseMode default `
            -UseBinaryName -OmitArguments -DefaultContent '任务已启动'
        $minimalText = [IO.File]::ReadAllText((Join-Path $taskCatalogRoot "$minimal/task.toml"))
        if ($minimalText -match '(?m)^\s*(args|file_name|timeout_seconds)\s*=') { throw 'Minimal async fixture accidentally wrote an omitted configuration field' }
        $reply = Run-TaskFixture $minimal
        Assert-TaskSuccess $reply '任务已启动'
        if ($reply.data.message -ne '任务已启动') { throw 'Minimal async task did not return exactly its configured default response' }
        $checks.Add('dot-path async configuration without args, file_name or timeout starts and returns its configured response')

        # 第一项确实指向存在且可执行的相邻任务，避免把“文件不存在”误认为路径越界防护通过。
        $invalidPaths = @("../$plain/tool.exe", "./../$plain/tool.exe", './bin/../tool.exe', './tool.exe/..', './tool.exe/', './tool.exe/.', '.', './')
        for ($index = 0; $index -lt $invalidPaths.Count; $index++) {
            $invalid = New-TaskFixture -Suffix "binary_invalid_$index" -Arguments @('identity') -BinaryPath $invalidPaths[$index]
            [IO.Directory]::CreateDirectory((Join-Path $taskCatalogRoot "$invalid/bin")) | Out-Null
            if ((Run-TaskFixture $invalid).ok) { throw "Unsafe or fileless binary path case $index unexpectedly executed" }
        }
        Assert-TaskSuccess (Run-TaskFixture $plain) 'exe:'
        $checks.Add('parent traversal and dot-only binary paths are rejected while a neighboring valid task still executes')
    }

    function Invoke-TaskBindingNetworkProbe {
        param([string]$TaskName)
        # 先固定原真实 Kik，避免独立假主连接的上线/下线改变此次负例的任务目标。
        $selected = Invoke-ApiCommand -Command @{kind='local_now'} -RequestId 'task-probe-target'
        if (!$selected.ok -or !$selected.data.value.Kik.id) { throw 'Task probe requires a real selected Kik' }
        $targetId = $selected.data.value.Kik.id
        $selectReply = Invoke-ApiCommand -Command @{kind='local_use';kik_id=$targetId} -RequestId 'task-probe-select'
        if (!$selectReply.ok) { throw 'Could not pin the real target for the task binding probe' }
        $probeDirectory = Join-Path $taskRunRoot 'binding-probe'
        [IO.Directory]::CreateDirectory($probeDirectory) | Out-Null
        $probe = Start-E2eProcess -Name "${taskPrefix}_binding_probe" `
            -ExePath (Join-Path $RepositoryRoot 'target/debug/examples/task_binding_probe.exe') `
            -EnvMap @{
                'RTC_E2E_TASK_NOISE_PORT' = "$KikNoisePort"
                'RTC_E2E_TASK_NOISE_PUBLIC_KEY' = $noise.Public
                'RTC_E2E_TASK_KIK_ID' = $targetId
                'RTC_E2E_TASK_PROBE_DIR' = $probeDirectory
            }
        $client = [Net.Http.HttpClient]::new()
        $client.Timeout = [TimeSpan]::FromSeconds(15)
        $client.DefaultRequestHeaders.Authorization = [Net.Http.Headers.AuthenticationHeaderValue]::new('Bearer', $ApiToken)
        try {
            $deadline = [DateTime]::UtcNow.AddSeconds(45)
            while (!(Test-Path -LiteralPath (Join-Path $probeDirectory 'ready'))) {
                if ($probe.Proc.HasExited) { throw "Task binding probe failed: $($probe.StdErrTask.Result)" }
                if ([DateTime]::UtcNow -ge $deadline) { throw 'Task binding probe readiness timed out' }
                Start-Sleep -Milliseconds 50
            }
            # 六次真实下发使数据池轮询多次经过第四连接；该连接只声明 Kik ID，没有归属证明。
            for ($index = 0; $index -lt 6; $index++) {
                # 缓存上线后重复调用可能零传输；每次更新测试尾字节，保持此负例真实覆盖六次数据分发。
                Add-TaskFixtureTrailer $TaskName ([byte]($index + 1))
                $body = @{version=1;request_id="task-binding-$index";command=@{kind='run_task';task_name=$TaskName}} | ConvertTo-Json -Depth 5 -Compress
                $content = [Net.Http.StringContent]::new($body, [Text.Encoding]::UTF8, 'application/json')
                try {
                    $response = $client.PostAsync("http://127.0.0.1:$HttpPort/api/v1/commands", $content).GetAwaiter().GetResult()
                    try {
                        $reply = $response.Content.ReadAsStringAsync().GetAwaiter().GetResult() | ConvertFrom-Json
                        Assert-TaskSuccess $reply 'arg:中文 参数'
                    } finally { $response.Dispose() }
                } finally { $content.Dispose() }
                if ($probe.Proc.HasExited) { throw "Unbound observer failed during task transfer: $($probe.StdErrTask.Result)" }
            }
            [IO.File]::WriteAllText((Join-Path $probeDirectory 'stop'), 'stop')
            if (!$probe.Proc.WaitForExit(10000)) { throw 'Task binding network probe did not finish' }
            if ($probe.Proc.ExitCode -ne 0) { throw "Task binding network probe failed: $($probe.StdErrTask.Result)" }
            $report = $probe.StdOutTask.Result | ConvertFrom-Json
            if (@($report.assertions).Count -ne 5) { throw 'Task binding probe returned an incomplete report' }
            foreach ($assertion in $report.assertions) { $checks.Add([string]$assertion) }
        } finally {
            $client.Dispose()
            [IO.File]::WriteAllText((Join-Path $probeDirectory 'stop'), 'stop')
            if (!$probe.Proc.HasExited -and !$probe.Proc.WaitForExit(3000)) {
                $probe.Proc.Kill($true)
                [void]$probe.Proc.WaitForExit(5000)
            }
        }
    }

    try {
        Invoke-TaskCacheChecks
        Invoke-TaskBinaryPathChecks
        $default = New-TaskFixture -Suffix default -NoConfig
        Assert-TaskSuccess (Run-TaskFixture $default) 'task-fixture-default'
        if (Test-Path -LiteralPath (Join-Path $taskCatalogRoot "$default/task.toml")) { throw 'Default run wrote config' }
        $checks.Add('missing configuration executes the unique EXE synchronously with defaults')

        $echo = New-TaskFixture -Suffix echo -Arguments @('echo','中文 参数','a"b','C:\tail\','&echo no-shell')
        $reply = Run-TaskFixture $echo
        foreach ($expected in @('arg:中文 参数','arg:a"b','arg:C:\tail\','arg:&echo no-shell')) { Assert-TaskSuccess $reply $expected }
        $checks.Add('Unicode, spaces, quotes, trailing backslashes and shell symbols reach the EXE unchanged')

        $fixed = New-TaskFixture -Suffix fixed -Arguments @('echo','must-not-be-returned') -ResponseMode default
        $reply = Run-TaskFixture $fixed
        Assert-TaskSuccess $reply 'fixture-default-response'
        if ($reply.data.message.Contains('must-not-be-returned')) { throw 'Fixed response leaked output' }
        $quiet = New-TaskFixture -Suffix quiet -Arguments @('quiet')
        Assert-TaskSuccess (Run-TaskFixture $quiet) 'fixture-default-response'
        $checks.Add('fixed responses wait for exit and empty output uses the default text')

        $failed = New-TaskFixture -Suffix failed -Arguments @('exit') -ResponseMode default
        $reply = Run-TaskFixture $failed
        if ($reply.ok -or !$reply.error.message.Contains('exit_code=7')) { throw 'Nonzero exit reported success' }
        $checks.Add('nonzero exit is returned as failure even with a configured success message')

        $flood = New-TaskFixture -Suffix flood -Arguments @('flood') -TimeoutSeconds 20
        $reply = Run-TaskFixture $flood
        Assert-TaskSuccess $reply '输出已截断'
        if ($reply.data.message.Length -gt 140000) { throw 'Output cap exceeded' }
        $checks.Add('concurrent 4 MiB stdout and stderr are drained with bounded response memory')

        $childMarker = Join-Path $taskRunRoot 'child.pid'
        $timed = New-TaskFixture -Suffix timeout -Arguments @('child',$childMarker) -TimeoutSeconds 2
        $watch = [Diagnostics.Stopwatch]::StartNew()
        $reply = Run-TaskFixture $timed
        if ($reply.ok -or !$reply.error.message.Contains('task_timeout') -or $watch.Elapsed.TotalSeconds -gt 15) { throw 'Hard timeout failed' }
        if (!(Test-Path -LiteralPath $childMarker)) { throw 'Timeout child did not start' }
        $childProcessId = [int][IO.File]::ReadAllText($childMarker)
        if (Get-Process -Id $childProcessId -ErrorAction SilentlyContinue) { throw 'Timeout left child process running' }
        $checks.Add('sync timeout kills the executable and its child before returning')

        $naturalChildMarker = Join-Path $taskRunRoot 'natural-child.pid'
        $natural = New-TaskFixture -Suffix natural -Arguments @('child-exit',$naturalChildMarker) -TimeoutSeconds 15
        Assert-TaskSuccess (Run-TaskFixture $natural) 'parent-finished'
        $naturalChildId = [int][IO.File]::ReadAllText($naturalChildMarker)
        if (Get-Process -Id $naturalChildId -ErrorAction SilentlyContinue) { throw 'Successful sync task left its descendant running' }
        $checks.Add('successful synchronous exit also closes descendant processes and inherited output handles')

        $badDirectory = Join-Path $taskCatalogRoot $failed
        [IO.File]::WriteAllText((Join-Path $badDirectory 'task.toml'), 'broken = [')
        if ((Run-TaskFixture $failed).ok) { throw 'Malformed configuration fell back to execution' }
        Assert-TaskSuccess (Run-TaskFixture $default) 'task-fixture-default'
        Copy-Item -LiteralPath $fixture -Destination (Join-Path $taskCatalogRoot "$default/second.EXE")
        if ((Run-TaskFixture $default).ok) { throw 'Ambiguous program executed' }
        Remove-Item -LiteralPath (Join-Path $taskCatalogRoot "$default/second.EXE")
        $checks.Add('broken neighboring configuration and multiple EXEs fail independently')

        $disabled = New-TaskFixture -Suffix disabled -Arguments @('quiet')
        $configPath = Join-Path $taskCatalogRoot "$disabled/task.toml"
        [IO.File]::WriteAllText($configPath, ([IO.File]::ReadAllText($configPath).Replace('enabled = true','enabled = false')))
        if ((Run-TaskFixture $disabled).ok) { throw 'Disabled task executed' }
        Remove-Item -LiteralPath $configPath
        Assert-TaskSuccess (Run-TaskFixture $disabled) 'task-fixture-default'
        $checks.Add('manual config update takes effect and removing config restores defaults')

        # 无配置的回退仅适用于文件确实不存在，不能吞掉空配置或把子目录程序当默认入口。
        $empty = New-TaskFixture -Suffix empty -NoConfig
        [IO.File]::WriteAllText((Join-Path $taskCatalogRoot "$empty/task.toml"), '')
        if ((Run-TaskFixture $empty).ok) { throw 'Empty configuration fell back to execution' }
        $missing = New-TaskFixture -Suffix missing -NoConfig
        Remove-Item -LiteralPath (Join-Path $taskCatalogRoot "$missing/tool.exe")
        if ((Run-TaskFixture $missing).ok) { throw 'Missing executable reported success' }
        $nested = New-TaskFixture -Suffix nested -NoConfig
        [IO.Directory]::CreateDirectory((Join-Path $taskCatalogRoot "$nested/bin")) | Out-Null
        Move-Item -LiteralPath (Join-Path $taskCatalogRoot "$nested/tool.exe") -Destination (Join-Path $taskCatalogRoot "$nested/bin/tool.exe")
        if ((Run-TaskFixture $nested).ok) { throw 'Default executable search entered a subdirectory' }
        Assert-TaskSuccess (Run-TaskFixture $default) 'task-fixture-default'
        $checks.Add('empty config, missing EXE and nested-only EXE fail without affecting a healthy task')

        # 20 个独立命令先后确认启动；最早的程序仍在运行，证明没有隐藏的 16 进程上限。
        $asyncNames = @()
        for ($index = 0; $index -lt 20; $index++) {
            $marker = Join-Path $taskRunRoot "async-$index.pid"
            $asyncNames += New-TaskFixture -Suffix "async$index" -Arguments @('sleep','30000',$marker) -Mode async -ResponseMode default
        }
        $watch.Restart()
        foreach ($name in $asyncNames) { Assert-TaskSuccess (Run-TaskFixture $name) 'fixture-default-response' }
        if ($watch.Elapsed.TotalSeconds -ge 25) { throw 'Async commands waited for execution completion' }
        $activeChildren = @(Get-ChildItem -LiteralPath $taskRunRoot -Filter 'async-*.pid' | ForEach-Object {
            Get-Process -Id ([int][IO.File]::ReadAllText($_.FullName)) -ErrorAction SilentlyContinue
        })
        if ($activeChildren.Count -lt 17) { throw 'Could not prove more than 16 active asynchronous processes' }
        $taskReport.metrics.async_start_ms = $watch.ElapsedMilliseconds
        $taskReport.metrics.async_active_processes = $activeChildren.Count
        $checks.Add('twenty asynchronous task processes start without waiting or a Kik run-count cap')

        # 冷缓存的准备/传输耗时不应改变运行数断言：所有同步进程保持在屏障前，观测齐20个才放行。
        # fixture自身75秒和Kik硬超时90秒提供兜底；失败路径仍只终止本轮记录的测试PID。
        $syncRelease = Join-Path $taskRunRoot 'sync-release'
        $syncNames = 0..19 | ForEach-Object { New-TaskFixture -Suffix "sync$_" -Arguments @('barrier',$syncRelease,(Join-Path $taskRunRoot "sync-$_.pid")) -TimeoutSeconds 90 }
        $client = [Net.Http.HttpClient]::new()
        $client.DefaultRequestHeaders.Authorization = [Net.Http.Headers.AuthenticationHeaderValue]::new('Bearer', $ApiToken)
        $syncContents = [Collections.Generic.List[Net.Http.StringContent]]::new()
        $syncResponses = [Collections.Generic.List[Net.Http.HttpResponseMessage]]::new()
        $startupWatch = [Diagnostics.Stopwatch]::StartNew()
        try {
            $pending = [Collections.Generic.List[Threading.Tasks.Task[Net.Http.HttpResponseMessage]]]::new()
            # 分两批提交，等首批已运行再提交第二批，避免把入站突发保护误当作运行数限制。
            foreach ($batch in @(0, 10)) {
                for ($index = $batch; $index -lt $batch + 10; $index++) {
                    $body = @{version=1;request_id="task-sync-$index";command=@{kind='run_task';task_name=$syncNames[$index]}} | ConvertTo-Json -Depth 5 -Compress
                    $content = [Net.Http.StringContent]::new($body, [Text.Encoding]::UTF8, 'application/json')
                    $syncContents.Add($content)
                    $pending.Add($client.PostAsync("http://127.0.0.1:$HttpPort/api/v1/commands", $content))
                }
                $deadline = [DateTime]::UtcNow.AddSeconds(30)
                while (@(Get-ChildItem -LiteralPath $taskRunRoot -Filter 'sync-*.pid').Count -lt $batch + 10) {
                    if ([DateTime]::UtcNow -gt $deadline) {
                        # 先保留真实响应，区分拒绝、启动失败与尚在传输；仅凭 PID 未出现不能归因于并发许可。
                        $diagnostics = for ($probe = 0; $probe -lt $pending.Count; $probe++) {
                            $request = $pending[$probe]
                            $detail = @{ index = $probe; status = "$($request.Status)" }
                            if ($request.Status -eq [Threading.Tasks.TaskStatus]::RanToCompletion) {
                                $detail.response = $request.Result.Content.ReadAsStringAsync().Result
                            } elseif ($request.IsFaulted) { $detail.error = $request.Exception.GetBaseException().Message }
                            $detail
                        }
                        $started = @(Get-ChildItem -LiteralPath $taskRunRoot -Filter 'sync-*.pid').Count
                        $taskReport.metrics.sync_start_failure = @{ started = $started; expected = $batch + 10; requests = @($diagnostics) }
                        throw "Synchronous task startup deadline exceeded: started=$started expected=$($batch + 10); $($diagnostics | ConvertTo-Json -Depth 5 -Compress)"
                    }
                    Start-Sleep -Milliseconds 25
                }
                $taskReport.metrics["sync_batch_${batch}_started_ms"] = $startupWatch.ElapsedMilliseconds
            }
            $liveSync = @(Get-ChildItem -LiteralPath $taskRunRoot -Filter 'sync-*.pid' | ForEach-Object {
                Get-Process -Id ([int][IO.File]::ReadAllText($_.FullName)) -ErrorAction SilentlyContinue
            })
            if ($liveSync.Count -ne 20) { throw 'Did not observe twenty simultaneous synchronous processes' }
            if (@($liveSync.Id | Select-Object -Unique).Count -ne 20) { throw 'Synchronous process markers reused a PID' }
            $allowedProcessRoot = [IO.Path]::GetFullPath((Join-Path $WorkRoot 'task-temp')) + [IO.Path]::DirectorySeparatorChar
            foreach ($process in $liveSync) {
                if (!$process.Path -or !$process.Path.StartsWith($allowedProcessRoot, [StringComparison]::OrdinalIgnoreCase) -or $process.HasExited) {
                    throw 'Synchronous process is no longer running inside the owned fixture directory'
                }
            }
            $taskReport.metrics.sync_active_processes = $liveSync.Count
            $taskReport.metrics.sync_start_ms = $startupWatch.ElapsedMilliseconds
            $taskReport.metrics.sync_pid_markers = @(Get-ChildItem -LiteralPath $taskRunRoot -Filter 'sync-*.pid' | ForEach-Object { @{ name = $_.Name; observed_at = $_.LastWriteTimeUtc.ToString('o') } })
            [IO.File]::WriteAllText($syncRelease, 'release')
            $taskReport.metrics.sync_release_at = (Get-Date).ToString('o')
            if (![Threading.Tasks.Task]::WaitAll([Threading.Tasks.Task[]]$pending.ToArray(), 30000)) { throw 'Synchronous responses timed out' }
            for ($index = 0; $index -lt $pending.Count; $index++) {
                $response = $pending[$index].Result
                $syncResponses.Add($response)
                $reply = $response.Content.ReadAsStringAsync().Result | ConvertFrom-Json
                Assert-TaskSuccess $reply 'barrier-released'
                if ($reply.request_id -ne "task-sync-$index") { throw 'Synchronous response correlation changed' }
            }
            $taskReport.metrics.sync_responses_complete_ms = $startupWatch.ElapsedMilliseconds
        } finally {
            # release 留在本轮项目内目录；失败时迟到的进程也可立刻离开屏障，不依赖 HTTP 的取消时序。
            [IO.File]::WriteAllText($syncRelease, 'release')
            foreach ($response in $syncResponses) { $response.Dispose() }
            foreach ($content in $syncContents) { $content.Dispose() }
            $client.Dispose()
        }
        Assert-TaskSuccess (Run-TaskFixture $echo) 'arg:中文 参数'
        $checks.Add('twenty simultaneous synchronous processes bypass ordinary Kik permits and all responses remain associated')

        # 关闭真实控制端进程而非仅关闭一次 HTTP 请求，验证异步程序不依赖控制会话寿命。
        # secondary 是本轮 E2E 创建的实例，原链路断言此时已完成；所有权由上层清理列表保持。
        $detachedMarker = Join-Path $taskRunRoot 'detached.pid'
        $detached = New-TaskFixture -Suffix detached -Arguments @('sleep','1500',$detachedMarker) -Mode async -ResponseMode default
        $reply = Invoke-ApiCommand -Command @{kind='run_task'; task_name=$detached} -RequestId 'task-detached' -Port $SecondaryPort
        Assert-TaskSuccess $reply 'fixture-default-response'
        if ($SecondaryController.Proc.HasExited) { throw 'Secondary controller exited before disconnect test' }
        $SecondaryController.Proc.Kill($true)
        if (!$SecondaryController.Proc.WaitForExit(5000)) { throw 'Secondary controller did not stop' }
        $deadline = [DateTime]::UtcNow.AddSeconds(8)
        while (!(Test-Path -LiteralPath "$detachedMarker.done")) {
            if ([DateTime]::UtcNow -gt $deadline) { throw 'Async task did not complete after its control session closed' }
            Start-Sleep -Milliseconds 50
        }
        Assert-TaskSuccess (Run-TaskFixture $echo) 'arg:中文 参数'
        $checks.Add('async program completes after its real control process exits and another controller remains usable')

        # 同一 API 契约通过本地管道调用，命令名与 HTTP 保持一致。
        $json = @{version=1;request_id='task-pipe';command=@{kind='run_task';task_name=$echo}} | ConvertTo-Json -Depth 5 -Compress
        $pipe = Invoke-PipePayload -Payload ([Text.Encoding]::UTF8.GetBytes("RTCAPI1`0$json"))
        Assert-TaskSuccess $pipe 'arg:中文 参数'
        $checks.Add('named pipe and HTTP share the run_task contract')
        Invoke-TaskBindingNetworkProbe -TaskName $echo
        $taskReport.success = $true
        return $checks.ToArray()
    } catch {
        $originalFailure = $_
        $taskReport.error = $_.Exception.Message
        throw
    } finally {
        $cleanupFailures = [Collections.Generic.List[string]]::new()
        # 只结束 fixture 明确写入本轮私有目录的 PID，且校验进程 EXE 位于本轮 Kik 临时目录。
        foreach ($marker in @(Get-ChildItem -LiteralPath $taskRunRoot -Filter '*.pid' -ErrorAction SilentlyContinue)) {
            $fixtureProcessId = 0
            $process = $null
            try {
                if ([int]::TryParse([IO.File]::ReadAllText($marker.FullName), [ref]$fixtureProcessId)) {
                    $process = Get-Process -Id $fixtureProcessId -ErrorAction SilentlyContinue
                    if ($process -and $process.Path -and $process.Path.StartsWith((Join-Path $WorkRoot 'task-temp') + [IO.Path]::DirectorySeparatorChar, [StringComparison]::OrdinalIgnoreCase)) {
                        if (!$process.HasExited) { $process.Kill($true) }
                        if (!$process.WaitForExit(5000)) { throw 'Owned task fixture did not exit during cleanup' }
                    }
                }
            } catch {
                # Get-Process 与 Kill 之间自然退出是正常竞态；其他清理错误不能阻止剩余资源回收。
                if (!$process -or !$process.HasExited) { $cleanupFailures.Add($_.Exception.Message) }
            }
        }
        foreach ($directory in $created) {
            try {
                $resolved = [IO.Path]::GetFullPath($directory)
                $allowed = [IO.Path]::GetFullPath($taskCatalogRoot) + [IO.Path]::DirectorySeparatorChar
                if (!$resolved.StartsWith($allowed, [StringComparison]::OrdinalIgnoreCase) -or !(Split-Path $resolved -Leaf).StartsWith($taskPrefix)) {
                    throw "Unsafe fixture cleanup path: $resolved"
                }
                if (Test-Path -LiteralPath $resolved) { Remove-Item -LiteralPath $resolved -Recurse -Force }
            } catch { $cleanupFailures.Add($_.Exception.Message) }
        }
        $taskReport.checks = $checks.ToArray()
        $taskReport.completed_at = (Get-Date).ToString('o')
        $taskReport.cleanup_errors = $cleanupFailures.ToArray()
        if ($cleanupFailures.Count) { $taskReport.success = $false }
        # 独立报告在失败时也保留已经完成的任务断言；不让总 E2E 的提前抛错丢失这些证据。
        $taskReportDir = Join-Path $RepositoryRoot 'reports/e2e'
        [IO.Directory]::CreateDirectory($taskReportDir) | Out-Null
        $taskReport | ConvertTo-Json -Depth 8 | Set-Content -LiteralPath (Join-Path $taskReportDir 'task_execution_report.json') -Encoding UTF8
        if ($cleanupFailures.Count) {
            $cleanupMessage = 'Task fixture cleanup incomplete: ' + ($cleanupFailures -join '; ')
            if ($originalFailure) {
                Write-Warning $cleanupMessage
            } else {
                throw $cleanupMessage
            }
        }
    }
}
