# real_time_ctrl 维护规则

## 当前安全传输基线

- `real_ctrl -> ctrl_server` 生产控制面默认 pinned TLS，不得在强安全模式下自动降级明文。
- `ctrl_kik` 默认使用 Noise NK；不读取服务端证书、服务名、SPKI pin 或机器信息，只以内置原始 X25519 公钥验证对端持钥并加密传输。
- `screen_stream` 仍不属于当前安全传输改造范围。
- 发布脚本把控制面秘密、API token、TLS PEM 和 Noise 私钥作为受 ACL 保护的构建输入写入
  `real_ctrl` / `ctrl_server` 的加密字符串默认值；同名运行环境变量优先，便于应急轮换。秘密不得进入
  `common/config.json`、源码、日志、manifest 或命令行输出。
- 旧明文、静态摘要认证和同端口分流协议已经删除，不得重新增加兼容降级分支。

## 全局边界

- `start` 与 `screen_stream` 当前阶段冻结。`ctrl_kik/src/screen` 的捕获行为冻结，只允许把发布态
  运行文本迁移到 `hidden!`，不得借此重构捕获算法或线程模型。
- `screen_stream` 当前阶段不改动。需要远程屏幕流安全接入时，只通过它已有的 `AsyncRead/AsyncWrite` 边界适配。
- 生成或修改的项目文件必须留在本仓库内。Cargo 依赖、Rust 工具链缓存和全局依赖按系统默认路径处理。
- 代码注释优先使用中文。注释只解释安全边界、协议约束、复杂状态机和维护风险，避免解释显而易见的语句。
- 每个 Rust 源模块使用 `//!` 说明它在核心链路中的位置、输入输出和生命周期；关键共享状态、关联 ID、
  锁与资源许可使用 `///` 写明作用域和不变量。注释面向初次接触项目的维护者，但不能逐行翻译代码或描述
  尚未实现的行为；实现变化时必须同步修正注释。
- 不把 Android 控制端纳入当前实施范围。协议设计可以保留扩展空间，但不写 Android 端代码。

## 安全模型

- `real_ctrl` 是强身份控制端，必须校验 `ctrl_server` 身份，目标是防中间人攻击。
- `ctrl_kik` 是极小被控客户端，不做账号、PKI 或机器身份校验；除 IP/端口和固定原始公钥外不获取服务端信息。
- “主动中间人不可读取”至少需要验证服务端持有部署私钥；Noise NK 的固定公钥就是该密码学下限，不能退化为接受任意公钥的伪加密。
- 真正的授权、会话隔离、审计和限流应放在 `ctrl_server` 与 `real_ctrl` 管理面，不能依赖隐藏在 `ctrl_kik` 内的秘密。

## 协议维护

- 任何新增外部输入都必须有最大长度、超时和错误路径。
- `CmdOptions.timeout=false` 只表示使用约 4 小时的长任务时限，不代表永久等待；客户端、服务端和被控端的外层时限必须逐层留出收尾余量。
- 自定义二进制协议的 `from_buf` 必须对空帧、短帧、长度不匹配返回 `None` 或结构化错误，不能 panic。
- 控制通道和数据通道限制已经拆分：控制帧 1 MiB，数据帧约 64 MiB；大文件使用 4 MiB `FilePart`，不允许恢复 1 GiB 整文件帧。
- 修改协议字段时，需要同步更新根目录 `协议说明.md`、测试和维护记录。

## 验证要求

- 修改 `common` 或 `ctrl_common` 后至少运行 `cargo test -p common -p ctrl_common`。
- 修改连接路径后至少运行对应 crate 的 `cargo check`。
- 一般测试产物放在仓库 `target`；运行时大文件接收临时文件例外，必须放系统临时目录，使用随机文件名和 `.temp` 后缀，并在所有失败路径清理。
- 如果测试失败，要在最终回复中说明失败命令和原因；不能假装通过。

## AGENTS.md 粒度

当前采用根级 + 核心 crate 级规则：

- 根级 `AGENTS.md`：项目边界、安全模型、通用验证要求。
- `common/AGENTS.md`：共享协议、连接、认证和通用工具约束。
- `ctrl_common/AGENTS.md`：控制端业务帧和响应模型约束。
- `real_ctrl/AGENTS.md`：控制端、本地 API、服务端身份校验约束。
- `ctrl_server/AGENTS.md`：服务端会话、转发、隔离和审计约束。
- `ctrl_kik/AGENTS.md`：极小被控客户端约束。
- `ctrl_kik/src/screen/AGENTS.md`：截屏后端扩展、线程所有权、会话生命周期和测试产物约束。
- `string_obfuscation_macros/AGENTS.md`：构建期字符串加密、宏展开和运行时解密边界。

## 生产构建

- 根 `Cargo.lock` 属于应用发布输入，必须纳入版本控制；生产脚本统一使用 `--locked`，禁止发布时静默漂移依赖。
- 生产和本地测试都必须拆分监听端口：灰度 Kik Noise `9005` / real_ctrl TLS `9009`，正式 Kik Noise `9002` / real_ctrl TLS `9007`；相同端口必须启动失败。
- `hardened` 是 fat LTO、单 codegen unit 与 panic abort 的加固基线；`production` 继承该基线但
  保留 PDB/DWARF。加固只能提高逆向成本，不能替代协议安全和权限边界。
