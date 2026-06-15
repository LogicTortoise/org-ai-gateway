use crate::prelude::*;

/// Rotate the audit log once it exceeds this size (override via
/// `GATEWAY_AUDIT_ROTATE_BYTES`). One previous generation (`.1`) is kept and
/// still scanned by `read_audit_records`, so the 7-day usage windows survive a
/// rotation; older generations are dropped. Without rotation the append-only
/// log grows unboundedly and every stats/owner-usage scan slows with it.
fn audit_rotate_bytes() -> u64 {
    std::env::var("GATEWAY_AUDIT_ROTATE_BYTES")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|v| *v > 0)
        .unwrap_or(64 * 1024 * 1024)
}

fn rotated_audit_path(path: &std::path::Path) -> PathBuf {
    let mut os = path.as_os_str().to_owned();
    os.push(".1");
    PathBuf::from(os)
}

pub(crate) async fn append_audit(state: &AppState, record: &AuditRecord) -> Result<(), String> {
    // Live quota accounting first: the upstream tokens are consumed whether or
    // not the disk write below succeeds, so the ledgers must reflect them.
    crate::usage::note_request_usage(state, record).await;

    let json = serde_json::to_string(record).map_err(|e| e.to_string())?;

    // Size-based rotation, checked before each append. The rename is guarded by
    // `persist_lock` so two concurrent appenders can't both rotate.
    if let Ok(meta) = tokio::fs::metadata(&state.audit_file).await {
        if meta.len() >= audit_rotate_bytes() {
            let _guard = state.persist_lock.lock().await;
            // Re-check under the lock: the other racer may have just rotated.
            if let Ok(meta) = tokio::fs::metadata(&state.audit_file).await {
                if meta.len() >= audit_rotate_bytes() {
                    let rotated = rotated_audit_path(&state.audit_file);
                    if let Err(e) = tokio::fs::rename(&state.audit_file, &rotated).await {
                        warn!("audit rotation failed (continuing to append): {}", e);
                    } else {
                        info!("rotated audit log to {}", rotated.display());
                    }
                }
            }
        }
    }

    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&state.audit_file)
        .await
        .map_err(|e| e.to_string())?;

    // Write the record and its newline in a single call so two concurrent
    // appenders can't interleave into one corrupt (and silently dropped) line.
    let mut line = json;
    line.push('\n');
    file.write_all(line.as_bytes())
        .await
        .map_err(|e| e.to_string())?;
    file.flush().await.map_err(|e| e.to_string())?;

    Ok(())
}


pub(crate) async fn persist_all_accounts(state: &AppState) -> Result<(), String> {
    // Serialize the whole snapshot + write + rename. Without this, concurrent
    // persisters share the same temp path: their truncating writes interleave
    // and their renames race, which can publish a partial or stale snapshot of
    // the only credential store.
    let _guard = state.persist_lock.lock().await;

    let accounts = state.accounts.read().await.clone();
    let mut lines = String::new();
    for acc in accounts {
        let row = serde_json::to_string(&acc).map_err(|e| e.to_string())?;
        lines.push_str(&row);
        lines.push('\n');
    }

    // Atomic + durable write: temp file in the same directory, fsync it, then
    // rename, then fsync the directory. A process crash mid-write can never
    // truncate the only copy of the account credentials; without the fsyncs a
    // *machine* crash could still publish an empty/partial file because the
    // rename metadata may hit disk before the data does.
    let tmp = state.account_file.with_extension("ndjson.tmp");
    {
        let mut file = tokio::fs::File::create(&tmp)
            .await
            .map_err(|e| e.to_string())?;
        file.write_all(lines.as_bytes())
            .await
            .map_err(|e| e.to_string())?;
        file.sync_all().await.map_err(|e| e.to_string())?;
    }
    tokio::fs::rename(&tmp, &state.account_file)
        .await
        .map_err(|e| e.to_string())?;
    sync_parent_dir(&state.account_file).await;
    Ok(())
}

/// Fsync the parent directory of `path` so a just-completed rename survives a
/// power loss. Best-effort: directory fds can't be opened on all platforms.
pub(crate) async fn sync_parent_dir(path: &std::path::Path) {
    let Some(dir) = path.parent().map(|d| d.to_path_buf()) else {
        return;
    };
    let _ = tokio::task::spawn_blocking(move || {
        if let Ok(handle) = std::fs::File::open(&dir) {
            let _ = handle.sync_all();
        }
    })
    .await;
}


