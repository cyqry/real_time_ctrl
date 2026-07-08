# real_time_ctrl 安全与开放 API 改造计划

## 0. 本轮边界

本计划只覆盖 `real_ctrl`、`ctrl_server`、`ctrl_kik`、`common`、`ctrl_common` 及根构建配置。

`screen_stream` 模块暂不改动。后续如果远程桌面流要接入新的安全传输，只通过它已经暴露的 `AsyncRead/AsyncWrite` 边界接入，不进入 `screen_stream` 内部改造。

Android 控制端暂不实现。本计划只保留对未来 Android 接入有利的协议边界，不新增 Android 工程、JNI、UniFFI 或 Kotlin 代码。

## 1. 设计原则

### 1.1 `real_ctrl` 是强身份客户端

`real_ctrl` 代表控制者，必须校验 `ctrl_server` 身份，目标是防止中间人攻击。

具体原则：

- `real_ctrl -> ctrl_server` 使用经过身份校验的 TLS 1.3。
- 服务端身份不绑定 IP，避免服务端 IP 变化导致校验失效。
- 推荐使用服务端公钥 pinning 或私有 CA pinning，而不是依赖公网 CA 和固定域名。
- `real_ctrl` 持有的服务端身份材料只包含用于校验的公钥、证书或 CA 信息，不包含服务端机器指纹、主机名、硬件信息等额外信息。
- 连接前允许配置多个候选 IP/端口，连接后通过证书公钥确认“这是预期的服务端”。

### 1.2 `ctrl_kik` 是极小被控客户端

`ctrl_kik` 按“极小引入、极小可见、极小认知”设计。一个 `ctrl_kik` 实例即可理解为一个用户/被控实例，它不需要账号密码式身份校验。

具体原则：

- `ctrl_kik` 不引入完整身份认证链路。
- `ctrl_kik` 不主动获取除服务端 IP/端口之外的服务端机器信息。
- `ctrl_kik` 不依赖服务端证书、服务端机器指纹、域名、硬件标识或远程系统信息。
- `ctrl_kik` 的持久状态保持最小化，优先只保存本实例 id、服务端地址候选、必要运行配置。
- 如果无法在“不知道服务端身份”的前提下可靠抗 MITM，则 `ctrl_kik` 不做服务端身份校验，并在安全模型中明确这一点。

结论：`ctrl_kik -> ctrl_server` 可以做传输混淆或机会型加密，但不承诺抗 MITM。抗 MITM 的强安全边界放在 `real_ctrl -> ctrl_server`。

### 1.3 服务端不扩大客户端认知

`ctrl_server` 对 `real_ctrl` 提供可验证身份，对 `ctrl_kik` 不要求它理解服务端机器身份。

服务端需要承担：

- 保护 `real_ctrl` 管理面连接。
- 隔离不同 `real_ctrl` 会话。
- 管理 `ctrl_kik` 实例 id 与在线状态。
- 防止未授权 `real_ctrl` 控制任意 `ctrl_kik`。
- 对来自 `ctrl_kik` 的连接做速率限制、帧大小限制、资源隔离和异常断开处理。

## 2. 当前架构判断

现有执行路径如下：

```text
real_ctrl
  -> ctrl_conn: 控制通道
  -> ctrl_data_conn: 数据通道
  -> ctrl_server
  -> ChannelType 分派:
       Ctrl / CtrlData / Kik / KikData
  -> ctrl_kik
       kik_conn: 命令通道
       kik_data_conn: 数据通道
```

当前协议特征：

- 外层是 `u32 big-endian length + body`。
- 初始化阶段使用 `InitFrame`。
- 业务阶段使用 `Frame` / `KikFrame`。
- `real_ctrl` 现有鉴权是 `username/password` 经过静态摘要后发送。
- `ctrl_kik` 当前不使用真实账号密码，服务端分配或复用 kik id。
- 本地开放能力已经有雏形：
  - 命名管道：`real_ctrl/src/local_server`
  - 本地客户端：`real_ctrl/src/local_client`
  - HTTP：`real_ctrl/src/http_service`

主要问题：

- 远程 TCP 当前是明文。
- `real_ctrl` 的静态摘要认证不能抵抗中间人和重放。
- 连接类型、鉴权、会话绑定、业务帧缺少统一协议版本和能力协商。
- `real_ctrl` 的 CLI、管道、HTTP 还没有完全收敛到稳定开放 API。
- `ctrl_kik` 客户端需要继续控制依赖体积和代码可见面，不能把复杂安全状态机压进去。

