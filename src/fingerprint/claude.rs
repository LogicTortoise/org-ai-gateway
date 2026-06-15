//! Claude Code HTTP fingerprint (ported from `claude_fingerprint.go` +
//! `claude_sdk_compat.go`): stainless/version headers, the anthropic-beta list,
//! system-block injection with the billing attribution header + CCH checksum,
//! metadata.user_id injection, and tool-name obfuscation.
use crate::prelude::*;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::hash::Hasher;

const CC_VERSION: &str = "2.1.87";
const CC_SDK_VERSION: &str = "0.80.0";
pub(crate) const CC_ANTHROPIC_VERSION: &str = "2023-06-01";
const CC_NODE_VERSION: &str = "v22.22.0";
pub(crate) const CC_SYSTEM_PREFIX: &str =
    "You are Claude Code, Anthropic's official CLI for Claude.";
const CCH_SEED: u64 = 0x6E52_736A_C806_831E;
const CCH_MASK: u64 = 0xF_FFFF;

// ---- model capability checks ----

fn normalize_model(model: &str) -> String {
    model.to_ascii_lowercase().replace("[1m]", "").trim().to_string()
}

fn supports_isp(model: &str) -> bool {
    !normalize_model(model).contains("claude-3-")
}

fn supports_context_management(model: &str) -> bool {
    !normalize_model(model).contains("claude-3-")
}

fn supports_effort(model: &str) -> bool {
    let m = normalize_model(model);
    ["opus-4-6", "opus-4-7", "sonnet-4-6"].iter().any(|n| m.contains(n))
}

fn supports_structured_outputs(model: &str) -> bool {
    let m = normalize_model(model);
    [
        "claude-sonnet-4-6",
        "claude-sonnet-4-5",
        "claude-opus-4-1",
        "claude-opus-4-5",
        "claude-opus-4-6",
        "claude-opus-4-7",
        "claude-haiku-4-5",
    ]
    .iter()
    .any(|n| m.contains(n))
}

/// Build the `anthropic-beta` header value for a model.
fn anthropic_beta(model: &str, is_oauth: bool) -> String {
    let mut betas = vec!["claude-code-20250219"];
    if is_oauth {
        betas.push("oauth-2025-04-20");
    }
    if supports_isp(model) {
        betas.push("interleaved-thinking-2025-05-14");
    }
    betas.push("redact-thinking-2026-02-12");
    if supports_context_management(model) {
        betas.push("context-management-2025-06-27");
    }
    if supports_effort(model) {
        betas.push("effort-2025-11-24");
    }
    betas.push("prompt-caching-scope-2026-01-05");
    if supports_structured_outputs(model) {
        betas.push("structured-outputs-2025-12-15");
    }
    betas.join(",")
}

/// Apply the Claude Code stainless/version/beta fingerprint headers.
pub(crate) fn apply_claude_fingerprint(
    req: reqwest::RequestBuilder,
    model: &str,
    is_oauth: bool,
) -> reqwest::RequestBuilder {
    // anthropic-version + Content-Type are set by `apply_claude_auth_headers`.
    req.header("User-Agent", format!("claude-cli/{} (external, cli)", CC_VERSION))
        .header("anthropic-beta", anthropic_beta(model, is_oauth))
        .header("X-Stainless-Lang", "js")
        .header("X-Stainless-Runtime", "node")
        .header("X-Stainless-Runtime-Version", CC_NODE_VERSION)
        .header("X-Stainless-Arch", "arm64")
        .header("X-Stainless-Os", "Linux")
        .header("X-Stainless-Package-Version", CC_SDK_VERSION)
        .header("X-Stainless-Retry-Count", "0")
        .header("X-Stainless-Timeout", "600")
        .header("X-Stainless-Helper-Method", "stream")
}

// ---- derived IDs ----

