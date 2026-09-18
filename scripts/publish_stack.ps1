param(
    [ValidateSet("Gray", "Production")]
    [string]$Channel = "Gray",
    [switch]$BuildOnly,
    [switch]$DeployOnly,
    [switch]$SkipToolchainInstall,
    [switch]$SkipPublicTests,
    [switch]$SkipCtrlServer,
    [switch]$SkipCtrlKik,
    [switch]$SkipRealCtrl,
    [switch]$PlanOnly
)

$ErrorActionPreference = "Stop"
$ScriptDir = Split-Path -Parent $MyInvocation.MyCommand.Path
$Root = (Resolve-Path (Join-Path $ScriptDir "..")).Path
$TargetTriple = "x86_64-unknown-linux-musl"
# 必须使用完整版本号。cross 会据此寻找 Linux 主机工具链；1.90 与 1.90.0
# 在 rustup 中是不同的安装名，短别名会触发旧版 cross 的非本机工具链自动安装缺陷。
$BuildToolchain = "1.90.0"
$CrossConfig = Join-Path $Root "Cross.toml"
$ServerHost = "ytycc.com"
$TargetAlias = "ytycc"
$ChannelName = $Channel.ToLowerInvariant()
$KikNoisePort = if ($Channel -eq "Gray") { 9005 } else { 9002 }
$ControlTlsPort = if ($Channel -eq "Gray") { 9009 } else { 9007 }
if ($KikNoisePort -eq $ControlTlsPort) {
    throw "Kik Noise 与 real_ctrl TLS 端口必须分离"
}
$RemoteDir = if ($Channel -eq "Gray") {
    "/home/deploy/rust/gray"
} else {
    "/home/deploy/rust/ctrl_server"
}
$ServiceName = "real-time-ctrl-$ChannelName"
$DeployDir = Join-Path $Root "deploy\$ChannelName"
$IdentityDir = Join-Path $DeployDir "identity"
$ArtifactDir = Join-Path $DeployDir "artifacts"
$ReportDir = Join-Path $DeployDir "reports"
$ServerArtifact = Join-Path $Root "target\$TargetTriple\production\ctrl_server"
$KikArtifact = Join-Path $Root "target\ctrl-kik-protected\hardened\ctrl_kik.exe"
$KikReceipt = Join-Path $Root "target\ctrl-kik-protected\build-receipt.json"
. (Join-Path $ScriptDir 'publish_components.ps1')
$Selection = Get-PublishPlan -BuildOnly:$BuildOnly -DeployOnly:$DeployOnly -SkipCtrlServer:$SkipCtrlServer -SkipCtrlKik:$SkipCtrlKik -SkipRealCtrl:$SkipRealCtrl -SkipPublicTests:$SkipPublicTests
if ($PlanOnly) { [ordered]@{channel=$ChannelName;plan=$Selection;appIncluded=$false} | ConvertTo-Json -Depth 5; return }

function Assert-LastExitCode {
    param([string]$Message)
    if ($LASTEXITCODE -ne 0) {
        throw "$Message (exit=$LASTEXITCODE)"
    }
}

function Assert-CommandAvailable {
    param([string]$Name)
    if (-not (Get-Command $Name -ErrorAction SilentlyContinue)) {
        throw "缺少必需命令: $Name"
    }
}

function Assert-EmbeddedValuesEncrypted {
    param(
        [string]$Path,
        [Collections.IDictionary]$Needles
    )
    $bytes = [IO.File]::ReadAllBytes($Path)
    $binaryText = [Text.Encoding]::GetEncoding(28591).GetString($bytes)
    foreach ($entry in $Needles.GetEnumerator()) {
        $value = [string]$entry.Value
        if (-not [string]::IsNullOrEmpty($value) -and $binaryText.Contains($value)) {
            throw "$($entry.Key) 在产物中仍为可搜索明文: $Path"
        }
    }
}

function Assert-DockerReady {
    try {
        & docker info --format '{{.ServerVersion}}' 2>$null | Out-Null
    } catch {
        throw "Docker 未启动。请启动 Docker Desktop，确认 docker info 成功后重新执行本脚本。"
    }
    if ($LASTEXITCODE -ne 0) {
        throw "Docker 未启动。请启动 Docker Desktop，确认 docker info 成功后重新执行本脚本。"
    }
}

