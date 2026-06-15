//! Cursor provider: a standard HTTP upstream, like the Claude/Codex providers,
//! but speaking Cursor's private Connect-RPC protocol instead of plain JSON.
//!
//! A request is encoded as a protobuf `StreamUnifiedChatWithToolsRequest`, wrapped
//! in Connect length-prefixed frames (optionally gzipped), and POSTed to
//! `api2.cursor.sh` with the `WorkosCursorSessionToken` as a Bearer credential
//! plus an anti-abuse `x-cursor-checksum` header. The streamed response is a
//! sequence of the same frames carrying protobuf chunks; we decode them, collect
//! the assistant text, and render it back in the client's chat format.
//!
//! This is reverse-engineered from the Cursor IDE's traffic (see the open-source
//! cursor2api/Cursor-To-OpenAI projects); the protobuf field layout and checksum
//! algorithm can change when Cursor updates, in which case they must be re-synced.
use crate::prelude::*;
use crate::auth::jwt_exp;
use crate::pool::storage::persist_all_accounts;
use crate::util::expand_home;
use crate::util::http_client;
use crate::util::truncate_text;
use crate::util::hex_lower;
use base64::Engine;
use flate2::read::GzDecoder;
use flate2::write::GzEncoder;
use flate2::Compression;
use prost::Message as ProstMessage;
use sha2::{Digest, Sha256};
use std::io::{Read, Write};

pub(crate) const CURSOR_UPSTREAM: &str =
    "https://api2.cursor.sh/aiserver.v1.ChatService/StreamUnifiedChatWithTools";
const CURSOR_CLIENT_VERSION: &str = "2.6.21";

/// Shared client for the Cursor upstream (connection pooling + its own timeout,
/// configurable via CURSOR_TIMEOUT_SECS, default 120s).
pub(crate) fn cursor_http_client() -> &'static reqwest::Client {
    static CLIENT: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    CLIENT.get_or_init(|| {
        let timeout_secs = std::env::var("CURSOR_TIMEOUT_SECS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .filter(|v| *v > 0)
            .unwrap_or(120);
        reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(10))
            .timeout(std::time::Duration::from_secs(timeout_secs))
            .build()
            .expect("failed building cursor http client")
    })
}

// ---------------------------------------------------------------------------
// Model routing
// ---------------------------------------------------------------------------

/// Returns true if the requested model should be served by the Cursor upstream.
/// Models use the `cursor/<upstream-model>` form (the prefix is stripped before
/// the upstream call); a bare `cursor` selects the agent's auto model.
pub(crate) fn is_cursor_model(model: &str) -> bool {
    let m = model.trim().to_ascii_lowercase();
    m == "cursor" || m.starts_with("cursor/")
}

/// Maps a gateway model name to the upstream Cursor model name. The bare
/// `cursor` slug selects the upstream's auto option, which its model list
/// names `default` (`auto` is rejected with ERROR_BAD_MODEL_NAME).
pub(crate) fn cursor_canonical_model(model: &str) -> String {
    let m = model.trim();
    if m.eq_ignore_ascii_case("cursor") {
        return "default".to_string();
    }
    if let Some(rest) = m.strip_prefix("cursor/").or_else(|| m.strip_prefix("Cursor/")) {
        return rest.to_string();
    }
    m.to_string()
}

// ---------------------------------------------------------------------------
// Request-format handling (shared with the response renderers below)
// ---------------------------------------------------------------------------

/// Detected request format for message extraction and response rendering.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CursorFormat {
    /// OpenAI Chat Completions (`/v1/chat/completions`).
    OpenAI,
    /// Anthropic Messages (`/v1/messages`).
    Claude,
    /// OpenAI Responses API (`/v1/responses`).
    Responses,
}

/// A single chat turn extracted from the request, normalized across formats.
#[derive(Debug, Clone)]
pub(crate) struct ChatTurn {
    /// `1` = user, `2` = assistant (the Cursor protobuf role encoding).
    pub(crate) role: i32,
    pub(crate) content: String,
}

/// Normalized request: the system instruction (joined), the conversation turns,
/// and whether the client asked for streaming.
pub(crate) struct ExtractedRequest {
    pub(crate) instruction: String,
    pub(crate) turns: Vec<ChatTurn>,
    pub(crate) stream: bool,
}

/// Extracts the system instruction and conversation turns from an OpenAI /
/// Anthropic / Responses payload into a single normalized shape.
pub(crate) fn extract_request(payload: &Value) -> Result<ExtractedRequest, String> {
    let obj = payload.as_object().ok_or("request body is not a JSON object")?;
    let stream = obj.get("stream").and_then(|v| v.as_bool()).unwrap_or(false);

    let mut instructions: Vec<String> = Vec::new();
    let mut turns: Vec<ChatTurn> = Vec::new();

    // Top-level system / instructions (Claude `system`, Responses `instructions`).
    match obj.get("system") {
        Some(Value::String(s)) if !s.is_empty() => instructions.push(s.clone()),
        Some(Value::Array(parts)) => {
            let t = text_from_parts(parts);
            if !t.is_empty() {
                instructions.push(t);
            }
        }
        _ => {}
    }
    if let Some(Value::String(s)) = obj.get("instructions") {
        if !s.is_empty() {
            instructions.push(s.clone());
        }
    }

    let mut push_msg = |role: &str, content: String| {
        if content.is_empty() {
            return;
        }
        match role {
            "system" | "developer" => instructions.push(content),
            "assistant" => turns.push(ChatTurn { role: 2, content }),
            _ => turns.push(ChatTurn { role: 1, content }),
        }
    };

    if let Some(messages) = obj.get("messages").and_then(|v| v.as_array()) {
        for m in messages {
            let Some(msg) = m.as_object() else { continue };
            let role = msg.get("role").and_then(|v| v.as_str()).unwrap_or("user");
            let content = match msg.get("content") {
                Some(Value::String(s)) => s.clone(),
                Some(Value::Array(parts)) => text_from_parts(parts),
                _ => String::new(),
            };
            push_msg(role, content);
        }
    }

    // Responses API `input` (string or array of message items).
    match obj.get("input") {
        Some(Value::String(s)) => push_msg("user", s.clone()),
        Some(Value::Array(items)) => {
            for item in items {
                let Some(it) = item.as_object() else { continue };
                let role = it.get("role").and_then(|v| v.as_str()).unwrap_or("user");
                let content = match it.get("content") {
                    Some(Value::String(s)) => s.clone(),
                    Some(Value::Array(parts)) => text_from_parts(parts),
                    _ => String::new(),
                };
                push_msg(role, content);
            }
        }
        _ => {}
    }

    if turns.is_empty() {
        return Err("no user/assistant messages found in request".to_string());
    }
    Ok(ExtractedRequest {
        instruction: instructions.join("\n"),
        turns,
        stream,
    })
}

