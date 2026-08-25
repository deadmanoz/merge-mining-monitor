//! Sync-status ownership for the durable Bitcoin Core suffix queue.

use anyhow::{Context, Result, bail};
use bitcoin::BlockHash;
use bitcoin::hashes::Hash as _;
use serde_json::{Value, json};
use tokio_postgres::types::Json;
use tokio_postgres::{Client, GenericClient};

const SYNC_MODE_CONTIGUOUS: &str = "contiguous";
const RECONCILE_PENDING: &str = "backbone_reorg_reconcile_pending";
const SUSPENDED_ERROR_KEY: &str = "suspended_error";

pub(super) struct ReplacementPending {
    pub(super) contiguous_complete_height: i32,
    pub(super) common_ancestor_height: i32,
    pub(super) first_height: i32,
    pub(super) target_tip_height: i32,
    pub(super) target_tip_hash: BlockHash,
    pub(super) displaced_blocks: usize,
    pub(super) queued_hashes: usize,
}

#[derive(Debug, Clone, PartialEq)]
struct SuspendedSyncError {
    code: String,
    height: Option<i32>,
    message: Option<String>,
    details: Value,
}

impl SuspendedSyncError {
    fn to_json(&self) -> Value {
        json!({
            "code": self.code,
            "height": self.height,
            "message": self.message,
            "details": self.details,
        })
    }

    fn from_json(value: Option<&Value>) -> Result<Option<Self>> {
        let Some(value) = value else {
            return Ok(None);
        };
        let object = value
            .as_object()
            .context("Bitcoin Core suspended sync error must be a JSON object")?;
        let code = object
            .get("code")
            .and_then(Value::as_str)
            .context("Bitcoin Core suspended sync error is missing its code")?
            .to_owned();
        let height = match object.get("height") {
            None | Some(Value::Null) => None,
            Some(value) => Some(
                i32::try_from(
                    value
                        .as_i64()
                        .context("Bitcoin Core suspended sync error height must be an integer")?,
                )
                .context("Bitcoin Core suspended sync error height overflows i32")?,
            ),
        };
        let message = match object.get("message") {
            None | Some(Value::Null) => None,
            Some(value) => Some(
                value
                    .as_str()
                    .context("Bitcoin Core suspended sync error message must be a string")?
                    .to_owned(),
            ),
        };
        let details = object.get("details").cloned().unwrap_or_else(|| json!({}));
        Ok(Some(Self {
            code,
            height,
            message,
            details,
        }))
    }
}

async fn load_suspended_sync_error<C: GenericClient>(
    client: &C,
    source_id: i64,
) -> Result<Option<SuspendedSyncError>> {
    let row = client
        .query_one(
            "SELECT last_error_code, last_error_height, last_error, last_error_details \
             FROM bitcoin_core_sync_state WHERE source_id = $1 AND sync_mode = $2",
            &[&source_id, &SYNC_MODE_CONTIGUOUS],
        )
        .await
        .context("load Bitcoin Core sync error before marking suffix pending")?;
    let code: Option<String> = row.get(0);
    let details: Json<Value> = row.get(3);
    if code.as_deref() == Some(RECONCILE_PENDING) {
        return SuspendedSyncError::from_json(details.0.get(SUSPENDED_ERROR_KEY));
    }
    Ok(code.map(|code| SuspendedSyncError {
        code,
        height: row.get(1),
        message: row.get(2),
        details: details.0,
    }))
}

