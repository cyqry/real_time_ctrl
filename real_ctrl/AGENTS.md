# real_ctrl 维护规则

## 阶段二安全传输补充

- `real_ctrl` 必须使用 pinned TLS 连接 `ctrl_server`，旧明文路径已经删除。
- 强安全模式缺少 CA 证书或 SPKI pin 时必须失败，不能自动降级到明文。
- `server_host` 是连接地址，服务端身份由证书和 SPKI pin 校验，不绑定 IP。
- TLS 模式下控制通道必须走 challenge/session，数据通道必须绑定 session；不要恢复静态摘要过线。
- `REAL_CTRL_AUTH_SECRET` 至少 32 个 ASCII 字符。发布构建必须提供经 `hidden!(env!(...))` 加密的
  默认值以支持 EXE 直启；同名运行环境变量优先且仍需通过长度校验。

## 职责边界

`real_ctrl` 是控制端。它负责校验 `ctrl_server` 身份、本地 CLI、本地命名管道 API、HTTP API 适配和控制命令发起。

## 安全约束

- `real_ctrl -> ctrl_server` 生产链路必须使用服务端身份可验证的 pinned TLS 连接。
- 服务端身份校验不绑定 IP，优先使用服务端公钥 pin 或私有 CA pin。
- pinned TLS 连接在 ClientHello 前发送固定 `RTCT v4` 前导以穿越公网中间设备；不得把此前导当作身份凭据、把它做成明文降级开关，或绕过后续 TLS 和 pin 校验。
- 本地 HTTP 默认只监听 `127.0.0.1`，不能默认暴露到公网地址。
- 本地命名管道需要限制访问主体，不能让任意本机低权限进程直接调用高危控制能力。
- `Exec(String)` 属于高危能力，开放 API 默认不应无条件暴露。
- 三个进程形态统一使用 `runtime_config::connection_config`。`real_ctrl/build.rs` 提供公开生产
  默认值，`REAL_CTRL_*` 运行环境变量优先；不要复用 `ctrl_kik` 的编译期配置覆盖规则。
- `REAL_CTRL_ACCOUNT_ID` 确定授权域；`REAL_CTRL_INSTANCE_ID` 缺失时每个进程生成随机 UUID，避免同一
  账号的多个直启实例互相踢线。需要稳定重连身份时才显式配置 instance ID。

## API 约束

- CLI、命名管道、HTTP 应收敛到同一个服务层，避免三处各自实现权限和错误处理。
- 当前统一服务层入口是 `real_ctrl::api_service::RealCtrlApi`。新增 pipe 或 HTTP 能力时，优先扩展该入口，再由具体协议层做适配。
- API 响应应使用稳定错误码和结构化 body，不要把内部 `anyhow` 字符串直接当长期契约。
- HTTP 开放 API 使用 `POST /api/v1/commands`，请求/响应契约在 `api_contract.rs` 中维护。
- 新版命名管道 API 使用 `RTCAPI1\0` magic + JSON `ApiRequest` / `ApiResponse`；不要改回 postcard，postcard 不支持当前 serde 内部标签枚举。
- 旧命名管道 postcard 协议已经删除；无 `RTCAPI1\0` magic 的请求必须拒绝。
- `Exec` 与其他命令使用同一 API 分派路径；发布构建默认允许，运行时可用
  `REAL_CTRL_API_ALLOW_EXEC` 覆盖。服务端仍保留独立的第二层授权。