- `real_ctrl` / `ctrl_server` 使用 `scripts/build_hardened.ps1` 的 `production` profile，保留运行诊断
  和性能观测能力；`ctrl_kik` 才使用剥离符号的 protected hardened 流水线。
- 地址、端口、TLS 身份、pin、控制认证秘密、API token、Noise 私钥和 Exec 策略由各 crate
  `build.rs` 提供可覆盖默认值，发布时使用 `RTC_*_BUILD_*` 注入；运行时同名业务环境变量优先。
  编译默认值必须经 `hidden!(env!(...))` 加密，且发布脚本必须审计原始值不以明文存在于产物。
- 完整发布必须通过 `scripts/audit_production_dependencies.ps1`；审计脚本需要先证明整仓
  Cargo.lock 中受豁免条目不在生产活动依赖树，再对其他 RustSec 漏洞和警告实行零容忍。
- `ctrl_kik` 只能使用 `scripts/build_ctrl_kik_protected.ps1` 发布；它使用独立固定 Rust 提交、编译标准库、剔除日志、审计源码痕迹和 PE 缓解属性，不能混入通用构建入口。
- `ctrl_kik` 非复用运行文本必须使用 `common::hidden!`；复用或需要集中调整的非秘密字符串必须进入 `common/config.json`。
- `common/config.json` 和 `hidden!` 只提高静态搜索成本，不构成秘密保险箱；授权秘密、token 和私钥
  只能按进程最小化进入必须持有它们的 `real_ctrl` / `ctrl_server`，绝不进入 `ctrl_kik`。
- `ctrl_kik` 不把 Authenticode 作为防逆向或生产就绪门禁；`production_ready` 由全项目发布态明文、
  源码/日志痕迹与 PE 缓解审计共同决定，`ctrl_kik/src/screen` 不再享有明文豁免。
- `ctrl_kik` 默认编译并执行 `Exec`，与其他远程命令使用同一分派路径；管理面仍由 `REAL_CTRL_API_ALLOW_EXEC=1` 和 `CTRL_SERVER_ALLOW_EXEC=1` 两层显式授权。
- 灰度/正式 `ctrl_kik` 必须通过构建变量 `RTC_CTRL_KIK_NOISE_SERVER_PUBLIC_KEY` 注入 Noise 公钥；
  对应私钥只允许进入 `ctrl_server` 的编译默认值和运行时覆盖，不得进入客户端。
- 发布 artifact 中的 EXE 必须可直接双击运行，不依赖启动脚本、证书或配置 sidecar。每次构建先清空
  旧 artifact 目录；敏感身份文件只留在仓库 `target/deploy/<channel>/identity`，不得进入 artifact manifest。
- 每次服务器发布都要安装 `/home/deploy/rust/reboot.sh`；它必须分别重启正式和灰度服务，未安装的
  通道可跳过，但一个通道失败不能阻止另一个通道被尝试。

## 开放 API

- `real_ctrl` 的 HTTP 与新版命名管道 API 必须共用稳定契约，不要让协议层直接绕过服务层。
- 默认本地入口不能暴露公网地址；确需远程 HTTP 访问时必须显式配置 token 和原因。
- 控制台保持逐条请求/响应；HTTP 与命名管道通过关联 ID 路由并发响应。每个请求必须持有有界许可，
  独立拥有响应 oneshot 和数据 ID 队列，禁止恢复共享 Receiver 或全局串行门禁。
- 服务端按账号、实例、Kik 和全局四级配额限制命令并发；同账号不同实例、不同账号不得相互替换会话，
  同账号同实例重连只替换自身旧会话。
- 每个控制会话没有当前目标时自动选择最近上线且 ACL 允许的在线 Kik；自动选择不得覆盖显式
  `sys_use`，当前目标完整下线时需要切换到仍在线的合法候选。

## 端到端验收

- `ctrl_server`、`ctrl_kik` 与多个 `real_ctrl_invoker_http_service` 实例的端到端测试使用 `scripts/e2e_ctrl_stack.ps1`。
- 该脚本默认使用本地端口 `9002` / `19443` / `9000`；脚本通过 Cargo 构建期变量生成测试专用 `ctrl_kik`，运行时仍不得覆盖端点或 Noise 公钥。
- E2E 会通过 HTTP API 验证 `health`、token 拒绝、Kik 自动选择、`sys_list`、`sys_history`、`sys_use`、`sys_now` 和 `ctrl_ls`，并在结束时清理全部测试进程。
- E2E 还必须验证错误 SPKI pin、端口角色隔离、Kik/KikData 只能通过 Noise NK 接入，以及真实 Kik 退出后记录最近下线时间。
- E2E 必须同时验证同账号多实例、多账号多实例和同一 HTTP 进程的命令乱序响应隔离；用有耗时的
  独立命令证明请求确实并行，不能只验证多个请求最终都成功。
- 修改连接读循环时，不要把 `framed_arc.lock().await.next()` 直接放进 `match` 条件里；如果 match 臂内还需要同一个 reader，会被临时值生命周期拖住形成隐式自锁。
