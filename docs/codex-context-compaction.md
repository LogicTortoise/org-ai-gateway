# Codex CLI 自动压缩阈值（`context_compacted` 触发点）

> 状态：参考手册  
> 适用：Codex CLI 0.147+ 通过 org-ai-gateway 转发到任何上游 provider

## 一句话答案

Codex 在 **input_tokens ≥ `effective_context_window_percent% × context_window`** 时自动压缩会话历史（写 `event_msg.type=context_compacted` + `response_item.type=compacted`）。对当前所有 gpt-5.x 模型，这个百分位是 **95%**，触发后保留 `truncation_policy.limit = 10000` tokens 的滚动摘要。

## 配置项（按优先级）

| 字段 | 位置 | 类型 | 默认 | 作用 |
|---|---|---|---|---|
| `model_auto_compact_token_limit` | `~/.codex/config.toml` | int | `null`（用 catalog 兜底） | **直接覆盖触发阈值**，单位 token |
| `model_auto_compact_token_limit_scope` | `~/.codex/config.toml` | enum | `null` | `model` / `user` / `account` 作用域 |
| `model_context_window` | `~/.codex/config.toml` | int | `null`（用 catalog） | 把"有效窗口"调小，等价于提前触发 |
| `effective_context_window_percent` | 模型 catalog（`~/.codex/models_cache.json`） | int | **95** | 兜底：catalog 写死的百分比阈值，**用户不可改** |
| `context_window` | 模型 catalog | int | 272000（gpt-5.x） | 模型原始上下文窗口 |
| `truncation_policy.limit` | 模型 catalog | int | 10000 | 压缩**后**保留的 token 数 |

CLI 覆盖：
```bash
codex -c model_auto_compact_token_limit=200000 exec "..."
# 或临时一次性覆盖
codex -c model_context_window=250000 exec "..."
```

## 触发计算

```
effective_window = context_window × effective_context_window_percent / 100
                 = 272000 × 95 / 100
                 = 258400 tokens  ← 这就是 rollout 里 token_count 报告的 model_context_window
```

模型 catalog 字段值（来自 `~/.codex/models_cache.json`，client_version=0.147.0）：

| 模型 | context_window | max_context_window | effective_context_window_percent | auto_compact_token_limit | truncation_policy |
|---|---|---|---|---|---|
| gpt-5.5 | 272000 | 272000 | 95 | null | `{mode:"tokens", limit:10000}` |
| gpt-5.4 | 272000 | **1000000** | 95 | null | `{mode:"tokens", limit:10000}` |
| gpt-5.4-mini | 272000 | 272000 | 95 | null | `{mode:"tokens", limit:10000}` |
| gpt-5.6-terra | 272000 | 272000 | 95 | null | `{mode:"tokens", limit:10000}` |
| gpt-5.6-luna | 272000 | 272000 | 95 | null | `{mode:"tokens", limit:10000}` |

注意 `gpt-5.4` 的 `max_context_window=1000000`——这是配置最大能力，但**实际触发压缩仍然按 272000 × 95% = 258400**，多出来的 728k tokens 是 pre-compaction 余量（用来给压缩后留出足够空间继续工作）。

## 触发后的 token 行为

`compaction` 事件（response_item）会把历史替换成一段 `encrypted_content`（加密的压缩摘要），压缩**后**下一次请求的 `last_token_usage.total_tokens` 落到 **7k - 40k** 区间（中位 ~25k，远低于 258400），给后续对话留出 ~230k 余量。

## rollout 字段对照

Codex CLI 本地 `~/.codex/sessions/.../rollout-*.jsonl` 里的 `token_count` 事件，`info.model_context_window` 报的就是**已经乘了百分比后的 effective_window**（258400），不是模型 catalog 里的原始 `context_window`（272000）。需要原始窗口去查 `models_cache.json`。

## 不要做的事

- **不要靠 `model_context_window_usage_bar` 的进度条估算"还有多少空间"**——它在压缩前那一刻会从 ~90% 直接归零到 ~10%（因为总上下文被替换成 25k 的滚动摘要），不是 bug
- **不要把 95% 当成"minimax/claude 这种上游的 context 限制"**——这是 Codex 客户端策略，跟上游无关；上游 provider 在 258400 之内随便跑，超过才会自己 400
- **不要以为 `max_context_window=1000000` 能用**——它是 catalog 元数据，**`effective_context_window_percent` 是硬上限**，gpt-5.4 仍然在 258400 就压

## 反推数据（仅供交叉验证）

`~/.codex/sessions/` 下 130 次 `context_compacted` 事件（4 月-8 月期间）：

| window | 触发时 input_tokens (min/med/max) | input / window (med) | 样本数 |
|---|---|---|---|
| 258400（gpt-5.4/5.5） | 206055 / 229473 / 244518 | **88.8%** | 122 |
| 353400（少数 session） | 249983 / 312943 / 332392 | **88.6%** | 8 |

中位 88-89% < 95% 阈值 = 多数 session 在触发前最后一次请求 input 已很高（接近上限），与 catalog 写死的 95% 完全吻合。个别 session 在 70% 就触发可能是模型主动策略（reasoning 摘要提前写盘）。

## 相关资源

- 完整配置表：参考 Codex CLI 配置 doc（user-level + project-level）
- 模型 catalog：`~/.codex/models_cache.json`（`effective_context_window_percent` 字段）
- 排查对称的"压缩为什么不触发"问题：看 `~/.codex/sessions/` 里 `info.model_context_window` 是否达到阈值；没达到就检查 `truncation_policy.limit` 是否被改大、或者用户消息体量本来就不够