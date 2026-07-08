# real_time_ctrl 维护规则

## 阶段二安全传输补充

- `real_ctrl -> ctrl_server` 生产控制面默认 pinned TLS，不得在强安全模式下自动降级明文。
- `ctrl_kik` 默认不接入服务端身份校验，不读取服务端证书、服务名、SPKI pin 或其他服务端机器信息。
- `screen_stream` 仍不属于当前安全传输改造范围。

## 全局边界

- `screen_stream` 当前阶段不改动。需要远程屏幕流安全接入时，只通过它已有的 `AsyncRead/AsyncWrite` 边界适配。
- 生成或修改的项目文件必须留在本仓库内。Cargo 依赖、Rust 工具链缓存和全局依赖按系统默认路径处理。
- 代码注释优先使用中文。注释只解释安全边界、协议约束、复杂状态机和维护风险，避免解释显而易见的语句。
- 不把 Android 控制端纳入当前实施范围。协议设计可以保留扩展空间，但不写 Android 端代码。

## 安全模型

- `real_ctrl` 是强身份控制端，必须校验 `ctrl_server` 身份，目标是防中间人攻击。
- `ctrl_kik` 是极小被控客户端，不做账号式身份校验，不获取除服务端 IP/端口外的服务端机器信息。
- 如果 `ctrl_kik` 不校验服务端身份，就不能宣称它具备抗中间人能力。相关代码和文档必须如实表达这个边界。
- 真正的授权、会话隔离、审计和限流应放在 `ctrl_server` 与 `real_ctrl` 管理面，不能依赖隐藏在 `ctrl_kik` 内的秘密。

## 协议维护

- 任何新增外部输入都必须有最大长度、超时和错误路径。
- 自定义二进制协议的 `from_buf` 必须对空帧、短帧、长度不匹配返回 `None` 或结构化错误，不能 panic。
- 控制通道和数据通道的限制应逐步拆分。兼容旧文件传输前，不要把全局默认帧上限改得过小。
- 修改协议字段时，需要同步更新协议文档、测试和维护记录。

## 验证要求

- 修改 `common` 或 `ctrl_common` 后至少运行 `cargo test -p common -p ctrl_common`。
- 修改连接路径后至少运行对应 crate 的 `cargo check`。
- 单元测试需要生成文件时，优先放在仓库 `target` 目录下；不要依赖开发机私有盘符、已有 release 产物或外部临时目录。
- 如果测试失败，要在最终回复中说明失败命令和原因；不能假装通过。

## AGENTS.md 粒度

当前采用根级 + 核心 crate 级规则：

- 根级 `AGENTS.md`：项目边界、安全模型、通用验证要求。
- `common/AGENTS.md`：共享协议、连接、认证和通用工具约束。
- `ctrl_common/AGENTS.md`：控制端业务帧和响应模型约束。
- `real_ctrl/AGENTS.md`：控制端、本地 API、服务端身份校验约束。
- `ctrl_server/AGENTS.md`：服务端会话、转发、隔离和审计约束。
- `ctrl_kik/AGENTS.md`：极小被控客户端约束。

## 生产构建

- 生产发布优先使用 `--profile hardened`，不要只依赖默认 debug/release。
- `hardened` profile 用于符号剥离、fat LTO、单 codegen unit、panic abort；这只能提高逆向成本，不能替代协议安全和权限边界。

## 开放 API

- `real_ctrl` 的 HTTP 与新版命名管道 API 必须共用稳定契约，不要让协议层直接绕过服务层。
- 默认本地入口不能暴露公网地址；确需远程 HTTP 访问时必须显式配置 token 和原因。

## 端到端验收

- `ctrl_server`、`ctrl_kik`、`real_ctrl_invoker_http_service` 三实例端到端测试使用 `scripts/e2e_ctrl_stack.ps1`。
- 该脚本默认使用本地端口 `9002` / `19443` / `9000`；`ctrl_kik` 只使用编译期 `PORT()` 和 `LOCK_FILE_PATH()`，脚本不得通过环境变量覆盖它。
- E2E 会通过 HTTP API 验证 `health`、token 拒绝、`sys_list`、`sys_use`、`sys_now` 和 `ctrl_ls`，并在结束时清理三个进程。
- 修改连接读循环时，不要把 `framed_arc.lock().await.next()` 直接放进 `match` 条件里；如果 match 臂内还需要同一个 reader，会被临时值生命周期拖住形成隐式自锁。