/// Derive a stable UUIDv4-shaped id from a seed + label (Go `ccDerivedID`):
/// sha256("codex-pool/" + label + "/" + seed), first 16 bytes, v4 bits set.
fn derived_id(seed: &str, label: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(format!("codex-pool/{}/{}", label, seed).as_bytes());
    let digest = hasher.finalize();
    let mut b = [0u8; 16];
    b.copy_from_slice(&digest[..16]);
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7], b[8], b[9], b[10], b[11], b[12], b[13], b[14], b[15]
    )
}

/// The attribution billing header text. The 4-byte tag is
/// sha256(prefix + "\n" + existing_system + cc_version)[..4] in hex.
fn billing_header(existing_system: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(format!("{}\n{}", CC_SYSTEM_PREFIX, existing_system).as_bytes());
    hasher.update(CC_VERSION.as_bytes());
    let digest = hasher.finalize();
    let tag = format!("{:02x}{:02x}", digest[0], digest[1]);
    format!(
        "x-anthropic-billing-header: cc_version={}.{}; cc_entrypoint=cli; cch=00000;",
        CC_VERSION, tag
    )
}

/// Inject the Claude Code system blocks + metadata.user_id into the payload.
/// Replaces a string `system` with the block array `[billing, prefix(1h cache), ...orig]`.
pub(crate) fn inject_request(payload: &mut Value, account: &UpstreamAccount, user_id: &str) {
    let Some(obj) = payload.as_object_mut() else {
        return;
    };

    // Preserve any existing system content as trailing blocks.
    let existing = obj.remove("system");
    let existing_text = match &existing {
        Some(Value::String(s)) => s.clone(),
        _ => String::new(),
    };
    let mut blocks: Vec<Value> = Vec::new();
    blocks.push(json!({ "type": "text", "text": billing_header(&existing_text) }));
    blocks.push(json!({
        "type": "text",
        "text": CC_SYSTEM_PREFIX,
        "cache_control": { "type": "ephemeral", "ttl": "1h" }
    }));
    match existing {
        Some(Value::String(s)) if !s.trim().is_empty() => {
            blocks.push(json!({ "type": "text", "text": s }));
        }
        Some(Value::Array(arr)) => blocks.extend(arr),
        _ => {}
    }
    obj.insert("system".to_string(), Value::Array(blocks));

    // metadata.user_id carries derived device/session ids + account uuid.
    let device_id = derived_id(user_id, "device");
    let session_id = derived_id(user_id, "session");
    let account_uuid = if account.account_id.trim().is_empty() {
        derived_id(&account.id, "account")
    } else {
        account.account_id.trim().to_string()
    };
    let user_blob = json!({
        "device_id": device_id,
        "account_uuid": account_uuid,
        "session_id": session_id,
    })
    .to_string();
    let metadata = obj
        .entry("metadata".to_string())
        .or_insert_with(|| json!({}));
    if let Some(mobj) = metadata.as_object_mut() {
        mobj.insert("user_id".to_string(), Value::String(user_blob));
    }
}

/// Replace the `cch=00000` placeholder in the serialized body with the xxhash64
/// checksum of the body (Go `ccReplaceCCHPlaceholder`).
pub(crate) fn replace_cch(body: &str) -> String {
    if !body.contains("cch=00000") {
        return body.to_string();
    }
    let mut hasher = twox_hash::XxHash64::with_seed(CCH_SEED);
    hasher.write(body.as_bytes());
    let cch = hasher.finish() & CCH_MASK;
    body.replace("cch=00000", &format!("cch={:05x}", cch))
}

// ---- tool-name obfuscation ----

/// Obfuscate a tool name to `t_<8 hex>` from sha256(name \0 salt_be32) (Go
/// `hashedClaudeToolName`).
fn hashed_tool_name(name: &str, salt: u32) -> String {
    let mut hasher = Sha256::new();
    hasher.update(name.as_bytes());
    hasher.update([0u8]);
    hasher.update(salt.to_be_bytes());
    let digest = hasher.finalize();
    format!("t_{}", crate::util::hex_lower(&digest[..4]))
}