## 3. 目标架构

### 3.1 连接安全分层

```text
real_ctrl 管理面:
  TCP
    -> TLS 1.3, real_ctrl 校验 ctrl_server 身份
      -> length-delimited frame
        -> InitFrame / Frame

ctrl_kik 被控面:
  TCP
    -> 可选机会型加密或保持当前明文模式
      -> length-delimited frame
        -> InitFrame / KikFrame
```

`real_ctrl` 和 `ctrl_kik` 不使用同一套安全强度假设。这样可以同时满足：

- 控制端必须防 MITM。
- 被控端保持极小实现，不学习服务端身份。
- 服务端 IP 变化不影响 `real_ctrl` 的身份校验。
- `screen_stream` 不参与本阶段改造。

### 3.2 服务端身份模型

`ctrl_server` 生成一组长期服务端身份材料：

```text
server private key
server certificate 或 public key
server identity id = hash(public key)
```

`real_ctrl` 保存：

```text
server_public_key_pin 或 private_ca_cert
allowed_server_identity_id
candidate_endpoints = [ip:port, ip:port, ...]
```

校验逻辑：

1. `real_ctrl` 连接候选 IP。
2. TLS 握手时拿到服务端证书。
3. 校验证书链或 SPKI pin。
4. 校验通过后才发送 `CtrlAuthReq` 或新握手帧。
5. 校验失败立即断开，不回退明文。

因为校验对象是公钥/证书，而不是 IP，所以服务端 IP 变化不会破坏身份校验。

### 3.3 `ctrl_kik` 实例模型

`ctrl_kik` 首次连接：

```text
ctrl_kik -> ctrl_server: KikReq { id: None, name/minimal_info }
ctrl_server -> ctrl_kik: KikId(id)
ctrl_kik 本地保存 id
```

重连：

```text
ctrl_kik -> ctrl_server: KikReq { id: Some(existing_id), name/minimal_info }
ctrl_server -> ctrl_kik: KikId(existing_id)
```

这里的 `id` 是实例标识，不是安全认证凭据。它用于服务端识别同一个被控实例的重连和状态管理。真正的授权应由 `real_ctrl` 管理面完成，而不是让 `ctrl_kik` 承担身份系统。

`ctrl_kik` 不应获取：

- 服务端主机名。
- 服务端系统版本。
- 服务端机器码。
- 服务端证书详情。
- 服务端硬件信息。
- 服务端账号或租户信息。

`ctrl_kik` 可知道：

- 连接 IP。
- 连接端口。
- 本实例 id。
- 当前协议版本。
- 服务端返回的最小必要控制帧。

## 4. 改造阶段

## 阶段一：架构基线与协议护栏

目标：不改变业务语义，先把当前行为固定下来。

改动范围：

- `common`
- `ctrl_common`
- `ctrl_server`
- `real_ctrl`
- `ctrl_kik`

不改：

- `screen_stream`

任务：

1. 增加协议文档。
   - 记录 `InitFrame`、`Frame`、`KikFrame` 的当前字段。
   - 记录四类通道状态转换。
   - 记录 `real_ctrl`、`ctrl_server`、`ctrl_kik` 的启动和重连流程。

2. 增加协议测试。
   - 长度帧 round-trip。
   - `InitFrame` round-trip。
   - `Frame` / `KikFrame` round-trip。
   - 非法帧、短帧、超大帧拒绝。

3. 给解码器加最大帧长度。
   - 控制通道建议默认 1 MiB。
   - 数据通道按文件分片需求单独配置。
   - 超过限制直接断开，防止内存打爆。

4. 增加协议版本字段。
   - 新增 `ProtocolHello` 或扩展 `InitFrame`。
   - 包含 `version`、`role`、`capabilities`。
   - 老协议保留兼容窗口，但新安全能力只在新协议上启用。

验收：

- 原有 CLI 命令仍可用。
- `ctrl_kik` 仍可连接、上线、执行命令。
- 不引入 `screen_stream` 改动。
- `cargo test -p common -p ctrl_common` 通过。

当前状态（2026-07-08）：

