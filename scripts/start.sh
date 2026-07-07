#!/usr/bin/env bash
# 启动 OrgAI Gateway
# 端口固定为 8088（默认 8080 常被 BlueStack 等占用）。
# 可用环境变量覆盖，例如: GATEWAY_BIND_ADDR=0.0.0.0:9000 ./scripts/start.sh
set -euo pipefail

cd "$(dirname "$0")/.."

# 端口 / 监听地址：当前环境固定 8088
export GATEWAY_BIND_ADDR="${GATEWAY_BIND_ADDR:-0.0.0.0:8088}"

BIN="target/release/org-ai-gateway"

# 没有二进制才构建；日常启动直接跑已编译好的二进制，不再每次编译。
# 需要重新编译时手动执行: cargo build --release
if [[ ! -x "$BIN" ]]; then
  echo "未找到二进制，开始构建 (cargo build --release) ..."
  cargo build --release
fi

echo "Starting OrgAI Gateway on ${GATEWAY_BIND_ADDR} ..."
exec "$BIN"