/// Concatenates text from an OpenAI/Anthropic/Responses content-part array,
/// skipping non-text parts (images, tool calls/results).
fn text_from_parts(parts: &[Value]) -> String {
    let mut out: Vec<&str> = Vec::new();
    for p in parts {
        if let Some(s) = p.as_str() {
            if !s.is_empty() {
                out.push(s);
            }
            continue;
        }
        let Some(part) = p.as_object() else { continue };
        if let Some(t) = part.get("type").and_then(|v| v.as_str()) {
            if !matches!(t, "text" | "input_text" | "output_text") {
                continue;
            }
        }
        if let Some(text) = part.get("text").and_then(|v| v.as_str()) {
            if !text.is_empty() {
                out.push(text);
            }
        }
    }
    out.join("\n")
}

// ---------------------------------------------------------------------------
// Protobuf wire types (hand-written prost messages — no protoc needed).
// Field tags mirror the Cursor IDE schema; the many fixed "unknownN" fields are
// constants the upstream expects, ported from the reference implementations.
// ---------------------------------------------------------------------------

#[derive(Clone, PartialEq, ProstMessage)]
pub(crate) struct StreamRequest {
    #[prost(message, optional, tag = "1")]
    pub(crate) request: Option<InnerRequest>,
}

#[derive(Clone, PartialEq, ProstMessage)]
pub(crate) struct InnerRequest {
    #[prost(message, repeated, tag = "1")]
    pub(crate) messages: Vec<PbMessage>,
    #[prost(int32, tag = "2")]
    pub(crate) unknown2: i32,
    #[prost(message, optional, tag = "3")]
    pub(crate) instruction: Option<Instruction>,
    #[prost(int32, tag = "4")]
    pub(crate) unknown4: i32,
    #[prost(message, optional, tag = "5")]
    pub(crate) model: Option<Model>,
    #[prost(string, tag = "8")]
    pub(crate) web_tool: String,
    #[prost(int32, tag = "13")]
    pub(crate) unknown13: i32,
    #[prost(message, optional, tag = "15")]
    pub(crate) cursor_setting: Option<CursorSetting>,
    #[prost(int32, tag = "19")]
    pub(crate) unknown19: i32,
    #[prost(string, tag = "23")]
    pub(crate) conversation_id: String,
    #[prost(message, optional, tag = "26")]
    pub(crate) metadata: Option<Metadata>,
    #[prost(int32, tag = "27")]
    pub(crate) unknown27: i32,
    #[prost(message, repeated, tag = "30")]
    pub(crate) message_ids: Vec<MessageId>,
    #[prost(int32, tag = "35")]
    pub(crate) large_context: i32,
    #[prost(int32, tag = "38")]
    pub(crate) unknown38: i32,
    #[prost(int32, tag = "46")]
    pub(crate) chat_mode_enum: i32,
    #[prost(string, tag = "47")]
    pub(crate) unknown47: String,
    #[prost(int32, tag = "48")]
    pub(crate) unknown48: i32,
    #[prost(int32, tag = "49")]
    pub(crate) unknown49: i32,
    #[prost(int32, tag = "51")]
    pub(crate) unknown51: i32,
    #[prost(int32, tag = "53")]
    pub(crate) unknown53: i32,
    #[prost(string, tag = "54")]
    pub(crate) chat_mode: String,
}

#[derive(Clone, PartialEq, ProstMessage)]
pub(crate) struct PbMessage {
    #[prost(string, tag = "1")]
    pub(crate) content: String,
    #[prost(int32, tag = "2")]
    pub(crate) role: i32,
    #[prost(string, tag = "13")]
    pub(crate) message_id: String,
    #[prost(int32, tag = "47")]
    pub(crate) chat_mode_enum: i32,
}

#[derive(Clone, PartialEq, ProstMessage)]
pub(crate) struct Instruction {
    #[prost(string, tag = "1")]
    pub(crate) instruction: String,
}

#[derive(Clone, PartialEq, ProstMessage)]
pub(crate) struct Model {
    #[prost(string, tag = "1")]
    pub(crate) name: String,
    #[prost(bytes = "vec", tag = "4")]
    pub(crate) empty: Vec<u8>,
}

#[derive(Clone, PartialEq, ProstMessage)]
pub(crate) struct CursorSetting {
    #[prost(string, tag = "1")]
    pub(crate) name: String,
    #[prost(bytes = "vec", tag = "3")]
    pub(crate) unknown3: Vec<u8>,
    #[prost(message, optional, tag = "6")]
    pub(crate) unknown6: Option<CursorSettingInner>,
    #[prost(int32, tag = "8")]
    pub(crate) unknown8: i32,
    #[prost(int32, tag = "9")]
    pub(crate) unknown9: i32,
}

#[derive(Clone, PartialEq, ProstMessage)]
pub(crate) struct CursorSettingInner {
    #[prost(bytes = "vec", tag = "1")]
    pub(crate) unknown1: Vec<u8>,
    #[prost(bytes = "vec", tag = "2")]
    pub(crate) unknown2: Vec<u8>,
}

#[derive(Clone, PartialEq, ProstMessage)]
pub(crate) struct Metadata {
    #[prost(string, tag = "1")]
    pub(crate) os: String,
    #[prost(string, tag = "2")]
    pub(crate) arch: String,
    #[prost(string, tag = "3")]
    pub(crate) version: String,
    #[prost(string, tag = "4")]
    pub(crate) path: String,
    #[prost(string, tag = "5")]
    pub(crate) timestamp: String,
}

