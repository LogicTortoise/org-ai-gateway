#!/usr/bin/env bash
# 停止 OrgAI Gateway
# 通过匹配二进制名结束进程。
set -euo pipefail

cd "$(dirname "$0")/.."

BIN_NAME="org-ai-gateway"
PID_FILE="data/gateway.pid"

gateway_pids() {
  # Prefer the PID file, but validate that it still belongs to this binary.
  # The fallback pattern uses `[t]arget` so pgrep cannot select itself.
  local pid command
  if [[ -r "$PID_FILE" ]]; then
    pid="$(tr -d '[:space:]' <"$PID_FILE")"
    if [[ "$pid" =~ ^[0-9]+$ ]] && kill -0 "$pid" 2>/dev/null; then
      command="$(ps -p "$pid" -o command= 2>/dev/null || true)"
      if [[ "$command" == *"target/release/${BIN_NAME}"* ]]; then
        echo "$pid"
        return
      fi
    fi
  fi
  pgrep -f "[t]arget/release/${BIN_NAME}" || true
}

PIDS="$(gateway_pids)"

if [[ -z "$PIDS" ]]; then
  echo "未发现运行中的 ${BIN_NAME} 进程。"
  rm -f "$PID_FILE"
  exit 0
fi

echo "停止 ${BIN_NAME} (PID: ${PIDS}) ..."
for PID in $PIDS; do
  kill "$PID"
done

# 等待优雅退出，最多 5 秒，未退出则强杀
for _ in 1 2 3 4 5; do
  sleep 1
  PIDS="$(gateway_pids)"
  [[ -z "$PIDS" ]] && { echo "已停止。"; rm -f "$PID_FILE"; exit 0; }
done

echo "进程未退出，强制结束 (kill -9) ..."
for PID in $PIDS; do
  kill -9 "$PID"
done
rm -f "$PID_FILE"
echo "已强制停止。"
