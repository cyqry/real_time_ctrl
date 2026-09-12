# common 维护规则

## 阶段二安全传输补充

- `secure_transport` 是 `real_ctrl -> ctrl_server` 强身份传输入口；不要在业务 crate 中绕过它手写 TLS 连接。
- SPKI pin 指的是服务端叶子证书 SubjectPublicKeyInfo DER 的 SHA-256，不是整张证书文件 hash。
- 强安全模式缺少 CA 证书或 SPKI pin 时必须失败，不能自动回退明文。
- pinned TLS 客户端固定先发送 `RTCT v3` 非秘密前导，以兼容会复位非标准端口首包 TLS 的公网中间设备；此前导不是认证、授权或降级信号，后续仍必须完成 TLS 1.3、证书链、DNS 名称和 SPKI pin 校验。
- `ctrl_kik` 的全部控制/数据帧必须先经过 Noise NK 记录层；每条连接独立握手、严格递增 nonce，AEAD 失败立即关闭连接。
- Noise 单记录密文不得超过 65535 字节；大协议帧由流适配器自动分段，不得改变上层 4 MiB `FilePart` 契约。
- `session_auth` 负责 real_ctrl 会话 proof；不要在业务 crate 中复制 HMAC 拼接逻辑。
- 当前 `InitFrame` 只保留 v3 Kik、challenge/session 和会话数据通道帧；修改时同步升级传输版本并整体发布，不维护旧线协议分支。
- HMAC key 直接使用部署注入的高熵控制面秘密；禁止恢复历史静态摘要认证。
- 握手阶段使用 4 KiB 上限，角色认证完成后才能切换到控制/数据帧上限。

## 职责边界

`common` 承载跨 crate 复用的协议、连接、认证、文件和时间工具。这里的改动影响 `real_ctrl`、`ctrl_server`、`ctrl_kik`；当前协议不承诺兼容旧版本，发布时必须保证三端版本一致。

## 协议约束

- 解码外部输入前必须先检查剩余长度，不能让 `bytes::Buf` 的读取触发 panic。
- 长度前缀帧必须有最大帧长度。控制通道为 1 MiB，数据通道为约 64 MiB；大文件必须使用 4 MiB 分片，禁止恢复整文件单帧传输。
- 大文件下载元数据只允许通过 `ClientSuccessResp::BigFile` 随控制响应返回；数据通道只承载 `Dok::FilePart` 或 `Dok::Err`，不得恢复 `FileMeta` 数据首帧。
- 文件分片和外层数据帧使用连续缓冲编码。这里有意接受受上限约束的复制成本，以换取完整帧重试、协议审计和维护确定性；禁止恢复多切片向量写与原地改写转发的双实现。
- 大文件发送窗口最多同时保留 3 个分片。每帧在多条数据连接间轮询，写失败可在其他连接重试；连接写超时或 I/O 错误后必须标记关闭，不能继续复用可能残留半帧的 TCP 流。
- `FileRangeTracker` 允许完全相同地址范围的幂等重试，仍须拒绝部分重叠、越界、长度不符和过量分片；最终 SHA-256 是内容一致性边界。
- `ctrl_server` 必须完整解析并重新编码 CtrlData/KikData 数据帧，再在多条下游连接间背压转发；不得绕过帧边界、ID 长度和 UTF-8 校验。
- `BufSerializable::from_buf` 的失败语义是返回 `None`，除非调用方已经选择结构化错误类型。
- `transfer_encode` 只负责封帧，不负责认证、授权或加密。

## 安全约束

- 新传输安全能力应通过独立模块引入；Noise 是 `ctrl_kik` 默认安全边界，不得改成可选 feature 或自动明文降级。
- 任何涉及密钥、pin、token 的实现都要说明存储位置和生命周期。
- `build.rs` 生成代码只能写入 Cargo `OUT_DIR`。编译期字符串混淆不是秘密存储，不能放入控制凭据或 API token。
- 需要集中复用或调整的非秘密字符串统一维护在 `config.json`；nonce 必须绑定字段名和字段值，生成函数使用 `OnceLock` 缓存解密结果。
- `common` 会被受保护 Kik 静态链接：除 `#[cfg(test)]` 测试数据和编译期路径外，运行时字符串都必须来自 `config.json` 或 `common::hidden!`；静态片段不得重新放回 `format!`、`anyhow!`、断言消息或 `io::Error` 明文字面量。
- 字符串混淆 key 会进入二进制，只用于提高静态搜索成本，禁止复用于 TLS、认证、文件加密或业务数据保护。
- `Channel` 的写入、flush 和 shutdown 必须受超时控制；业务 crate 不应自行绕过这些方法裸写。
- 协议当前每帧都会 flush，`Channel` 不再套 `BufWriter`；若未来引入批量 flush，必须先证明不会增加命令延迟或破坏心跳时序。
- `Channel` 的连接 ID 使用 `Option<String>` 表达初始化状态，禁止恢复 `undefined_id` 一类哨兵值或在读取未初始化 ID 时 panic。
- 角色相关连接属性必须使用 `ChannelAttributeKey<T>` 和唯一非零数值 ID；禁止恢复字符串键或调用点手写 downcast。
- `common::Command` 只表示线上命令；本地生命周期命令保留在 `InputCommand`，不能重新塞进线协议枚举。
- 大文件接收临时文件必须由 `create_random_temp_file` 在系统临时目录以 CSPRNG 随机名和 `.temp` 后缀创建；同卷提交可原子替换，跨卷只能保证校验后 copy+fsync+remove，不得误写成始终原子。

## 测试要求

- 协议新增字段或修复解码分支时，补 round-trip 和畸形输入测试。
- 修改本 crate 后运行 `cargo test -p common`。
- 修改字符串生成或解密后，还要运行 `cargo test -p string_obfuscation_macros -p common` 和受保护二进制审计。
