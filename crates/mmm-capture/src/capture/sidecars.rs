//! Per-chain capture evidence sidecar payloads and revocation-reason constants.
//!
//! These are the chain-specific data shapes that the otherwise chain-agnostic
//! [`capture`](super) core carries: the 1:1 evidence sidecar rows (RSK, Hathor)
//! that do not fit the shared `merge_mining_event` row, and the per-chain
//! automatic revocation reasons. They are re-exported from `capture` so callers
//! reference them at the unchanged `capture::*` paths.

/// Sidecar evidence for the RSK structure/capture slice. Persisted as a 1:1
/// row in `rsk_merge_mining_evidence` per `merge_mining_event`.
///
/// `pool_identity_id` is the late-fill provenance reference: NULL when the
/// RSK miner address is not yet in the embedded registry, populated once the
/// registry resolves it. `proof_format` is a discriminator; the initial slice
/// stores all proofs as the opaque RSK-RPC byte stream until RSKj source
/// resolves the era boundaries precisely.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RskEvidencePayload {
    pub rsk_block_hash: Vec<u8>,
    pub rsk_height: i32,
    pub is_uncle: bool,
    pub uncle_index: Option<i32>,
    pub uncle_parent_height: Option<i32>,
    pub rsk_miner: Vec<u8>,
    pub pool_identity_id: Option<i64>,
    pub merge_mining_hash: Vec<u8>,
    pub merkle_proof: Option<Vec<u8>>,
    pub coinbase_tail: Option<Vec<u8>>,
    pub proof_format: &'static str,
}

/// `proof_format` discriminator for `RskEvidencePayload`: the opaque RSK-RPC
/// byte stream stored verbatim until RSKj source resolves the era boundaries.
/// Persisted literal in the rsk sidecar; matched by store/producers.
pub const RSK_PROOF_FORMAT_OPAQUE: &str = "rskj_rpc_opaque";

/// Hathor-specific evidence sidecar payload. Carries the RFC 0006 proof bytes
/// that do not fit `merge_mining_event` (the `aux_pow` blob, the `funds_graph`
/// prefix, the brute-forced split offset) plus the offline `expected_btc_nbits`
/// recorded in an overwrite-safe place. There is no `is_voided` field (capture
/// only writes non-voided
/// blocks, so the voided state is the event revocation, not a sidecar flag).
#[derive(Debug, Clone, PartialEq)]
pub struct HathorEvidencePayload {
    pub hathor_block_hash: Vec<u8>,
    pub hathor_height: i32,
    pub aux_pow: Vec<u8>,
    pub funds_graph: Vec<u8>,
    pub funds_graph_split: i32,
    pub reward_output_details: Option<serde_json::Value>,
    pub reward_addresses: Option<serde_json::Value>,
    pub expected_btc_nbits: i64,
    pub proof_format: &'static str,
}

/// `proof_format` discriminator for `HathorEvidencePayload`: RFC 0006
/// merge-mining proof layout (aux_pow blob + funds_graph prefix + brute-forced
/// split offset). Persisted literal in the hathor sidecar.
pub const HATHOR_PROOF_FORMAT_RFC0006: &str = "hathor_rfc0006";

/// Revocation reasons the Hathor producer applies automatically on a child-DAG
/// reorg. A re-capture of the same `(source, height, hash)` auto-restores ONLY
/// these reversible reasons; a `hathor_nbits_classifier_conflict` or any manual
/// revoke is sticky.
pub const HATHOR_REVOKE_VOIDED: &str = "hathor_voided";
pub const HATHOR_REVOKE_SUPERSEDED: &str = "hathor_superseded";
pub const HATHOR_REVOKE_NBITS_CONFLICT: &str = "hathor_nbits_classifier_conflict";
/// Reversible: the current canonical block's parent classified as non-BTC under
/// the offline nBits verdict (BCH/indeterminate). If the embedded nBits table is
/// later corrected so the block is Valid, a recapture auto-restores it.
pub const HATHOR_REVOKE_NON_BTC: &str = "hathor_non_btc";

/// Revocation reasons the Elastos producer applies automatically when a captured
/// height's verdict flips to rejected on a replay/backfill (a regenerated nBits
/// table or a now-enabled classifier). A later Valid recapture of the same
/// `(source, height, hash)` auto-restores ONLY the reversible
/// `ELASTOS_REVOKE_NON_BTC`; an `ELASTOS_REVOKE_CLASSIFIER_CONFLICT` or any manual
/// revoke is sticky.
pub const ELASTOS_REVOKE_NON_BTC: &str = "elastos_non_btc";
pub const ELASTOS_REVOKE_CLASSIFIER_CONFLICT: &str = "elastos_nbits_classifier_conflict";