/// Build a deterministic obfuscation map for all tool names referenced in the
/// payload, rewrite them in place, and return `obfuscated -> original` for
/// restoring the response. Covers `tools[].name`, `tool_choice.name`, and
/// `tool_use` blocks in messages.
pub(crate) fn obfuscate_tool_names(payload: &mut Value) -> HashMap<String, String> {
    let mut forward: HashMap<String, String> = HashMap::new(); // original -> obfuscated
    let mut used: std::collections::HashSet<String> = std::collections::HashSet::new();

    let mut assign = |original: &str| -> String {
        if let Some(o) = forward.get(original) {
            return o.clone();
        }
        let mut salt = 0u32;
        loop {
            let candidate = hashed_tool_name(original, salt);
            if !used.contains(&candidate) {
                used.insert(candidate.clone());
                forward.insert(original.to_string(), candidate.clone());
                return candidate;
            }
            salt = salt.wrapping_add(1);
            if salt > 1_000_000 {
                return candidate;
            }
        }
    };

    if let Some(tools) = payload.get_mut("tools").and_then(|t| t.as_array_mut()) {
        for tool in tools {
            if let Some(name) = tool.get("name").and_then(|n| n.as_str()).map(|s| s.to_string()) {
                let obf = assign(&name);
                tool["name"] = Value::String(obf);
            }
        }
    }
    if let Some(name) = payload
        .get("tool_choice")
        .and_then(|tc| tc.get("name"))
        .and_then(|n| n.as_str())
        .map(|s| s.to_string())
    {
        let obf = assign(&name);
        payload["tool_choice"]["name"] = Value::String(obf);
    }
    if let Some(messages) = payload.get_mut("messages").and_then(|m| m.as_array_mut()) {
        for msg in messages {
            if let Some(content) = msg.get_mut("content").and_then(|c| c.as_array_mut()) {
                for block in content {
                    if block.get("type").and_then(|t| t.as_str()) == Some("tool_use") {
                        if let Some(name) = block.get("name").and_then(|n| n.as_str()).map(|s| s.to_string()) {
                            let obf = assign(&name);
                            block["name"] = Value::String(obf);
                        }
                    }
                }
            }
        }
    }

    // Reverse map for response restoration.
    forward.into_iter().map(|(orig, obf)| (obf, orig)).collect()
}

/// Restore obfuscated tool names in a buffered response body. Restores
/// structurally — only the `name` field of `tool_use`-shaped JSON objects —
/// rather than a blanket substring replace, which could corrupt model-generated
/// text that happens to contain an obfuscated name (e.g. the model quoting the
/// tool name it was shown). Handles both a single JSON document and a buffered
/// SSE stream of `data: {...}` lines.
pub(crate) fn restore_tool_names(body: &[u8], reverse: &HashMap<String, String>) -> Vec<u8> {
    if reverse.is_empty() {
        return body.to_vec();
    }
    let text = String::from_utf8_lossy(body);

    // Non-streaming: one JSON document.
    if let Ok(mut v) = serde_json::from_str::<Value>(&text) {
        restore_in_value(&mut v, reverse);
        if let Ok(out) = serde_json::to_string(&v) {
            return out.into_bytes();
        }
    }

    // Streaming: rewrite the JSON payload of each `data: {...}` SSE line;
    // everything else (event lines, blanks, unparseable lines) passes through.
    let mut out = String::with_capacity(text.len());
    for (i, line) in text.split('\n').enumerate() {
        if i > 0 {
            out.push('\n');
        }
        if let Some(rest) = line.strip_prefix("data: ") {
            if let Ok(mut v) = serde_json::from_str::<Value>(rest) {
                restore_in_value(&mut v, reverse);
                if let Ok(s) = serde_json::to_string(&v) {
                    out.push_str("data: ");
                    out.push_str(&s);
                    continue;
                }
            }
        }
        out.push_str(line);
    }
    out.into_bytes()
}

