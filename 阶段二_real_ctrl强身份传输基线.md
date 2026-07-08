# 阶段二：real_ctrl 强身份传输基线

日期：2026-07-08

## 1. 阶段目标

本阶段完成 `real_ctrl -> ctrl_server` 的强身份传输基线：

- `real_ctrl` 默认使用 TLS 1.3 连接 `ctrl_server` 的管理端口。
- `real_ctrl` 在 TLS 握手完成后校验服务端证书 SPKI SHA-256 pin。
- 服务端 IP 可以变化；`real_ctrl` 校验的是服务端证书/公钥身份，不是 IP。
- `ctrl_server` 保留原明文端口给 `ctrl_kik` 和兼容链路，同时新增 `real_ctrl` 专用 TLS 管理端口。
- `ctrl_kik` 不接入服务端身份校验，不新增证书、pin 或服务端机器信息依赖。
- `screen_stream` 仍不改动。

## 2. 新增代码结构

核心入口：

- `common::secure_transport`
  - `connect_real_ctrl`：`real_ctrl` 客户端统一连接入口。
  - `build_server_tls_acceptor`：构建 `ctrl_server` TLS acceptor。
  - `accept_tls`：服务端接收 TLS 连接并拆分读写半边。
  - `certificate_spki_sha256_hex`：计算证书 SPKI SHA-256 hex pin。
  - `spki_sha256_hex_from_pem_file`：从 PEM 证书计算 pin，便于后续工具化。
- `common::config::SecurityConfig`
  - `real_ctrl_from_env`：控制端默认 pinned TLS。
  - `ctrl_server_from_env`：服务端按环境变量启用 TLS 管理端口。
  - `plain`：仅给 `ctrl_kik` 和显式兼容场景使用。
- `common::channel::Channel`
  - writer 已从 `TcpStream::OwnedWriteHalf` 改为异步写 trait object，用于复用明文和 TLS 后的业务路径。
  - `from_tcp_writer` 保留旧明文 TCP 写半边兼容构造器。

接入点：

- `real_ctrl::ctrl_conn` 和 `real_ctrl::ctrl_data_conn` 通过 `connect_real_ctrl` 建立连接。
- `ctrl_server::core::server::run` 在配置证书和私钥后额外监听 TLS 管理端口。
- `ctrl_kik::kik_conn` 和 `ctrl_kik::kik_data_conn` 仍使用原明文 TCP 路径。

## 3. 运行配置

### 3.1 ctrl_server

明文端口仍按原配置监听 `server_port`，当前硬编码为 `9002`。

启用 TLS 管理端口需要同时配置：

```powershell
$env:CTRL_SERVER_TLS_CERT="D:\path\server.crt"
$env:CTRL_SERVER_TLS_KEY="D:\path\server.key"
$env:CTRL_SERVER_TLS_PORT="9443"
```

规则：

- 只配置证书或只配置私钥会启动失败。
- TLS 管理端口只接受 TLS 1.3。
- TLS 握手完成后仍复用现有 `InitFrame`、`Frame` 和 `ChannelType` 分派逻辑。
- 明文端口继续存在，主要服务 `ctrl_kik` 和迁移兼容，不作为 `real_ctrl` 生产入口。

### 3.2 real_ctrl

`real_ctrl` 默认启用 pinned TLS。必须配置：

```powershell
$env:REAL_CTRL_TLS_CA_CERT="D:\path\server-or-private-ca.crt"
$env:REAL_CTRL_TLS_SERVER_SPKI_SHA256="sha256 hex of server SPKI"
$env:REAL_CTRL_TLS_PORT="9443"
$env:REAL_CTRL_TLS_SERVER_NAME="real-ctrl-server"
```

说明：

- `server_host` 仍是连接目标地址，可以是变化的 IP。
- `REAL_CTRL_TLS_SERVER_NAME` 是证书 SAN/CN 校验用的稳定服务端名称，不要求等于 IP。
- `REAL_CTRL_TLS_SERVER_SPKI_SHA256` 是服务端叶子证书 SubjectPublicKeyInfo DER 的 SHA-256 hex。
- pin 支持 `sha256:` 前缀和冒号分隔格式，内部会归一化为小写 hex。
- 缺少 CA 证书或 SPKI pin 时，强安全连接会直接失败，不会自动降级到明文。

仅本地开发或迁移兼容时允许显式回到旧明文：

```powershell
$env:REAL_CTRL_ALLOW_PLAIN="1"
```

该开关不能用于生产环境。

## 4. 安全边界

已完成：

- `real_ctrl` 默认不再“忘配即明文”。
- `real_ctrl` 强安全模式下不会自动降级。
- TLS 只允许 TLS 1.3。
- 服务端身份校验不绑定 IP，适合服务端 IP 变化场景。
- `ctrl_kik` 不获取服务端证书、服务端主机名、机器码或其他服务端机器信息。

仍未完成：

- `CtrlAuthReq` / `CtrlDataConnReq` 仍是历史静态摘要，后续需要替换为 nonce challenge、session token 和数据通道绑定。
- `ctrl_server` 明文端口仍存在，后续需要按连接角色限流、隔离和审计。
- TLS 证书生成、轮换、pin 发布和吊销流程还未工具化。
- 本地 HTTP API 还未统一接入 token、ACL 和完整 schema。

## 5. 证书与 pin 生成建议

生产建议：

- 使用私有 CA 或离线生成的服务端证书。
- 证书 SAN 中包含稳定服务名，例如 `real-ctrl-server`。
- 将私钥只放在 `ctrl_server` 机器上，并收紧文件 ACL。
- 将 CA 证书和 SPKI pin 通过可信安装流程写入 `real_ctrl` 配置。

pin 内容：

```text
hex(sha256(leaf_certificate.subject_public_key_info_der))
```

项目内已提供计算函数：

```rust
common::secure_transport::spki_sha256_hex_from_pem_file(path)
```

后续可以基于该函数做一个受控的项目内小工具，避免人工用错“整张证书 hash”和“SPKI hash”。

## 6. 验收命令

```powershell
cargo test -p common -p ctrl_common
cargo check -p real_ctrl -p ctrl_kik -p ctrl_server
git diff -- screen_stream
```

本阶段的运行态联调需要准备证书、私钥、CA/证书文件和 SPKI pin。没有这些材料时，编译和单元测试仍应通过，但 `real_ctrl` 默认连接会拒绝明文。

## 7. 下一阶段入口

下一阶段建议做服务端会话和授权收敛：

1. `real_ctrl` TLS 成功后使用 nonce challenge 替换静态摘要。
2. 服务端签发 `session_id`。
3. `CtrlData` 连接通过 `session_id + channel_nonce + auth_tag` 绑定控制会话。
4. `ctrl_server` 对 `real_ctrl` 会话、`ctrl_kik` 实例和命令能力做统一授权。
5. 明确 `Exec` 等高危能力默认禁用或必须经策略显式放行。
