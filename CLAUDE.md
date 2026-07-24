# CLAUDE.md — OrgAI Gateway

组织内共享 AI 账号池网关（Rust / Axum 单二进制）。把成员捐出的 Codex / Claude / Cursor 等账号汇成共享池，按入站协议智能分发。项目全貌见 [README.md](README.md)。

## 配置 / 环境变量（SSOT）

**所有环境变量的单一事实源 → [`docs/configuration.md`](docs/configuration.md)**。改动或新增 env 变量务必同步该文件；README 的表格只是摘录。变量在进程启动时读取一次，改动需重启生效。

调度链（provider 顺序 / failover vs round_robin）由 `data/provider_chains.json` 控制，不是 env——说明也在上面那份 SSOT 里。

## 构建 / 启停

```bash
./scripts/build.sh          # cargo build --release
./scripts/start.sh          # 后台启动（跑现有二进制，不编译）；本机固定端口 8088
./scripts/restart.sh        # 重启（默认不编译）
./scripts/restart.sh -b     # 先 cargo build --release 再重启（改了代码用这个）
./scripts/stop.sh
```

日志：`data/gateway.out.log` / `data/gateway.err.log`。健康检查：`curl http://127.0.0.1:8088/health`。

## 代码地图

- `src/routes/proxy.rs` —— 请求代理执行器：按 chain 依次尝试 provider，处理降级 / 重试 / 审计。
- `src/provider/chains.rs` —— 调度链模型（`ChainSlot` / `ChainMode` / `ordered_attempts`）与持久化。
- `src/pool/mod.rs` —— 账号选择、可见性、共享闸门（share cap / 每日额度 / **owner 保护**）。
- `src/provider/{claude,codex,cursor,glm,kimi,ollama}.rs` —— 各上游实现。
- `src/quota.rs` —— 按用户 token 预算 / RPM 限流。
- `src/usage/` —— 用量账本、健康探测、容量预测。
- `data/` —— 运行时状态（`accounts.ndjson` / `audit.ndjson` / `capacity.ndjson` / `provider_chains.json`），已 gitignore。

## 约定

- 回复用简体中文。
- bugfix / 重构不为老代码做兼容保留。
