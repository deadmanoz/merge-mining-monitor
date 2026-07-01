//! Backbone integrity guards: the structural fail-stop checks the contiguous
//! sync and the live producer share. They detect same-height canonical
//! conflicts, broken parent links, and target-tip reorgs, record the error on
//! `bitcoin_core_sync_state`, and return a typed [`BackboneIntegrityError`]
//! marker the live adapter downcasts to tell a structural fail-stop apart from a
//! transient fetch error. The SQL helpers (`load_canonical_rows_at_height`,
//! `update_sync_error`) stay in the parent module; the guards reach them as
//! private parent items via `super::`.

use anyhow::{Result, anyhow};
use bitcoin::BlockHash;
use bitcoin::hashes::Hash as _;
use serde_json::json;
use tokio_postgres::Client;

use super::{CanonicalHeightRow, load_canonical_rows_at_height, update_sync_error};

/// Marker attached (via `anyhow` context) to the backbone integrity-guard
/// errors a live producer must NOT retry: they require operator recovery, so a
/// live producer fail-stops on them rather than masquerading as healthy. The
/// live adapter downcasts to this to separate a structural fail-stop from a
/// transient fetch error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BackboneIntegrityError {
    HeightConflict,
    LinkMismatch,
    LiveWindowInvariant,
    TargetTipChanged,
}

impl std::fmt::Display for BackboneIntegrityError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let detail = match self {
            Self::HeightConflict => "same-height canonical conflict",
            Self::LinkMismatch => "canonical link mismatch",
            Self::LiveWindowInvariant => "live backbone window invariant failed",
            Self::TargetTipChanged => "target tip changed (same-height reorg)",
        };
        write!(f, "backbone integrity error: {detail}")
    }
}

impl std::error::Error for BackboneIntegrityError {}

/// True when `err` carries a `BackboneIntegrityError` marker anywhere in its
/// chain (a structural fail-stop, not a transient fetch error).
pub(crate) fn is_backbone_integrity_error(err: &anyhow::Error) -> bool {
    err.downcast_ref::<BackboneIntegrityError>().is_some()
}

/// Build a structural integrity error: the descriptive `message` stays the
/// top-level display (so existing operator-facing messages and their tests are
/// unchanged) while the typed `kind` marker remains downcastable for the live
/// adapter's transient-vs-structural classification.
pub(crate) fn integrity_error(kind: BackboneIntegrityError, message: String) -> anyhow::Error {
    anyhow::Error::new(kind).context(message)
}

pub(super) async fn guard_same_height_conflicts(
    client: &Client,
    source_id: i64,
    height: i32,
    core_hash: BlockHash,
    rows: &[CanonicalHeightRow],
) -> Result<()> {
    let core_hash_bytes = core_hash.to_byte_array();
    let conflict = rows.len() > 1
        || rows
            .iter()
            .any(|row| row.hash.as_slice() != core_hash_bytes.as_slice());
    if !conflict {
        return Ok(());
    }
    let hashes = rows
        .iter()
        .map(|row| hex::encode(&row.hash))
        .collect::<Vec<_>>();
    let err = anyhow!(
        "same-height canonical conflict at height {height}: Core={}, existing={}",
        core_hash,
        hashes.join(",")
    );
    update_sync_error(
        client,
        source_id,
        height,
        "backbone_height_conflict",
        &err.to_string(),
        json!({
            "core_hash": core_hash.to_string(),
            "existing_hashes": hashes,
        }),
    )
    .await?;
    Err(integrity_error(
        BackboneIntegrityError::HeightConflict,
        err.to_string(),
    ))
}

pub(super) async fn guard_existing_link(
    client: &Client,
    source_id: i64,
    height: i32,
    row: &CanonicalHeightRow,
    default_contiguous_pass: bool,
) -> Result<()> {
    guard_header_link(
        client,
        source_id,
        height,
        &row.prev_hash,
        default_contiguous_pass,
    )
    .await
}

pub(super) async fn guard_header_link(
    client: &Client,
    source_id: i64,
    height: i32,
    prev_hash: &[u8],
    default_contiguous_pass: bool,
) -> Result<()> {
    if height == 0 {
        return Ok(());
    }
    let prev_rows = load_canonical_rows_at_height(client, height - 1).await?;
    let Some(prev_row) = prev_rows.first() else {
        if default_contiguous_pass {
            let err = anyhow!("previous canonical height {} is missing", height - 1);
            update_sync_error(
                client,
                source_id,
                height,
                "backbone_link_mismatch",
                &err.to_string(),
                json!({ "previous_height": height - 1 }),
            )
            .await?;
            return Err(integrity_error(
                BackboneIntegrityError::LinkMismatch,
                err.to_string(),
            ));
        }
        return Ok(());
    };
    if prev_rows.len() > 1 || prev_row.hash.as_slice() != prev_hash {
        let err = anyhow!(
            "canonical link mismatch at height {height}: prev={} previous_height_hashes={}",
            hex::encode(prev_hash),
            prev_rows
                .iter()
                .map(|row| hex::encode(&row.hash))
                .collect::<Vec<_>>()
                .join(",")
        );
        update_sync_error(
            client,
            source_id,
            height,
            "backbone_link_mismatch",
            &err.to_string(),
            json!({
                "previous_height": height - 1,
                "expected_prev_hash": hex::encode(prev_hash),
                "previous_hashes": prev_rows.iter().map(|row| hex::encode(&row.hash)).collect::<Vec<_>>(),
            }),
        )
        .await?;
        return Err(integrity_error(
            BackboneIntegrityError::LinkMismatch,
            err.to_string(),
        ));
    }
    Ok(())
}