- 阶段一已完成一版基线实现，详见根目录 `阶段一_协议与执行路径基线.md`。
- 已完成长度帧最大值限制、短帧/空帧解析保护、`ProtocolHello` 预留结构、本地 pipe API 服务层入口和 pipe 消息大小护栏。
- 已验证 `cargo test -p common -p ctrl_common` 通过。
- 已验证 `cargo check -p real_ctrl -p ctrl_kik -p ctrl_server` 通过。
- 已确认 `screen_stream` 无 diff。

## 阶段二：`real_ctrl` 强校验传输

目标：让 `real_ctrl` 防中间人攻击。

依赖策略：

- TLS 相关依赖优先放在 `common` 的 feature 中，例如 `secure-transport`。
- `real_ctrl`、`ctrl_server` 启用该 feature。
- `ctrl_kik` 默认不启用，避免引入复杂 TLS 校验链路。

推荐技术：

- `rustls`
- `tokio-rustls`
- `rustls-pemfile`
- 可选：`rcgen` 仅用于开发环境生成测试证书，不进入生产默认路径。

改造点：

1. 抽象连接创建。

   新增类似：

   ```text
   common::transport
     PlainClientConnector
     TlsClientConnector
     PlainServerAcceptor
     TlsServerAcceptor
   ```

   业务代码不直接 `TcpStream::connect` 后裸拆分，而是先得到一个已完成安全握手的 stream。

2. `ctrl_server` 同端口兼容策略。

   推荐短期使用两个端口：

   ```text
   9002: 旧明文或 ctrl_kik 极小客户端
   9443: real_ctrl 安全管理面
   ```

   原因：

   - 实现简单。
   - 不需要在同一个连接上 sniff TLS/plain。
   - 降低误判和调试成本。
   - 不影响 `ctrl_kik` 最小化。

3. `real_ctrl` 服务端身份校验。

   配置示例：

   ```toml
   [server]
   endpoints = ["1.2.3.4:9443", "5.6.7.8:9443"]
   identity_pin = "sha256/base64-of-spki"
   ```

   行为：

   - 遍历 endpoints。
   - 连接成功后校验 pin。
   - pin 不匹配直接断开。
   - 不允许降级到明文。

4. 替换 `real_ctrl` 当前静态摘要鉴权。

   第一阶段可以保留业务层账号，但必须运行在 TLS 内。

   后续替换为：

   ```text
   real_ctrl local secret
     -> TLS server-auth
     -> nonce challenge
     -> HMAC / signed challenge
     -> server issues session token
   ```

   这样避免静态 token 被重放。

验收：

- `real_ctrl` 连接错误服务端证书时失败。
- `real_ctrl` 连接正确服务端证书时成功。
- 服务端 IP 改变但证书 pin 不变时仍可连接。
- `ctrl_kik` 不需要新增服务端证书配置。

当前状态（2026-07-08）：

- 阶段二已完成一版强身份传输基线，详见根目录 `阶段二_real_ctrl强身份传输基线.md`。
- `real_ctrl` 默认 pinned TLS，显式 `REAL_CTRL_ALLOW_PLAIN=1` 才允许明文兼容。
- `ctrl_server` 支持额外 TLS 管理端口，使用 `CTRL_SERVER_TLS_CERT` / `CTRL_SERVER_TLS_KEY` 启用。
- TLS 管理端口限制为 TLS 1.3，并在客户端额外校验服务端叶子证书 SPKI SHA-256 pin。
- `ctrl_kik` 未接入 TLS 身份校验链路，继续保持极小客户端边界。

## 阶段三：`ctrl_kik` 极小客户端收敛

目标：降低 `ctrl_kik` 依赖、可见信息和安全状态复杂度。

任务：

1. 保持 `ctrl_kik` 默认明文或极简机会型加密。

   如果启用机会型加密，只能声明：

   - 可防被动旁路抓包。
   - 不防主动 MITM。
   - 不校验服务端身份。

   如果引入机会型加密会显著增加依赖或暴露服务端信息，则本阶段不做。

2. 移除 `ctrl_kik` 对账号密码概念的依赖。

   当前 `ctrl_kik` 配置中 `username/password` 为空，应进一步把这个语义从被控端路径剥离。

   目标：

   ```text
   ctrl_kik:
     server_endpoint
     instance_id
     display_name
     protocol_version
   ```

