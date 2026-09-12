# real_time_ctrl

Windows 被控端、Windows 控制端与 Linux 中继服务组成的远程控制项目。当前协议为 v3：
`ctrl_kik` 使用 Noise NK，`real_ctrl` 使用 pinned TLS 1.3，控制与数据均不允许明文降级。

## 一键灰度发布

直接双击根目录 `publish_gray.cmd`，即可完成灰度构建、发布和公网验收。命令行等价入口：

```powershell
.\scripts\publish_stack.ps1 -Channel Gray
```

脚本会生成或复用身份、审计依赖、构建三个组件、发布 Linux 服务并完成公网验收。
`target/deploy/gray/artifacts/` 中的程序已经包含当前灰度环境的加密构建默认值，可直接双击：

- `real_ctrl.exe`：交互控制台；
- `real_ctrl_local_server.exe`：命名管道服务；
- `real_ctrl_invoker_http_service.exe`：HTTP 与命名管道组合服务；
- `ctrl_kik.exe`：被控端。

地址、端口、TLS PEM、SPKI pin、控制认证秘密、API token 和 Exec 策略由 `build.rs` 接收
`RTC_*_BUILD_*` 并以混淆密文写入发布产物；同名运行环境变量优先，可用于轮换和应急切换。
这种设计消除了 sidecar 和启动脚本，但不能阻止有能力的攻击者动态提取进程内秘密。

正式身份和直启产物可单独生成：

```powershell
.\scripts\generate_production_identity.ps1 -ServerAddress ytycc.com
.\scripts\build_hardened.ps1
```

生成后的产物位于 `target/prod/`。`ctrl_kik` 正式发布仍必须使用
`scripts/build_ctrl_kik_protected.ps1`，不能使用通用 hardened 产物。

## 固定端口

| 通道 | ctrl_kik Noise | real_ctrl TLS |
| --- | ---: | ---: |
| 灰度 | 9005 | 9009 |
| 正式 | 9002 | 9007 |

## 主要文档

- `生产架构与安全验收.md`：安全模型、API 和协议护栏；
- `生产发布与公网验收.md`：发布入口、远端目录和最新公网结果；
- `文件传输架构与性能.md`：分片、乱序、落盘和性能边界；
- `ctrl_kik/防逆向发布.md`：字符串保护、固定工具链与 PE 审计；
- `生产环境变量_IDEA.txt`：构建默认值、可覆盖项和秘密注入边界。

`start`、`screen_stream` 保持未修改；`ctrl_kik/src/screen` 只完成发布态字符串保护，捕获行为未重构。
