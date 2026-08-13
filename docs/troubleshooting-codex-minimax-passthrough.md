# 排查：Codex CLI 走 minimax 报「upstream returned a response with no output items」

> 状态：✅ 已修（commit `b87dcb8`，2026-08-13）
> 适用：MiniMax（M3 / M2 / 等官方 catalog id）作为 Codex slot 的 provider——具体触发条件是 **客户端发 `stream: false` 但上游实际被网关强制成 `stream: true`**，gateway 拿到 SSE body 后当 JSON 解析失败，误判成 empty。
>
> ### 跟 `docs/troubleshooting-codex-tools.md`（commit `565f034`）的关系
>
> 那条 doc 讲的是 Codex `apply_patch` / `codex_app` / `image_gen` 这种 `type:"custom"` / `type:"namespace"` 的 tool 被 minimax 路径吞掉的问题。它走的是 **if/else 修补**（`convert_responses_tools` 里给认识的 type rewrap），是一种**过渡方案**，依赖 Codex 当前已知的 tool type 集合——本质上**很难完备**（Codex 加新 type 还会再踩）。
>
> 本 doc 的修法（`b87dcb8`）走了另一条路：**根本别翻译**，让 minimax 自己处理 Responses 协议。这也顺手废掉了 `565f034` 那条 fix 在 minimax 路径上的全部代码——`minimax.rs::convert_responses_tools` 整个删了，"tool 被吞" 这个 bug 在 minimax 上已不存在。
>
> `565f034` 那条 fix 在 **deepseek / glm / kimi**（仍走 Chat Completions 适配层）上还活着，但哪天这些 provider 官宣支持 `/v1/responses` 的话，同样应该改透传 + 删对应的 `convert_responses_tools`。

## 症状

- Codex CLI（`wire_api = "responses"`）发一个普通请求，gateway audit 里这条 record 标记为：
  ```
  status=upstream_empty_stream  (or similar empty branch)
  routed_provider=minimax
  ```
- gateway 返回给客户端：
  ```
  HTTP 502 Bad Gateway
  {"error":"minimax upstream returned a response with no output items (truncated or reasoning-only); try again in 30s","provider":"minimax"}
  ```
- **直接 curl 上游** `https://api.minimaxi.com/v1/responses`（同样的 body + `stream: false`），拿到的是 HTTP 200 + `Content-Type: application/json` + 完整的 Codex Responses 形状 JSON（含 `output`、`usage`、`id` 等），**一切正常**——证明是网关这一段把它弄坏了，不是 minimax 端问题。

## 真因（一句话）

`src/routes/proxy.rs::proxy_responses_inner` 在 dispatch 之前调 `ensure_codex_payload_defaults(&mut payload)`，**它会把 `payload.stream` 强制设成 `true`**（这是为了让所有上游都走 SSE 流，避免有的上游不接受非流式调用）。所以**上游永远**收到 `stream: true`，无论客户端实际发的是什么。

`serve_minimax_responses_passthrough` 的非流式分支之前是这么写的（错的）：

```rust
if !client_wants_stream {
    let body = read_body_capped(...).await?;
    let parsed: Value = serde_json::from_str(&body_str).unwrap_or(Value::Null);
    let output_has_content = parsed.get("output").and_then(|v| v.as_array())
        .map(|arr| !arr.is_empty()).unwrap_or(false);
    if !output_has_content {
        // empty 分支 → 返回 "no output items" 错误
    }
}
```

`read_body_capped` 拿到的是**整个 SSE 字节流**（`event: response.created\ndata:{...}\n\n...`），`serde_json::from_str` 在第一个 `{` 就 parse 失败 → `parsed == Value::Null` → `parsed.get("output")` 是 None → `output_has_content` 是 false → 触发 empty 误判。即使上游正常返回了 "1+1=2." 也会被报成空响应。

## 修复

