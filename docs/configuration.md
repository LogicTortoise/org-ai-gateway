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
| `GLM_DEFAULT_MODEL` | `glm-5.2` | **默认档**（裸 `glm` slug；独立项，不是 Claude tier） |
| `GLM_OPUS_MODEL` | `glm-5.2` | **opus 档**（`claude-opus-*`） |
| `GLM_SONNET_MODEL` | `glm-5.2` | **sonnet + haiku 档**（`claude-sonnet-*`、`claude-haiku-*`） |
| `GLM_FABLE_MODEL` | `glm-5.2` | **fable 档**（`claude-fable-*`） |
| `GLM_MODELS` | 内置目录 | 逗号分隔，覆盖 model 目录 |
| `GLM_TIMEOUT_SECS` | `600` | 超时（秒） |
| `GLM_PRIMARY_LIMIT_TOKENS` | 不限 | **5h 窗口 token 上限**（网关本地聚合用；不设 = 不参与撞墙预判，仅作 burn rate 观测） |
| `GLM_WEEKLY_LIMIT_TOKENS` | 不限 | **周窗口 token 上限**（同上） |

### Kimi（Moonshot）
| 变量 | 默认 | 说明 |
|---|---|---|
| `KIMI_BASE_URL` | 内置 Moonshot OpenAI 端点 | OpenAI 兼容端点前缀 |
| `KIMI_ANTHROPIC_BASE_URL` | `https://api.moonshot.cn/anthropic` | Anthropic 兼容端点前缀 |
| `KIMI_DEFAULT_MODEL` | `kimi-k2-0711-preview` | **默认档**（裸 `kimi` slug；独立项，不是 Claude tier） |
| `KIMI_OPUS_MODEL` | `kimi-k2-0711-preview` | **opus 档**（`claude-opus-*`） |
| `KIMI_SONNET_MODEL` | `kimi-k2-0711-preview` | **sonnet + haiku 档**（`claude-sonnet-*`、`claude-haiku-*`） |
| `KIMI_FABLE_MODEL` | `kimi-k2-0711-preview` | **fable 档**（`claude-fable-*`） |
| `KIMI_MODELS` | 内置目录 | 逗号分隔，覆盖 model 目录 |
| `KIMI_TIMEOUT_SECS` | `600` | 超时（秒） |
| `KIMI_PRIMARY_LIMIT_TOKENS` | 不限 | **5h 窗口 token 上限**（网关本地聚合用；不设 = 不参与撞墙预判，仅作 burn rate 观测） |
| `KIMI_WEEKLY_LIMIT_TOKENS` | 不限 | **周窗口 token 上限**（同上） |

### Trae（trae2anthropic sidecar）

