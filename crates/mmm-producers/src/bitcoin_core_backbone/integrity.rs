//! Backbone integrity guards: structural fail-stop checks shared by contiguous
//! sync and follow mode.

use anyhow::{Result, anyhow};
use bitcoin::BlockHash;
use bitcoin::hashes::Hash as _;
use serde_json::Value;
use serde_json::json;
use tokio_postgres::GenericClient;

use super::{CanonicalHeightRow, load_canonical_rows_at_height, update_sync_error};

/// Marker attached to structural backbone errors that live mode must fail-stop
/// on rather than retry.
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

#[derive(Debug, Clone)]
pub(super) struct BackboneIntegrityFailure {
    kind: BackboneIntegrityError,
    code: &'static str,
    height: i32,
    message: String,
    details: Value,
}

impl std::fmt::Display for BackboneIntegrityFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl BackboneIntegrityFailure {
    pub(super) fn new(
        kind: BackboneIntegrityError,
        code: &'static str,
        height: i32,
        message: String,
        details: Value,
    ) -> Self {
        Self {
            kind,
            code,
            height,
            message,
            details,
        }
    }

    pub(super) async fn persist<C: GenericClient>(&self, client: &C, source_id: i64) -> Result<()> {
        update_sync_error(
            client,
            source_id,
            self.height,
            self.code,
            &self.message,
            self.details.clone(),
        )
        .await
    }

    pub(super) fn into_error(self) -> anyhow::Error {
        anyhow::Error::new(self.kind).context(self)
    }
}

pub(crate) fn is_backbone_integrity_error(err: &anyhow::Error) -> bool {
    err.downcast_ref::<BackboneIntegrityError>().is_some()
}

pub(crate) fn integrity_error(kind: BackboneIntegrityError, message: String) -> anyhow::Error {
    anyhow::Error::new(kind).context(message)
}

pub(super) async fn guard_same_height_conflicts<C: GenericClient>(
    client: &C,
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
    let message = anyhow!(
        "same-height canonical conflict at height {height}: Core={}, existing={}",
        core_hash,
        hashes.join(",")
    )
    .to_string();
    let failure = BackboneIntegrityFailure::new(
        BackboneIntegrityError::HeightConflict,
        "backbone_height_conflict",
        height,
        message,
        json!({
            "core_hash": core_hash.to_string(),
            "existing_hashes": hashes,
        }),
    );
    failure.persist(client, source_id).await?;
    Err(failure.into_error())
}

pub(super) async fn guard_existing_link<C: GenericClient>(
    client: &C,
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

pub(super) async fn guard_header_link<C: GenericClient>(
    client: &C,
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
            let message =
                anyhow!("previous canonical height {} is missing", height - 1).to_string();
            let failure = BackboneIntegrityFailure::new(
                BackboneIntegrityError::LinkMismatch,
                "backbone_link_mismatch",
                height,
                message,
                json!({ "previous_height": height - 1 }),
            );
            failure.persist(client, source_id).await?;
            return Err(failure.into_error());
        }
        return Ok(());
    };
    if prev_rows.len() > 1 || prev_row.hash.as_slice() != prev_hash {
        let message = anyhow!(
            "canonical link mismatch at height {height}: prev={} previous_height_hashes={}",
            hex::encode(prev_hash),
            prev_rows
                .iter()
                .map(|row| hex::encode(&row.hash))
                .collect::<Vec<_>>()
                .join(",")
        )
        .to_string();
        let failure = BackboneIntegrityFailure::new(
            BackboneIntegrityError::LinkMismatch,
            "backbone_link_mismatch",
            height,
            message,
            json!({
                "previous_height": height - 1,
                "expected_prev_hash": hex::encode(prev_hash),
                "previous_hashes": prev_rows.iter().map(|row| hex::encode(&row.hash)).collect::<Vec<_>>(),
            }),
        );
        failure.persist(client, source_id).await?;
        return Err(failure.into_error());
    }
    Ok(())
}
