# common 维护规则

## 阶段二安全传输补充

- `secure_transport` 是 `real_ctrl -> ctrl_server` 强身份传输入口；不要在业务 crate 中绕过它手写 TLS 连接。
- SPKI pin 指的是服务端叶子证书 SubjectPublicKeyInfo DER 的 SHA-256，不是整张证书文件 hash。
- 强安全模式缺少 CA 证书或 SPKI pin 时必须失败，不能自动回退明文。
- `session_auth` 负责 real_ctrl 会话 proof；不要在业务 crate 中复制 HMAC 拼接逻辑。
- 新增 `InitFrame` v2 帧时必须保证旧帧编码不变，避免破坏 `ctrl_kik`。
- v2 HMAC key 直接使用部署注入的高熵控制面秘密；历史 `auth_util` 摘要只允许明文迁移协议使用。
- 握手阶段使用 4 KiB 上限，角色认证完成后才能切换到控制/数据帧上限。

## 职责边界

`common` 承载跨 crate 复用的协议、连接、认证、文件和时间工具。这里的改动影响 `real_ctrl`、`ctrl_server`、`ctrl_kik`，必须保持向后兼容意识。

## 协议约束

- 解码外部输入前必须先检查剩余长度，不能让 `bytes::Buf` 的读取触发 panic。
- 长度前缀帧必须有最大帧长度。默认值为兼容旧文件传输而保守偏大，后续控制通道应单独收紧。
- `BufSerializable::from_buf` 的失败语义是返回 `None`，除非调用方已经选择结构化错误类型。
- `transfer_encode` 只负责封帧，不负责认证、授权或加密。

## 安全约束

- `auth_util` 里的静态摘要只能视为历史兼容逻辑，不能作为抗中间人的安全边界。
- 新传输安全能力应通过 feature 或独立模块引入，避免强行增加 `ctrl_kik` 默认依赖。
- 任何涉及密钥、pin、token 的实现都要说明存储位置和生命周期。
- `build.rs` 生成代码只能写入 Cargo `OUT_DIR`。编译期字符串混淆不是秘密存储，不能放入控制凭据或 API token。
- `Channel` 的写入、flush 和 shutdown 必须受超时控制；业务 crate 不应自行绕过这些方法裸写。
- `Channel` 的连接 ID 使用 `Option<String>` 表达初始化状态，禁止恢复 `undefined_id` 一类哨兵值或在读取未初始化 ID 时 panic。
- 角色相关连接属性必须使用 `ChannelAttributeKey<T>`；禁止恢复裸字符串键配合调用点手写 downcast。
- `common::Command` 只表示线上命令；本地生命周期命令保留在 `InputCommand`，不能重新塞进线协议枚举。

## 测试要求

- 协议新增字段或修复解码分支时，补 round-trip 和畸形输入测试。
- 修改本 crate 后运行 `cargo test -p common`。
