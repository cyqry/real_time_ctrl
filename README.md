# real_time_ctrl

Windows 被控端、Windows 控制端与 Linux 中继服务组成的远程控制项目，Android 客户端位于相邻的
`real_time_ctrl_app/`。当前协议为 v4：Kik 使用 Noise NK，PC/App 控制面使用 pinned TLS 1.3；认证失败不降级。

服务端支持多账号、多控制实例、账号 ACL 与有界命令并行。CLI 逐条请求/响应；HTTP API 最多并行
32 个请求，通过命令和数据关联 ID 隔离乱序响应。运行架构见 [生产架构与安全验收](生产架构与安全验收.md)。

## 使用与任务

```text
$sys_list
$local_use <kik_id>
$local_now
$run task_name
```

客户端首次启动从在线列表自动选一次；空列表时，在设备上线后手动使用 `$local_use`。
目标下线或客户端重连仍保留 ID，每条远端命令都明确携带 Kik ID，失败不会改发另一台。
HTTP/pipe 可在请求信封传入 `target_kik_id` 固定本次目标；PC 与 App 需配套升级。
任务配置、同步输出、异步启动与文件缓存见 [任务执行.md](任务执行.md)。

## 构建与发布

双击 `publish_gray.cmd` 或 `publish_production.cmd`；命令行入口：

```powershell
.\scripts\publish_stack.ps1 -Channel Gray
.\scripts\publish_stack.ps1 -Channel Production
# 仅构建、归档正式 PC 系列和 Kik
.\scripts\publish_stack.ps1 -Channel Production -BuildOnly -SkipCtrlServer
```

脚本复用通道身份，审计依赖，按选择构建，发布 Linux 服务并验收。组件跳过、仅部署、回退和工具链
要求见 [生产发布与公网验收](生产发布与公网验收.md)。已有正式身份位于 `deploy/production/identity/`，
日常发布不得重新生成；Android 使用 [独立生产构建入口](../real_time_ctrl_app/docs/一键生产构建.md)。

`deploy/<channel>/artifacts/` 为交付目录，Windows EXE 可直接启动：

| 产物 | 用途 |
| --- | --- |
| `real_ctrl.exe` | 交互控制台 |
| `real_ctrl_local_server.exe` | 命名管道服务 |
| `real_ctrl_invoker_http_service.exe` | HTTP 与命名管道组合服务 |
| `ctrl_kik.exe` | 被控端 |
| `ctrl_server` | Linux 中继服务，按 systemd 发布 |

| 通道 | Kik Noise | 控制端 TLS |
| --- | ---: | ---: |
| 灰度 | 9005 | 9009 |
| 正式 | 9002 | 9007 |

Kik 的端点和 Noise 公钥只在构建期确定；`LOCK_FILE_PATH` 唯一来源是
`common/config.json`，构建环境、发布脚本和运行环境均不能覆盖。Kik 必须使用 protected 构建入口。
PC/服务端支持运行时覆盖其构建默认值；秘密存放、轮换和产物边界见部署文档。

## 维护文档

- [Kik端程序规范.md](Kik端程序规范.md)：通用信息最小化、安全、产物和交付要求。
- [ctrl_kik/部署规范.md](ctrl_kik/部署规范.md)：本项目配置、目录、构建和验收入口。
- [ctrl_kik/防逆向发布.md](ctrl_kik/防逆向发布.md)：字符串保护、固定工具链与 PE 审计。
- [协议说明.md](协议说明.md)：连接角色、命令响应、关联 ID、数据面和本地 API。
- [并发与渗透测试计划.md](并发与渗透测试计划.md)：测试范围、压力模型、门禁和待人工覆盖项。
- [文件传输架构与性能.md](文件传输架构与性能.md)：分片、乱序、落盘、恢复与性能边界。
- [生产环境变量_IDEA.txt](生产环境变量_IDEA.txt)：构建默认值、覆盖项和秘密注入。

`target/` 可重建；身份、交付产物、回退和报告分别在 `deploy/`、`reports/`，不能依赖 `target/` 长期存在。
`start`、`screen_stream` 与 Kik 截屏捕获行为保持冻结；维护边界见 [AGENTS.md](AGENTS.md)。
