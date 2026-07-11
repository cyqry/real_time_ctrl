# real_ctrl 维护规则

## 阶段二安全传输补充

- `real_ctrl` 默认必须使用 pinned TLS 连接 `ctrl_server`。只有本地开发或迁移兼容可显式设置 `REAL_CTRL_ALLOW_PLAIN=1`。
- 强安全模式缺少 CA 证书或 SPKI pin 时必须失败，不能自动降级到明文。
- `server_host` 是连接地址，服务端身份由证书和 SPKI pin 校验，不绑定 IP。
- TLS 模式下控制通道必须走 challenge/session，数据通道必须绑定 session；不要恢复静态摘要过线。
- `REAL_CTRL_AUTH_SECRET` 至少 32 个 ASCII 字符，缺失或过短时必须启动失败；不得提供编译期默认值。

## 职责边界

`real_ctrl` 是控制端。它负责校验 `ctrl_server` 身份、本地 CLI、本地命名管道 API、HTTP API 适配和控制命令发起。

## 安全约束

- `real_ctrl -> ctrl_server` 生产链路必须使用服务端身份可验证的 pinned TLS 连接。
- 服务端身份校验不绑定 IP，优先使用服务端公钥 pin 或私有 CA pin。
- 本地 HTTP 默认只监听 `127.0.0.1`，不能默认暴露到公网地址。
- 本地命名管道需要限制访问主体，不能让任意本机低权限进程直接调用高危控制能力。
- `Exec(String)` 属于高危能力，开放 API 默认不应无条件暴露。
- 运行时服务端地址和端口只使用 `REAL_CTRL_SERVER_HOST`、`REAL_CTRL_SERVER_PORT`；不要复用 `ctrl_kik` 的编译期配置覆盖规则。

## API 约束

- CLI、命名管道、HTTP 应收敛到同一个服务层，避免三处各自实现权限和错误处理。
- 当前统一服务层入口是 `real_ctrl::api_service::RealCtrlApi`。新增 pipe 或 HTTP 能力时，优先扩展该入口，再由具体协议层做适配。
- API 响应应使用稳定错误码和结构化 body，不要把内部 `anyhow` 字符串直接当长期契约。
- HTTP 开放 API 使用 `POST /api/v1/commands`，请求/响应契约在 `api_contract.rs` 中维护。
- 新版命名管道 API 使用 `RTCAPI1\0` magic + JSON `ApiRequest` / `ApiResponse`；不要改回 postcard，postcard 不支持当前 serde 内部标签枚举。
- 旧命名管道 postcard `InputCommand` 只作为兼容协议保留，新能力优先走新版 API 契约。
- `Exec` 对开放 API 默认禁用，只能通过 `REAL_CTRL_API_ALLOW_EXEC=1` 显式开启。
- `config/app.toml` 默认必须绑定 `127.0.0.1`；如果改成非 loopback，必须同步配置 `REAL_CTRL_API_TOKEN` 并记录原因。
- 本地管道创建时必须保留 SDDL DACL 和 `accept_remote(false)` / `inheritable(false)`。
- 生产发布优先使用根目录 `scripts/build_hardened.ps1`，由脚本统一启用 hardened profile、锁定依赖和 Windows CFG。
- 三个进程形态统一依赖 `real_ctrl/src/lib.rs` 的 library 组合根；禁止在各 bin 中重新 `mod` 同一批业务模块。
- HTTP 路由必须由启动组合根通过 `routes::router()` 显式安装；不能只依赖 library 内的 inventory 注册，否则链接器可能丢弃未直接引用的注册对象并形成空路由表。
- 已成功写出的控制命令不得在断线后自动重放；连接恢复后应返回“结果未知”，由调用者查询状态后决定是否重试。
- HTTP 非 loopback 绑定必须在启动时验证至少 32 字符的 `REAL_CTRL_API_TOKEN`；配置文件缺失时不能回退到框架的 `0.0.0.0` 默认值。
- 命名管道每次读写必须有超时，服务端并发连接上限为 16；HTTP 请求体默认上限 1 MiB。

## 测试要求

- 修改本地 API 后运行 `cargo check -p real_ctrl`，并优先补服务层单元测试。
- 修改 `real_ctrl` 控制/数据通道握手后，运行根目录 `scripts/e2e_ctrl_stack.ps1` 验证 pinned TLS、HTTP API 和 `ctrl_kik` 转发链路。
- 读循环中不要把 `framed_arc.lock().await.next()` 直接写进 `match` 条件；读锁应限制在局部块内，避免后续握手处理需要同一 reader 时隐式自锁。