- `config/app.toml` 默认必须绑定 `127.0.0.1`；如果改成非 loopback，必须同步配置 `REAL_CTRL_API_TOKEN` 并记录原因。
- 本地管道创建时必须保留 SDDL DACL 和 `accept_remote(false)` / `inheritable(false)`。
- 生产发布优先使用根目录 `scripts/build_hardened.ps1`，由脚本统一启用 hardened profile、锁定依赖和 Windows CFG。
- 三个进程形态统一依赖 `real_ctrl/src/lib.rs` 的 library 组合根；禁止在各 bin 中重新 `mod` 同一批业务模块。
- HTTP 路由必须由启动组合根通过 `routes::router()` 显式安装；不能只依赖 library 内的 inventory 注册，否则链接器可能丢弃未直接引用的注册对象并形成空路由表。
- 已成功写出的控制命令不得在断线后自动重放；连接恢复后应返回“结果未知”，由调用者查询状态后决定是否重试。
- HTTP 非 loopback 绑定必须在启动时验证至少 32 字符的 `REAL_CTRL_API_TOKEN`；配置文件缺失时不能回退到框架的 `0.0.0.0` 默认值。
- 命名管道每次读写必须有超时，服务端并发连接上限为 16；HTTP 请求体默认上限 1 MiB。
- 所有 `RealCtrlApi` 实例共享 32 个请求许可；每个命令使用独立关联 ID、oneshot 响应和数据队列，
  permit 持有到控制响应及关联数据处理全部结束。达到上限返回稳定 `busy`，不得排入无界队列。
- 交互控制台由输入循环保持串行；HTTP 允许独立请求并行执行和乱序返回，不能用 CLI 的串行语义
  限制 HTTP。连接故障只允许单飞重连，已写出的命令不得自动重放。
- 开放 API 的 `ctrl_get_big_file` 必须提供 `local_path` 并流式写入系统临时目录的随机 `.temp` 文件；禁止把大文件聚合为 HTTP/管道内存响应。
- `$sys_history [kik_id]`、HTTP 与命名管道的 `sys_history` 必须共用服务层；返回服务端本进程观察到的最近上下线时间，不得伪装成跨重启持久历史。
- 大文件上传后台任务必须由当前命令持有 `JoinHandle`；服务端提前拒绝或连接失败时要 abort，不能污染下一条命令。
- 上传数据任务必须等待 `Agent` 成功写出控制帧后才启动；该本地 oneshot 门禁不能提前触发，也不能改成额外网络往返。
- 每个控制会话目标建立 3 条 CtrlData 连接；至少一条成功时允许降级运行。大文件使用最多 3 个在途分片逐帧轮询连接，接收端必须按地址范围支持乱序和完全重复帧。
- 三条 CtrlData 连接必须使用绑定当前 session 的长期监督槽位；单连接退出后自动补建，主连接
  换代时先取消旧监督器并清空旧池。数据类命令发出前必须允许一个有界恢复窗口。
- CtrlData 池恢复时必须广播唤醒全部并行 API 请求，且等待 future 必须先登记再检查池状态；不得
  使用仅唤醒一个等待者的连接可用通知。
- 下载完成或失败后必须发送 `DataAck` 并删除本地数据路由；服务端用 Ack 释放对应会话的大文件路由。
- 服务端数据可比控制响应先到；real_ctrl 只允许最多 16 条、合计 64 MiB、TTL 30 秒的未认领
  数据路由，响应处理开始后标记为活动。禁止把跨连接乱序误判为协议错误，也禁止永久保留迟到帧。
- `GetBigFile` 的 `data_id`、总长度和 SHA-256 必须从控制响应取得，数据队列只消费 `FilePart`/`Err`；不得等待数据面元数据首帧。
- API token 先做 SHA-256 固定长度摘要，再使用 `subtle` 常量时间原语比较；不要恢复手写比较循环。
- `RUST_LOG` 已配置时必须尊重运维值；未配置时才使用编译 profile 的默认级别，hardened 默认 INFO。
- 调试日志只能记录命令类型、帧类型、长度和关联状态；不得使用 `Debug` 输出完整 API 请求、认证帧、
  文件内容、Exec 命令、proof、nonce、session ID、token 或 secret。

## 测试要求

- 修改本地 API 后运行 `cargo check -p real_ctrl`，并优先补服务层单元测试。
- 修改 `real_ctrl` 控制/数据通道握手后，运行根目录 `scripts/e2e_ctrl_stack.ps1` 验证 pinned TLS、HTTP API 和 `ctrl_kik` 转发链路。
- 读循环中不要把 `framed_arc.lock().await.next()` 直接写进 `match` 条件；读锁应限制在局部块内，避免后续握手处理需要同一 reader 时隐式自锁。
