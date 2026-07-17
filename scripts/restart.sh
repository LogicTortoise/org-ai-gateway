#!/usr/bin/env bash
# 重启 OrgAI Gateway（后台运行）
# 默认直接用现有二进制重启（不编译）；改了代码想重编时加 -b。
# 用法:
#   scripts/restart.sh        后台重启，跑现有二进制（默认，不编译）
#   scripts/restart.sh -b     先 cargo build --release 再后台重启
set -euo pipefail

cd "$(dirname "$0")/.."

case "${1:-}" in
  -b|--build) ./scripts/build.sh ;;
esac

./scripts/stop.sh
./scripts/start.sh