function Assert-PublishToolchains {
    # cross 在 Windows 上使用 Docker 内的 Linux rustc，除了本机编译器，还需要
    # Linux 工具链及 musl 标准库。先核对现有安装，避免构建 Kik 后才暴露环境问题。
    $installed = (& rustup toolchain list) -join "`n"
    Assert-LastExitCode "读取 Rust 工具链失败"
    $required = @()
    if ('real_ctrl' -in $Selection.build -or 'ctrl_server' -in $Selection.build) { $required += "$BuildToolchain-x86_64-pc-windows-msvc" }
    if ('ctrl_server' -in $Selection.build) { $required += "$BuildToolchain-x86_64-unknown-linux-gnu" }
    foreach ($toolchain in $required) {
        if ($installed -notmatch "(?m)^$([regex]::Escape($toolchain))(\s|$)") {
            throw "缺少发布工具链 $toolchain。先按发布文档准备工具链；脚本不会改变全局默认版本或自动下载另一套别名。"
        }
    }
    if ('ctrl_server' -notin $Selection.build) { return }
    $targets = @(& rustup target list --installed --toolchain "$BuildToolchain-x86_64-unknown-linux-gnu")
    Assert-LastExitCode "读取 Linux Rust 标准库目标失败"
    if ($TargetTriple -notin $targets) {
        throw "Linux 工具链缺少 $TargetTriple 标准库，无法执行 cross 发布构建。"
    }
    if (-not (Test-Path -LiteralPath $CrossConfig -PathType Leaf)) {
        throw "缺少发布 Cross.toml；不能在丢失服务端构建身份变量的情况下继续。"
    }
}

function Write-LfText {
    param([string]$Path, [string]$Text)
    # PowerShell here-string 会保留脚本检出时的 CRLF。上传前统一为 LF，
    # 否则 Linux bash 会把 pipefail 后面的回车当成选项内容，部署阶段直接失败。
    $normalized = $Text.Replace("`r`n", "`n").Replace("`r", "`n")
    [IO.File]::WriteAllText($Path, $normalized, [Text.UTF8Encoding]::new($false))
}

function Set-PrivateAcl {
    param(
        [Parameter(Mandatory = $true)]
        [string]$Path,
        [switch]$Container
    )

    # 部署密钥与环境文件均只允许当前发布用户读取。根 .gitignore 只能避免误提交，
    # 不能替代操作系统访问控制，因此 ACL 收紧失败必须中止发布。
    $permission = if ($Container) { "${env:USERNAME}:(OI)(CI)F" } else { "${env:USERNAME}:F" }
    & icacls $Path /inheritance:r /grant:r $permission 2>$null | Out-Null
    Assert-LastExitCode "收紧本机敏感文件 ACL 失败: $Path"
}

function New-RandomBase64Url {
    param([int]$Length)
    $bytes = New-Object byte[] $Length
    $rng = [Security.Cryptography.RandomNumberGenerator]::Create()
    try {
        $rng.GetBytes($bytes)
    } finally {
        $rng.Dispose()
    }
    [Convert]::ToBase64String($bytes).TrimEnd('=').Replace('+', '-').Replace('/', '_')
}

function Get-DerTailBase64 {
    param([string]$Path)
    $bytes = [IO.File]::ReadAllBytes($Path)
    if ($bytes.Count -lt 32) {
        throw "X25519 DER 文件长度异常: $Path"
    }
    [Convert]::ToBase64String([byte[]]$bytes[($bytes.Count - 32)..($bytes.Count - 1)])
}