#[derive(Clone, PartialEq, ProstMessage)]
pub(crate) struct MessageId {
    #[prost(string, tag = "1")]
    pub(crate) message_id: String,
    #[prost(string, tag = "2")]
    pub(crate) summary_id: String,
    #[prost(int32, tag = "3")]
    pub(crate) role: i32,
}

#[derive(Clone, PartialEq, ProstMessage)]
pub(crate) struct StreamResponse {
    #[prost(message, optional, tag = "2")]
    pub(crate) message: Option<RespMessage>,
}

#[derive(Clone, PartialEq, ProstMessage)]
pub(crate) struct RespMessage {
    #[prost(string, tag = "1")]
    pub(crate) content: String,
    #[prost(message, optional, tag = "25")]
    pub(crate) thinking: Option<MessageThinking>,
}

#[derive(Clone, PartialEq, ProstMessage)]
pub(crate) struct MessageThinking {
    #[prost(string, tag = "1")]
    pub(crate) content: String,
}

/// Builds the protobuf request body for a normalized request.
pub(crate) fn build_request_proto(model: &str, req: &ExtractedRequest) -> StreamRequest {
    let messages: Vec<PbMessage> = req
        .turns
        .iter()
        .map(|t| PbMessage {
            content: t.content.clone(),
            role: t.role,
            message_id: Uuid::new_v4().to_string(),
            chat_mode_enum: if t.role == 1 { 1 } else { 0 },
        })
        .collect();
    let message_ids: Vec<MessageId> = messages
        .iter()
        .map(|m| MessageId {
            message_id: m.message_id.clone(),
            summary_id: String::new(),
            role: m.role,
        })
        .collect();

    StreamRequest {
        request: Some(InnerRequest {
            messages,
            unknown2: 1,
            instruction: Some(Instruction {
                instruction: req.instruction.clone(),
            }),
            unknown4: 1,
            model: Some(Model {
                name: model.to_string(),
                empty: Vec::new(),
            }),
            web_tool: String::new(),
            unknown13: 1,
            cursor_setting: Some(CursorSetting {
                name: "cursor\\aisettings".to_string(),
                unknown3: Vec::new(),
                unknown6: Some(CursorSettingInner {
                    unknown1: Vec::new(),
                    unknown2: Vec::new(),
                }),
                unknown8: 1,
                unknown9: 1,
            }),
            unknown19: 1,
            conversation_id: Uuid::new_v4().to_string(),
            metadata: Some(Metadata {
                os: "win32".to_string(),
                arch: "x64".to_string(),
                version: "10.0.22631".to_string(),
                path: "C:\\Program Files\\PowerShell\\7\\pwsh.exe".to_string(),
                timestamp: Utc::now().to_rfc3339(),
            }),
            unknown27: 0,
            message_ids,
            large_context: 0,
            unknown38: 0,
            chat_mode_enum: 1,
            unknown47: String::new(),
            unknown48: 0,
            unknown49: 0,
            unknown51: 0,
            unknown53: 1,
            chat_mode: "Ask".to_string(),
        }),
    }
}

// ---------------------------------------------------------------------------
// Connect-RPC envelope framing
// ---------------------------------------------------------------------------

/// Wraps a protobuf payload in a Connect frame: `[flag:1][len:u32 BE][payload]`.
/// Payloads are gzipped (flag `1`) once the conversation is long enough, matching
/// the reference clients (threshold: 3+ turns).
pub(crate) fn encode_frame(payload: &[u8], gzip: bool) -> Vec<u8> {
    // On any compression failure fall back to an UNCOMPRESSED frame (flag 0).
    // Falling back to raw bytes while keeping flag 1 would hand the upstream a
    // frame it can't gunzip.
    let (flag, body): (u8, Vec<u8>) = if gzip {
        let mut enc = GzEncoder::new(Vec::new(), Compression::default());
        match enc.write_all(payload).and_then(|_| enc.finish()) {
            Ok(compressed) => (1, compressed),
            Err(_) => (0, payload.to_vec()),
        }
    } else {
        (0, payload.to_vec())
    };
    let mut out = Vec::with_capacity(5 + body.len());
    out.push(flag);
    out.extend_from_slice(&(body.len() as u32).to_be_bytes());
    out.extend_from_slice(&body);
    out
}

/// Decoded result of a full Connect response stream.
#[derive(Default)]
pub(crate) struct DecodedResponse {
    pub(crate) text: String,
    /// A JSON control/error frame (flags 2/3), if the upstream sent one.
    pub(crate) error_json: Option<String>,
}

/// Parses a complete buffered Connect response stream into assistant text.
/// Frame flags: `0`/`1` = protobuf (1 = gzipped), `2`/`3` = JSON control/error
/// (3 = gzipped). Malformed trailing bytes are ignored.
///
/// Some models (composer-*) stream the ENTIRE reply through the thinking
/// channel (`RespMessage.thinking`, field 25), terminating the reasoning with
/// a literal `</think>` marker followed by the visible answer — `content`
/// stays empty the whole stream. When that happens, the text after the marker
/// is the answer.
pub(crate) fn decode_response(buf: &[u8]) -> DecodedResponse {
    let mut out = DecodedResponse::default();
    let mut thinking = String::new();
    let mut i = 0usize;
    while i + 5 <= buf.len() {
        let flag = buf[i];
        let len = u32::from_be_bytes([buf[i + 1], buf[i + 2], buf[i + 3], buf[i + 4]]) as usize;
        let start = i + 5;
        let end = start + len;
        if end > buf.len() {
            break;
        }
        let data = &buf[start..end];
        match flag {
            0 | 1 => {
                let decoded = if flag == 1 { gunzip(data) } else { Some(data.to_vec()) };
                if let Some(bytes) = decoded {
                    tracing::debug!(
                        "cursor frame flag={} len={} b64={}",
                        flag,
                        bytes.len(),
                        base64::engine::general_purpose::STANDARD.encode(&bytes)
                    );
                    if let Ok(resp) = StreamResponse::decode(bytes.as_slice()) {
                        if let Some(msg) = resp.message {
                            if !msg.content.is_empty() {
                                out.text.push_str(&msg.content);
                            }
                            if let Some(t) = msg.thinking {
                                thinking.push_str(&t.content);
                            }
                        }
                    }
                }
            }
            2 | 3 => {
                let decoded = if flag == 3 { gunzip(data) } else { Some(data.to_vec()) };
                if let Some(bytes) = decoded {
                    let s = String::from_utf8_lossy(&bytes).trim().to_string();
                    // Empty JSON ("{}") is a benign end-of-stream marker.
                    if !s.is_empty() && s != "{}" {
                        out.error_json = Some(s);
                    }
                }
            }
            _ => {}
        }
        i = end;
    }
    if out.text.is_empty() && !thinking.is_empty() {
        // Marker vocabulary varies by model: composer-* ends reasoning with
        // `</think>`, the `default` (auto) model with `<｜final｜>`. Take the
        // text after the last marker; with no marker, surface the whole
        // stream — better than nothing.
        let mut answer = thinking.as_str();
        for marker in ["</think>", "<｜final｜>"] {
            if let Some(idx) = answer.rfind(marker) {
                answer = &answer[idx + marker.len()..];
            }
        }
        out.text = answer.trim().to_string();
    }
    out
}

