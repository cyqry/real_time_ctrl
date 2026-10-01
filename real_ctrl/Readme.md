# real_ctrl

远程线协议、文件数据面和本地开放 API 的完整语义说明见根目录 `协议说明.md`。

`real_ctrl` 是 Windows 控制端，三个入口共用同一套业务服务、关联路由和有界并发门禁：

- `real_ctrl.exe`：交互控制台；
- `real_ctrl_local_server.exe`：本地命名管道 API；
- `real_ctrl_invoker_http_service.exe`：本地 HTTP API 与命名管道 API。

灰度发布后可直接双击 `deploy/gray/artifacts/` 中对应的 EXE，不需要启动脚本、证书或
配置 sidecar。HTTP 配置已通过 `include_str!` 编入服务进程，锁文件默认放在系统临时目录。

服务端地址、TLS 端口、服务名、CA PEM、SPKI pin、日志级别、HTTP 监听、锁文件名、账号、实例、
控制认证秘密、API token 和 Exec 策略均有加密构建默认值；同名 `REAL_CTRL_*` 运行环境变量优先。
混淆用于阻止直接二进制搜索，不等同硬件密钥存储；默认秘密轮换后应重新构建全部关联组件。

常用命令：

- `$sys_list`：列出当前在线 Kik；
- `$sys_history [kik_id]`：查询最近上线、下线时间；
- `$local_use <kik_id>`：查询在线列表验证权限后，切换本进程的选择；
- `$local_now`：查看本地选择快照，不进行实时在线探测；
- `$ls <path>`：读取远端目录；
- `$getfile <remote> to <local>`：下载不超过 32 MiB 的小文件；
- `$getbigfile <remote> to <local>`：以 4 MiB 乱序分片下载不超过 1 GiB 的大文件；
- `$setfile <local> to <remote>`：上传文件；
- `$local_exit`：断开本地控制会话。

首次建立客户端上下文时查询一次在线列表，选择最近上线的 Kik；列表为空则保持未选。
此后设备上线、下线或控制连接重连都不会自动换机。已选设备离线仍保留 ID，远程操作由服务端拒绝，
不会回退到另一台在线设备。`$sys_use`、`$sys_now` 已移除，旧命令会明确报错。
同一进程的并发 `local_use` 按在线列表校验完成并写入状态的顺序生效，后一次覆盖前一次；
已有远端操作持有自己的目标副本，不受这种切换影响。每次选择响应返回本次选中的设备快照。

HTTP/管道分别使用 `local_use`（参数 `kik_id`）和 `local_now`。两者成功响应均为
`data.kind="local_now"`，`data.value` 保留 `Kik`/`None` 结构；`Kik` 仅代表选择时的设备快照。
Exec、目录、截图、上传下载及任务命令可在请求信封显式提供 `target_kik_id`：

```json
{"version":1,"request_id":"list-a","target_kik_id":"设备ID","command":{"kind":"ctrl_ls","path":"C:/"}}
```

该字段只固定本次操作，不改变其他调用者的本地选择；系统查询和本地命令不接受此字段。
省略时在操作入口快照当前选择，后续并发切换、上传散列或重连都不会改动本次目标。
没有本地选择也没有显式目标时，API 返回 `no_target`，CLI 提示先执行 `$local_use`。
远端操作一律使用 `TargetedCmd`，已发送请求断线后不会自动重放。

控制台输入循环保持串行。HTTP 和命名管道最多同时执行 32 个独立请求，每个请求用关联 ID、
oneshot 响应和私有数据队列隔离；达到上限才返回 `busy`，乱序响应不会串单。
同账号多实例应使用不同 `REAL_CTRL_HTTP_PORT` / `REAL_CTRL_HTTP_LOCK_PATH`；未显式配置
`REAL_CTRL_INSTANCE_ID` 时进程会生成随机 UUID，因此直接启动的多个实例不会互相替换。
网络控制面固定使用证书链、DNS 名称和 SPKI 三重校验的 TLS 1.3，不提供明文降级。
