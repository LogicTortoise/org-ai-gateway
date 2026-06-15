use crate::prelude::*;

/// Aggregate a Codex SSE response: prefer the final complete text from the
/// terminal `response.completed` event; fall back to accumulated deltas. The two
/// sources carry the SAME text, so they must never be concatenated (that would
/// double the output).
pub(crate) fn extract_output_text_from_sse(body: &str) -> Option<String> {
    let mut deltas = String::new();
    let mut completed: Option<String> = None;
    for line in body.lines() {
        let line = line.trim();
        if !line.starts_with("data:") {
            continue;
        }
        let data = line.strip_prefix("data:").unwrap_or_default().trim();
        if data == "[DONE]" || data.is_empty() {
            continue;
        }
        let value: Value = match serde_json::from_str(data) {
            Ok(v) => v,
            Err(_) => continue,
        };
        // Only accumulate OUTPUT-text deltas. Codex also emits a top-level
        // `delta` string for `response.reasoning_summary_text.delta` and
        // `response.refusal.delta`; folding those into the output would leak
        // reasoning/refusal text when the stream is truncated before
        // `response.completed` (the only case this delta fallback is used).
        if value.get("type").and_then(|t| t.as_str()) == Some("response.output_text.delta") {
            if let Some(delta) = value.get("delta").and_then(|v| v.as_str()) {
                deltas.push_str(delta);
            }
        }
        // Full text from a terminal event (top-level `output`/`output_text`, or
        // nested under `response` in response.completed). Last one wins.
        if let Some(text) = extract_output_text(data) {
            completed = Some(text);
        } else if let Some(resp) = value.get("response") {
            if let Ok(raw) = serde_json::to_string(resp) {
                if let Some(text) = extract_output_text(&raw) {
                    completed = Some(text);
                }
            }
        }
    }
    let out = completed.unwrap_or(deltas);
    if out.trim().is_empty() {
        None
    } else {
        Some(out)
    }
}

/// Aggregate a buffered Codex SSE stream into the single JSON object a
/// non-streaming client expects: the `response` payload of the last
/// `response.completed` (or `response.failed`) event.
///
/// Some upstreams send a terminal `response.completed` whose `output` array is
/// EMPTY — the message items were only ever streamed as separate
/// `response.output_item.done` events. Returning that terminal object verbatim
/// would hand the client a 200 with no content. So we collect the completed
/// output items and backfill `output` when the terminal object lacks them.
pub(crate) fn aggregate_codex_sse_to_response_json(body: &str) -> Option<Value> {
    let mut last: Option<Value> = None;
    let mut items: Vec<(i64, Value)> = Vec::new();
    for line in body.lines() {
        let line = line.trim();
        if !line.starts_with("data:") {
            continue;
        }
        let data = line.strip_prefix("data:").unwrap_or_default().trim();
        if data == "[DONE]" || data.is_empty() {
            continue;
        }
        let Ok(value) = serde_json::from_str::<Value>(data) else {
            continue;
        };
        match value.get("type").and_then(|t| t.as_str()).unwrap_or("") {
            "response.completed" | "response.failed" | "response.incomplete" => {
                if let Some(resp) = value.get("response") {
                    last = Some(resp.clone());
                }
            }
            "response.output_item.done" => {
                if let Some(item) = value.get("item") {
                    let idx = value
                        .get("output_index")
                        .and_then(|v| v.as_i64())
                        .unwrap_or(items.len() as i64);
                    items.push((idx, item.clone()));
                }
            }
            _ => {}
        }
    }

    let mut resp = last?;
    let terminal_output_empty = resp
        .get("output")
        .and_then(|v| v.as_array())
        .map(|a| a.is_empty())
        .unwrap_or(true);
    if terminal_output_empty && !items.is_empty() {
        items.sort_by_key(|(idx, _)| *idx);
        let arr: Vec<Value> = items.into_iter().map(|(_, item)| item).collect();
        if let Some(obj) = resp.as_object_mut() {
            obj.insert("output".to_string(), Value::Array(arr));
        }
    }
    Some(resp)
}


#[cfg(test)]
mod tests {
    use super::*;