fn gunzip(data: &[u8]) -> Option<Vec<u8>> {
    let mut dec = GzDecoder::new(data);
    let mut out = Vec::new();
    dec.read_to_end(&mut out).ok().map(|_| out)
}

// ---------------------------------------------------------------------------
// Auth: token normalization + anti-abuse checksum
// ---------------------------------------------------------------------------

/// Normalizes a stored `WorkosCursorSessionToken` to the raw JWT the upstream
/// expects. The cookie value is often `user_xxx::<jwt>` (or URL-encoded
/// `%3A%3A`); only the part after the separator is the bearer token.
pub(crate) fn normalize_token(raw: &str) -> String {
    let t = raw.trim();
    if let Some(idx) = t.find("%3A%3A") {
        return t[idx + 6..].to_string();
    }
    if let Some(idx) = t.find("::") {
        return t[idx + 2..].to_string();
    }
    t.to_string()
}

/// Expiry of a Cursor session token, read from the `exp` claim of the embedded
/// JWT (`WorkosCursorSessionToken` is `user_xxx::<jwt>`). Cursor exposes no OAuth
/// refresh flow the way Codex/Claude do — the JWT is presented directly — so the
/// only health signal we have is when it lapses.
pub(crate) fn cursor_token_expiry_from_str(raw: &str) -> Option<DateTime<Utc>> {
    jwt_exp(&normalize_token(raw))
}

/// The Cursor account id (`user_xxx`) needed for the usage endpoints' `?user=`
/// query and `WorkosCursorSessionToken` cookie. Taken from the token's
/// `user_xxx::<jwt>` prefix if present, otherwise from the JWT `sub` claim
/// (`auth0|user_xxx`) — the `/local` import stores only the bare JWT.
pub(crate) fn cursor_session_user_id(raw: &str) -> Option<String> {
    let t = raw.trim();
    for sep in ["%3A%3A", "::"] {
        if let Some(idx) = t.find(sep) {
            let prefix = t[..idx].trim();
            if prefix.starts_with("user_") {
                return Some(prefix.to_string());
            }
        }
    }
    let sub = crate::auth::jwt_claim_str(&normalize_token(t), "sub")?;
    sub.split('|')
        .find(|p| p.starts_with("user_"))
        .or_else(|| Some(sub.as_str()).filter(|s| s.starts_with("user_")))
        .map(|s| s.to_string())
}

/// The `WorkosCursorSessionToken` cookie value (`user_xxx%3A%3A<jwt>`) the usage
/// endpoints authenticate with.
pub(crate) fn cursor_usage_cookie(user_id: &str, jwt: &str) -> String {
    format!("WorkosCursorSessionToken={}%3A%3A{}", user_id, jwt)
}

/// Fetch the account's available Cursor models via the Connect-RPC
/// `AvailableModels` endpoint. Each upstream model `m` is exposed to clients as
/// `cursor/<m>` (the prefix is stripped by `cursor_canonical_model` before the
/// upstream call); a bare `cursor` (= auto) is prepended as the default.
pub(crate) async fn fetch_cursor_models(account: &UpstreamAccount) -> Result<Vec<ModelInfo>, String> {
    let jwt = normalize_token(account.access_token.trim());
    if jwt.is_empty() {
        return Err("cursor account has empty session token".to_string());
    }
    let client = http_client();
    let resp = client
        .post("https://api2.cursor.sh/aiserver.v1.AiService/AvailableModels")
        .bearer_auth(&jwt)
        .header("Connect-Protocol-Version", "1")
        .header(CONTENT_TYPE, "application/json")
        .json(&json!({}))
        .send()
        .await
        .map_err(|e| format!("failed to fetch cursor models: {}", e))?;
    let status = resp.status();
    let body = resp
        .text()
        .await
        .map_err(|e| format!("failed to read cursor models response: {}", e))?;
    if !status.is_success() {
        return Err(format!(
            "cursor models api error {}: {}",
            status.as_u16(),
            truncate_text(&body, 300)
        ));
    }
    let value: Value =
        serde_json::from_str(&body).map_err(|e| format!("invalid cursor models response: {}", e))?;
    let arr = value
        .get("models")
        .and_then(|v| v.as_array())
        .ok_or_else(|| "cursor models response missing `models` field".to_string())?;

    // `cursor` (auto) first, then one entry per upstream model.
    let mut out = vec![ModelInfo {
        slug: "cursor".to_string(),
        display_name: "cursor (auto)".to_string(),
    }];
    for item in arr {
        let name = item.get("name").and_then(|v| v.as_str()).unwrap_or("").trim();
        if name.is_empty() || name == "default" {
            continue; // `default` is the bare `cursor` auto option above.
        }
        let display = item
            .get("clientDisplayName")
            .and_then(|v| v.as_str())
            .filter(|s| !s.trim().is_empty())
            .unwrap_or(name);
        out.push(ModelInfo {
            slug: format!("cursor/{}", name),
            display_name: display.to_string(),
        });
    }
    Ok(out)
}

