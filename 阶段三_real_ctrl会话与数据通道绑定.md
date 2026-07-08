# 阶段三：real_ctrl 会话与数据通道绑定

日期：2026-07-08

## 1. 阶段目标

本阶段在阶段二 pinned TLS 的基础上，移除 `real_ctrl` 默认链路对历史静态摘要过线的依赖：

- `real_ctrl` 控制通道使用 nonce challenge + HMAC-SHA256 proof。
- `ctrl_server` 校验 proof 后签发随机 `session_id`。
- `real_ctrl` 数据通道使用 `session_id + channel_nonce + HMAC proof` 绑定已认证控制会话。
- `ctrl_server` 记录已使用的数据通道 nonce，防止同一 session 下重放。
- 旧 `CtrlAuthReq` / `CtrlDataConnReq` 仅保留给显式明文兼容模式。
- `ctrl_kik` 协议不改变，仍保持最小客户端边界。

## 2. 新增协议帧

新增 `InitFrame` 变体：

```text
CtrlAuthStart(client_nonce)
CtrlAuthChallenge(server_nonce)
CtrlAuthProof { client_nonce, proof }
CtrlAuthSession(session_id)

CtrlDataSessionReq { session_id, channel_nonce, proof }
CtrlDataSessionReply(bool)
```

v2 新帧使用长度前缀字符串，解析时会拒绝短帧和尾随字节。旧帧保持原编码格式，避免破坏 `ctrl_kik` 和明文兼容链路。

## 3. 认证计算

公共实现位于 `common::session_auth`。

控制通道 proof：

```text
HMAC-SHA256(secret, "real_ctrl.auth.v2" || client_nonce || server_nonce)
```

数据通道 proof：

```text
HMAC-SHA256(secret, "real_ctrl.data.v2" || session_id || channel_nonce)
```

当前 `secret` 仍复用历史 `config.id.encrypt()` 的结果。它不再直接通过网络发送，但后续生产化仍建议替换成独立本地 secret 或控制端私钥。

## 4. 服务端状态

`ctrl_server::core::context::Context` 新增控制会话状态：

```text
CtrlSession {
  session_id,
  ctrl_channel_id,
  created_at,
  used_data_nonces,
}
```

行为：

- 新控制会话建立时替换旧控制连接，并清空旧数据通道。
- 控制连接删除时清空当前 session。
- 数据通道必须提供当前 session id。
- 每个 `channel_nonce` 在当前 session 下只能使用一次。

## 5. 当前边界

已完成：

- 默认 `real_ctrl` TLS 模式不再发送静态摘要。
- 数据通道绑定控制 session。
- 明文兼容模式仍可使用旧帧。
- `ctrl_kik` 未引入新依赖或服务端身份认知。

仍待后续：

- session 过期时间、最大数据通道数和最大命令并发数还未完整策略化。
- `Exec` 等高危命令还未按能力策略显式授权。
- 本地 pipe/HTTP API 还未统一到稳定 envelope 和 token/ACL。
- 证书和 pin 的生成/轮换工具还未实现。

## 6. 验收命令

```powershell
cargo test -p common -p ctrl_common
cargo check -p real_ctrl -p ctrl_kik -p ctrl_server
git diff -- screen_stream
```
