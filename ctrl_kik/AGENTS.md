# ctrl_kik 维护规则

## 阶段二安全传输补充

- `ctrl_kik` 默认走 Noise NK，不读取服务端证书、服务名、SPKI pin 或其他服务端机器信息。
- 不要为了 `real_ctrl` 的 pinned TLS 管理面把 TLS 证书依赖引入 `ctrl_kik`；被控端只内置原始 X25519 公钥。
- Noise 持钥证明是阻断主动中间人读取的最低边界，禁止做成可选 feature 或接受任意公钥。

## 职责边界

`ctrl_kik` 是极小被控客户端。一个 `ctrl_kik` 实例可理解为一个被控用户/实例。

## 最小化原则

- 默认不做账号式身份校验。
- 不做账号、PKI、服务名或机器身份校验；固定 X25519 公钥只校验传输对端持钥，是防主动中间人读取所需的密码学最小信息。
- 不主动获取除服务端 IP/端口之外的服务端机器信息。
- 不引入复杂安全依赖，除非该能力明确可选并且不会扩大默认代码可见面。
- 持久状态只保留必要实例 id、服务端地址候选和运行配置。
- `LOCK_FILE_PATH()`、`PORT()`、`get_host()` 必须继续走编译期配置；不要为默认 `ctrl_kik` 增加锁路径、服务端地址或端口环境变量。
- 灰度/正式构建必须注入服务器 Noise X25519 公钥；运行时不得从环境、文件或网络替换该公钥。
- 命令与数据连接都必须使用 Noise NK，禁止自动回退明文。
- 需要多处复用或集中调整的非秘密字符串进入 `common/config.json`；一次性错误、响应、进程参数和线程名使用 `common::hidden!`。
- `hidden!` 的动态参数应与静态片段分开传入，禁止先用明文 `format!` 生成完整字符串再包裹。

## 命令安全

- `Exec(String)` 是高危能力，生产默认应被策略限制。
- 新增命令必须说明它是否访问文件系统、进程、屏幕、网络或凭据。
- 错误信息不要暴露不必要的本地路径和内部协议细节。
- `Exec` 默认编译并按普通远程命令执行，不再使用 Cargo feature 门禁；最终授权仍由 real_ctrl API 和 ctrl_server 管理面负责。
- 命令队列和数据队列必须有界；禁止用裸指针保存当前等待的数据 id，禁止恢复已删除的释放后解引用测试。
- 大文件先写系统临时目录中 CSPRNG 随机命名的 `.temp` 文件，校验 SHA-256 后再提交；传输失败不能提前截断原目标文件，所有失败路径都必须尽力清理临时文件。
- 大文件分片允许跨数据连接乱序到达；完全相同的地址范围作为重试幂等跳过，部分重叠、越界、长度不符和过量分片必须拒绝。
- 大文件下载必须通过 `ClientSuccessResp::BigFile(data_id, total, hash)` 在控制响应中返回元数据；响应成功写出后，使用最多 3 个在途任务把 `FilePart` 轮询发送到多条数据连接，禁止恢复数据面 `FileMeta`。
- 大文件源文件的大小、摘要和分片流必须复用同一已打开句柄；FilePart 和外层数据帧使用连续缓冲编码，复制峰值由 4 MiB 分片和 3 帧窗口约束。
- 大文件临时文件只允许打开和预分配一次，逐分片只做 seek/write，结束后统一 flush、fsync、SHA-256 和提交；同卷提交可原子替换，系统临时目录与目标跨卷时会退化为 copy+fsync+remove。
- Kik 初始化响应通道使用具体 `String` 类型，禁止恢复 `Box<dyn Any>` downcast。
- 单实例锁获取失败必须启动失败，不能 fail-open 继续运行第二个实例。

## 测试要求

- 修改连接或命令执行逻辑后运行 `cargo check -p ctrl_kik`。
- 修改主连接或数据连接读循环后，优先运行根目录 `scripts/e2e_ctrl_stack.ps1`，确认 `ctrl_kik` 仍能被 `real_ctrl` 通过 API 发现、选择和执行 `ctrl_ls`。
- 读循环中不要把 `framed_arc.lock().await.next()` 直接写进 `match` 条件；读锁应限制在局部块内，避免后续处理需要同一 reader 时隐式自锁。

## 受保护发布

- 正式发布只使用根目录 `scripts/build_ctrl_kik_protected.ps1`；通用 `build_hardened.ps1` 不负责构建 `ctrl_kik`。
- protected 构建必须使用 `--no-default-features`，保证 `ctrl_kik` 自身日志代码在编译期为空；开发和测试构建继续保留诊断能力。
- Rust 版本由 `rust-toolchain.toml` 和 `scripts/ctrl_kik_hardening.psd1` 双重固定到完整 commit hash；升级必须重跑二进制审计和 E2E。
- `.rs`、Cargo/源码绝对路径、PDB、panic 位置、应用模块路径、日志框架标记和 `dev_debug!` 静态文本是硬失败项；Rust 标准库和 Tokio 的公开运行时字符串只做计数回归，不能为了表面归零而私自分叉上游依赖。
- `common/config.json` 值和整个 `ctrl_kik` 项目发布态源码的可搜索明文是硬失败项；
  `screen` 不再豁免。标准库短词碰撞只允许在 receipt 中按已知运行时指纹单独计数。
- PE 必须同时通过 ASLR、NX、CFG（header、instrumented、FID table）、CET、无 RWX section 和无 CodeView/PDB 审计。
- protected 构建开始时必须先使旧产物和 receipt 失效；任何编译或审计失败后都不能残留可被误认作当前源码的 `production_ready=true` 记录。
- Authenticode 不属于当前 `ctrl_kik` 防逆向方案，也不是发布依赖；不得把完整性签名描述为代码混淆或不可逆向能力。
- 保护目标是降低静态信息泄漏和篡改风险，不得宣称能阻止确定性的反汇编、动态调试或内存提取，也不得在客户端内新增可被提取的授权秘密。