3. 最小化 `KikInfo`。

   当前 `KikInfo` 至少包含 `id`、`name`。保持这个方向，避免加入：

   - 主机硬件指纹。
   - 系统完整信息。
   - 用户账号详情。
   - 证书信息。

4. 限制 `ctrl_kik` 可执行命令面。

   现在 `Exec(String)` 能直接执行任意命令，生产上风险非常高。

   需要增加策略层：

   - 默认禁用任意 shell。
   - 文件、截图、目录列表是显式能力。
   - shell 执行需要构建期开关或服务端策略显式放行。
   - 每条命令带能力标识，服务端授权后才转发。

5. 降低可见性。

   - release profile 启用符号剥离。
   - 减少日志。
   - 错误信息不暴露内部路径和协议细节。
   - 构建时区分 debug/release 行为。

验收：

- `ctrl_kik` 不需要服务端证书文件。
- `ctrl_kik` 不读取服务端除 IP/端口之外的机器信息。
- `ctrl_kik` 仍可上线、重连、执行允许的命令。
- `ctrl_kik` 默认依赖不显著膨胀。

## 阶段四：服务端会话与授权

目标：服务端承担安全边界，避免把复杂身份问题压给 `ctrl_kik`。

任务：

1. 引入控制会话。

   ```text
   real_ctrl authenticated session
     -> session_id
     -> selected_kik_id
     -> allowed_capabilities
   ```

2. 数据通道绑定控制会话。

   当前 `CtrlDataConnReq` 使用同一身份摘要。改为：

   ```text
   CtrlDataConnReq {
     session_id,
     channel_nonce,
     auth_tag
   }
   ```

   数据通道只能绑定已认证的 `real_ctrl` 控制会话。

3. `ctrl_kik` 数据通道绑定实例 id。

   `KikDataConnReq(id)` 可保留，但服务端必须校验：

   - 该 id 当前有在线命令通道。
   - 数据通道来源和命令通道生命周期匹配。
   - 异常数据通道不能创建新被控实例。

4. 服务端资源隔离。

   - 每个 `real_ctrl` 同时只允许有限会话数。
   - 每个 `ctrl_kik` 限制数据通道数。
   - 每个连接限制最大帧、最大队列、最大命令执行时间。
   - 心跳超时后主动清理。

验收：

- 未认证 `real_ctrl` 不能创建数据通道。
- 过期 session 不能继续发送数据。
- 未上线 `ctrl_kik` 的数据通道请求被拒绝。
- 大帧和高频连接不会拖垮服务端。

当前状态（2026-07-08）：

- 阶段三已完成一版控制会话与数据通道绑定，详见根目录 `阶段三_real_ctrl会话与数据通道绑定.md`。
- `real_ctrl` 默认 TLS 模式使用 nonce challenge + HMAC-SHA256 proof，不再发送历史静态摘要。
- `ctrl_server` 签发 `session_id`，并要求 `CtrlData` 使用 `session_id + channel_nonce + proof` 绑定控制会话。
- 服务端会记录已使用数据通道 nonce，防止同一 session 下重放。
- 旧静态摘要帧仅用于显式明文兼容模式。

## 阶段五：`real_ctrl` 开放 API

目标：提供稳定、本地可控、可扩展的 `real_ctrl` API。

现有基础：

- 命名管道服务：`real_ctrl/src/local_server`
- 命名管道客户端：`real_ctrl/src/local_client`
- HTTP service：`real_ctrl/src/http_service`
- 统一执行入口：`dispatch::distribution_other`

改造方向：

1. 抽出统一服务层。

   建议新增：

   ```text
   real_ctrl::api_service
     CommandService
     DeviceService
     FileService
     ScreenService
   ```

   CLI、命名管道、HTTP 都只做协议适配，不直接拼业务逻辑。

2. 命名管道协议升级。

   当前是：

   ```text
   u32 len + postcard(InputCommand)
   ```

   改为：

   ```text
   u32 len
   ApiEnvelope {
     version,
     request_id,
     deadline_ms,
     method,
     body
   }
   ```

   响应：

   ```text
   ApiResponse {
     request_id,
     status,
     error_code,
     body
   }
   ```

3. 命名管道安全。

   - 管道名固定但带产品前缀。
   - 设置 Windows ACL，只允许当前用户和管理员访问。
   - 限制单请求大小。
   - 限制并发请求。
   - 对长任务返回 task id，避免一个请求长期占住管道。

