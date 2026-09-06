# ADR 0001: 按请求模型覆盖 Provider 和上游模型

- 状态：Accepted
- 日期：2026-09-06
- 决策者：OrgAI Gateway maintainers

## 背景

网关原本只按协议入口维护 Provider chain：

- `codex`：OpenAI Responses 入口。
- `claude`：Anthropic Messages 入口。

这意味着同一个入口内的所有模型共享一条 Provider chain，无法表达“只有某个
Codex 模型走 MiniMax，其他 Codex 模型继续走原生 Codex”。

这个需求不需要新的 failover 模型。需要的只是一个可选覆盖：请求模型有配置时，
选择指定 Provider 和上游 model；没有配置时，保持原有 chain 行为。

## 决策

增加按 slot 隔离的精确模型路由表。每条规则只包含：

- 一个请求模型名，作为规则 key。
- 一个 Provider。
- 一个必须显式填写的上游 model。

配置持久化到 `data/model_provider_routing.json`：

```json
{
  "version": 1,
  "slots": {
    "codex": {
      "rules": {
        "gpt-5.6-luna": {
          "provider": "minimax",
          "model": "MiniMax-M3"
        }
      }
    }
  }
}
```

不支持 slot default、多 target、route 内 failover、glob 或 regex。

### 领域模型

```rust
struct ModelRouting {
    version: u32,
    slots: BTreeMap<String, SlotRouting>,
}

struct SlotRouting {
    rules: BTreeMap<String, ModelRoute>,
}

struct ModelRoute {
    provider: String,
    model: String,
}

struct RouteDecision {
    rule_id: String,
    requested_model: String,
    provider: Provider,
    upstream_model: String,
}
```

运行时使用 `Provider` enum；持久化文档使用字符串。

### 匹配与执行

规则 key 在保存时 trim 并转小写。请求匹配同样 trim 后做大小写不敏感精确匹配。

执行优先级：

1. 显式 Provider namespace，例如 `cursor/*`、`ollama/*`。
2. 当前 slot 的精确模型 route。
3. `provider_chains.json` 中该 slot 的正常 chain。

命中 route 后：

- 只尝试 route 指定的 Provider。
- 把 route 的 `model` 按字面值发送给上游，不再做 Provider model mapping。
- 不回退到全局 chain。
- Provider 内部已有的账号选择、重试和退避逻辑保持不变。
- 不推进全局 chain 的 round-robin 计数器。

未命中 route 时：

- 完全使用原有 chain 顺序和 mode。
- 客户端请求模型保持原样进入现有 Provider model mapping。

### 校验和持久化

PUT 和启动加载都使用整文档严格校验：

- `version` 必须为 `1`。
- slot 只能是 `codex` 或 `claude`。
- 请求模型 key 不能为空，且规范化后不能重复。
- Provider 必须存在并支持对应 slot。
- 上游 model 不能为空。

更新时先原子落盘，再发布内存快照。落盘失败时运行中的配置不变。

启动时文档无效不会阻止网关启动；网关记录 warning，并使用空模型路由，让所有
请求继续走正常 chain。

本 schema 尚未发布，不保留此前多 target 草稿的兼容解析。

### API 和 UI

管理 API：

- `GET /v1/provider/model-routing`
- `PUT /v1/provider/model-routing`

GET 返回 routing 文档和每个 slot 允许的 Provider。PUT 完整替换 routing 文档，
非法内容返回 `400`。

WebUI 每条规则只编辑四项：

- slot。
- 请求 model。
- Provider。
- 上游 model。

Provider model catalog 只作为请求模型输入提示，不作为保存硬限制。

### 传输范围

第一版只应用于：

- `/v1/responses`
- `/v1/messages`

Codex WebSocket 和旧 `/v1/gateway/relay` 不应用模型路由。

### 可观测性

审计记录区分：

- `requested_model`：客户端模型。
- `upstream_model`：实际发送给上游的模型。
- `routing_rule`：命中的规范化规则 key。
- `routed_provider`：实际 Provider。

统计按 `requested_model` 聚合；历史记录没有该字段时回退到旧 `model` 字段。

## 后果

### 正面

- 满足同一客户端入口按模型选择不同 Provider 的需求。
- 未配置模型完全保持原 chain 行为。
- 配置和 UI 简单，没有第二套 failover 语义。
- 上游模型显式可见，避免依赖 Provider 默认映射。

### 代价

- 命中 route 的 Provider 不可用时，请求直接失败，不会回退全局 chain。
- 每个需要覆盖的模型都要明确配置上游 model。
- WebSocket 和 legacy relay 暂不支持。

## 当前环境

当前只配置：

```text
codex / gpt-5.6-luna -> minimax / MiniMax-M3
```

其他 Codex 模型没有 route，继续使用 Codex 全局 chain，模型名不改。

## 验收条件

- 命中模型规则时只使用指定 Provider 和字面上游 model。
- 未命中规则时 provider 顺序、round-robin 和模型处理与改动前一致。
- Luna 可以映射到 `minimax / MiniMax-M3`。
- 其他 Codex 模型继续走原生 Codex，且请求模型不被改写。
- 保存失败不改变运行中配置。
- direct `cursor/*` / `ollama/*` 优先级不回归。
- 审计同时记录 requested model、upstream model、Provider 和 rule。
- WebUI 不显示 default、target 顺序或 route 内 failover 控件。
