# real_ctrl

`real_ctrl` 是 Windows 控制端，三个入口共用同一套业务服务、关联路由和有界并发门禁：

- `real_ctrl.exe`：交互控制台；
- `real_ctrl_local_server.exe`：本地命名管道 API；
- `real_ctrl_invoker_http_service.exe`：本地 HTTP API 与命名管道 API。

灰度发布后可直接双击 `target/deploy/gray/artifacts/` 中对应的 EXE，不需要启动脚本、证书或
配置 sidecar。HTTP 配置已通过 `include_str!` 编入服务进程，锁文件默认放在系统临时目录。

服务端地址、TLS 端口、服务名、CA PEM、SPKI pin、日志级别、HTTP 监听、锁文件名、账号、实例、
控制认证秘密、API token 和 Exec 策略均有加密构建默认值；同名 `REAL_CTRL_*` 运行环境变量优先。
混淆用于阻止直接二进制搜索，不等同硬件密钥存储；默认秘密轮换后应重新构建全部关联组件。

常用命令：

- `$sys_list`：列出当前在线 Kik；
- `$sys_history [kik_id]`：查询最近上线、下线时间；
- `$sys_use <kik_id>`：显式切换当前 Kik；未选择时会自动使用最近上线且账号有权访问的在线 Kik；
- `$sys_now`：查询当前选择；
- `$ls <path>`：读取远端目录；
- `$getfile <remote> to <local>`：下载不超过 32 MiB 的小文件；
- `$getbigfile <remote> to <local>`：以 4 MiB 乱序分片下载不超过 1 GiB 的大文件；
- `$setfile <local> to <remote>`：上传文件；
- `$local_exit`：断开本地控制会话。

控制台输入循环保持串行。HTTP 和命名管道最多同时执行 32 个独立请求，每个请求用关联 ID、
oneshot 响应和私有数据队列隔离；达到上限才返回 `busy`，乱序响应不会串单。
同账号多实例应使用不同 `REAL_CTRL_HTTP_PORT` / `REAL_CTRL_HTTP_LOCK_PATH`；未显式配置
`REAL_CTRL_INSTANCE_ID` 时进程会生成随机 UUID，因此直接启动的多个实例不会互相替换。
网络控制面固定使用证书链、DNS 名称和 SPKI 三重校验的 TLS 1.3，不提供明文降级。
