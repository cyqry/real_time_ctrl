# real_ctrl

`real_ctrl` 是 Windows 控制端，三个入口共用同一套业务服务和命令门禁：

- `real_ctrl.exe`：交互控制台；
- `real_ctrl_local_server.exe`：本地命名管道 API；
- `real_ctrl_invoker_http_service.exe`：本地 HTTP API 与命名管道 API。

灰度发布后可直接双击 `target/deploy/gray/artifacts/` 中对应的 EXE，不需要启动脚本、证书或
配置 sidecar。HTTP 配置已通过 `include_str!` 编入服务进程，锁文件默认放在系统临时目录。

服务端地址、TLS 端口、服务名、CA PEM、SPKI pin、日志级别、锁文件名、控制认证秘密、
API token 和 Exec 策略均有加密构建默认值；同名 `REAL_CTRL_*` 运行环境变量优先。
混淆用于阻止直接二进制搜索，不等同硬件密钥存储；默认秘密轮换后应重新构建全部关联组件。

常用命令：

- `$sys_list`：列出当前在线 Kik；
- `$sys_history [kik_id]`：查询最近上线、下线时间；
- `$sys_use <kik_id>`：选择当前 Kik；
- `$sys_now`：查询当前选择；
- `$ls <path>`：读取远端目录；
- `$getfile <remote> to <local>`：下载文件；
- `$setfile <local> to <remote>`：上传文件；
- `$local_exit`：断开本地控制会话。

HTTP 和命名管道共享单命令门禁；已有命令执行时稳定返回 `busy`，不会并发消费同一数据响应。
网络控制面固定使用证书链、DNS 名称和 SPKI 三重校验的 TLS 1.3，不提供明文降级。