4. HTTP API 安全。

   - 默认只监听 `127.0.0.1`。
   - 生产默认需要 bearer token。
   - token 随机生成并保存在本机受控路径。
   - 不默认监听 `0.0.0.0`。
   - HTTP 只是本地调用入口，不承担远程公网控制入口。

5. API 方法。

   第一批稳定 API：

   ```text
   GET  /health
   GET  /api/v1/kiks
   GET  /api/v1/kiks/current
   POST /api/v1/kiks/current
   POST /api/v1/commands/exec
   POST /api/v1/files/list
   POST /api/v1/files/download
   POST /api/v1/files/upload
   GET  /api/v1/screen
   ```

   其中 `exec` 默认应受策略限制，不能无条件开放。

验收：

- CLI、pipe、HTTP 返回同一类结构化结果。
- HTTP 调用不绕过权限和策略。
- 管道和 HTTP 都能复用同一个 `CommandService`。
- 错误码稳定，不把内部 anyhow 字符串直接当 API contract。

## 阶段六：Android 远程连接（暂缓）

目标：当前不实施，只保留未来支持 Android 控制端所需的协议边界。

由于 `real_ctrl` 是强身份客户端，Android 控制端应走与 `real_ctrl` 类似的安全模型：

```text
Android controller
  -> TLS 1.3
  -> server public key pinning
  -> controller auth
  -> ctrl_server
```

设计路线：

1. 协议 SDK 化。

   把协议编解码、命令模型、响应模型抽到可复用 crate：

   ```text
   ctrl_sdk_core
     protocol codec
     command model
     response model
     secure connector interface
   ```

   注意：这一步不要把 Windows 专有逻辑带进去。

2. Android 调用方式。

   可选路线：

   - Rust core + UniFFI/Kotlin。
   - Rust core + JNI。
   - Kotlin 原生实现协议，Rust 仅作为服务端/Windows 侧。

   推荐先用 Kotlin 原生实现控制协议，降低移动端构建复杂度；等协议稳定后再评估 UniFFI。

3. Android 身份材料。

   - 服务端 public key pin 内置或通过首次配对写入 Android Keystore。
   - 控制端 token/private key 存 Android Keystore。
   - IP 可变时只更新 endpoint，不更新身份 pin。

4. 屏幕流。

   本阶段不改 `screen_stream`。

   后续接入时遵循：

   - 服务端不解码再编码。
   - Android 优先使用 MediaCodec 解码 H.264。
   - 如果需要浏览器/移动通用实时视频，再单独评估 WebRTC。

暂缓期间验收：

- 不新增 Android 端代码。
- 不新增 Android 构建配置。
- 不为了 Android 改动 `screen_stream`。
- `real_ctrl` 管理面安全模型保持可被未来 Android 控制端复用。

## 5. 反逆向与生产加固

这部分重点针对 `real_ctrl`、`ctrl_server` 和 `ctrl_kik` 的生产发布。

原则：

- 不能把“防逆向”作为唯一安全边界。
- 客户端里的秘密必须假设可能被提取。
- 真正的授权、隔离、审计必须在服务端完成。

`ctrl_kik` 加固：

- release 构建剥离符号。
- 降低日志和错误信息细节。
- 不内置高价值服务端秘密。
- 不内置可用于控制全局系统的账号密码。
- 尽量减少依赖和功能面。
- 默认禁用任意 shell 命令。

`real_ctrl` 加固：

- 服务端身份 pin 不以明文配置随意散落。
- 控制端凭据放本机安全存储。
- 本地 HTTP token 放受限权限文件。
- API 调用记录审计日志。

`ctrl_server` 加固：

- 私钥文件权限收紧。
- 管理面 TLS 强制开启。
- 控制端认证失败限速。
- kik 上线、下线、被控选择、命令执行写审计。
- 所有外部输入都有长度限制。

构建加固：

- release profile 增加 `strip = "symbols"`。
- 可按目标 crate 设置 `panic = "abort"`。
- 保持 LTO。
- CI 输出可复现构建信息。
- Windows 产物做代码签名。

## 6. 推荐实施顺序

