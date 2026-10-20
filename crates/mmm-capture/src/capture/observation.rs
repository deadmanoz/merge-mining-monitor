//! `child_block_observation` rows: one per observed child block on every chain.

use std::collections::BTreeSet;

use bitcoin::block::Header;
use bitcoin::consensus::serialize;
use bitcoin::hashes::Hash as _;

use super::{EventPoolAttribution, MergeMiningEventPayload, ParentKind, PoolAttributionSide};

/// Where the block's Bitcoin side lives (`child_block_observation.observation_kind`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObservationKind {
    /// A `merge_mining_event` holds the parent's Bitcoin classification and proof.
    Evidence,
    /// The parent misses Bitcoin's target; its header and coinbase stay on the observation.
    Near,
    /// The block carries no AuxPoW.
    NotMergeMined,
    /// The AuxPoW parent failed the Bitcoin lineage gate; no parent field is stored.
    NonBitcoinParent,
    /// The child block is authenticated but its AuxPoW proof is malformed or
    /// incomplete, so its parent cannot be checked; no parent field is stored.
    UnverifiedParent,
}

impl ObservationKind {
    pub fn as_db_str(self) -> &'static str {
        match self {
            Self::Evidence => "evidence",
            Self::Near => "near",
            Self::NotMergeMined => "not_merge_mined",
            Self::NonBitcoinParent => "non_bitcoin_parent",
            Self::UnverifiedParent => "unverified_parent",
        }
    }

    /// The observation carries no Bitcoin parent (neither evidence nor near).
    pub fn is_child_only(self) -> bool {
        !matches!(self, Self::Evidence | Self::Near)
    }
}

/// A near parent's own evidence, kept because no event holds it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NearParentEvidence {
    pub header_bytes: Vec<u8>,
    pub header_time: i64,
    pub coinbase_txid: Option<Vec<u8>>,
    pub coinbase_script: Option<Vec<u8>>,
    pub coinbase_outputs: Option<Vec<u8>>,
}

/// One raw child-side identifier, keyed like `pool_identity`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct ChildIdentity {
    pub namespace: String,
    pub identifier: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChildBlockObservation {
    pub kind: ObservationKind,
    pub child_height: Option<i32>,
    pub child_block_hash: Option<Vec<u8>>,
    pub child_block_time: Option<i64>,
    pub child_header_bytes: Option<Vec<u8>>,
    pub child_nbits: Option<u32>,
    pub child_prev_block_hash: Option<Vec<u8>>,
    pub child_coinbase_txid: Option<Vec<u8>>,
    pub child_coinbase_script: Option<Vec<u8>>,
    pub child_coinbase_outputs: Option<Vec<u8>>,
    pub pow_validates_child_target: Option<bool>,
    /// Set for evidence and near rows: the parent the block's proof names.
    pub btc_parent_header_hash: Option<Vec<u8>>,
    pub near_parent: Option<NearParentEvidence>,
    pub child_identities: Vec<ChildIdentity>,
    pub child_pool_id: Option<i64>,
    /// The block's known child pools disagree, so its child pool is Unknown even over a
    /// stored one; otherwise a write that resolves none keeps the stored pool.
    pub child_pools_conflict: bool,
    /// Near rows only: the unique known pool the parent coinbase names.
    pub btc_parent_pool_id: Option<i64>,
    pub recorded_at: i64,
}

impl ChildBlockObservation {
    /// The evidence observation an event payload describes. Its identities and pools are
    /// left empty: the store takes them from the event's attribution rows in the same
    /// transaction.
    pub fn from_event_payload(payload: &MergeMiningEventPayload) -> Self {
        let mut observation = Self::child_side(ObservationKind::Evidence, payload);
        observation.btc_parent_header_hash = Some(payload.btc_parent_header_hash.clone());
        observation
    }

    /// A near payload's observation, which no event backs: it keeps the parent header and
    /// coinbase, and its identities and both pools come from the payload's attributions.
    pub fn near(payload: &MergeMiningEventPayload) -> Self {
        debug_assert_eq!(payload.btc_parent_kind, ParentKind::Near);
        let mut observation = Self::child_side(ObservationKind::Near, payload);
        observation.btc_parent_header_hash = Some(payload.btc_parent_header_hash.clone());
        observation.near_parent = Some(NearParentEvidence {
            header_bytes: payload.btc_parent_header_bytes.clone(),
            header_time: payload.btc_parent_header_time,
            coinbase_txid: payload.btc_parent_coinbase_txid.clone(),
            coinbase_script: payload.btc_parent_coinbase_script.clone(),
            coinbase_outputs: payload.btc_parent_coinbase_outputs.clone(),
        });
        observation.set_child_side_attribution(&payload.pool_attributions);
        observation.btc_parent_pool_id =
            unique_pool(&payload.pool_attributions, PoolAttributionSide::BtcParent);
        observation
    }

    /// The child side of a payload whose parent failed the Bitcoin lineage gate. With
    /// no event, its identities and child pool come from the payload's attributions.
    pub fn non_bitcoin_parent(payload: &MergeMiningEventPayload) -> Self {
        let mut observation = Self::child_side(ObservationKind::NonBitcoinParent, payload);
        observation.set_child_side_attribution(&payload.pool_attributions);
        observation
    }