/// Walk a JSON tree and swap restored names into `tool_use` blocks (covers both
/// `message.content[]` and SSE `content_block_start.content_block`).
fn restore_in_value(v: &mut Value, reverse: &HashMap<String, String>) {
    match v {
        Value::Object(map) => {
            let is_tool_use = map
                .get("type")
                .and_then(|t| t.as_str())
                .map(|t| t == "tool_use" || t == "server_tool_use")
                .unwrap_or(false);
            if is_tool_use {
                if let Some(Value::String(name)) = map.get_mut("name") {
                    if let Some(orig) = reverse.get(name.as_str()) {
                        *name = orig.clone();
                    }
                }
            }
            for (_, child) in map.iter_mut() {
                restore_in_value(child, reverse);
            }
        }
        Value::Array(arr) => {
            for child in arr {
                restore_in_value(child, reverse);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derived_id_is_uuid_v4_shaped() {
        let id = derived_id("koltyu", "device");
        assert_eq!(id.len(), 36);
        assert_eq!(&id[14..15], "4"); // version nibble
        assert!(["8", "9", "a", "b"].contains(&&id[19..20]));
        // deterministic
        assert_eq!(id, derived_id("koltyu", "device"));
        assert_ne!(id, derived_id("koltyu", "session"));
    }

    #[test]
    fn cch_replaced_deterministically() {
        let body = "{\"x\":\"cch=00000\"}";
        let out = replace_cch(body);
        assert!(!out.contains("cch=00000"));
        assert!(out.contains("cch="));
        assert_eq!(out, replace_cch(body));
    }

    #[test]
    fn tool_names_roundtrip() {
        let mut payload = json!({
            "tools": [{ "name": "Bash" }, { "name": "Read" }],
            "messages": [{ "role": "assistant", "content": [
                { "type": "tool_use", "name": "Bash", "id": "x" }
            ]}]
        });
        let reverse = obfuscate_tool_names(&mut payload);
        let obf = payload["tools"][0]["name"].as_str().unwrap().to_string();
        assert!(obf.starts_with("t_"));
        // same name -> same obfuscation
        assert_eq!(payload["messages"][0]["content"][0]["name"].as_str().unwrap(), obf);

        // Restores the name on a tool_use block in a JSON response.
        let body = json!({ "content": [{ "type": "tool_use", "name": obf, "id": "x" }] });
        let restored = restore_tool_names(body.to_string().as_bytes(), &reverse);
        let restored: Value = serde_json::from_slice(&restored).unwrap();
        assert_eq!(restored["content"][0]["name"].as_str().unwrap(), "Bash");
    }

    #[test]
    fn restore_targets_tool_use_blocks_only() {
        let mut payload = json!({ "tools": [{ "name": "Bash" }] });
        let reverse = obfuscate_tool_names(&mut payload);
        let obf = payload["tools"][0]["name"].as_str().unwrap().to_string();

        // The model quoting an obfuscated name inside text must NOT be rewritten.
        let body = json!({ "content": [{ "type": "text", "text": format!("called {}", obf) }] });
        let restored = restore_tool_names(body.to_string().as_bytes(), &reverse);
        let restored: Value = serde_json::from_slice(&restored).unwrap();
        assert_eq!(
            restored["content"][0]["text"].as_str().unwrap(),
            format!("called {}", obf)
        );
    }

    #[test]
    fn restore_handles_sse_streams() {
        let mut payload = json!({ "tools": [{ "name": "Bash" }] });
        let reverse = obfuscate_tool_names(&mut payload);
        let obf = payload["tools"][0]["name"].as_str().unwrap().to_string();

        let sse = format!(
            "event: content_block_start\ndata: {}\n\nevent: ping\ndata: {{\"type\": \"ping\"}}\n\n",
            json!({
                "type": "content_block_start",
                "index": 0,
                "content_block": { "type": "tool_use", "id": "x", "name": obf }
            })
        );
        let restored = String::from_utf8(restore_tool_names(sse.as_bytes(), &reverse)).unwrap();
        assert!(restored.contains("\"name\":\"Bash\""));
        assert!(restored.contains("event: ping"));
    }
}