1. 写架构文档和协议测试，固定当前行为。
2. 给帧解码加最大长度和错误路径测试。
3. 为 `real_ctrl` 和 `ctrl_server` 抽象 transport。
4. 增加 `real_ctrl` 专用 TLS 管理端口。
5. 实现 `real_ctrl` 服务端 public key pinning。
6. 把 `real_ctrl` 数据通道绑定到认证 session。
7. 清理 `ctrl_kik` 的身份语义，只保留实例 id。
8. 收敛 `real_ctrl` 的 CLI、pipe、HTTP 到统一服务层。
9. 增加 API schema、错误码、权限策略。
10. 暂缓 Android 控制端实现，仅在协议文档中保留未来扩展点。

## 7. 风险与取舍

### 7.1 `ctrl_kik` 不校验服务端身份的风险

如果 `ctrl_kik` 不校验服务端身份，则主动中间人可以冒充服务端与 `ctrl_kik` 建立连接。

这是由设计约束决定的安全边界：不知道服务端身份，就无法证明对端是正确服务端。

缓解方式：

- 服务端不信任 `ctrl_kik` 上报的高价值身份。
- 真正控制授权在 `real_ctrl` 管理面完成。
- 限制 `ctrl_kik` 默认命令能力。
- 对异常 kik 连接做限速和隔离。
- 可选提供企业版/高安全版 `ctrl_kik`，允许配置服务端 pin。

### 7.2 IP 变化与服务端身份校验

IP 变化和身份校验并不冲突。`real_ctrl` 不应校验“是不是这个 IP”，而应校验“这个 IP 上的服务端是否持有预期私钥”。

因此推荐：

```text
endpoint 可变
server public key pin 稳定
```

### 7.3 机会型加密的边界

没有身份校验的加密只能防被动监听，不能防主动中间人。

如果未来给 `ctrl_kik` 做机会型加密，文档和日志必须明确：

```text
encrypted but not authenticated
```

不能把它描述成防 MITM。

## 8. 完成后的目标状态

完成后系统应达到：

- `screen_stream` 未被本阶段改动。
- `real_ctrl` 能校验 `ctrl_server` 身份，防止中间人攻击。
- 服务端 IP 可变化，不影响 `real_ctrl` 的身份校验。
- `ctrl_kik` 保持极小依赖和极小服务端认知。
- `ctrl_kik` 不需要账号密码式身份校验。
- 服务端承担会话授权、资源隔离和审计。
- 本地 pipe 和 HTTP API 稳定、结构化、可扩展。

## 阶段四执行结果：开放 API 与本地入口加固

阶段四已经完成以下落地项：

- `real_ctrl` 新增稳定 API 契约：`ApiRequest`、`ApiCommand`、`ApiResponse`、`ApiResponseData`、`ApiErrorBody`。
- HTTP 入口新增 `POST /api/v1/commands`，并保留 `GET /api/screen`。
- HTTP handler 通过 spring-web `Component<RealCtrlApi>` 直接调用服务层，不再绕本地命名管道。
- 命名管道同时支持旧协议和新版开放 API：
  - 旧协议：postcard `InputCommand` / `ServerResponse`
  - 新协议：`RTCAPI1\0` magic + JSON `ApiRequest` / `ApiResponse`
- 新版命名管道改用 JSON 的原因是 postcard 不支持当前 serde 内部标签枚举。
- `real_ctrl_invoker_http_service` 只创建一份 `Context`，管道和 HTTP 共享同一条 real_ctrl 控制连接上下文。
- `config/app.toml` 默认 HTTP 监听收紧到 `127.0.0.1`，CORS 不再使用 `*`。
- HTTP 支持可选 `REAL_CTRL_API_TOKEN`。
- 开放 API 默认禁止 `Exec`，需要 `REAL_CTRL_API_ALLOW_EXEC=1` 显式开启。
- 命名管道创建时设置 Windows SDDL DACL：`D:P(A;;GA;;;SY)(A;;GA;;;BA)(A;;GA;;;OW)`。
- 新增 `hardened` Cargo profile，生产构建可使用 `cargo build -p real_ctrl --profile hardened`。

剩余不在当前阶段内的事项：

- 安卓控制端仍按用户要求暂不实现。
- 进一步的商业级反逆向（壳、虚拟化、完整性自校验、远程证明）不建议在本项目内手写，需要结合发行渠道和威胁模型专项设计。
- Android 控制端可以复用 `real_ctrl` 管理面安全模型。
