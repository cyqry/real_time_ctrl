# ctrl_server 维护规则

## 阶段二安全传输补充

- `ctrl_server` 的 `real_ctrl` 管理面 TLS 端口由 `CTRL_SERVER_TLS_CERT` / `CTRL_SERVER_TLS_KEY` 启用。
- 明文端口存在是为了 `ctrl_kik` 和迁移兼容；生产控制面必须走 TLS 管理端口。
- 不要把 `ctrl_kik` 最小客户端假设和 `real_ctrl` 强身份管理面混成同一套认证要求。
- `CtrlData` 数据通道必须绑定当前 `CtrlSession`，并拒绝重复 `channel_nonce`。
- TLS 管理端口只接受 `Ctrl` / `CtrlData`，明文端口默认只接受 `Kik` / `KikData`。
- 替换控制连接必须原子切换当前连接并关闭旧数据通道；旧连接的 inactive 回调不得清除新会话。

## 职责边界

`ctrl_server` 是中继与授权边界。它负责管理 `real_ctrl` 控制会话、`ctrl_kik` 在线状态、数据通道绑定、限流和审计。

## 安全约束

- 服务端必须把 `real_ctrl` 管理面视为强认证入口。
- `ctrl_kik` 的实例 id 只用于重连和状态管理，不是安全凭据。
- 未认证或过期的 `real_ctrl` 会话不能创建数据通道或控制任何 `ctrl_kik`。
- 所有连接都需要帧大小限制、读超时、心跳清理和异常路径日志。

## 实现约束

- 不要在读循环里执行长耗时业务，避免阻塞心跳和其他连接处理。
- 转发大数据时保持背压意识，避免无限队列。
- 新增连接状态时，必须明确 inactive 清理路径。
- 服务端读循环如果读取后还要调整 `FramedRead` decoder，必须先把 reader 锁限制在独立代码块内释放，不能在 `match timeout(... lock().await.next())` 的臂里再次锁同一个 reader。
- 活动 TCP/TLS 连接总量默认上限 512，控制数据通道上限 8，单个 Kik 数据通道上限 4；不得改回无界任务或通道。
- 控制会话默认 12 小时过期，单会话最多记录 1024 个数据 nonce；会话失效时必须关闭其数据通道。
- 数据转发必须在读循环中等待下游写入形成背压，禁止为每个大帧无界 `tokio::spawn`。
- 当前控制连接、数据连接和 `CtrlSession` 必须共同存放在单个 `CtrlState` 锁内原子替换；禁止重新拆成互相独立的锁。
- 已认证帧处理按 `handler/read_handle/{control,data,kik,init}.rs` 分责；新增认证状态只进入 `init`，不要重新堆回单文件。
- 在线 Kik 表或当前选择锁内禁止等待网络关闭或内部连接锁；先克隆/移出共享句柄，再执行异步 I/O。
- 非 debug Cargo profile 的编译期默认日志级别为 INFO；显式 `RUST_LOG` 优先。

## 测试要求

- 修改连接或分派逻辑后运行 `cargo check -p ctrl_server`。