Trae 不是直连的云端 API：Trae IDE 用的是私有的 `api/agent/v3` agent 协议（请求体 AES-256-GCM 加密），网关不直接实现它，而是把流量交给本机的 [`trae2anthropic`](https://github.com/ProjectEio/trae2api) sidecar，由它翻译成 Trae 协议。所以这里配的是 **sidecar 的地址**，不是 Trae 官方地址；具体的 Trae 登录态、多账号轮换、额度耗尽自动禁用都在 sidecar 自己的管理面板（`<base_url>/admin/`）里维护，网关看不到也不管。

| 变量 | 默认 | 说明 |
|---|---|---|
| `TRAE_BASE_URL` | `http://127.0.0.1:8788` | sidecar 地址；账号自带的 `base_url` 优先级更高 |
| `TRAE_DEFAULT_MODEL` | `minimax-m3` | **默认档**（裸 `trae` slug；独立项，不是 Claude tier） |
| `TRAE_OPUS_MODEL` | `minimax-m3` | **opus 档**（`claude-opus-*`） |
| `TRAE_SONNET_MODEL` | `minimax-m3` | **sonnet + haiku 档**（`claude-sonnet-*`、`claude-haiku-*`） |
| `TRAE_FABLE_MODEL` | `minimax-m3` | **fable 档**（`claude-fable-*`） |
| `TRAE_MODELS` | 内置目录 | 逗号分隔，覆盖 model 目录（设了就不再向 sidecar 拉活列表） |
| `TRAE_TIMEOUT_SECS` | `600` | 超时（秒） |

**只走 Claude slot。** sidecar 只暴露 Anthropic 形状的 `/v1/messages`，没有 OpenAI 兼容端点，因此 Trae 不能进 Codex 链（chains 校验会拒绝）。

**模型名必须显式带 `trae` 前缀**（`trae` / `trae/<model>` / `trae-<model>`）。Trae 转售的是各家原名（`gpt-5.4`、`kimi-k2.5`、`gemini-3.1-pro`…），裸名不会被识别成 Trae——否则会劫持 Codex / Kimi / GLM 的路由。裸 `trae` 走默认档，从 Claude 链降级过来的 `claude-*` 名字按 3 档改写到对应 `TRAE_*_MODEL`。

**API Key 是可选的**：sidecar 在管理面板没生成 key 之前 API 是开放的，此时账号留空即可；生成了 key 就填进去（网关同时发 `x-api-key` 和 `Authorization: Bearer`）。这里的 401 是 **sidecar 拒绝网关的 key**，不是 Trae 登录失效。

### MiniMax（开放平台，API Key）

直连 MiniMax 官方的 **Anthropic 兼容端点**（`/anthropic/v1/messages`）和 **OpenAI 兼容端点**（`/v1/text/chatcompletion_v2`），不需要 sidecar，只要一把 API Key。Anthropic 路径同时发 `Authorization: Bearer` 和 `x-api-key`，两种鉴权风格都能接住。

| 变量 | 默认 | 说明 |
|---|---|---|
| `MINIMAX_BASE_URL` | `https://api.minimaxi.com/v1` | **OpenAI 兼容端点**前缀（Codex 路径用）；账号自带的 `base_url` 优先级更高。 |
| `MINIMAX_ANTHROPIC_BASE_URL` | `https://api.minimaxi.com/anthropic` | **Anthropic 兼容端点**前缀（Claude 路径用）；账号自带的 `base_url_alt` 优先级更高。 |
| `MINIMAX_DEFAULT_MODEL` | `MiniMax-M3` | **默认档**（裸 `minimax` slug；独立项，不是 Claude tier） |
| `MINIMAX_OPUS_MODEL` | `MiniMax-M3` | **opus 档**（`claude-opus-*`） |
| `MINIMAX_SONNET_MODEL` | `MiniMax-M3` | **sonnet + haiku 档**（`claude-sonnet-*`、`claude-haiku-*`） |
| `MINIMAX_FABLE_MODEL` | `MiniMax-M3` | **fable 档**（`claude-fable-*`） |
| `MINIMAX_MODELS` | 内置目录 | 逗号分隔，覆盖 model 目录（MiniMax 的 Anthropic 面没有 `/models`，所以目录是静态的） |
| `MINIMAX_TIMEOUT_SECS` | `600` | 超时（秒） |
| `MINIMAX_PRIMARY_LIMIT_TOKENS` | 不限 | **5h 窗口 token 上限**（网关本地聚合用；不设 = 不参与撞墙预判，仅作 burn rate 观测） |
| `MINIMAX_WEEKLY_LIMIT_TOKENS` | 不限 | **周窗口 token 上限**（同上） |

**双协议都接：**
- **Claude 路径**（`/v1/messages`）：原样透传 Anthropic 形状 payload，tool_use 走原生通道完整保留。
- **Codex 路径**（`/v1/responses`）：通过 Responses↔Chat Completions 适配层转到 `/v1/text/chatcompletion_v2`（**注意不是标准的 `/v1/chat/completions`**，MiniMax 的 OpenAI 端用的是 v2 路径），`function_call`/`function_call_output` 块双向翻译、tool 调用完整保留。

**模型名大小写会自动修正**：MiniMax 的官方 id 是混合大小写（`MiniMax-M3`），客户端传小写 `minimax-m3` 会被 400；网关按内置目录把已知 id 的大小写还原回去。裸 `minimax` 走默认档，从 Claude 链降级过来的 `claude-*` 名字按 3 档改写到对应 `MINIMAX_*_MODEL`。

**老账号迁移提示**：早期版本 `MINIMAX_BASE_URL` 默认指向 Anthropic 端（`https://api.minimaxi.com/anthropic`）。新版本该 env 默认改回 OpenAI 端。如果你的账号还在用老的 Anthropic URL，请在 WebUI 把 `base_url` 改成 `https://api.minimaxi.com/anthropic`，**或者**导出 `MINIMAX_ANTHROPIC_BASE_URL=https://api.minimaxi.com/anthropic`（新 env 显式覆盖 Anthropic 端点），把 `MINIMAX_BASE_URL` 留默认。

### DeepSeek（开放平台，API Key）

直连 DeepSeek 官方的 **Anthropic 兼容端点**（`/anthropic/v1/messages`）和 **OpenAI 兼容端点**（`/v1/chat/completions`），一把 API Key 即可。

| 变量 | 默认 | 说明 |
|---|---|---|
| `DEEPSEEK_BASE_URL` | `https://api.deepseek.com/v1` | **OpenAI 兼容端点**前缀（Codex 路径用）；账号自带的 `base_url` 优先级更高。 |
| `DEEPSEEK_ANTHROPIC_BASE_URL` | `https://api.deepseek.com/anthropic` | **Anthropic 兼容端点**前缀（Claude 路径用）；账号自带的 `base_url_alt` 优先级更高。 |
| `DEEPSEEK_OPENAI_MODEL` | `deepseek-chat` | Codex / OpenAI 路径统一发送的上游模型 id。DeepSeek 两面发布 **不同**的 model id（Anthropic 面是 `deepseek-v4-pro`，OpenAI 面是 `deepseek-chat`/`deepseek-reasoner`），所以 OpenAI 路径把全部流量路由到这一个可配置 id，不做 tier 改写。要走 reasoner 就设成 `deepseek-reasoner`。 |
| `DEEPSEEK_DEFAULT_MODEL` | `deepseek-v4-pro` | Anthropic 路径的 **默认档**（裸 `deepseek` slug；独立项，不是 Claude tier） |
| `DEEPSEEK_OPUS_MODEL` | `deepseek-v4-pro` | Anthropic 路径的 **opus 档**（`claude-opus-*`） |
| `DEEPSEEK_SONNET_MODEL` | `deepseek-v4-pro` | Anthropic 路径的 **sonnet + haiku 档**（`claude-sonnet-*` 和 `claude-haiku-*` 共享一个上游目标） |
| `DEEPSEEK_FABLE_MODEL` | `deepseek-v4-flash` | Anthropic 路径的 **fable 档**（Claude Code 最便宜的 tier） |
| `DEEPSEEK_MODELS` | 内置目录 | 逗号分隔，覆盖 model 目录（Anthropic 路径的 id 目录） |
| `DEEPSEEK_TIMEOUT_SECS` | `600` | 超时（秒） |
| `DEEPSEEK_PRIMARY_LIMIT_TOKENS` | 不限 | **5h 窗口 token 上限**（网关本地聚合用；不设 = 不参与撞墙预判，仅作 burn rate 观测） |
| `DEEPSEEK_WEEKLY_LIMIT_TOKENS` | 不限 | **周窗口 token 上限**（同上） |

**双协议都接：**
- **Claude 路径**（`/v1/messages`）：原样透传 Anthropic 形状 payload，tool_use 走原生通道完整保留。
- **Codex 路径**（`/v1/responses`）：通过 Responses↔Chat Completions 适配层转到 `/v1/chat/completions`，`function_call`/`function_call_output` 块双向翻译、tool 调用完整保留。

**Anthropic 路径按 Claude Code 的 3 档 tier 改写**：`claude-opus-*` → `DEEPSEEK_OPUS_MODEL`；`claude-sonnet-*` 和 `claude-haiku-*`（`claude-haiku-4-5-*` / `claude-3-5-haiku-*` 两种写法都认）→ `DEEPSEEK_SONNET_MODEL`（**haiku 合并到 sonnet** —— 两个 tier 共享一个上游目标，因为大多数第三方厂商没有独立的 sonnet 变体）；`claude-fable-*` → `DEEPSEEK_FABLE_MODEL`；裸 `deepseek` slug 或其它未知名 → `DEEPSEEK_DEFAULT_MODEL`（独立项，不是 Claude tier）。想要 1M 上下文就把 `DEEPSEEK_OPUS_MODEL` / `DEEPSEEK_SONNET_MODEL` 设成 `deepseek-v4-pro[1m]`。

> `DEEPSEEK_MODELS` 的目录是 Anthropic 路径的 id 目录，是刻意的：DeepSeek 的 `GET /models` 返回的是它 **OpenAI 面** 的 id（`deepseek-chat` / `deepseek-reasoner`），不是 Anthropic 面文档里的 id，拉活列表只会让人选到随后被静默改写的模型名。

### Ollama（本地）
| 变量 | 默认 | 说明 |
|---|---|---|
| `OLLAMA_BASE_URL` | `http://127.0.0.1:11434` | Ollama 服务地址 |
| `OLLAMA_DEFAULT_MODEL` | `llama3` | **默认档**（裸 `ollama` slug；独立项，不是 Claude tier） |
| `OLLAMA_OPUS_MODEL` | `llama3` | **opus 档**（`claude-opus-*`） |
| `OLLAMA_SONNET_MODEL` | `llama3` | **sonnet + haiku 档**（`claude-sonnet-*`、`claude-haiku-*`） |
| `OLLAMA_FABLE_MODEL` | `llama3` | **fable 档**（`claude-fable-*`） |
| `OLLAMA_TIMEOUT_SECS` | `600` | 超时（秒） |

> GLM / Kimi / Trae / MiniMax / DeepSeek / Ollama 是否真正参与某个协议（Claude / Codex）的调度，取决于 **`data/provider_chains.json`** 里对应 slot 的 `providers` 列表和 `mode`，与这里的 env **无关**。env 只配"怎么连"，chains 配"要不要用、什么顺序"。详见下方。

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
- `providers`：该 slot 依次尝试的 provider 列表。Claude slot 合法值：`claude`/`glm`/`kimi`/`minimax`/`deepseek`/`trae`/`ollama`/`cursor`；Codex slot：`codex`/`glm`/`kimi`/`minimax`/`deepseek`/`ollama`/`cursor`（`trae` 只接了 Anthropic 形状的端点，不能进 Codex slot）。
- **注意**：链 `mode` 只管 **provider 之间**的轮换/降级；**同一 provider 的多个账号之间**的 round-robin 由 pool 选择器单独完成，与链 mode 无关。

例：想让 Kimi 当 Claude 的**降级账号**（claude 全耗尽才用 kimi），配：

```json
"claude": { "mode": "failover", "providers": ["claude", "kimi"] }
```

（用 `round_robin` 会让 claude 健康时也分流到 kimi，不是降级。）

Trae / MiniMax / DeepSeek 在 **Claude 链**上同理，且只能用 `failover`：

```json
"claude": { "mode": "failover", "providers": ["claude", "minimax", "deepseek", "trae"] }
```

顺序就是尝试顺序，在 WebUI 的「优先级链路」面板里用 ↑/↓ 直接拖，存的就是这个数组。

### 降级语义：额度降级 ≠ 失败降级

链上的 Kimi / GLM / MiniMax / DeepSeek / Trae 只承担**额度降级**——Claude 账号池的配额真的用尽（429 / 每日额度打满 / 全部 cooling）时，用便宜的上游把请求接住。

**它们不负责失败降级。** 上游的瞬时故障（Anthropic 的 529 `overloaded_error`、5xx、Cloudflare 挑战）不该把请求甩给另一个 provider，原因有两条：

1. **模型不等价**：529 只是上游一时过载，同一个 Claude 账号几十秒后就能正常服务。为此把请求降到 Kimi，是用回答质量换一次本来可以等到的重试。
2. **prompt cache 会全废**：换 provider（甚至只是换账号）意味着这段对话的缓存前缀完全失效。一个百万字符量级的 transcript 在缓存命中时只付一两千 `cache_creation` token，换人后要全量重建——请求体积暴涨十倍，正好撞在上游最优先拒绝的那一类上，失败概率反而更高。

所以瞬时故障的正解是**原账号退避重试**，实现在 `src/routes/proxy.rs` 的 `TRANSIENT_SAME_ACCOUNT_BACKOFF_SECS`（15s / 45s / 90s，每请求共享 3 次预算，期间用 `forced_account` 把请求钉在缓存热的那个账号上）。只有退避耗尽、或遇到 429 这类真正的额度信号，才轮到换账号 → 换 provider。

**结论**：不要为了"提高成功率"往 claude 链里加 kimi/glm。链是额度兜底，退避是故障兜底，两者不要混。

---

## 本地聚合窗口（GLM / Kimi / DeepSeek / MiniMax 的 5h + 周撞墙预判）

Codex / Claude / Cursor 都在响应头里告诉网关自己用了多少，dashboard 和选择器直接读这些头就够了。GLM / Kimi / DeepSeek / MiniMax 是直连的 API Key 端点，**不返回任何限流头**（DeepSeek、GLM 官方文档明确没有 `x-ratelimit-*` 之类的字段；MiniMax 的 Anthropic 兼容端点也只在 429 响应体里给一个 `error.code=2056` 的 "5h usage limit exceeded"，response header 全空）。所以这四家走 **网关本地按审计 token 自聚合 5h / 7d 用量**的路径，跟上游真正用的 RPM/TPM 滑动窗口不是一回事，但能抓住最常见的撞墙场景（"这个号被打满了"），并跟 Claude 用同一道 95 / 99 阈值决定选择器要不要主动绕开。

实现位置：`src/provider/usage_window.rs`。两路生产者各管自己的失效条件：

- **被动**（`pool/storage.rs::append_audit`）：每次审计写入后只 `invalidate_cache` 该账号的缓存项，不立即重算——避免每次请求都扫整个 audit 文件。
- **主动**（`capacity::run_capacity_maintenance` 每分钟一次；`usage::probe_one_account` 探测周期兜底）：把缓存填回去。

**配套 env**：每家一对：

| 变量 | 默认 | 说明 |
|---|---|---|
| `GLM_PRIMARY_LIMIT_TOKENS` / `GLM_WEEKLY_LIMIT_TOKENS` | 不限 | GLM 5h / 周窗口 token 上限 |
| `KIMI_PRIMARY_LIMIT_TOKENS` / `KIMI_WEEKLY_LIMIT_TOKENS` | 不限 | Kimi 同上 |
| `DEEPSEEK_PRIMARY_LIMIT_TOKENS` / `DEEPSEEK_WEEKLY_LIMIT_TOKENS` | 不限 | DeepSeek 同上 |
| `MINIMAX_PRIMARY_LIMIT_TOKENS` / `MINIMAX_WEEKLY_LIMIT_TOKENS` | 不限 | MiniMax 同上 |

**默认行为**：`0` / 未设 / 解析失败 → 该窗口 `used_percent = None`，dashboard 显示"不适用"，选择器不靠它做硬排除。

**兜底机制**：即使两 env 都不设，选择器仍会用"近 5h 限流错误次数 >= 5"（audit status 含 `rate_limit` 或 `429`）作为撞墙信号的兜底，避免那种"号明显被上游打 429 了但本地 percentage 还是 0%"的盲区。

**注意事项**：
- `recent_rate_limit_errors_5h` 不写进 capacity history（避免污染历史样本），只在实时 `RateLimitSnapshot` 里携带。
- 这条路径只用来"预判"撞墙；真的撞墙时仍然由 `retry.rs` 的现有逻辑（`looks_rate_limited` / `TRANSIENT_SAME_ACCOUNT_BACKOFF_SECS`）负责退避重试 + 换号，跟 Claude 完全一致。
- 上游 RPM/TPM 真实窗口和这里用的"过去 5h 总 token"不是一回事；用户配的上限要按自己账号的实际 quota 拍脑袋设（参考 `usage::tokens` 计费的真实 billable = `input_uncached + output`，不是 input 总量）。

---

## 相关：模型映射运行时覆盖（`data/provider_models.json`）

上面每个 provider 的 `*_DEFAULT_MODEL` / `*_OPUS_MODEL` / `*_SONNET_MODEL` / `*_FABLE_MODEL` / `*_MODELS` 这类 env，都可以在**运行时**被 `data/provider_models.json` 覆盖，不用改 env、不用重启。这是 WebUI「模型映射」面板保存的东西。

**优先级（从高到低）**：

```
data/provider_models.json（手改）  >  环境变量  >  代码内置常量
```

文件形状：provider 名 → 该 provider 的覆盖项，字段全部"空即未覆盖"：

```json
{
  "deepseek": {
    "default_model": "deepseek-v4-pro",
    "opus_model": "deepseek-v4-pro",
    "sonnet_model": "deepseek-v4-pro",
    "fable_model": "deepseek-v4-flash",
    "models": ["deepseek-v4-pro", "deepseek-v4-flash"]
  },
  "minimax": { "default_model": "minimax-m3" }
}
```

- `default_model` —— 默认档（裸 provider slug 和未知名兜底，**独立项**，不是 Claude tier）：裸 provider slug（如请求 `model: "kimi"`），以及经链路降级过来但未匹配到任何 Claude 档的模型名，都落到这一档。**每个 provider 都有这个独立项**。
- `opus_model` —— Opus 档（`claude-opus-*`）。**所有 provider 都声明这一档**；不是 DeepSeek 专属。
- `sonnet_model` —— Sonnet + Haiku 档（两个 tier 共享一个上游目标，因为大多数第三方厂商没有独立的 sonnet 变体）。**所有 provider 都声明这一档**。`haiku_model` 和旧名 `sonnet_haiku_model` 写入时仍被接受（serde alias 兼容老配置），新配置请用 `sonnet_model`。
- `fable_model` —— Fable 档（`claude-fable-*`，Claude Code 最便宜的 tier）。**所有 provider 都声明这一档**。
- `models` —— 整份模型清单覆盖。**非空即"钉死"**：GLM / Kimi / Trae / Ollama 平时会去上游 `GET /models` 拉清单，一旦这里填了内容（或对应 `*_MODELS` env 有值），就不再拉，直接用这份——否则刚填的清单会立刻被上游拉回的覆盖掉。
- 空字符串 / 空数组 = 未覆盖，回落到 env；某 provider 全部字段都空则整条记录不落盘，文件不积垃圾。

**读写方式**：

| 端点 | 权限 | 作用 |
|---|---|---|
| `GET /v1/provider/model-map` | 任意已识别调用方 | 每档的 `matches` / env 名 / 内置值 / 当前覆盖 / **实际生效值 + 来源标记**（`override` / `env` / `builtin` / `live`）。每个 provider 都返回 4 行（`default` / `opus` / `sonnet` / `fable`）。 |
| `PUT /v1/provider/model-map` | 仅 owner-trusted | 覆盖任意子集；未提及的 provider 保持原样；保存失败会回滚内存态 |
| `POST /v1/provider/model-map/test` | 仅 owner-trusted | 按**当前已保存**的映射真发一次 `max_tokens: 1` 的最小请求，回报 HTTP 状态、耗时、发出的模型、**上游实际应答的模型**（`parse_response_model`）、token 用量。`slot` 字段可传 `default` / `opus` / `sonnet`（亦接受简写 `haiku`，已合并到 sonnet 档） / `fable`。 |

改动**立即对下一个请求生效**（没有任何地方缓存解析结果），不需要重启——这是它和 env 最大的区别。

> 匹配**规则**（哪些客户端模型名落到哪一档）写死在 Rust 里（`src/provider/model_config.rs` 的 `PROVIDER_MODEL_SPECS`），面板只编辑各档的**目标模型**和清单，不编辑规则。
