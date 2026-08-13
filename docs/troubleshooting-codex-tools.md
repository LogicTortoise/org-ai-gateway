# 排查：Codex 0.147 Desktop 发不出 tool_call（`apply_patch` 被吞）

> 状态：⚠️ **过渡方案，已过期**（commit `565f034`，2026-08 初）
> 适用（历史）：Codex CLI / Codex Desktop 0.147+ 通过 `/v1/responses` → org-ai-gateway → minimax / deepseek / glm / kimi 这种 Chat Completions 兼容上游的链路
>
> ### 为什么这条 fix 已过期
>
> `565f034` 的修法是在 `convert_responses_tools` 里加 if/else 把 `type:"custom"` / `type:"namespace"` 重新包成 OpenAI Chat Completions 的 `type:"function"`。**这只对 deepseek / glm / kimi 这种"上游只吃 Chat Completions"的 provider 有效**——minimax 路径在 `b87dcb8`（2026-08-13）已经从适配层改成原生 `/v1/responses` 透传了，整个 `convert_responses_tools` 在 minimax 上不再调用。
>
> ### 为什么这条 fix 本身很难完备
>
> Codex 客户端的 `tools` 数组里的 `type` 是 OpenAI Responses API spec 的扩展集合，spec 允许的类型在持续增加（`function` / `custom` / `namespace` / `web_search` / `tool_search` / `file_search` / 未来的 MCP tool 容器……）。`convert_responses_tools` 现在是"遇到认识的 type 就 rewrap，不认识的就静默丢"——这是**修补**不是根治：每次 Codex 升级加新的 tool type，就有可能再出现"tool 被吞"。任何还在走 Chat Completions 适配层的 provider（deepseek / glm / kimi）都仍然暴露在这条风险下。
>
> 真正的根治方向是让 Codex slot 走的上游**自己支持 Responses API**（minimax 就是）。如果哪天 deepseek / glm / kimi 哪家官宣 `/v1/responses`，应该同样改成透传，删掉它对应的 `convert_responses_tools`，而不是继续在 if/else 里加分支。
>
> 本 doc 的诊断方法（看 Codex rollout、dump 请求体、加 INFO 日志）做 deepseek / glm / kimi 排查时仍然有用；修法部分对 minimax 不再适用。

## 症状

- Codex Desktop（`originator: "Codex Desktop"` / `source: "vscode"` / `cli_version: 0.147.x`）跑的 session 在长上下文里"卡在冒号"：模型输出纯文本（如 `修单测:`），没有 `function_call` 派发
- 同一个用户在 Claude Code 里跑 minimax 长上下文**完全没事**（说明 minimax 模型本身没问题）
- audit 里看 `origin=codex_cli` + 该 session 累计 `(assistant, function_call): 0` 但 `(assistant, output_text): N`
- Codex CLI rollout（`~/.codex/sessions/.../rollout-*.jsonl`）里 `response_item` 计数全是 `output_text`，0 条 `function_call`

## 真因（一句话）

Codex 0.147 Desktop 发出的 `tools` 数组**不全是** `type:"function"`。`apply_patch`（唯一文件编辑工具）标 `type:"custom"`；`codex_app` / `image_gen` 包成 `type:"namespace"` 容器。旧版 `src/provider/minimax.rs::convert_responses_tools` 用 `if ty != "function" { skip; }` 静默丢弃所有这些 → 上游收到 `tools: []` → 模型没工具可调。

## 怎么确认（5 分钟诊断）

### 1. 看 Codex session rollout 的 `(role, type)` 计数

```bash
python3 - <<'EOF'
import json, os
path = max(
    (os.path.join(r, f) for r, _, fs in os.walk(os.path.expanduser("~/.codex/sessions")) for f in fs if f.endswith(".jsonl")),
    key=os.path.getmtime,
)
with open(path) as fh:
    rows = [json.loads(l) for l in fh if l.strip()]
from collections import Counter
c = Counter((p.get("role","?"), part.get("type","?")) for r in rows
            if r.get("type") == "response_item"
            for part in r.get("payload", {}).get("content", []) if isinstance(part, dict))
for k, v in c.most_common():
    print(f"  {k}: {v}")
EOF
```

如果 `("assistant", "function_call"): 0` 同时 `("assistant", "output_text") > 50`，基本确认是 gateway 翻译丢工具。

### 2. 加一条 INFO 日志看 Codex 入站 tools 的真实形态

在 `src/routes/proxy.rs::proxy_responses_inner` 里 `ensure_codex_payload_defaults(&mut payload);` 之后加：

```rust
if let Some(tools) = payload.get("tools").and_then(|v| v.as_array()) {
    let summary = tools.iter().map(|t| format!(
        "type={} name={}",
        t.get("type").and_then(|v| v.as_str()).unwrap_or("?"),
        t.pointer("/function/name").or_else(|| t.get("name"))
            .and_then(|v| v.as_str()).unwrap_or("")))
        .collect::<Vec<_>>().join(" | ");
    info!("CODEX_REQ_TOOLS user={} count={} [{}]",
        user_id, tools.len(), summary);
}
```

跑一次让 Codex 发请求，去 `data/gateway.out.log` 看 `CODEX_REQ_TOOLS user=koltyu ...` 那一行。**如果里面有 `type=custom name=apply_patch` 或 `type=namespace name=codex_app`——确认问题。**