    const CODEX_SSE: &str = concat!(
        "data: {\"type\":\"response.output_text.delta\",\"delta\":\"Hello \"}\n\n",
        "data: {\"type\":\"response.output_text.delta\",\"delta\":\"world\"}\n\n",
        "data: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\",\"output\":[{\"type\":\"message\",\"content\":[{\"type\":\"output_text\",\"text\":\"Hello world\"}]}],\"usage\":{\"input_tokens\":1,\"output_tokens\":2}}}\n\n",
        "data: [DONE]\n\n",
    );

    #[test]
    fn sse_text_is_not_double_counted() {
        // Deltas + the final response.completed carry the SAME text; the
        // aggregate must be the text once, not twice.
        assert_eq!(extract_output_text_from_sse(CODEX_SSE).unwrap(), "Hello world");
    }

    #[test]
    fn sse_falls_back_to_deltas_without_completed_event() {
        let body = "data: {\"type\":\"response.output_text.delta\",\"delta\":\"partial\"}\n\n";
        assert_eq!(extract_output_text_from_sse(body).unwrap(), "partial");
    }

    #[test]
    fn aggregates_sse_to_terminal_response_json() {
        let v = aggregate_codex_sse_to_response_json(CODEX_SSE).unwrap();
        assert_eq!(v["status"], "completed");
        assert_eq!(v["usage"]["output_tokens"], 2);
        assert!(aggregate_codex_sse_to_response_json("data: {\"type\":\"response.output_text.delta\",\"delta\":\"x\"}\n\n").is_none());
    }

    #[test]
    fn backfills_output_when_terminal_event_is_empty() {
        // Real-world shape: the terminal response.completed has output:[]; the
        // message only arrived via response.output_item.done. The aggregate must
        // still carry the message content, not an empty output array.
        let body = concat!(
            "data: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"type\":\"message\",\"role\":\"assistant\",\"content\":[{\"type\":\"output_text\",\"text\":\"hi there\"}]}}\n\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\",\"output\":[],\"usage\":{\"output_tokens\":3}}}\n\n",
            "data: [DONE]\n\n",
        );
        let v = aggregate_codex_sse_to_response_json(body).unwrap();
        assert_eq!(v["status"], "completed");
        assert_eq!(v["output"][0]["content"][0]["text"], "hi there");
    }
}


pub(crate) fn extract_claude_output_text_from_sse(body: &str) -> Option<String> {
    let mut out = String::new();
    for line in body.lines() {
        let line = line.trim();
        if !line.starts_with("data:") {
            continue;
        }
        let data = line.strip_prefix("data:").unwrap_or_default().trim();
        if data == "[DONE]" || data.is_empty() {
            continue;
        }
        let value: Value = match serde_json::from_str(data) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if let Some(text) = value
            .get("delta")
            .and_then(|d| d.get("text"))
            .and_then(|v| v.as_str())
        {
            out.push_str(text);
        }
        if let Some(text) = extract_claude_output_text(data) {
            out.push_str(&text);
        }
    }
    if out.trim().is_empty() {
        None
    } else {
        Some(out)
    }
}


pub(crate) fn extract_output_text(body: &str) -> Option<String> {
    let value: Value = serde_json::from_str(body).ok()?;

    if let Some(text) = value.get("output_text").and_then(|v| v.as_str()) {
        return Some(text.to_string());
    }

    let mut parts = Vec::new();
    if let Some(items) = value.get("output").and_then(|v| v.as_array()) {
        for item in items {
            if let Some(content_items) = item.get("content").and_then(|v| v.as_array()) {
                for content in content_items {
                    if let Some(text) = content.get("text").and_then(|v| v.as_str()) {
                        if !text.trim().is_empty() {
                            parts.push(text.to_string());
                        }
                    }
                }
            }
        }
    }

    if parts.is_empty() {
        None
    } else {
        Some(parts.join("\n"))
    }
}


pub(crate) fn extract_claude_output_text(body: &str) -> Option<String> {
    let value: Value = serde_json::from_str(body).ok()?;
    let mut parts = Vec::new();

    if let Some(content) = value.get("content").and_then(|v| v.as_array()) {
        for item in content {
            if item.get("type").and_then(|v| v.as_str()) == Some("text") {
                if let Some(text) = item.get("text").and_then(|v| v.as_str()) {
                    if !text.trim().is_empty() {
                        parts.push(text.to_string());
                    }
                }
            }
        }
    }

    if parts.is_empty() {
        None
    } else {
        Some(parts.join("\n"))
    }
}


