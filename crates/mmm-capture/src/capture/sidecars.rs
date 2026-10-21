//! Per-chain 1:1 capture evidence sidecar payloads (RSK, Hathor).

/// An RSK block's capture evidence: the merge-mining proof the
/// `rsk_merge_mining_evidence` sidecar keeps, and the block facts its observation keeps.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RskEvidencePayload {
    pub rsk_block_hash: Vec<u8>,
    pub rsk_height: i32,
    pub is_uncle: bool,
    pub uncle_index: Option<i32>,
    pub uncle_parent_height: Option<i32>,
    pub rsk_miner: Vec<u8>,
    pub merge_mining_hash: Vec<u8>,
    pub merkle_proof: Option<Vec<u8>>,
    pub coinbase_tail: Option<Vec<u8>>,
    pub proof_format: &'static str,
    /// RSK-reported difficulty as a decimal string; written to the observation only.
    pub child_difficulty_reported: Option<String>,
}

pub const RSK_PROOF_FORMAT_OPAQUE: &str = "rskj_rpc_opaque";

/// 1:1 `hathor_merge_mining_evidence` payload (RFC 0006 `aux_pow` blob,
/// `funds_graph` prefix, brute-forced split offset, and parsed reward outputs).
#[derive(Debug, Clone, PartialEq)]
pub struct HathorEvidencePayload {
    pub hathor_block_hash: Vec<u8>,
    pub hathor_height: i32,
    pub aux_pow: Vec<u8>,
    pub funds_graph: Vec<u8>,
    pub funds_graph_split: i32,
    pub reward_output_details: Option<serde_json::Value>,
    pub reward_addresses: Option<serde_json::Value>,
    pub proof_format: &'static str,
}

pub const HATHOR_PROOF_FORMAT_RFC0006: &str = "hathor_rfc0006";
