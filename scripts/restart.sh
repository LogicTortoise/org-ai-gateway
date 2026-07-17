#!/usr/bin/env bash
# 重启 OrgAI Gateway（后台运行）
# 默认先重新编译（cargo build --release）再重启——改了代码直接跑它即可。
# 用法:
#   scripts/restart.sh        重新编译 + 后台重启（默认）
#   scripts/restart.sh -n     跳过编译，直接用现有二进制重启
set -euo pipefail

cd "$(dirname "$0")/.."

SKIP_BUILD=0
case "${1:-}" in
  -n|--no-build|--skip-build) SKIP_BUILD=1 ;;
esac

if [[ "$SKIP_BUILD" == "0" ]]; then
  ./scripts/build.sh
fi

./scripts/stop.sh
./scripts/start.sh
