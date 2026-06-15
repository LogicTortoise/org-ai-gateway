//! cyber_policy hot-swap helpers, ported from `cyber_swap_ws.go`. When a Codex
//! upstream returns a `cyber_policy` error, the gateway transparently swaps the
//! conversation onto an account flagged `cyber_access` instead of surfacing the
//! refusal: on WebSocket it reconnects mid-stream and replays the last
//! `response.create`; on HTTP/SSE (buffered here) it pins the conversation and
//! retries on the cyber account.
use crate::prelude::*;
use crate::pool::account_visible_to_user;
use std::collections::HashSet;

/// Pick a `cyber_access` Codex account for the user, excluding already-tried ids.
pub(crate) fn cyber_access_candidate(
    accounts: &[UpstreamAccount],
    provider: &str,
    user_id: &str,
    excluded: &HashSet<String>,
) -> Option<UpstreamAccount> {
    accounts
        .iter()
        .filter(|a| a.provider == provider)
        .filter(|a| account_visible_to_user(a, user_id))
        .filter(|a| a.runtime.cyber_access && !a.runtime.dead && !a.runtime.disabled)
        .find(|a| !excluded.contains(&a.id))
        .cloned()
}

/// Whether a client WebSocket text frame is a `response.create` (which must be
/// replayed against the swapped upstream).
pub(crate) fn is_codex_response_create(text: &str) -> bool {
    serde_json::from_str::<Value>(text)
        .ok()
        .and_then(|v| v.get("type").and_then(|t| t.as_str()).map(|s| s == "response.create"))
        .unwrap_or(false)
}

/// Privacy guard for the shared pool — the WS analogue of the HTTP path's
/// forced `store=false` (`ensure_codex_payload_defaults`): rewrite a client
/// `response.create` frame so the turn is never persisted server-side under the
/// pool account's identity. `previous_response_id` is left intact here — within
/// one WS connection the same account minted those ids, so continuity is safe.
/// Returns the rewritten frame, or None when the frame isn't a `response.create`
/// or needs no change.
pub(crate) fn enforce_response_create_privacy(text: &str) -> Option<String> {
    let mut v: Value = serde_json::from_str(text).ok()?;
    if v.get("type").and_then(|t| t.as_str()) != Some("response.create") {
        return None;
    }
    let mut changed = false;
    if let Some(obj) = v.as_object_mut() {
        if obj.get("store").and_then(|s| s.as_bool()) != Some(false) && obj.contains_key("store") {
            obj.insert("store".into(), Value::Bool(false));
            changed = true;
        }
        if let Some(resp) = obj.get_mut("response").and_then(|r| r.as_object_mut()) {
            // `store` defaults to true upstream, so force it even when absent.
            if resp.get("store").and_then(|s| s.as_bool()) != Some(false) {
                resp.insert("store".into(), Value::Bool(false));
                changed = true;
            }
        }
    }
    if changed {
        Some(v.to_string())
    } else {
        None
    }
}

/// Strip `previous_response_id` (top-level and under `response`) from a replayed
/// `response.create`. The swapped account never minted that id, so leaving it
/// would trigger `previous_response_not_found`; losing prior reasoning context is
/// the lesser evil.
pub(crate) fn strip_previous_response_id(text: &str) -> String {
    let Ok(mut v) = serde_json::from_str::<Value>(text) else {
        return text.to_string();
    };
    if let Some(obj) = v.as_object_mut() {
        obj.remove("previous_response_id");
        if let Some(resp) = obj.get_mut("response").and_then(|r| r.as_object_mut()) {
            resp.remove("previous_response_id");
        }
    }
    v.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn response_create_gets_store_false() {
        let out = enforce_response_create_privacy(
            r#"{"type":"response.create","response":{"model":"gpt-5","store":true}}"#,
        )
        .unwrap();
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["response"]["store"], false);

        // store absent defaults to true upstream — must be forced too.
        let out = enforce_response_create_privacy(
            r#"{"type":"response.create","response":{"model":"gpt-5"}}"#,
        )
        .unwrap();
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["response"]["store"], false);
    }

    #[test]
    fn non_response_create_frames_pass_through() {
        assert!(enforce_response_create_privacy(r#"{"type":"session.update"}"#).is_none());
        assert!(enforce_response_create_privacy("not json").is_none());
    }

    #[test]
    fn keeps_previous_response_id_on_privacy_rewrite() {
        let out = enforce_response_create_privacy(
            r#"{"type":"response.create","response":{"previous_response_id":"resp_1"}}"#,
        )
        .unwrap();
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["response"]["previous_response_id"], "resp_1");
    }
}
