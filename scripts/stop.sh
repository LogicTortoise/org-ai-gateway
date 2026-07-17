#!/usr/bin/env bash
# 停止 OrgAI Gateway
# 通过匹配二进制名结束进程。
set -euo pipefail

cd "$(dirname "$0")/.."

BIN_NAME="org-ai-gateway"
PID_FILE="data/gateway.pid"

PIDS="$(pgrep -f "target/release/${BIN_NAME}" || true)"

if [[ -z "$PIDS" ]]; then
  echo "未发现运行中的 ${BIN_NAME} 进程。"
  rm -f "$PID_FILE"
  exit 0
fi

echo "停止 ${BIN_NAME} (PID: ${PIDS}) ..."
# shellcheck disable=SC2086
kill $PIDS

# 等待优雅退出，最多 5 秒，未退出则强杀
for _ in 1 2 3 4 5; do
  sleep 1
  PIDS="$(pgrep -f "target/release/${BIN_NAME}" || true)"
  [[ -z "$PIDS" ]] && { echo "已停止。"; rm -f "$PID_FILE"; exit 0; }
done

echo "进程未退出，强制结束 (kill -9) ..."
# shellcheck disable=SC2086
kill -9 $PIDS
rm -f "$PID_FILE"
echo "已强制停止。"
