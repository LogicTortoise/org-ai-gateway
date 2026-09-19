#!/usr/bin/env bash
# 重启 OrgAI Gateway（后台运行）。
#
# 先脱离当前调用会话启动 worker，再由 worker 停旧进程、启新进程。
# 这点很重要：Codex 本身依赖 Gateway 时，前台 shell 若先停 Gateway，
# 它所在的控制链可能随即断开，导致同一 shell 永远执行不到 start.sh。
# 默认直接用现有二进制重启（不编译）；改了代码想重编时加 -b。
# 用法:
#   scripts/restart.sh        后台重启，跑现有二进制（默认，不编译）
#   scripts/restart.sh -b     先 cargo build --release 再后台重启
set -euo pipefail

cd "$(dirname "$0")/.."

ROOT_DIR="$(pwd)"
LOG_DIR="${ROOT_DIR}/data"
RESTART_LOG="${LOG_DIR}/gateway.restart.log"
RESTART_ERR_LOG="${LOG_DIR}/gateway.restart.err.log"
LOCK_DIR="${LOG_DIR}/gateway.restart.lock"
LOCK_PID_FILE="${LOCK_DIR}/pid"
RESTART_SESSION_FILE="${LOG_DIR}/gateway.restart.session"

if [[ "${1:-}" == "--worker" ]]; then
  mkdir -p "$LOG_DIR"
  # A hard-killed worker cannot run its EXIT trap. Clear only a demonstrably
  # stale lock; a live worker keeps its PID in the lock directory.
  if [[ -d "$LOCK_DIR" ]]; then
    STALE_PID="$(tr -d '[:space:]' <"$LOCK_PID_FILE" 2>/dev/null || true)"
    if [[ ! "$STALE_PID" =~ ^[0-9]+$ ]] || ! kill -0 "$STALE_PID" 2>/dev/null; then
      rm -f "$LOCK_PID_FILE"
      rmdir "$LOCK_DIR" 2>/dev/null || true
    fi
  fi
  if ! mkdir "$LOCK_DIR" 2>/dev/null; then
    echo "已有 Gateway 重启 worker 在运行；拒绝启动第二个 worker。" >&2
    exit 1
  fi
  echo "$$" >"$LOCK_PID_FILE"
  trap 'rm -f "$LOCK_PID_FILE"; rmdir "$LOCK_DIR" 2>/dev/null || true' EXIT

  echo "[$(date '+%Y-%m-%d %H:%M:%S')] restart worker $$: stopping Gateway"
  ./scripts/stop.sh
  echo "[$(date '+%Y-%m-%d %H:%M:%S')] restart worker $$: starting Gateway"
  ./scripts/start.sh
  echo "[$(date '+%Y-%m-%d %H:%M:%S')] restart worker $$: Gateway healthy"
  exit 0
fi

case "${1:-}" in
  "" ) ;;
  -b|--build) ./scripts/build.sh ;;
  *)
    echo "用法: scripts/restart.sh [-b|--build]" >&2
    exit 2
    ;;
esac

mkdir -p "$LOG_DIR"
# `nohup ... &` is still a child of the command runner on some Codex desktop
# paths and can be killed when stopping this very Gateway disconnects that
# runner. A detached tmux server keeps the worker outside that process group.
# launchd cannot be used here: its GUI jobs lack permission to read this
# project's Documents directory on macOS.
if command -v tmux >/dev/null 2>&1; then
  RESTART_SESSION="org-ai-gateway-restart-$$-${RANDOM}"
  WORKER_COMMAND="$(printf '%q ' /bin/bash "${ROOT_DIR}/scripts/restart.sh" --worker)"
  tmux new-session -d -s "$RESTART_SESSION" \
    "${WORKER_COMMAND}>$(printf '%q' "$RESTART_LOG") 2>$(printf '%q' "$RESTART_ERR_LOG")"
  echo "$RESTART_SESSION" >"$RESTART_SESSION_FILE"
  echo "Gateway 重启 worker 已启动于 tmux 会话 (${RESTART_SESSION})。"
else
  nohup /bin/bash "$0" --worker >"$RESTART_LOG" 2>&1 < /dev/null &
  WORKER_PID=$!
  echo "Gateway 重启 worker 已启动 (PID: ${WORKER_PID})。"
fi

echo "  worker 日志: ${RESTART_LOG}"
echo "  worker 会先停止旧服务，再启动并等待 /health 返回 200。"
