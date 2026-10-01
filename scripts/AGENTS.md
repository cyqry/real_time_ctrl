# 发布与验收脚本维护规则

## Kik 部署规范入口

- 构建、归档、保留或分发 Kik 时遵循 [Kik 端程序通用规范](../Kik端程序规范.md)，具体入口见 [ctrl_kik 部署适配说明](../ctrl_kik/部署规范.md)。
  修改脚本不得扩大 Kik 获知的远端信息、引入运行时配置覆盖或绕过对应产物审计。

## 文件边界

- `target/` 只放 Cargo 中间产物和一次性测试工作文件，必须允许用户随时整目录删除。
- 可复用部署身份、环境、直启 artifact 和通道报告保存在仓库 `deploy/`；本地机器可读验收报告保存在
  `reports/`。两者必须被根 `.gitignore` 忽略，敏感文件还必须收紧 ACL。
- 远端文件只允许写入当前通道目录：灰度 `/home/deploy/rust/gray/`，正式 `/home/deploy/rust/ctrl_server/`。
- 不读取或打印 SSH 配置、凭据、会话 token、控制面秘密、Noise 私钥或 TLS 私钥。

## 构建

- `ctrl_kik` 必须调用 `build_ctrl_kik_protected.ps1`，并检查 receipt 的 `production_ready=true`。
- Kik 单实例锁路径仅来自 `common/config.json`，发布脚本禁止覆盖。回执记录实际配置来源与锁路径；自动测试启动前检查锁文件位于项目内。
  本地多实例 E2E 使用独立测试源码副本的配置文件，原仓库配置不得为测试临时改写；测试副本不可混入发布 artifact。
- `real_ctrl` 和 `ctrl_server` 使用 `production` profile，保留 PDB/DWARF；服务端必须通过 `cross` 构建 musl 目标。
- 构建 ctrl_server 时必须先以 `docker info` 校验 Docker；跳过服务端时不要求 Docker，也不连接远端。
- 普通发布必须运行 `audit_production_dependencies.ps1`。Cargo.lock 中只属于冻结 `start` 或
  未启用可选 feature 的 advisory，必须先用实际生产依赖树证明不进入产物，禁止无条件忽略。
- `DeployOnly` 只用于已有产物的 unit/环境修订，不得伪装成一次完整生产构建。
- artifact 更新前必须验证路径位于仓库，只替换选中组件并保留回退副本。显式跳过的组件必须校验旧摘要，
  原文件保持不动、manifest 标记 retained 并保留原构建时间；禁止将旧包标记为本轮新构建。
  只归档二进制、调试符号和审计 receipt；不得混入运行启动器、sidecar 配置、证书或秘密环境文件。

## 密钥与部署

- 灰度端口固定为 Kik Noise `9005`、real_ctrl TLS `9009`；正式端口固定为 Kik Noise `9002`、real_ctrl TLS `9007`，发布脚本和报告禁止再用单一 `server_port` 混淆语义。
- 部署身份首次生成后保存在 `deploy/<channel>/identity/` 并复用；不得静默轮换，否则现有 ctrl_kik 公钥将失配。
- 控制秘密、TLS 私钥和 Noise 私钥只允许从权限收紧的 `deploy` 身份文件进入构建或部署环境；
  禁止出现在命令行、清单和标准输出。发布必须扫描产物，确保原始构建值不可明文搜索。
- 服务器发布必须把 `scripts/remote/reboot.sh` 安装为 `/home/deploy/rust/reboot.sh` 并设为 0750；
  脚本要独立尝试正式与灰度服务，不能因某个服务未安装而漏重启另一个。
- 远端替换必须先校验 SHA-256，保留上一版本并在启动失败时回滚。
- systemd 服务必须以非 root 部署用户运行；发布前预检证书可读、日志目录可写，避免依赖 root 的 DAC 绕过能力。

## 公网验收

- 至少验证错误 SPKI pin、HTTP token、Noise Kik 接入、系统命令、Kik 最近上下线状态、Exec 双层授权、小文件和大文件双向 SHA-256。
- 性能报告必须包含应用吞吐和独立链路基线，不能把公网带宽上限误报为协议瓶颈。
- 测试结束必须清理本地进程和远端临时基线文件；失败也必须落结构化报告。
- artifact 的无环境直启回归使用 `test_direct_artifacts.ps1`；HTTP 入口必须通过真实 health 请求证明
  已完成启动。多实例验收需使用不同锁路径/HTTP 端口，并验证同账号与不同账号的会话不会相互替换。

## 并发与渗透回归

- 统一入口为 `test_security_concurrency.ps1`；默认只测试仓库内本地全栈，只有显式传入
  `-IncludePublic -Channel Gray` 才允许触达公网灰度。不得把 Production 作为自动压力测试默认值。
- 本地突发需要同时超过 API 和单实例许可，验证超额请求快速拒绝、许可释放及关联 ID 隔离；公网突发
  必须保持有界，不能通过扩大连接数模拟拒绝服务。
- 畸形 HTTP、管道、TLS 和 Noise 探针之后必须再次执行健康检查与合法命令，单纯观察连接被关闭不算通过。
- 固定种子的协议随机测试属于发布门禁；新增网络解析入口时，必须同步纳入 `protocol_robustness` 测试。
- 测试载荷与日志只能写入 `target/` 的专用工作目录；结构化报告写入 `reports/` 或当前通道的
  `deploy/<channel>/reports`，失败路径也要生成报告并清理进程。测试计划的威胁假设、阈值和剩余风险维护在根目录 `并发与渗透测试计划.md`。