/// Whether a Cursor account's session token has lapsed. Prefers the stored
/// `expires_at` (set at connect), falling back to parsing the token live.
pub(crate) fn cursor_account_expired(account: &UpstreamAccount, now: DateTime<Utc>) -> bool {
    let exp = account
        .runtime
        .expires_at
        .or_else(|| cursor_token_expiry_from_str(&account.access_token));
    match exp {
        Some(exp) => now >= exp,
        // Unknown expiry (non-JWT token): treat as alive — we have no signal.
        None => false,
    }
}

/// Default per-OS location of Cursor's `state.vscdb`.
fn cursor_local_db_path() -> String {
    if cfg!(target_os = "macos") {
        expand_home("~/Library/Application Support/Cursor/User/globalStorage/state.vscdb")
    } else if cfg!(target_os = "windows") {
        match std::env::var("APPDATA") {
            Ok(appdata) => format!("{}\\Cursor\\User\\globalStorage\\state.vscdb", appdata),
            Err(_) => expand_home("~/AppData/Roaming/Cursor/User/globalStorage/state.vscdb"),
        }
    } else {
        expand_home("~/.config/Cursor/User/globalStorage/state.vscdb")
    }
}

/// Best-effort recovery for an expired Cursor account on a single-host deploy:
/// read a fresh `cursorAuth/accessToken` straight out of the locally signed-in
/// Cursor's SQLite store. Returns true only when a newer, non-expired token was
/// found and persisted. (On a server with no local Cursor this simply no-ops.)
pub(crate) async fn try_reimport_cursor_from_local(
    state: &AppState,
    account: &UpstreamAccount,
) -> bool {
    let db_path = cursor_local_db_path();
    if !std::path::Path::new(&db_path).exists() {
        return false;
    }
    // Single-flight per account id (same map as the OAuth refreshes): the
    // health probe and a reactive caller can race here, and the loser would
    // re-run the sqlite read + persist for nothing.
    let lock = {
        let mut locks = state.refresh_locks.lock().await;
        locks
            .entry(account.id.clone())
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone()
    };
    let _guard = lock.lock().await;
    let output = match tokio::process::Command::new("sqlite3")
        .arg("-cmd")
        .arg(".timeout 3000")
        .arg(&db_path)
        .arg("SELECT value FROM ItemTable WHERE key='cursorAuth/accessToken';")
        .output()
        .await
    {
        Ok(o) if o.status.success() => o,
        _ => return false,
    };
    let fresh = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if fresh.is_empty() || fresh == account.access_token.trim() {
        return false;
    }
    let now = Utc::now();
    let fresh_exp = cursor_token_expiry_from_str(&fresh);
    // Only adopt a token that is actually still valid.
    if matches!(fresh_exp, Some(exp) if exp <= now) {
        return false;
    }
    {
        let mut accounts = state.accounts.write().await;
        let Some(a) = accounts.iter_mut().find(|a| a.id == account.id) else {
            return false;
        };
        a.access_token = fresh;
        a.runtime.expires_at = fresh_exp;
        // Fresh credentials revive the account, same as a manual re-import
        // (`upsert_account` resets `dead` for the same reason).
        a.runtime.dead = false;
        a.runtime.penalty = 0.0;
    }
    persist_all_accounts(state).await.is_ok()
}

fn hashed64_hex(input: &str, salt: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(input.as_bytes());
    hasher.update(salt.as_bytes());
    hex_lower(&hasher.finalize())
}

/// Generates the `x-cursor-checksum` header value. Ported verbatim from the
/// reference clients: a timestamp obfuscation prefix plus two salted SHA-256
/// device hashes. `now_ms` is injected for testability.
pub(crate) fn generate_checksum(token: &str, now_ms: i64) -> String {
    let machine_id = hashed64_hex(token, "machineId");
    let mac_machine_id = hashed64_hex(token, "macMachineId");

    // NB: the reference uses JS `>>`, whose shift amount is taken mod 32, so the
    // `40`/`32` shifts behave as `8`/`0`. We replicate that exactly.
    let ts = (now_ms / 1_000_000) as i32;
    let shifts = [40u32, 32, 24, 16, 8, 0];
    let mut bytes = [0u8; 6];
    for (i, &sh) in shifts.iter().enumerate() {
        bytes[i] = ((ts >> (sh & 31)) & 255) as u8;
    }
    // Rolling XOR + index obfuscation (values wrap to u8 as in a JS Uint8Array).
    let mut t: i32 = 165;
    for (r, b) in bytes.iter_mut().enumerate() {
        let x = ((*b as i32) ^ t) + (r as i32 % 256);
        *b = (x & 255) as u8;
        t = *b as i32;
    }
    let encoded = base64::engine::general_purpose::STANDARD.encode(bytes);
    format!("{encoded}{machine_id}/{mac_machine_id}")
}

// ---------------------------------------------------------------------------
// Upstream call
// ---------------------------------------------------------------------------

/// Outcome of a Cursor upstream call.
pub(crate) struct CursorResult {
    pub(crate) text: String,
    pub(crate) status: reqwest::StatusCode,
    /// Error detail from an HTTP error or a JSON control frame, if any.
    pub(crate) error: Option<String>,
}

