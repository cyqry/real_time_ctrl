# ctrl_server 维护规则

## 阶段二安全传输补充

- `ctrl_server` 的 `real_ctrl` 管理面 TLS 端口由 `CTRL_SERVER_TLS_CERT` / `CTRL_SERVER_TLS_KEY` 启用。
- 明文端口存在是为了 `ctrl_kik` 和迁移兼容；生产控制面必须走 TLS 管理端口。
- 不要把 `ctrl_kik` 最小客户端假设和 `real_ctrl` 强身份管理面混成同一套认证要求。
- `CtrlData` 数据通道必须绑定当前 `CtrlSession`，并拒绝重复 `channel_nonce`。

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

## 测试要求

- 修改连接或分派逻辑后运行 `cargo check -p ctrl_server`。