把 Codex 路径对 minimax 改成**透明管道**——直接转发客户端原始 payload 到 MiniMax 的 `/v1/responses`，响应也按 Codex Responses 协议透传回客户端，不再做任何 Responses↔Chat Completions 改写。原因：MiniMax 官方 Codex 接入面**就是** `/v1/responses`（[`platform.minimaxi.com/docs/token-plan/codex`](https://platform.minimaxi.com/docs/token-plan/codex)），Codex CLI 用 `wire_api = "responses"` 直接对得上，多余的翻译只会引入 bug。

### 代码改动（commit `b87dcb8`）

| 文件 | 改什么 |
|---|---|
| `src/provider/minimax.rs` | 删 `convert_responses_to_chat_messages` / `convert_responses_tools` / `convert_one_responses_tool` / `collect_text_parts` / `build_minimax_openai_body` / `estimate_message_tokens` / `estimate_messages_tokens` / `truncate_messages_to_context_window` / `send_minimax_openai` / `send_minimax_openai_streaming` / `parse_minimax_tool_calls` / 它们的测试 —— 一整层 Chat Completions 适配层都没用了<br>删 `send_minimax_responses`（non-streaming sender）—— non-streaming 客户端也走 streaming sender + 服务端聚合<br>留 `send_minimax_responses_streaming`，文档更新说明它现在是**唯一** caller<br>`probe_minimax_openai` 改用 `/v1/responses` shape 探测 |
| `src/routes/proxy.rs` | dispatch (`proxy_responses_inner` 附近的 chain 派发) 把 minimax 从 Chat Completions 适配层拆出来，专门走 `serve_minimax_responses_passthrough`<br>新增 `serve_minimax_responses_passthrough`：账号选择 + 退避重试 + 真实 usage 审计 + 空响应重试<br>新增 `stream_minimax_responses_passthrough`：响应 SSE 透传 + 边带解析 `response.completed` 拿 usage + 检测 `error` 事件合成 `response.failed` 终端事件（让 Codex client 能 surface 这个错并 retry）<br>非流式分支：buffer 整个 SSE body → `sse::aggregate_codex_sse_to_response_json` 还原成完整 Response 对象 → 当 JSON 返回 |
| `docs/configuration.md` | `MINIMAX_BASE_URL` 默认从 `https://api.minimaxi.com/v1` 改成 `https://api.minimaxi.com`（**主机**，不再带 `/v1`，否则会拼成 `/v1/v1/responses` 404）<br>MiniMax 章节的"Codex 路径"段改成"网关是透明管道" |
| `docs/troubleshooting-codex-tools.md` | 加历史 note 说明 minimax 路径已不再走 Chat Completions 适配层（这条 doc 当时讲的就是 minimax + 其它 provider 的 `convert_responses_tools`，minimax 那部分已不适用） |

### 关键设计点

1. **MINIMAX_BASE_URL 必须只到主机**：`api.minimaxi.com`，不要 `api.minimaxi.com/v1`。`send_minimax_responses_streaming` 内部 `format!("{}{}", base, MINIMAX_RESPONSES_PATH)` 会自己拼 `/v1/responses`。账号自带的 `base_url` 同理，只填主机。

2. **上游永远 stream=true**，但**响应形态按客户端需求**：
   - 客户端 `stream: true` → 直接 pipe SSE 字节流回客户端
   - 客户端 `stream: false` → buffer + 聚合成 Responses JSON 当 HTTP 200 返回
   这是和 Chat Completions 适配层时代完全一致的设计（`ensure_codex_payload_defaults` 强制 stream=true 是网关层面既定的，不会为了一个 provider 改回去）。

3. **`response.completed` 的边带解析**必须保留：SSE 流透传时仍然要监听 `response.completed` 事件，从 `response.usage` 拿真实 token 数写 audit（input / cached / output / reasoning），否则账单全 0。

4. **空流 + 内嵌 error 仍然处理**：和 Chat Completions 时代一样——`response.completed` 的 `output` 是空数组、或流里出现 `error` 事件 → 合成的 `response.failed` 终端事件，让 Codex client 看到 failure 并 retry（Codex 0.144 把 `response.error.code = "overloaded_error"` 当 `ApiError::Retryable`，自动发起下一轮）。

### 没了的部分（重要的清除项）

按 CLAUDE.md「代码重构或者 bugfix，不做老代码的兼容性保留」：

- **不做 prompt 截断**：之前 minimax.rs 有 `estimate_messages_tokens` + `truncate_messages_to_context_window`，按 token 数硬截 input。改透传后这个不需要了——Codex CLI 自己知道 model 的 context 窗口，传过来的 input 就让它传过去。MiniMax M3 公开 1M context，1M 内随便跑。
- **不做 tool 翻译**：之前 `convert_responses_tools` 把 Codex 的 `apply_patch` / `codex_app` / `image_gen`（`type:"custom"` / `type:"namespace"`）重新包成 OpenAI Chat Completions 的 `type:"function"`。透传后这些 gadget 原样发给 minimax `/v1/responses`，minimax 自己懂 Responses 协议。
- **不做 reasoning 压缩**：之前 streaming 是把 deltas 聚合成 `response.output_item.done` 一次性发完。现在是逐 token 透传，Codex 客户端能拿到完整的 reasoning 流。

## 验证（curl smoke）

需要 `data/accounts.ndjson` 里有健康的 minimax 账号。

### Case 1：non-stream + 无 tools（最常见的 regression case）

```bash
curl -s -w '\n--- HTTP %{http_code} ---\n' -X POST http://127.0.0.1:8088/v1/responses \
  -H 'Content-Type: application/json' \
  -H 'Authorization: Bearer dummy' \
  -H 'X-User-Id: e2e-test' \
  --max-time 120 \
  -d '{"model":"gpt-5.6-sol","stream":false,
       "input":[{"role":"user","content":[{"type":"input_text","text":"用一句话回答：1+1=?"}]}]}' \
  | tail -3
```

预期：HTTP 200 + 完整 Responses JSON（含 `output[].content[].text="1+1=2。"`、`usage.input_tokens`、`usage.output_tokens`、`model="MiniMax-M3"` 自动大小写修正）。

### Case 2：streaming + 无 tools

```bash
curl -sN -X POST http://127.0.0.1:8088/v1/responses \
  -H 'Content-Type: application/json' \
  -H 'Authorization: Bearer dummy' \
  -H 'X-User-Id: e2e-test' \
  --max-time 60 \
  -d '{"model":"gpt-5.6-sol","stream":true,
       "input":[{"role":"user","content":[{"type":"input_text","text":"用一句话回答：1+1=?"}]}]}' \
  | grep -E '^event:'
```

预期：依次出现 `response.created` / `response.in_progress` / `response.output_item.added` / `response.output_text.delta` ×N / `response.output_item.done` / `response.completed`。

### Case 3：non-stream + 带 `apply_patch` tools（Codex CLI 真实工作场景）

```bash
curl -s -w '\n--- HTTP %{http_code} ---\n' -X POST http://127.0.0.1:8088/v1/responses \
  -H 'Content-Type: application/json' \
  -H 'Authorization: Bearer dummy' \
  -H 'X-User-Id: e2e-test' \
  --max-time 120 \
  -d '{"model":"gpt-5.6-sol","stream":false,
       "input":[{"role":"user","content":[{"type":"input_text","text":"用一句话回答：天空是什么颜色?"}]}],
       "tools":[{"type":"function","name":"apply_patch","description":"patch a file",
                 "strict":false,
                 "parameters":{"type":"object","properties":{"patch":{"type":"string"}},
                               "required":["patch"]}}]}' \
  | tail -3
```

预期：HTTP 200 + `tools` 数组被原样回显 + 模型不调用 tool（只是回答问题）+ `usage.input_tokens > 0`。

### Case 4：base URL 配错（带 `/v1`）—— 应该 404

```bash
MINIMAX_BASE_URL='https://api.minimaxi.com/v1' ./scripts/restart.sh -b
curl -s -w '\n--- HTTP %{http_code} ---\n' -X POST http://127.0.0.1:8088/v1/responses \
  -H 'Content-Type: application/json' -H 'Authorization: Bearer dummy' \
  -d '{"model":"gpt-5.6-sol","stream":false,"input":[{"role":"user","content":[{"type":"input_text","text":"hi"}]}]}'
# gateway 日志里会看到: POST https://api.minimaxi.com/v1/v1/responses
```

→ 这条用来验证 `MINIMAX_BASE_URL` 不要带 `/v1` 的 doc 说明确实有道理。

## 排查时间线

1. 用户报「Codex CLI 在 minimax 上跑长 session 卡在冒号（`apply_patch` 被吞）」—— 这是 commit `565f034` 修的事，跟这次无关
2. 后续用户报「minimax 还是报错，换 minimax-m3 也不对」—— 直接 curl 上游没问题，curl 网关报 "no output items"
3. 加 debug 日志（`tracing::warn!("[minimax_debug] codex_passthrough_body ...")`），发现网关返回的 body 是 SSE 文本（`event: response.created...`），不是 JSON
4. 读 `proxy_responses_inner` line 312 注释：「Upstream is always called with stream=true」—— 确认 `ensure_codex_payload_defaults` 强制 stream=true
5. 修法：non-streaming 客户端走 streaming 上游 + SSE 聚合

## 不要做的事

- **不要再加 Chat Completions 适配层**：之前的那层是给"上游只支持 Chat Completions"的 provider 用的（GLM / Kimi / DeepSeek 还在用）。MiniMax 官方 `/v1/responses` 直接对得上 Responses 协议，加适配层只会重新引入这次的 bug。
- **不要把 `ensure_codex_payload_defaults` 改成按 provider 分流**：那条路径设计就是"上游永远 stream=true"，为了一个 provider 改回去会让 GLM / Kimi / DeepSeek 的非流式路径也跟着改，得不偿失。
- **不要重新加 input token 截断**：MiniMax M3 是 1M context，Codex CLI 知道自己的 context 窗口（256k），传过来的 input 在 1M 内就让它过。truncate 之前是给"上游只吃 128k / 32k"的便宜模型用的，minimax 不需要。
- **不要把 `model` 字段从 payload 里抹掉再让 sender 自己加**：当前 `send_minimax_responses_streaming` 会 `obj.insert("model", ...)`，这是有意的——Codex CLI 发过来的 model id 是 `gpt-5.6-sol` 这种 Bedrock-style 别名，必须在网关层用 `minimax_canonical_model` 翻成 `MiniMax-M3`，否则 minimax 收到 `gpt-5.6-sol` 会 400 "model not found"。

## 相关资源

- 修复 commit：`b87dcb8`
- MiniMax 官方 Codex 接入面：[`platform.minimaxi.com/docs/token-plan/codex`](https://platform.minimaxi.com/docs/token-plan/codex)
- 配 env：`docs/configuration.md` 里 MiniMax 章节（特别注意 `MINIMAX_BASE_URL` 必须只到主机）
- SSE 聚合 helper：`src/sse.rs::aggregate_codex_sse_to_response_json`（non-streaming 客户端路径用，把 buffer 起来的 Codex SSE 流还原成 Responses JSON 对象）
- 历史：commit `565f034`（修 Codex apply_patch `type:"custom"` 被吞的 bug）—— 那条 bug 在 minimax 路径上现在已经不会再出现（透传后 `apply_patch` 是 Codex 发的 `type:"custom"`，minimax 自己处理）