/// Sends one request to the Cursor upstream for `account`, returning the
/// assistant text. Mirrors the Claude/Codex `send_*_upstream` helpers: build
/// headers from the account credential, POST, and surface status + body.
pub(crate) async fn send_cursor_upstream(
    client: &reqwest::Client,
    account: &UpstreamAccount,
    model: &str,
    req: &ExtractedRequest,
    now_ms: i64,
) -> Result<CursorResult, String> {
    let token = normalize_token(&account.access_token);
    if token.is_empty() {
        return Err("cursor account has no session token".to_string());
    }
    let checksum = generate_checksum(&token, now_ms);
    let client_key = hashed64_hex(&token, "");
    let session_id = Uuid::new_v4().to_string();
    let config_version = Uuid::new_v4().to_string();

    let proto = build_request_proto(model, req);
    let payload = proto.encode_to_vec();
    let gzip = req.turns.len() >= 3;
    let frame = encode_frame(&payload, gzip);

    let resp = client
        .post(CURSOR_UPSTREAM)
        .header("authorization", format!("Bearer {token}"))
        .header("connect-accept-encoding", "gzip")
        .header("connect-content-encoding", "gzip")
        .header("connect-protocol-version", "1")
        .header(CONTENT_TYPE, "application/connect+proto")
        .header("user-agent", "connect-es/1.6.1")
        .header("x-client-key", client_key)
        .header("x-cursor-checksum", checksum)
        .header("x-cursor-client-version", CURSOR_CLIENT_VERSION)
        .header("x-cursor-config-version", config_version)
        .header("x-cursor-timezone", "Asia/Shanghai")
        .header("x-ghost-mode", "true")
        .header("x-request-id", Uuid::new_v4().to_string())
        .header("x-session-id", session_id)
        .body(frame)
        .send()
        .await
        .map_err(|e| format!("cursor upstream request failed: {e}"))?;

    let status = resp.status();
    let bytes = resp
        .bytes()
        .await
        .map_err(|e| format!("reading cursor upstream body failed: {e}"))?;

    if !status.is_success() {
        let detail = String::from_utf8_lossy(&bytes).trim().to_string();
        return Ok(CursorResult {
            text: String::new(),
            status,
            error: Some(if detail.is_empty() {
                format!("cursor upstream returned {status}")
            } else {
                detail
            }),
        });
    }

    let decoded = decode_response(&bytes);
    Ok(CursorResult {
        text: decoded.text,
        status,
        error: decoded.error_json,
    })
}

/// Relay-path adapter (`POST /v1/gateway/relay`): one user prompt in, plain
/// assistant text out. Counterpart of `call_codex_responses_api` /
/// `call_claude_messages_api`. Cursor session tokens have no refresh flow, so
/// unlike those there's no refresh-and-retry step.
pub(crate) async fn call_cursor_relay_api(
    account: &UpstreamAccount,
    payload: &RelayRequest,
) -> Result<UpstreamCallResult, UpstreamCallError> {
    let req = ExtractedRequest {
        instruction: String::new(),
        turns: vec![ChatTurn {
            role: 1,
            content: payload.prompt.clone(),
        }],
        stream: false,
    };
    let model = cursor_canonical_model(&payload.model);
    let result = send_cursor_upstream(
        cursor_http_client(),
        account,
        &model,
        &req,
        Utc::now().timestamp_millis(),
    )
    .await
    .map_err(|message| UpstreamCallError {
        message,
        rate_limit_snapshot: None,
    })?;

    // Same failure shape as the proxy path: an HTTP error status, or a JSON
    // control frame carrying an error with no assistant text.
    if !result.status.is_success() || (result.text.is_empty() && result.error.is_some()) {
        let detail = result
            .error
            .unwrap_or_else(|| format!("cursor upstream returned {}", result.status));
        return Err(UpstreamCallError {
            message: format!("cursor upstream error: {}", truncate_text(&detail, 500)),
            rate_limit_snapshot: None,
        });
    }
    Ok(UpstreamCallResult {
        output_text: result.text,
        rate_limit_snapshot: None,
    })
}

/// Reports whether an upstream error string indicates a usage/rate limit.
pub(crate) fn looks_rate_limited(s: &str) -> bool {
    let l = s.to_ascii_lowercase();
    l.contains("rate limit")
        || l.contains("usage limit")
        || l.contains("quota")
        || l.contains("too many requests")
        || l.contains("resource_exhausted")
}

// ---------------------------------------------------------------------------
// Response rendering (OpenAI / Claude / Responses)
// ---------------------------------------------------------------------------

/// CJK-aware token estimate for Cursor traffic (its protocol returns no usage
/// data, so usage fields are synthesized): ASCII text averages ~4 chars/token
/// while CJK runs ~1 token per char — a flat chars/4 undercounts Chinese ~4x.
pub(crate) fn estimate_text_tokens(text: &str) -> u64 {
    let (ascii, other) = text
        .chars()
        .fold((0u64, 0u64), |(a, o), c| if c.is_ascii() { (a + 1, o) } else { (a, o + 1) });
    (ascii / 4) + other
}

/// Token estimate over the whole extracted request (instruction + turns).
pub(crate) fn estimate_request_tokens(req: &ExtractedRequest) -> u64 {
    estimate_text_tokens(&req.instruction)
        + req
            .turns
            .iter()
            .map(|t| estimate_text_tokens(&t.content))
            .sum::<u64>()
}

/// Builds a complete non-streaming chat response body for the client format.
pub(crate) fn build_buffered_body(
    format: CursorFormat,
    request_id: &str,
    model: &str,
    text: &str,
    input_tokens: u64,
) -> Value {
    let created = Utc::now().timestamp();
    let output_tokens = estimate_text_tokens(text);
    match format {
        CursorFormat::Claude => json!({
            "id": format!("msg_{request_id}"),
            "type": "message",
            "role": "assistant",
            "model": model,
            "content": [{ "type": "text", "text": text }],
            "stop_reason": "end_turn",
            "stop_sequence": null,
            "usage": { "input_tokens": input_tokens, "output_tokens": output_tokens },
        }),
        CursorFormat::OpenAI => json!({
            "id": format!("chatcmpl-{request_id}"),
            "object": "chat.completion",
            "created": created,
            "model": model,
            "choices": [{
                "index": 0,
                "message": { "role": "assistant", "content": text },
                "finish_reason": "stop",
            }],
            "usage": {
                "prompt_tokens": input_tokens,
                "completion_tokens": output_tokens,
                "total_tokens": input_tokens + output_tokens,
            },
        }),
        CursorFormat::Responses => json!({
            "id": format!("resp_{request_id}"),
            "object": "response",
            "created_at": created,
            "model": model,
            "status": "completed",
            "output": [{
                "id": format!("msg_{request_id}"),
                "type": "message",
                "role": "assistant",
                "status": "completed",
                "content": [{ "type": "output_text", "text": text, "annotations": [] }],
            }],
            "usage": {
                "input_tokens": input_tokens,
                "output_tokens": output_tokens,
                "total_tokens": input_tokens + output_tokens,
            },
        }),
    }
}