function Initialize-DeploymentIdentity {
    [IO.Directory]::CreateDirectory($IdentityDir) | Out-Null
    $certPath = Join-Path $IdentityDir "server.crt"
    $keyPath = Join-Path $IdentityDir "server.key"
    $noisePrivatePem = Join-Path $IdentityDir "kik-noise-private.pem"
    $noisePrivateDer = Join-Path $IdentityDir "kik-noise-private.der"
    $noisePublicDer = Join-Path $IdentityDir "kik-noise-public.der"
    $controlSecretPath = Join-Path $IdentityDir "control-auth.secret"
    $apiTokenPath = Join-Path $IdentityDir "local-api-token.secret"
    $pinPath = Join-Path $IdentityDir "server-spki-sha256.txt"
    $opensslConfig = Join-Path $IdentityDir "openssl.cnf"

    if (-not (Test-Path -LiteralPath $certPath) -or -not (Test-Path -LiteralPath $keyPath)) {
        $configText = @"
[req]
default_bits = 3072
prompt = no
default_md = sha256
distinguished_name = dn
x509_extensions = v3_req

[dn]
CN = $ServerHost

[v3_req]
subjectAltName = @alt_names
keyUsage = critical,digitalSignature,keyEncipherment
extendedKeyUsage = serverAuth
basicConstraints = critical,CA:false

[alt_names]
DNS.1 = $ServerHost
"@
        [IO.File]::WriteAllText($opensslConfig, $configText, [Text.UTF8Encoding]::new($false))
        & openssl req -x509 -newkey rsa:3072 -sha256 -nodes -days 825 `
            -keyout $keyPath -out $certPath -config $opensslConfig -extensions v3_req 2>$null
        Assert-LastExitCode "生成 TLS 身份失败"
    }

    if (-not (Test-Path -LiteralPath $noisePrivatePem)) {
        & openssl genpkey -algorithm X25519 -out $noisePrivatePem 2>$null
        Assert-LastExitCode "生成 Kik Noise 私钥失败"
    }
    & openssl pkey -in $noisePrivatePem -outform DER -out $noisePrivateDer 2>$null
    Assert-LastExitCode "导出 Kik Noise 私钥失败"
    & openssl pkey -in $noisePrivatePem -pubout -outform DER -out $noisePublicDer 2>$null
    Assert-LastExitCode "导出 Kik Noise 公钥失败"

    if (-not (Test-Path -LiteralPath $controlSecretPath)) {
        [IO.File]::WriteAllText($controlSecretPath, (New-RandomBase64Url 48), [Text.UTF8Encoding]::new($false))
    }
    if (-not (Test-Path -LiteralPath $apiTokenPath)) {
        [IO.File]::WriteAllText($apiTokenPath, (New-RandomBase64Url 32), [Text.UTF8Encoding]::new($false))
    }

    $pubKeyPem = Join-Path $IdentityDir "server-pubkey.pem"
    $spkiDer = Join-Path $IdentityDir "server-spki.der"
    & openssl x509 -in $certPath -pubkey -noout -out $pubKeyPem
    Assert-LastExitCode "导出 TLS 公钥失败"
    & openssl pkey -pubin -in $pubKeyPem -outform DER -out $spkiDer
    Assert-LastExitCode "导出 TLS SPKI 失败"
    $hashOutput = & openssl dgst -sha256 $spkiDer
    Assert-LastExitCode "计算 TLS SPKI pin 失败"
    $pin = ($hashOutput -replace '^.*=\s*', '').Trim().ToLowerInvariant()
    [IO.File]::WriteAllText($pinPath, $pin, [Text.UTF8Encoding]::new($false))

    Set-PrivateAcl -Path $IdentityDir -Container

    $identity = [pscustomobject]@{
        Cert = $certPath
        Key = $keyPath
        NoisePrivatePem = $noisePrivatePem
        NoisePrivate = Get-DerTailBase64 $noisePrivateDer
        NoisePublic = Get-DerTailBase64 $noisePublicDer
        ControlSecret = (Get-Content -LiteralPath $controlSecretPath -Raw -Encoding UTF8).Trim()
        ApiToken = (Get-Content -LiteralPath $apiTokenPath -Raw -Encoding UTF8).Trim()
        Pin = $pin
    }
    # DER/SPKI 与 OpenSSL 配置只参与本轮派生；保留会扩大私钥副本数量并污染身份目录。
    foreach ($intermediate in @($noisePrivateDer, $noisePublicDer, $pubKeyPem, $spkiDer, $opensslConfig)) {
        [IO.File]::Delete($intermediate)
    }
    $identity
}

function Write-DeploymentFiles {
    param($Identity)
    $remoteIdentityDir = "$RemoteDir/identity"
    $serverEnv = @(
        "CTRL_SERVER_BIND_HOST=0.0.0.0"
        "CTRL_SERVER_PORT=$KikNoisePort"
        "CTRL_SERVER_TLS_PORT=$ControlTlsPort"
        "CTRL_SERVER_TLS_CERT=$remoteIdentityDir/server.crt"
        "CTRL_SERVER_TLS_KEY=$remoteIdentityDir/server.key"
        "CTRL_SERVER_KIK_NOISE_PRIVATE_KEY=$($Identity.NoisePrivate)"
        "CTRL_SERVER_AUTH_SECRET=$($Identity.ControlSecret)"
        "CTRL_SERVER_ALLOW_EXEC=1"
        "RUST_LOG=info"
        "RUST_BACKTRACE=1"
    ) -join "`n"
    $serverEnvPath = Join-Path $DeployDir "server.env"
    [IO.File]::WriteAllText($serverEnvPath, "$serverEnv`n", [Text.UTF8Encoding]::new($false))
    Set-PrivateAcl -Path $serverEnvPath

    $localEnv = [ordered]@{
        REAL_CTRL_SERVER_HOST = $ServerHost
        REAL_CTRL_TLS_PORT = "$ControlTlsPort"
        REAL_CTRL_TLS_SERVER_NAME = $ServerHost
        REAL_CTRL_TLS_CA_CERT = $Identity.Cert
        REAL_CTRL_TLS_SERVER_SPKI_SHA256 = $Identity.Pin
        REAL_CTRL_AUTH_SECRET = $Identity.ControlSecret
        REAL_CTRL_API_TOKEN = $Identity.ApiToken
        REAL_CTRL_API_ALLOW_EXEC = "1"
        # 锁文件是进程存活期状态，不属于发布身份或 artifact。放入系统临时目录，避免污染
        # 可长期保存的 deploy 目录；通道名保证灰度与正式实例不会互相抢锁。
        REAL_CTRL_HTTP_LOCK_PATH = (Join-Path ([IO.Path]::GetTempPath()) "real_ctrl-http-$ChannelName.lock")
        RUST_BACKTRACE = "1"
    }
    $localEnvPath = Join-Path $DeployDir "real_ctrl.env.json"
    [IO.File]::WriteAllText(
        $localEnvPath,
        ($localEnv | ConvertTo-Json),
        [Text.UTF8Encoding]::new($false)
    )
    Set-PrivateAcl -Path $localEnvPath

    $unitText = @"
[Unit]
Description=Real Time Ctrl Server ($ChannelName)
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
User=@@SERVICE_USER@@
Group=@@SERVICE_GROUP@@
WorkingDirectory=$RemoteDir
EnvironmentFile=$RemoteDir/server.env
ExecStart=$RemoteDir/ctrl_server
Restart=always
RestartSec=2
TimeoutStopSec=20
KillSignal=SIGTERM
LimitNOFILE=65536
TasksMax=1024
UMask=0077
NoNewPrivileges=true
PrivateTmp=true
PrivateDevices=true
ProtectSystem=full
# 程序与滚动日志按部署约束位于 /home/deploy；服务已经以非 root 用户运行，
# 再启用 ProtectHome 会在部分旧 systemd 上覆盖 ReadWritePaths，导致日志初始化失败。
ProtectHome=false
ReadWriteDirectories=$RemoteDir
CapabilityBoundingSet=

[Install]
WantedBy=multi-user.target
"@
    [IO.File]::WriteAllText((Join-Path $DeployDir "ctrl_server.service.in"), $unitText, [Text.UTF8Encoding]::new($false))
}

function Build-Stack {
    param($Identity)
    if ('ctrl_kik' -in $Selection.build) {
        $previousKikLock = $env:RTC_CTRL_KIK_BUILD_LOCK_PATH
        try {
            # 发布产物使用工作目录中的锁文件，不能沿用开发机配置里的固定绝对路径。
            $env:RTC_CTRL_KIK_BUILD_LOCK_PATH = "ctrl_kik.lock"
            & (Join-Path $ScriptDir "build_ctrl_kik_protected.ps1") `
                -SkipToolchainInstall:$SkipToolchainInstall `
                -ServerHost $ServerHost `
                -ServerPort $KikNoisePort `
                -BuildChannel $ChannelName `
                -NoiseServerPublicKey $Identity.NoisePublic | Out-Null
            Assert-LastExitCode "ctrl_kik 受保护构建失败"
        } finally {
            $env:RTC_CTRL_KIK_BUILD_LOCK_PATH = $previousKikLock
        }
        $receipt = Get-Content -LiteralPath $KikReceipt -Raw -Encoding UTF8 | ConvertFrom-Json
        if (-not $receipt.production_ready) { throw "ctrl_kik 二进制审计未达到 production_ready" }
    }

    $buildDefaults = [ordered]@{
        RTC_REAL_CTRL_BUILD_SERVER_HOST = $ServerHost
        RTC_REAL_CTRL_BUILD_TLS_PORT = "$ControlTlsPort"
        RTC_REAL_CTRL_BUILD_TLS_SERVER_NAME = $ServerHost
        RTC_REAL_CTRL_BUILD_TLS_CA_PEM_BASE64 = [Convert]::ToBase64String(
            [IO.File]::ReadAllBytes($Identity.Cert)
        )
        RTC_REAL_CTRL_BUILD_TLS_SPKI_SHA256 = $Identity.Pin
        RTC_REAL_CTRL_BUILD_HTTP_LOCK_PATH = "real_ctrl-http-$ChannelName.lock"
        RTC_REAL_CTRL_BUILD_HTTP_BINDING = "127.0.0.1"
        RTC_REAL_CTRL_BUILD_HTTP_PORT = "9000"
        RTC_REAL_CTRL_BUILD_ACCOUNT_ID = "default"
        RTC_REAL_CTRL_BUILD_INSTANCE_ID = ""
        RTC_REAL_CTRL_BUILD_AUTH_SECRET = $Identity.ControlSecret
        RTC_REAL_CTRL_BUILD_API_TOKEN = $Identity.ApiToken
        RTC_REAL_CTRL_BUILD_API_ALLOW_EXEC = "1"
        RTC_CTRL_SERVER_BUILD_BIND_HOST = "0.0.0.0"
        RTC_CTRL_SERVER_BUILD_KIK_PORT = "$KikNoisePort"
        RTC_CTRL_SERVER_BUILD_TLS_PORT = "$ControlTlsPort"
        RTC_CTRL_SERVER_BUILD_TLS_CERT_PEM_BASE64 = [Convert]::ToBase64String(
            [IO.File]::ReadAllBytes($Identity.Cert)
        )
        RTC_CTRL_SERVER_BUILD_TLS_KEY_PEM_BASE64 = [Convert]::ToBase64String(
            [IO.File]::ReadAllBytes($Identity.Key)
        )
        RTC_CTRL_SERVER_BUILD_AUTH_SECRET = $Identity.ControlSecret
        RTC_CTRL_SERVER_BUILD_ACCOUNTS_JSON_BASE64 = ""
        RTC_CTRL_SERVER_BUILD_KIK_NOISE_PRIVATE_KEY = $Identity.NoisePrivate
        RTC_CTRL_SERVER_BUILD_ALLOW_EXEC = "1"
    }
    $previousBuildDefaults = @{}
    foreach ($entry in $buildDefaults.GetEnumerator()) {
        $previousBuildDefaults[$entry.Key] = [Environment]::GetEnvironmentVariable(
            $entry.Key,
            [EnvironmentVariableTarget]::Process
        )
        [Environment]::SetEnvironmentVariable(
            $entry.Key,
            [string]$entry.Value,
            [EnvironmentVariableTarget]::Process
        )
    }
    $previousCrossConfig = $env:CROSS_CONFIG
    $previousCrossDebug = $env:CROSS_DEBUG
    try {
        # 固定配置只透传变量名；实际值仍来自当前进程，不写进 Cross.toml。
        # 构建包含私钥输入，禁止启用可能展开容器命令的 cross 调试输出。
        $env:CROSS_CONFIG = $CrossConfig
        $env:CROSS_DEBUG = $null
        if ('real_ctrl' -in $Selection.build) {
            & cargo "+$BuildToolchain" build --locked -p real_ctrl --bins --profile production
            Assert-LastExitCode "real_ctrl 生产构建失败"
        }
        if ('ctrl_server' -in $Selection.build) {
            & cross "+$BuildToolchain" build --locked -p ctrl_server --profile production --target $TargetTriple
            Assert-LastExitCode "ctrl_server 交叉编译失败"
        }
    } finally {
        $env:CROSS_CONFIG = $previousCrossConfig
        $env:CROSS_DEBUG = $previousCrossDebug
        foreach ($entry in $previousBuildDefaults.GetEnumerator()) {
            [Environment]::SetEnvironmentVariable(
                $entry.Key,
                $entry.Value,
                [EnvironmentVariableTarget]::Process
            )
        }
    }
    if ('ctrl_server' -in $Selection.build -and -not (Test-Path -LiteralPath $ServerArtifact)) {
        throw "缺少 ctrl_server 交叉编译产物: $ServerArtifact"
    }
    if ('real_ctrl' -in $Selection.build) {
        Get-ChildItem -LiteralPath (Join-Path $Root "target\production") -Filter "real_ctrl*.exe" -File |
            ForEach-Object {
                Assert-EmbeddedValuesEncrypted -Path $_.FullName -Needles ([ordered]@{
                    real_ctrl_auth_secret = $Identity.ControlSecret
                    real_ctrl_api_token = $Identity.ApiToken
                    real_ctrl_embedded_ca = [Convert]::ToBase64String([IO.File]::ReadAllBytes($Identity.Cert))
                })
            }
    }
    if ('ctrl_server' -in $Selection.build) {
        Assert-EmbeddedValuesEncrypted -Path $ServerArtifact -Needles ([ordered]@{
            ctrl_server_auth_secret = $Identity.ControlSecret
            ctrl_server_noise_private_key = $Identity.NoisePrivate
            ctrl_server_tls_private_key = [Convert]::ToBase64String([IO.File]::ReadAllBytes($Identity.Key))
        })
    }
    $sources = [ordered]@{}
    if ('ctrl_server' -in $Selection.build) { $sources['ctrl_server'] = $ServerArtifact }
    if ('ctrl_kik' -in $Selection.build) {
        $sources['ctrl_kik.exe'] = $KikArtifact
        $sources['ctrl_kik.build-receipt.json'] = $KikReceipt
    }
    if ('real_ctrl' -in $Selection.build) {
        foreach ($binary in @('real_ctrl','real_ctrl_local_server','real_ctrl_invoker_http_service')) {
            foreach ($extension in @('exe','pdb')) { $name="$binary.$extension"; $sources[$name] = Join-Path $Root "target/production/$name" }
        }
    }
    Save-SelectedArtifacts -Directory $ArtifactDir -ManifestPath (Join-Path $DeployDir 'manifest.json') -Channel $ChannelName -BackupDirectory (Join-Path $DeployDir "rollback-local/$RunId") -Sources $sources -Retained $RetainedArtifacts
}

function Publish-Server {
    $serverHash = (Get-FileHash -Algorithm SHA256 -LiteralPath $ServerArtifact).Hash.ToLowerInvariant()
    $candidateName = "ctrl_server.candidate.$($serverHash.Substring(0, 16))"
    $candidatePath = "$RemoteDir/$candidateName"
    $remoteIdentityDir = "$RemoteDir/identity"
    $remoteScriptPath = "$RemoteDir/deploy-$ChannelName.sh"
    $deployScriptPath = Join-Path $DeployDir "deploy-remote.sh"
    $rebootScriptPath = Join-Path $ScriptDir "remote\reboot.sh"
    $rebootUploadPath = Join-Path $DeployDir "reboot.sh"
    $deployScript = @"
#!/usr/bin/env bash
set -euo pipefail
remote_dir='$RemoteDir'
candidate='$candidatePath'
service_name='$ServiceName'
expected_hash='$serverHash'
kik_noise_port='$KikNoisePort'
control_tls_port='$ControlTlsPort'

test -d "`$remote_dir"
test -f "`$candidate"
actual_hash=`$(sha256sum "`$candidate" | awk '{print `$1}')
test "`$actual_hash" = "`$expected_hash"
service_user=`$(stat -c '%U' /home/deploy)
service_group=`$(stat -c '%G' /home/deploy)
test "`$service_user" != 'root'
sudo chown -R "`$service_user:`$service_group" "`$remote_dir"
chmod 0750 "`$remote_dir" "`$remote_dir/identity" "`$remote_dir/logs"
chmod 0755 "`$candidate"
chmod 0600 "`$remote_dir/server.env" "`$remote_dir/identity/server.key"
chmod 0644 "`$remote_dir/identity/server.crt"
sudo -u "`$service_user" test -r "`$remote_dir/identity/server.crt"
sudo -u "`$service_user" test -r "`$remote_dir/identity/server.key"
sudo -u "`$service_user" sh -c "touch '`$remote_dir/logs/.write-test' && rm -f '`$remote_dir/logs/.write-test'"

sed -e "s/@@SERVICE_USER@@/`$service_user/g" -e "s/@@SERVICE_GROUP@@/`$service_group/g" \
    "`$remote_dir/ctrl_server.service.in" > "`$remote_dir/ctrl_server.service"
sudo install -m 0644 "`$remote_dir/ctrl_server.service" "/etc/systemd/system/`$service_name.service"
sudo systemctl daemon-reload

rm -f "`$remote_dir/ctrl_server.rollback"
if test -f "`$remote_dir/ctrl_server"; then
    cp -p "`$remote_dir/ctrl_server" "`$remote_dir/ctrl_server.rollback"
fi
mv -f "`$candidate" "`$remote_dir/ctrl_server"

rollback() {
    if test -f "`$remote_dir/ctrl_server.rollback"; then
        mv -f "`$remote_dir/ctrl_server.rollback" "`$remote_dir/ctrl_server"
        sudo systemctl restart "`$service_name.service" || true
    fi
}
trap rollback ERR
sudo systemctl enable "`$service_name.service" >/dev/null
sudo systemctl restart "`$service_name.service"
for _ in `$(seq 1 30); do
    if sudo systemctl is-active --quiet "`$service_name.service" \
        && ss -ltn | grep -Eq ":`$kik_noise_port([[:space:]]|`$)" \
        && ss -ltn | grep -Eq ":`$control_tls_port([[:space:]]|`$)"; then
        trap - ERR
        exit 0
    fi
    sleep 1
done
sudo systemctl status "`$service_name.service" --no-pager >&2 || true
# 显式 exit 不会触发 ERR trap；false 会由 set -e 结束脚本，并先执行已有回滚。
# 这样进程活着但监听口一直未就绪的超时路径，也会恢复上一版并重启服务。
false
"@
    Write-LfText -Path $deployScriptPath -Text $deployScript
    Write-LfText -Path $rebootUploadPath -Text ([IO.File]::ReadAllText($rebootScriptPath))

    $session = $null
    try {
        $session = (sshtool --quiet --target $TargetAlias init).Trim()
        if ($LASTEXITCODE -ne 0 -or [string]::IsNullOrWhiteSpace($session)) {
            throw "无法初始化远程发布会话"
        }
        sshtool --quiet --session $session probe | Out-Null
        Assert-LastExitCode "远程发布会话不可用"
        sshtool --quiet --session $session exec "mkdir -p '$RemoteDir' '$remoteIdentityDir' '$RemoteDir/logs'"
        Assert-LastExitCode "创建远程发布目录失败"

        sshtool --quiet --session $session upload $ServerArtifact $candidatePath
        Assert-LastExitCode "上传 ctrl_server 失败"
        sshtool --quiet --session $session upload (Join-Path $DeployDir "server.env") "$RemoteDir/server.env"
        Assert-LastExitCode "上传服务端环境文件失败"
        sshtool --quiet --session $session upload (Join-Path $DeployDir "ctrl_server.service.in") "$RemoteDir/ctrl_server.service.in"
        Assert-LastExitCode "上传 systemd unit 模板失败"
        sshtool --quiet --session $session upload (Join-Path $IdentityDir "server.crt") "$remoteIdentityDir/server.crt"
        Assert-LastExitCode "上传 TLS 证书失败"
        sshtool --quiet --session $session upload (Join-Path $IdentityDir "server.key") "$remoteIdentityDir/server.key"
        Assert-LastExitCode "上传 TLS 私钥失败"
        sshtool --quiet --session $session upload $deployScriptPath $remoteScriptPath
        Assert-LastExitCode "上传远程发布脚本失败"
        sshtool --quiet --session $session upload $rebootUploadPath "/home/deploy/rust/reboot.sh"
        Assert-LastExitCode "上传 ctrl_server 双通道重启脚本失败"
        sshtool --quiet --session $session exec "chmod 0750 '/home/deploy/rust/reboot.sh'"
        Assert-LastExitCode "设置远程重启脚本权限失败"
        sshtool --quiet --session $session exec "bash '$remoteScriptPath'"
        Assert-LastExitCode "远程服务启动失败且已尝试回滚"
    } finally {
        if (-not [string]::IsNullOrWhiteSpace($session)) {
            sshtool --quiet --session $session close | Out-Null
        }
    }
}

$RunId = (Get-Date -Format 'yyyyMMdd-HHmmss') + '-' + [Guid]::NewGuid().ToString('N').Substring(0,8)
$runReport = [ordered]@{success=$false;channel=$ChannelName;plan=$Selection;app_included=$false;deployed=$false;public_tests='not-run';run_id=$RunId}
$previousEnvironment = @{}
$previousLocation = Get-Location
$buildLock = $null
[IO.Directory]::CreateDirectory($ReportDir) | Out-Null
try {
    Set-Location $Root
    $localPaths = @{TEMP=(Join-Path $Root 'target/tmp');TMP=(Join-Path $Root 'target/tmp');TMPDIR=(Join-Path $Root 'target/tmp');CARGO_TARGET_DIR=(Join-Path $Root 'target');XARGO_HOME=(Join-Path $Root 'target/xargo')}
    foreach ($entry in $localPaths.GetEnumerator()) {
        $previousEnvironment[$entry.Key] = [Environment]::GetEnvironmentVariable($entry.Key,'Process')
        [IO.Directory]::CreateDirectory($entry.Value) | Out-Null
        [Environment]::SetEnvironmentVariable($entry.Key,$entry.Value,'Process')
    }
    # 两个通道共享 Cargo 输出，必须互斥，防止并发构建互相拿到另一通道的二进制。
    $buildLock = [IO.File]::Open((Join-Path $Root 'target/publish-stack.lock'),'OpenOrCreate','ReadWrite','None')
    $commands = @('openssl','icacls')
    if (!$DeployOnly) { $commands += @('cargo','cargo-audit','rustup') }
    if ('ctrl_server' -in $Selection.build) { $commands += @('cross','docker') }
    if ($Selection.deployServer) { $commands += 'sshtool' }
    foreach ($command in $commands) { Assert-CommandAvailable $command }
    if ('ctrl_server' -in $Selection.build) { Assert-DockerReady }
    if (!$DeployOnly) { Assert-PublishToolchains }

    $RetainedArtifacts = @(Read-VerifiedArtifacts -Directory $ArtifactDir -ManifestPath (Join-Path $DeployDir 'manifest.json') -Channel $ChannelName -Components $Selection.skipped)
    if ($DeployOnly -and 'ctrl_server' -notin $RetainedArtifacts.name) { throw 'DeployOnly 必须有清单校验通过的 ctrl_server 归档，不能使用 target 中的临时产物。' }
    if ($Selection.publicTests) {
        foreach ($requirement in @(@('ctrl_kik','ctrl_kik.exe'),@('ctrl_kik','ctrl_kik.build-receipt.json'),@('real_ctrl','real_ctrl.exe'),@('real_ctrl','real_ctrl_invoker_http_service.exe'))) {
            if ($requirement[0] -notin $Selection.build -and $requirement[1] -notin $RetainedArtifacts.name) { throw "公网联调缺少已校验的 $($requirement[1])；请包含该组件，或显式使用 -SkipPublicTests。" }
        }
    }
    if ($RetainedArtifacts.Count -gt 0 -or $DeployOnly) {
        foreach ($name in @('server.crt','server.key','kik-noise-private.pem','control-auth.secret','local-api-token.secret','server-spki-sha256.txt')) {
            if (!(Test-Path -LiteralPath (Join-Path $IdentityDir $name))) { throw '旧产物对应的部署身份不完整，拒绝重新生成身份后复用旧产物。' }
        }
    }
    if (!$DeployOnly) {
        & (Join-Path $ScriptDir 'audit_production_dependencies.ps1')
        Assert-LastExitCode '生产依赖 RustSec 审计失败'
    }
    $identity = Initialize-DeploymentIdentity
    Write-DeploymentFiles $identity
    if (!$DeployOnly) { Build-Stack $identity }
    $ServerArtifact = Join-Path $ArtifactDir 'ctrl_server'

    if ($Selection.deployServer) {
        # SSH 工具继续读取自己的既有连接配置；仅会话状态改到本项目受 ACL 保护目录。
        $previousEnvironment['RTC_PUBLISH_SSH_STATE'] = [Environment]::GetEnvironmentVariable('RTC_PUBLISH_SSH_STATE','Process')
        $env:RTC_PUBLISH_SSH_STATE = Join-Path $DeployDir 'private/sshtool'
        [IO.Directory]::CreateDirectory($env:RTC_PUBLISH_SSH_STATE) | Out-Null
        Set-PrivateAcl -Path $env:RTC_PUBLISH_SSH_STATE -Container
        function sshtool {
            # 公网验收是子脚本，有自己的 script: 作用域；用进程环境传递状态目录，
            # 让发布与验收使用同一个项目内目录，并在最外层 finally 恢复调用者环境。
            $native = (Get-Command sshtool.exe -ErrorAction Stop).Source
            & $native --state-dir $env:RTC_PUBLISH_SSH_STATE @args
            $global:LASTEXITCODE = $LASTEXITCODE
        }
        # 远端替换中断时结果可能未知，不能在失败报告中错误断言“未部署”。
        $runReport.deployed = $null
        Publish-Server
        $runReport.deployed = $true
        if ($Selection.publicTests) {
            $runReport.public_tests = 'running'
            & (Join-Path $ScriptDir 'test_public_stack.ps1') -Channel $Channel
            Assert-LastExitCode '公网端到端验收失败'
            $runReport.public_tests = 'passed'
        } else { $runReport.public_tests = 'explicitly-skipped' }
    }
    $runReport.success = $true
} catch {
    $runReport.error = $_.Exception.Message
    throw
} finally {
    $runReport.completed_at = [DateTimeOffset]::UtcNow.ToString('o')
    [IO.File]::WriteAllText((Join-Path $ReportDir "publish-$RunId.json"),($runReport | ConvertTo-Json -Depth 8),[Text.UTF8Encoding]::new($false))
    if ($buildLock) { $buildLock.Dispose() }
    foreach ($entry in $previousEnvironment.GetEnumerator()) { [Environment]::SetEnvironmentVariable($entry.Key,$entry.Value,'Process') }
    Set-Location $previousLocation
}
$runReport | ConvertTo-Json -Depth 8
