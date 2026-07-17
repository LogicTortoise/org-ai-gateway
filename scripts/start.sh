#!/usr/bin/env bash
# 启动 OrgAI Gateway（后台运行）
# 端口固定为 8088（默认 8080 常被 BlueStack 等占用）。
# 可用环境变量覆盖，例如: GATEWAY_BIND_ADDR=0.0.0.0:9000 ./scripts/start.sh
set -euo pipefail

cd "$(dirname "$0")/.."

# 端口 / 监听地址：当前环境固定 8088
export GATEWAY_BIND_ADDR="${GATEWAY_BIND_ADDR:-0.0.0.0:8088}"

BIN="target/release/org-ai-gateway"
LOG_DIR="data"
OUT_LOG="${LOG_DIR}/gateway.out.log"
ERR_LOG="${LOG_DIR}/gateway.err.log"
PID_FILE="${LOG_DIR}/gateway.pid"

# 已在运行则不重复启动（重启请用 scripts/restart.sh）
if RUNNING="$(pgrep -f "${BIN}" || true)"; [[ -n "$RUNNING" ]]; then
  echo "OrgAI Gateway 已在运行 (PID: $(echo "$RUNNING" | tr '\n' ' '))。"
  echo "如需重启，请用: scripts/restart.sh"
  exit 0
fi

# 没有二进制才构建；日常启动直接跑已编译好的二进制，不再每次编译。
# 需要重新编译时执行: scripts/build.sh（或 scripts/restart.sh，默认会重编）
if [[ ! -x "$BIN" ]]; then
  echo "未找到二进制，开始构建 (cargo build --release) ..."
  cargo build --release
fi

mkdir -p "$LOG_DIR"
echo "Starting OrgAI Gateway on ${GATEWAY_BIND_ADDR} (后台) ..."
nohup "$BIN" >>"$OUT_LOG" 2>>"$ERR_LOG" &
PID=$!
echo "$PID" >"$PID_FILE"

# 确认进程存活（启动失败时给出日志路径）
sleep 1
if kill -0 "$PID" 2>/dev/null; then
  echo "已启动 (PID: ${PID})。"
  echo "  日志: ${OUT_LOG} / ${ERR_LOG}"
  echo "  健康检查: curl http://127.0.0.1:${GATEWAY_BIND_ADDR##*:}/health"
else
  echo "启动失败，请查看日志: ${ERR_LOG}" >&2
  exit 1
fi