/// Builds a complete SSE body for streaming clients. Because the gateway buffers
/// the upstream response, we emit the whole stream at once: start frames, one
/// text delta carrying the full reply, and the terminal frames.
pub(crate) fn build_sse_body(
    format: CursorFormat,
    request_id: &str,
    model: &str,
    text: &str,
    input_tokens: u64,
) -> String {
    let created = Utc::now().timestamp();
    let output_tokens = estimate_text_tokens(text);
    let mut out = String::new();
    let push = |out: &mut String, event: Option<&str>, data: &Value| {
        if let Some(ev) = event {
            out.push_str(&format!("event: {ev}\n"));
        }
        out.push_str(&format!("data: {}\n\n", data));
    };

    match format {
        CursorFormat::Claude => {
            push(
                &mut out,
                Some("message_start"),
                &json!({
                    "type": "message_start",
                    "message": {
                        "id": format!("msg_{request_id}"),
                        "type": "message",
                        "role": "assistant",
                        "model": model,
                        "content": [],
                        "stop_reason": null,
                        "stop_sequence": null,
                        "usage": { "input_tokens": input_tokens, "output_tokens": 0 },
                    },
                }),
            );
            push(
                &mut out,
                Some("content_block_start"),
                &json!({
                    "type": "content_block_start",
                    "index": 0,
                    "content_block": { "type": "text", "text": "" },
                }),
            );
            if !text.is_empty() {
                push(
                    &mut out,
                    Some("content_block_delta"),
                    &json!({
                        "type": "content_block_delta",
                        "index": 0,
                        "delta": { "type": "text_delta", "text": text },
                    }),
                );
            }
            push(
                &mut out,
                Some("content_block_stop"),
                &json!({ "type": "content_block_stop", "index": 0 }),
            );
            push(
                &mut out,
                Some("message_delta"),
                &json!({
                    "type": "message_delta",
                    "delta": { "stop_reason": "end_turn", "stop_sequence": null },
                    "usage": { "output_tokens": output_tokens },
                }),
            );
            push(
                &mut out,
                Some("message_stop"),
                &json!({ "type": "message_stop" }),
            );
        }
        CursorFormat::OpenAI => {
            if !text.is_empty() {
                push(
                    &mut out,
                    None,
                    &json!({
                        "id": format!("chatcmpl-{request_id}"),
                        "object": "chat.completion.chunk",
                        "created": created,
                        "model": model,
                        "choices": [{
                            "index": 0,
                            "delta": { "role": "assistant", "content": text },
                            "finish_reason": null,
                        }],
                    }),
                );
            }
            push(
                &mut out,
                None,
                &json!({
                    "id": format!("chatcmpl-{request_id}"),
                    "object": "chat.completion.chunk",
                    "created": created,
                    "model": model,
                    "choices": [{ "index": 0, "delta": {}, "finish_reason": "stop" }],
                    "usage": {
                        "prompt_tokens": input_tokens,
                        "completion_tokens": output_tokens,
                        "total_tokens": input_tokens + output_tokens,
                    },
                }),
            );
            out.push_str("data: [DONE]\n\n");
        }
        CursorFormat::Responses => {
            let response_obj =
                build_buffered_body(CursorFormat::Responses, request_id, model, text, input_tokens);
            push(
                &mut out,
                Some("response.created"),
                &json!({ "type": "response.created", "response": response_obj }),
            );
            if !text.is_empty() {
                push(
                    &mut out,
                    Some("response.output_text.delta"),
                    &json!({
                        "type": "response.output_text.delta",
                        "item_id": format!("msg_{request_id}"),
                        "output_index": 0,
                        "content_index": 0,
                        "delta": text,
                    }),
                );
            }
            push(
                &mut out,
                Some("response.completed"),
                &json!({ "type": "response.completed", "response": response_obj }),
            );
            out.push_str("data: [DONE]\n\n");
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_detection_and_canonicalization() {
        assert!(is_cursor_model("cursor"));
        assert!(is_cursor_model("cursor/gpt-4o"));
        assert!(is_cursor_model("Cursor/claude-3.5-sonnet"));
        assert!(!is_cursor_model("gpt-4o"));
        assert_eq!(cursor_canonical_model("cursor"), "default");
        assert_eq!(cursor_canonical_model("cursor/gpt-4o"), "gpt-4o");
    }

    #[test]
    fn extract_openai_messages() {
        let payload = json!({
            "model": "cursor/gpt-4o",
            "stream": true,
            "messages": [
                { "role": "system", "content": "Be terse." },
                { "role": "user", "content": "Hi" },
                { "role": "assistant", "content": "Hello" },
                { "role": "user", "content": [{ "type": "text", "text": "Bye" }] }
            ]
        });
        let req = extract_request(&payload).unwrap();
        assert!(req.stream);
        assert_eq!(req.instruction, "Be terse.");
        assert_eq!(req.turns.len(), 3);
        assert_eq!(req.turns[0].role, 1);
        assert_eq!(req.turns[1].role, 2);
        assert_eq!(req.turns[2].content, "Bye");
    }

    #[test]
    fn extract_claude_and_responses() {
        let claude = json!({
            "model": "cursor/x",
            "system": "Sys.",
            "messages": [{ "role": "user", "content": "Q" }]
        });
        let r = extract_request(&claude).unwrap();
        assert_eq!(r.instruction, "Sys.");
        assert_eq!(r.turns.len(), 1);

        let responses = json!({
            "model": "cursor",
            "instructions": "Sys2.",
            "input": [{ "role": "user", "content": [{ "type": "input_text", "text": "Q2" }] }]
        });
        let r2 = extract_request(&responses).unwrap();
        assert_eq!(r2.instruction, "Sys2.");
        assert_eq!(r2.turns[0].content, "Q2");

        let responses_str = json!({ "model": "cursor", "input": "plain" });
        let r3 = extract_request(&responses_str).unwrap();
        assert_eq!(r3.turns[0].content, "plain");
    }

    #[test]
    fn empty_conversation_is_error() {
        let payload = json!({ "model": "cursor", "messages": [] });
        assert!(extract_request(&payload).is_err());
    }

    #[test]
    fn token_normalization() {
        assert_eq!(normalize_token("user_01::jwtABC"), "jwtABC");
        assert_eq!(normalize_token("user_01%3A%3AjwtABC"), "jwtABC");
        assert_eq!(normalize_token("  rawjwt  "), "rawjwt");
    }

    #[test]
    fn checksum_is_stable_and_well_formed() {
        let c1 = generate_checksum("tok", 1_700_000_000_000);
        let c2 = generate_checksum("tok", 1_700_000_000_000);
        assert_eq!(c1, c2, "deterministic for fixed timestamp");
        // Layout: <base64 prefix><64 hex>/<64 hex>.
        let (_, rest) = c1.split_at(c1.len() - (64 + 1 + 64));
        let (a, b) = rest.split_once('/').unwrap();
        assert_eq!(a.len(), 64);
        assert_eq!(b.len(), 64);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn frame_roundtrip_uncompressed() {
        let payload = b"hello protobuf";
        let frame = encode_frame(payload, false);
        assert_eq!(frame[0], 0);
        let len = u32::from_be_bytes([frame[1], frame[2], frame[3], frame[4]]) as usize;
        assert_eq!(len, payload.len());
        assert_eq!(&frame[5..], payload);
    }

    #[test]
    fn protobuf_request_roundtrip_and_response_decode() {
        // Encode a request, ensure it is non-empty and re-decodable.
        let req = ExtractedRequest {
            instruction: "sys".into(),
            turns: vec![ChatTurn { role: 1, content: "hi".into() }],
            stream: false,
        };
        let proto = build_request_proto("gpt-4o", &req);
        let bytes = proto.encode_to_vec();
        assert!(!bytes.is_empty());
        let back = StreamRequest::decode(bytes.as_slice()).unwrap();
        let inner = back.request.unwrap();
        assert_eq!(inner.messages.len(), 1);
        assert_eq!(inner.messages[0].content, "hi");
        assert_eq!(inner.model.unwrap().name, "gpt-4o");
        assert_eq!(inner.chat_mode, "Ask");

        // Build a response frame the way the upstream would and decode it.
        let resp = StreamResponse {
            message: Some(RespMessage {
                content: "world".into(),
                thinking: None,
            }),
        };
        let frame = encode_frame(&resp.encode_to_vec(), false);
        let decoded = decode_response(&frame);
        assert_eq!(decoded.text, "world");

        // Two frames concatenate into the full reply.
        let mut two = encode_frame(
            &StreamResponse {
                message: Some(RespMessage { content: "Hello ".into(), thinking: None }),
            }
            .encode_to_vec(),
            false,
        );
        two.extend_from_slice(&encode_frame(
            &StreamResponse {
                message: Some(RespMessage { content: "world".into(), thinking: None }),
            }
            .encode_to_vec(),
            false,
        ));
        assert_eq!(decode_response(&two).text, "Hello world");
    }

    #[test]
    fn thinking_only_stream_yields_answer_after_marker() {
        // composer-* models stream everything via the thinking channel and mark
        // the visible answer with a literal `</think>`.
        let chunks = ["The user asks ", "simple math.\n</think>\n", "1+1等于2。"];
        let mut buf = Vec::new();
        for c in chunks {
            buf.extend_from_slice(&encode_frame(
                &StreamResponse {
                    message: Some(RespMessage {
                        content: String::new(),
                        thinking: Some(MessageThinking { content: c.into() }),
                    }),
                }
                .encode_to_vec(),
                false,
            ));
        }
        assert_eq!(decode_response(&buf).text, "1+1等于2。");

        // No marker at all: surface the whole thinking stream rather than nothing.
        let frame = encode_frame(
            &StreamResponse {
                message: Some(RespMessage {
                    content: String::new(),
                    thinking: Some(MessageThinking { content: "just thoughts".into() }),
                }),
            }
            .encode_to_vec(),
            false,
        );
        assert_eq!(decode_response(&frame).text, "just thoughts");

        // The `default` (auto) model uses a `<｜final｜>` sentinel instead.
        let frame = encode_frame(
            &StreamResponse {
                message: Some(RespMessage {
                    content: String::new(),
                    thinking: Some(MessageThinking {
                        content: "reasoning...<｜final｜>1+1等于2。".into(),
                    }),
                }),
            }
            .encode_to_vec(),
            false,
        );
        assert_eq!(decode_response(&frame).text, "1+1等于2。");

        // Real content present: thinking must not leak into the reply.
        let mut mixed = encode_frame(
            &StreamResponse {
                message: Some(RespMessage {
                    content: String::new(),
                    thinking: Some(MessageThinking { content: "hmm</think>draft".into() }),
                }),
            }
            .encode_to_vec(),
            false,
        );
        mixed.extend_from_slice(&encode_frame(
            &StreamResponse {
                message: Some(RespMessage { content: "final".into(), thinking: None }),
            }
            .encode_to_vec(),
            false,
        ));
        assert_eq!(decode_response(&mixed).text, "final");
    }

    #[test]
    fn gzipped_frame_decodes() {
        let resp = StreamResponse {
            message: Some(RespMessage { content: "zipped".into(), thinking: None }),
        };
        let frame = encode_frame(&resp.encode_to_vec(), true);
        assert_eq!(frame[0], 1, "gzip flag set");
        assert_eq!(decode_response(&frame).text, "zipped");
    }

    #[test]
    fn json_error_frame_surfaces() {
        let mut frame = vec![2u8];
        let body = br#"{"error":"rate limit exceeded"}"#;
        frame.extend_from_slice(&(body.len() as u32).to_be_bytes());
        frame.extend_from_slice(body);
        let decoded = decode_response(&frame);
        assert!(decoded.text.is_empty());
        assert!(decoded.error_json.as_deref().unwrap().contains("rate limit"));
        assert!(looks_rate_limited(decoded.error_json.as_deref().unwrap()));
    }

    #[test]
    fn openai_body_and_sse_shape() {
        let body = build_buffered_body(CursorFormat::OpenAI, "rid", "cursor/gpt-4o", "Hi", 40);
        assert_eq!(body["object"], "chat.completion");
        assert_eq!(body["choices"][0]["message"]["content"], "Hi");
        let sse = build_sse_body(CursorFormat::Claude, "rid", "cursor/x", "Yo", 8);
        assert!(sse.contains("event: message_start"));
        assert!(sse.contains(r#""text":"Yo""#));
    }
}
