#!/usr/bin/env bash
set -uo pipefail

# 两个服务独立处理：其中一个尚未安装或重启失败时，仍继续处理另一个，最后统一返回状态。
services=(
  real-time-ctrl-production.service
  real-time-ctrl-gray.service
)

found=0
failed=0
for service in "${services[@]}"; do
  # 目标机可能使用不支持 `systemctl show --value` 的旧 systemd；解析稳定的 key=value 输出。
  load_state=$(systemctl show --property=LoadState "$service" 2>/dev/null || true)
  load_state=${load_state#LoadState=}
  if [[ "$load_state" == "not-found" || -z "$load_state" ]]; then
    printf 'skip: %s is not installed\n' "$service"
    continue
  fi

  found=$((found + 1))
  if ! sudo systemctl restart "$service"; then
    printf 'failed: could not restart %s\n' "$service" >&2
    failed=1
    continue
  fi
  if ! sudo systemctl is-active --quiet "$service"; then
    printf 'failed: %s is not active after restart\n' "$service" >&2
    failed=1
    continue
  fi
  printf 'active: %s\n' "$service"
done

if (( found == 0 )); then
  printf 'failed: no ctrl_server service is installed\n' >&2
  exit 1
fi
exit "$failed"
