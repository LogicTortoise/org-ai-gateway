# OrgAI Gateway Agent Instructions

## 服务重启安全

- 每次需要重启网关时，必须直接使用项目自带的一体化重启脚本：
  - 仅重启现有二进制：`./scripts/restart.sh`
  - 修改代码后构建并重启：`./scripts/restart.sh -b`
- 禁止先单独执行 `./scripts/stop.sh`，再执行 `./scripts/start.sh`。
- 原因：当前 Codex 会话本身依赖这个网关 API。若先单独停服，Codex 可能立即失去模型连接，无法继续执行后续的启动命令。
- 健康检查和状态读取是安全的；涉及服务生命周期变更时只能走 `restart.sh`。
