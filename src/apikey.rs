//! Gateway-issued API keys — the "open API" credential path.
//!
//! A key (`oag_<64 hex>`) is a deliberately issued secret tied to an owner
//! user id. Unlike the self-asserted `user:<id>` bearer, presenting a valid key
//! IS proof of authorization, so the auth layer treats it as owner-trusted and
//! honors it even when a trusted edge is configured — this is exactly the
//! external-integration path (third-party programs / scripts that can't perform
//! the team's browser SSO).
//!
//! Only the sha256 hash is persisted; the plaintext is shown once at creation.
//! The live set is held in a process-global so the synchronous auth helpers
//! (`identify_caller` / `extract_user_id`) can resolve a key without threading
//! `AppState` through ~30 call sites.

use crate::prelude::*;
use sha2::{Digest, Sha256};
use std::sync::{OnceLock, RwLock as StdRwLock};

pub(crate) const KEY_PREFIX: &str = "oag_";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ApiKeyRecord {
    pub(crate) id: String,
    /// sha256(plaintext) in lowercase hex. The plaintext is never stored.
    pub(crate) key_hash: String,
    /// Masked hint for display, e.g. `oag_3f9a…c1d2`.
    pub(crate) display: String,
    pub(crate) owner_user_id: String,
    pub(crate) label: String,
    pub(crate) created_at: DateTime<Utc>,
    #[serde(default)]
    pub(crate) revoked: bool,
}

static STORE: OnceLock<StdRwLock<Vec<ApiKeyRecord>>> = OnceLock::new();

fn store() -> &'static StdRwLock<Vec<ApiKeyRecord>> {
    STORE.get_or_init(|| StdRwLock::new(Vec::new()))
}

/// Seed the live set at startup from the persisted snapshot.
pub(crate) fn init(records: Vec<ApiKeyRecord>) {
    *store().write().expect("apikey store poisoned") = records;
}

/// Clone of the live set (for listing / persistence).
pub(crate) fn snapshot() -> Vec<ApiKeyRecord> {
    store().read().expect("apikey store poisoned").clone()
}

fn hash_key(plaintext: &str) -> String {
    let mut h = Sha256::new();
    h.update(plaintext.as_bytes());
    crate::util::hex_lower(&h.finalize())
}

/// Resolve a presented bearer to its owner user id, iff it is a live
/// (non-revoked) gateway API key. Returns `None` for anything else (so the
/// caller falls through to edge / `user:` handling). Comparison is over the
/// sha256 hash, which already hides the secret, so a plain `==` leaks nothing.
pub(crate) fn resolve(bearer: &str) -> Option<String> {
    let token = bearer.trim();
    if !token.starts_with(KEY_PREFIX) {
        return None;
    }
    let hash = hash_key(token);
    let store = store().read().expect("apikey store poisoned");
    store
        .iter()
        .find(|r| !r.revoked && r.key_hash == hash)
        .map(|r| r.owner_user_id.clone())
}

pub(crate) struct CreatedKey {
    pub(crate) record: ApiKeyRecord,
    /// Shown to the caller exactly once; never persisted.
    pub(crate) plaintext: String,
}

/// Mint a new key for `owner_user_id`, add it to the live set, and return both
/// the record and the one-time plaintext. The caller must persist afterwards.
pub(crate) fn create(owner_user_id: String, label: String) -> CreatedKey {
    let plaintext = generate_key();
    let record = ApiKeyRecord {
        id: Uuid::new_v4().to_string(),
        key_hash: hash_key(&plaintext),
        display: mask(&plaintext),
        owner_user_id,
        label,
        created_at: Utc::now(),
        revoked: false,
    };
    store()
        .write()
        .expect("apikey store poisoned")
        .push(record.clone());
    CreatedKey { record, plaintext }
}

/// Revoke a key by id, but only if it belongs to `owner_user_id`. Returns true
/// if a live key was found and revoked (caller persists). Returns false if the
/// key is missing, already revoked, or owned by someone else.
pub(crate) fn revoke_owned(id: &str, owner_user_id: &str) -> bool {
    let mut store = store().write().expect("apikey store poisoned");
    if let Some(rec) = store
        .iter_mut()
        .find(|r| r.id == id && r.owner_user_id == owner_user_id && !r.revoked)
    {
        rec.revoked = true;
        return true;
    }
    false
}

/// 256 bits of randomness from two v4 UUIDs (getrandom-backed), hex-encoded.
fn generate_key() -> String {
    let mut bytes = Vec::with_capacity(32);
    bytes.extend_from_slice(Uuid::new_v4().as_bytes());
    bytes.extend_from_slice(Uuid::new_v4().as_bytes());
    format!("{}{}", KEY_PREFIX, crate::util::hex_lower(&bytes))
}

fn mask(plaintext: &str) -> String {
    let body = &plaintext[KEY_PREFIX.len()..];
    if body.len() <= 8 {
        return format!("{}…", KEY_PREFIX);
    }
    format!("{}{}…{}", KEY_PREFIX, &body[..4], &body[body.len() - 4..])
}

// --- persistence ---------------------------------------------------------

/// Load the persisted snapshot. Last write wins per id (the file is
/// append-free — we always rewrite the whole set — but tolerate dupes anyway).
pub(crate) async fn load(path: &PathBuf) -> Vec<ApiKeyRecord> {
    let data = match tokio::fs::read_to_string(path).await {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };
    let mut latest: HashMap<String, ApiKeyRecord> = HashMap::new();
    for rec in data
        .lines()
        .filter_map(|l| serde_json::from_str::<ApiKeyRecord>(l).ok())
    {
        latest.insert(rec.id.clone(), rec);
    }
    let mut out: Vec<ApiKeyRecord> = latest.into_values().collect();
    out.sort_by_key(|r| r.created_at);
    out
}

/// Atomic + durable rewrite of the whole key set (temp + fsync + rename + dir
/// fsync), mirroring `persist_all_accounts`. Serialized through the shared
/// persist lock so it can't race the account snapshot's rename.
pub(crate) async fn persist(state: &AppState) -> Result<(), String> {
    let _guard = state.persist_lock.lock().await;

    let mut lines = String::new();
    for rec in snapshot() {
        lines.push_str(&serde_json::to_string(&rec).map_err(|e| e.to_string())?);
        lines.push('\n');
    }

    let tmp = state.api_key_file.with_extension("ndjson.tmp");
    {
        let mut file = tokio::fs::File::create(&tmp)
            .await
            .map_err(|e| e.to_string())?;
        file.write_all(lines.as_bytes())
            .await
            .map_err(|e| e.to_string())?;
        file.sync_all().await.map_err(|e| e.to_string())?;
    }
    tokio::fs::rename(&tmp, &state.api_key_file)
        .await
        .map_err(|e| e.to_string())?;
    crate::pool::storage::sync_parent_dir(&state.api_key_file).await;
    Ok(())
}