pub(crate) async fn load_accounts(account_file: &PathBuf) -> Vec<UpstreamAccount> {
    let data = match tokio::fs::read_to_string(account_file).await {
        Ok(content) => content,
        Err(_) => return Vec::new(),
    };

    let mut latest: HashMap<String, UpstreamAccount> = HashMap::new();
    for account in data
        .lines()
        .filter_map(|line| serde_json::from_str::<UpstreamAccount>(line).ok())
    {
        let key = account_unique_key(&account);
        latest.insert(key, account);
    }
    let mut out: Vec<UpstreamAccount> = latest.into_values().collect();
    out.sort_by_key(|a| a.created_at);
    out
}


pub(crate) fn account_unique_key(account: &UpstreamAccount) -> String {
    if !account.account_id.trim().is_empty() {
        return format!(
            "{}|{}|{}",
            account.owner_user_id, account.provider, account.account_id
        );
    }
    format!(
        "{}|{}|{}",
        account.owner_user_id, account.provider, account.account_label
    )
}


/// Insert or merge `incoming` into the pool and return the record as it now
/// exists. On a re-import the returned record carries the EXISTING account's
/// `id`/`created_at`, so callers must use the return value (not their pre-merge
/// struct) when reporting the account id to clients.
pub(crate) fn upsert_account(
    accounts: &mut Vec<UpstreamAccount>,
    incoming: UpstreamAccount,
) -> UpstreamAccount {
    let key = account_unique_key(&incoming);
    if let Some(index) = accounts
        .iter()
        .position(|account| account_unique_key(account) == key)
    {
        let existing = &accounts[index];
        let mut merged = incoming;
        merged.id = existing.id.clone();
        merged.created_at = existing.created_at;
        // Share/donation caps are owner-set durable properties: a re-import
        // that doesn't explicitly carry one keeps the existing cap.
        merged.share_limit_percent = merged.share_limit_percent.or(existing.share_limit_percent);
        merged.daily_token_limit = merged.daily_token_limit.or(existing.daily_token_limit);
        // A re-import carries a default runtime — keep the durable flags from
        // the existing record. `cyber_access`/`disabled` are admin-set account
        // properties; `dead` is intentionally reset (fresh credentials are
        // expected to revive the account). `expires_at` prefers the incoming
        // value (new creds carry a new expiry).
        merged.runtime.cyber_access = existing.runtime.cyber_access;
        merged.runtime.disabled = existing.runtime.disabled;
        merged.runtime.expires_at = merged.runtime.expires_at.or(existing.runtime.expires_at);
        accounts[index] = merged.clone();
        return merged;
    }
    accounts.push(incoming.clone());
    incoming
}

/// Read all audit records as tolerant JSON values. Includes the previous
/// rotated generation (`.1`, read first so records stay roughly chronological)
/// — consumers compute trailing windows (7d usage, stats) and must not lose
/// history at the rotation boundary.
pub(crate) async fn read_audit_records(path: &std::path::Path) -> Vec<Value> {
    let mut out = Vec::new();
    for p in [rotated_audit_path(path), path.to_path_buf()] {
        if let Ok(content) = tokio::fs::read_to_string(&p).await {
            out.extend(
                content
                    .lines()
                    .filter_map(|l| serde_json::from_str::<Value>(l).ok()),
            );
        }
    }
    out
}

/// Aggregate per-account usage from audit records, keyed by `upstream_account_id`.
pub(crate) fn account_usage_map(records: &[Value]) -> HashMap<String, AccountUsage> {
    let mut map: HashMap<String, AccountUsage> = HashMap::new();
    for r in records {
        let acct = r
            .get("upstream_account_id")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        if acct.is_empty() {
            continue;
        }
        let e = map.entry(acct.to_string()).or_default();
        e.requests += 1;
        match r.get("status").and_then(|v| v.as_str()) {
            Some("success") => e.success += 1,
            _ => e.errors += 1,
        }
        e.output_bytes += r.get("output_length").and_then(|v| v.as_u64()).unwrap_or(0);
        if let Some(ts) = r
            .get("created_at")
            .and_then(|v| v.as_str())
            .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        {
            let ts = ts.with_timezone(&Utc);
            if e.last_used.is_none_or(|prev| ts > prev) {
                e.last_used = Some(ts);
            }
        }
    }
    map
}


