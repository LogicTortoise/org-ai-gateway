# 配置与环境变量（SSOT）

> 本文件是 OrgAI Gateway **所有环境变量的单一事实源（Single Source of Truth）**。
> 新增/修改任何 env 变量后，请同步更新这里；README 里的表格只是常用项摘录，以本文为准。
> 所有变量在**进程启动时读取一次**（`OnceLock` 缓存），改动需**重启**才生效。

## 怎么设置

`scripts/start.sh` 用 `nohup` 启动，会继承父 shell 的环境变量。临时生效：

```bash
GATEWAY_OWNER_PROTECTION=on GATEWAY_HTTP_TIMEOUT_SECS=900 ./scripts/restart.sh -b
```

想**永久生效**：把 `export XXX=...` 写进 `scripts/start.sh`，或放到单独的 env 文件里 source。否则下次不带 env 重启就恢复默认。

---

## 服务 / 网络

| 变量 | 默认 | 说明 |
|---|---|---|
| `GATEWAY_BIND_ADDR` | 代码默认 `0.0.0.0:8080`；本机 `start.sh` 固定 `0.0.0.0:8088` | 监听地址 |
| `GATEWAY_HTTP_TIMEOUT_SECS` | `600` | 上游 HTTP 总超时（秒）；connect timeout 固定 10s |
| `CODEX_PROXY_URL` | 无 | codex 上游可选出站代理 URL |
| `CODEX_UPSTREAM_WS_URL` | 内置 Codex WS 端点 | codex WebSocket 上游地址覆盖 |

## 身份 / 权限

| 变量 | 默认 | 说明 |
|---|---|---|
| `GATEWAY_EDGE_SECRET` | 无（不信任头） | 可信边缘共享密钥；设置即启用 `X-Gateway-Auth` + `X-User-Id` 身份头信任 |
| `GATEWAY_ADMIN_USERS` | 无 | 逗号分隔的管理员 user_id。**未设 = 单租户**，所有人看全量统计；设了则非管理员只看自己的数据 |

## 配额 / 限流（只约束"借用他人账号"的用量，不限 owner 用自己的号）

| 变量 | 默认 | 说明 |
|---|---|---|
| `GATEWAY_USER_DAILY_TOKEN_LIMIT` | 不限 | 每用户每 UTC 日借用 billable token 上限（`0`/未设关闭） |
| `GATEWAY_USER_WEEKLY_TOKEN_LIMIT` | 不限 | 每用户滚动 7 天借用 token 上限 |
| `GATEWAY_USER_RPM_LIMIT` | 不限 | 每用户每分钟请求数（跨 provider） |

> 超额不是一刀切断，而是被限制只能走自己拥有的账号；该 provider 一个自有号都没有时才 429。

## owner 重度使用保护 —— **默认关闭**

账号是捐给团队公用的，所以这个"把共享号保留给 owner"的保护**默认不启用**，共享号对所有成员完全公用。仅作为可选旋钮存在。逻辑见 `src/pool/mod.rs` 的 `OwnerProtectionConfig` / `owner_needs_protection`。

| 变量 | 默认 | 说明 |
|---|---|---|
| `GATEWAY_OWNER_PROTECTION` | **关** | 总开关；`1`/`on`/`true`/`yes` 开启 |
| `GATEWAY_OWNER_PROTECT_USAGE_PERCENT` | `60` | 开启后：周窗口用量 **高于**此值才可能保护 |
| `GATEWAY_OWNER_PROTECT_OWNER_SHARE` | `0.5` | 开启后：owner 占近 7 天 billable token 比例 **高于**此值才保护（0~1） |

- 两个条件是**且**关系：只有"周窗口高 **且** owner 用了多数"时才把号留给 owner。
- 关闭（默认）时，非 owner 在 owner 重度使用下**仍可借号**，一个号限流后能正常 fallback 到同类的另一个号。
- 启动日志会打印当前策略（`owner-heavy-usage protection enabled/DISABLED`）。

## 请求 / 响应 / 审计

| 变量 | 默认 | 说明 |
|---|---|---|
| `GATEWAY_MAX_REQUEST_BYTES` | `67108864`（64 MiB） | 入站请求体上限 |
| `GATEWAY_MAX_RESPONSE_BYTES` | `268435456`（256 MiB） | 上游响应体上限（防响应炸弹） |
| `GATEWAY_AUDIT_ROTATE_BYTES` | `67108864`（64 MiB） | 审计日志轮转阈值；保留一代 `.1` |
| `GATEWAY_HEALTH_PROBE_SECS` | `120` | 账号健康探测周期（秒）；`0` 关闭 |