pub(super) async fn mark_replacement_pending<C: GenericClient>(
    client: &C,
    source_id: i64,
    pending: &ReplacementPending,
) -> Result<()> {
    let target_hash_bytes = pending.target_tip_hash.to_byte_array().to_vec();
    // The suffix writer already holds this row FOR UPDATE. Suspend the visible
    // producer error inside the pending details so the final queue transaction
    // can restore it exactly. A re-enqueued suffix carries the existing tuple
    // instead of nesting pending states.
    let suspended_error = load_suspended_sync_error(client, source_id).await?;
    let mut details = json!({
        "common_ancestor_height": pending.common_ancestor_height,
        "replacement_start_height": pending.first_height,
        "replacement_target_height": pending.target_tip_height,
        "replacement_target_hash": pending.target_tip_hash.to_string(),
        "displaced_blocks": pending.displaced_blocks,
        "queued_hashes": pending.queued_hashes,
    });
    if let Some(suspended_error) = suspended_error {
        details[SUSPENDED_ERROR_KEY] = suspended_error.to_json();
    }
    let details = Json(details);
    client
        .execute(
            "UPDATE bitcoin_core_sync_state SET target_tip_height = $3, target_tip_hash = $4, \
                 contiguous_complete_height = $5, last_scanned_height = $3, \
                 last_attempted_height = $3, last_error_code = $6, \
                 last_error_height = $7, last_error = $8, last_error_details = $9, \
                 updated_at = extract(epoch from now())::bigint \
             WHERE source_id = $1 AND sync_mode = $2",
            &[
                &source_id,
                &SYNC_MODE_CONTIGUOUS,
                &pending.target_tip_height,
                &target_hash_bytes,
                &pending.contiguous_complete_height,
                &RECONCILE_PENDING,
                &pending.first_height,
                &"Bitcoin Core canonical suffix replaced; dependent reconciliation pending",
                &details,
            ],
        )
        .await
        .context("mark Bitcoin Core suffix cascade pending")?;
    Ok(())
}

pub(super) async fn lock_sync_state<C: GenericClient>(client: &C, source_id: i64) -> Result<()> {
    if !try_lock_sync_state(client, source_id).await? {
        bail!("Bitcoin Core contiguous sync state is missing");
    }
    Ok(())
}

async fn try_lock_sync_state<C: GenericClient>(client: &C, source_id: i64) -> Result<bool> {
    Ok(client
        .query_opt(
            "SELECT 1 FROM bitcoin_core_sync_state \
             WHERE source_id = $1 AND sync_mode = $2 FOR UPDATE",
            &[&source_id, &SYNC_MODE_CONTIGUOUS],
        )
        .await
        .context("lock Bitcoin Core sync state while completing reconcile queue")?
        .is_some())
}

pub(super) async fn clear_pending_error_in_transaction<C: GenericClient>(
    client: &C,
    source_id: i64,
) -> Result<()> {
    let pending: bool = client
        .query_one(
            "SELECT EXISTS (SELECT 1 FROM bitcoin_core_reconcile_queue WHERE source_id = $1)",
            &[&source_id],
        )
        .await
        .context("check remaining Bitcoin Core suffix reconcile seeds")?
        .get(0);
    if pending {
        return Ok(());
    }

    let row = client
        .query_opt(
            "SELECT last_error_details FROM bitcoin_core_sync_state \
             WHERE source_id = $1 AND sync_mode = $2 AND last_error_code = $3",
            &[&source_id, &SYNC_MODE_CONTIGUOUS, &RECONCILE_PENDING],
        )
        .await
        .context("load completed Bitcoin Core suffix reconcile status")?;
    let Some(row) = row else {
        return Ok(());
    };
    let pending_details: Json<Value> = row.get(0);
    let suspended_error =
        SuspendedSyncError::from_json(pending_details.0.get(SUSPENDED_ERROR_KEY))?;
    let (code, height, message, details) = match suspended_error {
        Some(error) => (Some(error.code), error.height, error.message, error.details),
        None => (None, None, None, json!({})),
    };
    client
        .execute(
            "UPDATE bitcoin_core_sync_state SET last_error_code = $4, \
                 last_error_height = $5, last_error = $6, last_error_details = $7, \
                 updated_at = extract(epoch from now())::bigint \
             WHERE source_id = $1 AND sync_mode = $2 AND last_error_code = $3",
            &[
                &source_id,
                &SYNC_MODE_CONTIGUOUS,
                &RECONCILE_PENDING,
                &code,
                &height,
                &message,
                &Json(details),
            ],
        )
        .await
        .context("restore Bitcoin Core status after suffix reconciliation")?;
    Ok(())
}

pub(super) async fn clear_pending_error_if_queue_empty(
    client: &mut Client,
    source_id: i64,
) -> Result<()> {
    let txn = client
        .transaction()
        .await
        .context("begin Bitcoin Core pending-status check")?;
    if try_lock_sync_state(&txn, source_id).await? {
        clear_pending_error_in_transaction(&txn, source_id).await?;
    }
    txn.commit()
        .await
        .context("commit Bitcoin Core pending-status check")
}
