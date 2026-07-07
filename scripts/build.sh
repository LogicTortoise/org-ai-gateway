#!/usr/bin/env bash
# 编译 OrgAI Gateway（release 模式）
# 只负责构建，不负责运行。启动用 scripts/start.sh，它会直接跑编译好的二进制。
set -euo pipefail

cd "$(dirname "$0")/.."

echo "Building OrgAI Gateway (cargo build --release) ..."
cargo build --release
echo "构建完成: target/release/org-ai-gateway"