## 上游 Provider

### 通用
| 变量 | 默认 | 说明 |
|---|---|---|
| `CLAUDE_CONFIG_DIR` | `~/.claude` | 读取本机 Claude Code 登录态的目录（捐号时用） |
| `CURSOR_TIMEOUT_SECS` | `120` | Cursor 上游超时（秒） |

### GLM（智谱）
| 变量 | 默认 | 说明 |
|---|---|---|
| `GLM_BASE_URL` | 空（回落到账号 `base_url`） | OpenAI 兼容端点前缀 |
| `GLM_ANTHROPIC_BASE_URL` | 空（回落到账号 `base_url_alt`） | Anthropic 兼容端点前缀 |
| `GLM_DEFAULT_MODEL` | `glm-5.2` | 降级到 GLM 时的默认模型 |
| `GLM_MODELS` | 内置目录 | 逗号分隔，覆盖 model 目录 |
| `GLM_TIMEOUT_SECS` | `600` | 超时（秒） |

### Kimi（Moonshot）
| 变量 | 默认 | 说明 |
|---|---|---|
| `KIMI_BASE_URL` | 内置 Moonshot OpenAI 端点 | OpenAI 兼容端点前缀 |
| `KIMI_ANTHROPIC_BASE_URL` | `https://api.moonshot.cn/anthropic` | Anthropic 兼容端点前缀 |
| `KIMI_DEFAULT_MODEL` | `kimi-k2-0711-preview` | 降级到 Kimi 时的默认模型 |
| `KIMI_MODELS` | 内置目录 | 逗号分隔，覆盖 model 目录 |
| `KIMI_TIMEOUT_SECS` | `600` | 超时（秒） |

### Ollama（本地）
| 变量 | 默认 | 说明 |
|---|---|---|
| `OLLAMA_BASE_URL` | `http://127.0.0.1:11434` | Ollama 服务地址 |
| `OLLAMA_DEFAULT_MODEL` | `llama3` | 默认模型 |
| `OLLAMA_TIMEOUT_SECS` | `600` | 超时（秒） |

> GLM / Kimi / Ollama 是否真正参与某个协议（Claude / Codex）的调度，取决于 **`data/provider_chains.json`** 里对应 slot 的 `providers` 列表和 `mode`，与这里的 env **无关**。env 只配"怎么连"，chains 配"要不要用、什么顺序"。详见下方。

## 日志

| 变量 | 默认 | 说明 |
|---|---|---|
| `RUST_LOG` | 见 `main.rs` | tracing 日志过滤（如 `info`、`org_ai_gateway=debug`） |

## 系统变量（仅读取，无需手动设）

`HOME`、`USER`、`APPDATA` —— 用于定位本机凭据/配置路径。

---

## 相关：Provider 调度链（不是 env，但常一起配）

调度不由 env 控制，而由 `data/provider_chains.json` 决定，按**入站协议 slot**（`claude` / `codex`）各配一条链：

```json
{
  "codex":  { "mode": "failover",    "providers": ["codex"] },
  "claude": { "mode": "round_robin", "providers": ["claude"] }
}
```

- `mode`：
  - `failover` —— 永远从第一个 provider 开始，只有耗尽/失败才降到下一个（**降级**语义）。
  - `round_robin` —— 每个请求轮换**起始 provider**，分摊负载，再按序降级。
- `providers`：该 slot 依次尝试的 provider 列表。Claude slot 合法值：`claude`/`glm`/`kimi`/`ollama`/`cursor`；Codex slot：`codex`/`glm`/`kimi`/`ollama`/`cursor`。
- **注意**：链 `mode` 只管 **provider 之间**的轮换/降级；**同一 provider 的多个账号之间**的 round-robin 由 pool 选择器单独完成，与链 mode 无关。

例：想让 Kimi 当 Claude 的**降级账号**（claude 全耗尽才用 kimi），配：

```json
"claude": { "mode": "failover", "providers": ["claude", "kimi"] }
```

（用 `round_robin` 会让 claude 健康时也分流到 kimi，不是降级。）
