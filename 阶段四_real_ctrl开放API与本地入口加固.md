# 阶段四：real_ctrl 开放 API 与本地入口加固

## 改造范围

本阶段不实现安卓控制端，不修改 `screen_stream`。

已完成内容：

- 新增 `real_ctrl/src/api_contract.rs`，定义稳定开放 API 契约。
- `RealCtrlApi` 成为 HTTP 和命名管道的统一服务入口。
- HTTP 不再绕本地管道调用，改为通过 spring-web `Component<RealCtrlApi>` 直接执行。
- 命名管道保留旧 postcard `InputCommand` 兼容协议，同时新增 magic + JSON 的 v1 API 协议。
- 本地 HTTP 默认绑定 `127.0.0.1`，CORS 不再使用 `*`。
- 开放 API 默认禁用 `Exec`，必须显式设置 `REAL_CTRL_API_ALLOW_EXEC=1`。
- HTTP 可选启用 `REAL_CTRL_API_TOKEN`，启用后要求 `Authorization: Bearer <token>` 或 `X-Real-Ctrl-Token`。
- 命名管道创建时设置 SDDL DACL：仅允许 System、Administrators、对象 Owner 访问。
- 新增 `hardened` Cargo profile，用于生产发布硬化。

## HTTP API

配置文件：`config/app.toml`

默认监听：

```toml
[web]
binding = "127.0.0.1"
port = 9000
global_prefix = "/api"
```

### 健康检查

```http
GET http://127.0.0.1:9000/api/health
```

### 执行命令

```http
POST http://127.0.0.1:9000/api/v1/commands
Content-Type: application/json
```

示例：

```json
{
  "version": 1,
  "request_id": "req-001",
  "command": {
    "kind": "sys_now"
  }
}
```

响应：

```json
{
  "version": 1,
  "request_id": "req-001",
  "ok": true,
  "data": {
    "kind": "sys_now",
    "value": "..."
  }
}
```

当前命令类型：

- `sys_list`
- `sys_now`
- `sys_use`：`kik_id`
- `ctrl_ls`：`path`
- `ctrl_screen`：`save_path` 可选，HTTP JSON 响应中二进制数据使用 base64。
- `ctrl_get_file`：`remote_path`，`local_path` 可选。
- `ctrl_get_big_file`：`remote_path`，`local_path` 可选。
- `ctrl_set_file`：`local_path`，`remote_path`
- `ctrl_set_big_file`：`local_path`，`remote_path`
- `exec`：`command`，默认禁用。

### 兼容截图接口

```http
GET http://127.0.0.1:9000/api/screen
```

该接口直接返回 `image/png`，内部同样通过 `RealCtrlApi` 执行，不绕本地管道。

## 命名管道 API

管道名：

```text
\\.\pipe\real_ctrl_service_pipe
```

旧协议：

- 请求：4 字节 big-endian 长度 + postcard `InputCommand`
- 响应：4 字节 big-endian 长度 + postcard `ServerResponse`

新协议：

- 请求：4 字节 big-endian 长度 + `RTCAPI1\0` + JSON `ApiRequest`
- 响应：4 字节 big-endian 长度 + `RTCAPI1\0` + JSON `ApiResponse`

选择 JSON 的原因：

- `postcard` 不支持当前 `#[serde(tag = "kind")]` 内部标签枚举。
- 开放 API 需要可观察、可调试、跨语言友好。
- 旧协议继续使用 postcard，避免破坏现有调用方。

## 安全策略

### real_ctrl 到 ctrl_server

延续阶段二和阶段三：

- `real_ctrl` 默认使用 pinned TLS。
- TLS 模式下控制通道走 challenge/session。
- 数据通道必须绑定控制通道 session。

### HTTP

- 默认只绑定 `127.0.0.1`。
- 如果改成非 loopback 地址，必须配置 `REAL_CTRL_API_TOKEN`。
- token 比较使用固定时间比较，避免明显时序泄漏。
- HTTP 不承担公网远程控制入口职责。

### 管道

管道 SDDL：

```text
D:P(A;;GA;;;SY)(A;;GA;;;BA)(A;;GA;;;OW)
```

含义：

- `SY`：System
- `BA`：Administrators
- `OW`：对象 Owner

同时显式配置：

- `accept_remote(false)`
- `inheritable(false)`

## 发布硬化

新增 Cargo profile：

```powershell
cargo build -p real_ctrl --profile hardened
cargo build -p ctrl_server --profile hardened
cargo build -p ctrl_kik --profile hardened
```

`hardened` profile：

- `strip = "symbols"`
- `lto = "fat"`
- `codegen-units = 1`
- `panic = "abort"`
- `debug = 0`
- `incremental = false`

说明：

- 这不是“防逆向银弹”，只能降低静态分析便利性和符号信息暴露。
- 不能替代服务端身份校验、密钥最小暴露、权限边界和审计。

## 维护坑

- 不要把新版 pipe API 改回 postcard。`ApiCommand` 使用 serde 内部标签，postcard 会报 “This is a feature that PostCard will never implement”。
- `screen_stream` 本阶段没有改动，后续仍按用户要求独立处理。
- 不要让 HTTP handler 直接拼内部命令并绕过 `RealCtrlApi`。
- 不要默认开启 `Exec`；如果必须开放，要显式设置 `REAL_CTRL_API_ALLOW_EXEC=1` 并记录调用方。
- 不要把 `config/app.toml` 默认绑定改回 `0.0.0.0`。

## 生产收敛补充（2026-07-11）

- 新增 `real_ctrl/src/lib.rs` 作为三个 bin 的统一组合根，API、连接、策略和协议适配不再重复编译三份。
- HTTP 非 loopback 绑定在启动阶段强制要求至少 32 字符 token；配置文件缺失会失败，不会使用框架 `0.0.0.0` 默认值。
- HTTP 增加 1 MiB body limit、370 秒请求 timeout、panic 捕获和 48 MiB API 二进制响应上限。
- pipe 增加 30 秒读写超时、16 连接并发上限；锁文件与 pipe 锁获取均改为 fail-closed。
- API 字段新增 request id、kik id、路径、Exec 命令最大长度和 NUL 拒绝；内部错误详情只记录日志，对外返回稳定通用错误。
- ctrl_kik 默认包含 Exec；管理面保留 real_ctrl API 与 ctrl_server 两层显式授权。