    /// A block with no Bitcoin parent on record (`not_merge_mined` or
    /// `unverified_parent`), described by its own 80-byte header.
    pub fn child_only_from_header(
        kind: ObservationKind,
        child_height: i32,
        header: &Header,
        recorded_at: i64,
    ) -> Self {
        Self {
            child_header_bytes: Some(serialize(header)),
            child_nbits: Some(header.bits.to_consensus()),
            ..Self::child_only(
                kind,
                child_height,
                header.block_hash().to_byte_array().to_vec(),
                Some(i64::from(header.time)),
                Some(header.prev_blockhash.to_byte_array().to_vec()),
                recorded_at,
            )
        }
    }

    /// A block with no Bitcoin parent on record on a chain whose block is not an
    /// 80-byte header: its hash, time and predecessor (internal byte order) as the
    /// chain reports them.
    pub fn child_only(
        kind: ObservationKind,
        child_height: i32,
        child_block_hash: Vec<u8>,
        child_block_time: Option<i64>,
        child_prev_block_hash: Option<Vec<u8>>,
        recorded_at: i64,
    ) -> Self {
        Self {
            kind,
            child_height: Some(child_height),
            child_block_hash: Some(child_block_hash),
            child_block_time,
            child_header_bytes: None,
            child_nbits: None,
            child_prev_block_hash,
            child_coinbase_txid: None,
            child_coinbase_script: None,
            child_coinbase_outputs: None,
            pow_validates_child_target: None,
            btc_parent_header_hash: None,
            near_parent: None,
            child_identities: Vec::new(),
            child_pool_id: None,
            child_pools_conflict: false,
            btc_parent_pool_id: None,
            recorded_at,
        }
    }

    /// The identities and child pool the payload's child-side attributions name.
    fn set_child_side_attribution(&mut self, attributions: &[EventPoolAttribution]) {
        self.child_identities = child_identities(attributions);
        self.child_pool_id = unique_pool(attributions, PoolAttributionSide::ChildBlock);
        self.child_pools_conflict =
            known_pools(attributions, PoolAttributionSide::ChildBlock).len() > 1;
    }

    fn child_side(kind: ObservationKind, payload: &MergeMiningEventPayload) -> Self {
        Self {
            kind,
            child_height: payload.child_height,
            child_block_hash: payload.child_block_hash.clone(),
            child_block_time: payload.child_block_time,
            child_header_bytes: payload.child_header_bytes.clone(),
            child_nbits: payload.child_nbits,
            child_prev_block_hash: payload.child_prev_block_hash.clone().or_else(|| {
                payload
                    .child_header_bytes
                    .as_deref()
                    .and_then(header_prev_block_hash)
            }),
            child_coinbase_txid: payload.child_coinbase_txid.clone(),
            child_coinbase_script: payload.child_coinbase_script.clone(),
            child_coinbase_outputs: payload.child_coinbase_outputs.clone(),
            pow_validates_child_target: payload.pow_validates_child_target,
            btc_parent_header_hash: None,
            near_parent: None,
            child_identities: Vec::new(),
            child_pool_id: None,
            child_pools_conflict: false,
            btc_parent_pool_id: None,
            recorded_at: payload.discovered_at,
        }
    }
}

/// Child-side attribution rows that name an identifier rather than a
/// Bitcoin pool-snapshot rule match (`btc_coinbase_tag`, `btc_payout_address`).
/// The store's SQL (`namespace NOT LIKE 'btc\_%'`) and migration 0033 apply the same rule.
fn is_child_identity_namespace(namespace: &str) -> bool {
    !namespace.starts_with("btc_")
}

fn child_identities(attributions: &[EventPoolAttribution]) -> Vec<ChildIdentity> {
    let mut identities = attributions
        .iter()
        .filter(|row| {
            row.side == PoolAttributionSide::ChildBlock
                && is_child_identity_namespace(row.namespace)
        })
        .map(|row| ChildIdentity {
            namespace: row.namespace.to_owned(),
            identifier: row.matched_value.clone(),
        })
        .collect::<Vec<_>>();
    identities.sort();
    identities.dedup();
    identities
}

/// The unique known pool on one attribution side, as an evidence observation's child pool
/// is derived from its event's attribution rows.
fn unique_pool(attributions: &[EventPoolAttribution], side: PoolAttributionSide) -> Option<i64> {
    let mut pools = known_pools(attributions, side).into_iter();
    match (pools.next(), pools.next()) {
        (Some(pool), None) => Some(pool),
        _ => None,
    }
}

fn known_pools(attributions: &[EventPoolAttribution], side: PoolAttributionSide) -> BTreeSet<i64> {
    attributions
        .iter()
        .filter(|row| row.side == side)
        .filter_map(|row| row.pool_id)
        .collect()
}

/// `hashPrevBlock` from an 80-byte header (bytes 4..36, internal order).
fn header_prev_block_hash(header: &[u8]) -> Option<Vec<u8>> {
    (header.len() == 80).then(|| header[4..36].to_vec())
}