### 3. curl 复现：用 flat `type:"function"` tools 看 minimax 是否发 tool_call

```bash
curl -sN -X POST http://127.0.0.1:8088/v1/responses \
  -H "Content-Type: application/json" -H "Authorization: Bearer dummy" \
  -d '{"model":"gpt-5.6-sol","stream":true,
       "input":[{"role":"user","content":[{"type":"input_text","text":"用 apply_patch 写文件"}]}],
       "tools":[{"type":"function","name":"apply_patch",
                 "strict":false,
                 "parameters":{"type":"object","properties":{"patch":{"type":"string"}},
                               "required":["patch"]}}]}' \
  | grep -E 'function_call|apply_patch'
```

预期：minimax 立刻 SSE 输出 `event: response.output_item.added`，item 里 `name:"apply_patch"`、`arguments:"{\"patch\":\"*** Begin Patch\\n...*** End Patch\"}"`。如果没出现 → 翻译层还有别的问题；如果出现 → 证明是 Codex `type:"custom"` 那条路径没翻译。

## 修复

文件 `src/provider/minimax.rs::convert_responses_tools`，按 `type` 分流：

| Codex 发的 `type` | 处理 |
|---|---|
| `"function"` 或 `"custom"` | 按 Chat Completions 形态 rewrap 成 `{type:"function", function:{name, description, parameters, strict}}`；空 `name` 继续丢（"function is empty" 错误防护） |
| `"namespace"` | 递归处理嵌套的 `tools[]`，里面每个按上条规则 rewrap |
| 其它（`web_search` / `tool_search` / `file_search` 等） | 静默丢（这些是 Codex 内置 server tool，minimax 等没有等价物，不 400） |

测试覆盖在同文件 `#[cfg(test)]` 块 `responses_tools_rewraps_flat_shape_passes_through_wrapped_shape`：
- case 4：`type:"custom" name:"apply_patch"` 必须 rewrap，不被丢
- case 5：`type:"namespace"` 必须展平，嵌套 function 全部保留
- case 6：`web_search` / `tool_search` / `file_search` 仍然丢

curl smoke（修了以后）：

```bash
curl -sN -X POST http://127.0.0.1:8088/v1/responses \
  -H "Content-Type: application/json" -H "Authorization: Bearer dummy" \
  --max-time 60 \
  -d '{"model":"gpt-5.6-sol","stream":true,
       "input":[{"role":"user","content":[{"type":"input_text","text":"用 apply_patch 给 hello.txt 加 hello world"}]}],
       "tools":[{"type":"function","name":"apply_patch","description":"patch a file",
                 "strict":false,
                 "parameters":{"type":"object","properties":{"patch":{"type":"string"}},
                               "required":["patch"]}}]}' \
  | grep '"name":"apply_patch"'
```

预期能看到 `event: response.output_item.added` 带 `name:"apply_patch"`，最终 `response.output_item.done` 的 `arguments` 是 `*** Begin Patch / +hello world / *** End Patch`。

## 排查时间线（这次的真实过程）

1. 用户报"Codex 跑 backtest-lab 跑一会儿就停在冒号"（18:40 / 19:43 / 20:04 / 20:16 / 20:29 CST 五次）
2. 先查 audit：`origin=codex_cli` 请求 status 全 success，0 个 upstream_empty_stream；最后一条 `req=075d5400` `out_len=252` 正常
3. 一度误判为 minimax 模型在长上下文下不发 tool_call（错的）
4. 去看 Codex CLI 本地 rollout：`~/.codex/sessions/.../rollout-*.jsonl`，发现 `function_call: 0`，session 里 95 条 `output_text` 全是嘴说
5. 加 `CODEX_REQ_TOOLS` debug 日志，让用户跑一次，dump 看到 17 个 tools 里有 `type=custom name=apply_patch`、`type=namespace name=codex_app nested=3` 等
6. 定位 `convert_responses_tools` 里的 `if ty != "function"` 是元凶
7. curl smoke 验证 minimax 在 `type:"function"` tools 下能正常发 apply_patch，修复后能发 `apply_patch` → commit `565f034`

## 不要做的事

- **不要把"模型在长上下文不发 tool_call"当成 minimax 的 bug 写进 docs/configuration.md**——这不是 minimax 行为，是网关翻译 bug
- **不要靠 `output_tokens > 0 + output_length == 0` 这种 usage 指纹判断"模型自己停了"**——这是 reasoning 模型的正常形态，Codex 把 `last_agent_message: null` 当 final answer 才是症状
- **不要把 Codex 的 session_meta `dynamic_tools` 字段当成请求体里 `tools` 的形状**——`dynamic_tools` 是 namespace 容器，但请求体里 Codex 客户端已经按 Responses 协议平铺/嵌套展开过一次

## 相关资源

- 完整诊断 + commit message：commit `565f034`
- 上游 Codex 协议说明：`/v1/responses` 的 `tools` 字段允许的类型由 OpenAI Responses API spec 定义，包含 `function` / `custom` / `namespace` / `web_search` 等
- minimax 上游兼容的是 OpenAI Chat Completions，只认 `type:"function"`，但 `function.name` 可以是任意字符串