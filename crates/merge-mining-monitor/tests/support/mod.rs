//! Shared helpers for this crate's integration-test binaries.
//!
//! Each binary that does `mod support;` compiles its own copy, so the blanket
//! `allow(dead_code)` avoids unused-helper warnings under -D warnings. The
//! pure fixture/header helpers live in `mmm_capture::test_support` (anchored
//! on that crate's manifest dir) and are re-exported here so existing
//! `support::...` consumers keep compiling.

#![allow(dead_code)]

#[cfg(feature = "db-integration")]
use {
    anyhow::Result,
    bitcoin::{block::Header, consensus::serialize, hashes::Hash as _},
    mmm_bitcoin_core::{ConfiguredParentClassifier, ParentClassification},
    mmm_capture::{
        auxpow::ParsedAuxpowBlock,
        capture::{
            ClassificationProof, MergeMiningEventPayload, ParentKind, build_event_payload,
            resolve_event_pools,
        },
        pool_resolver::PoolResolver,
        source_registry::NAMECOIN_SOURCE_CODE,
    },
    mmm_read_model::capture_in_txn,
    mmm_store::{get_source_id, upsert_merge_mining_event, upsert_pool_snapshot},
    std::collections::HashMap,
    tokio_postgres::Client,
};

#[cfg(feature = "db-integration")]
pub mod db;
#[cfg(feature = "db-integration")]
pub mod scenario;
#[cfg(feature = "db-integration")]
pub mod seed;

// Re-export API: like the dead_code blanket above, items unused by a
// given binary are expected (each binary compiles its own support copy).
#[allow(unused_imports)]
#[cfg(feature = "test-support")]
pub use mmm_capture::test_support::{
    header_meeting_bits, parse_auxpow_fixture, valid_btc_header, valid_btc_header_two,
};

#[cfg(feature = "db-integration")]
pub type DefaultPoolSnapshot = (PoolResolver, HashMap<String, i64>);

#[cfg(feature = "db-integration")]
pub type NamecoinFixture = (
    PoolResolver,
    HashMap<String, i64>,
    i64,
    Box<ParsedAuxpowBlock>,
);

#[cfg(feature = "db-integration")]
pub async fn default_pool_snapshot(client: &Client) -> Result<DefaultPoolSnapshot> {
    let resolver = PoolResolver::from_default_snapshot()?;
    let pool_ids_by_slug = upsert_pool_snapshot(client, resolver.snapshot()).await?;
    Ok((resolver, pool_ids_by_slug))
}

#[cfg(feature = "db-integration")]
pub async fn namecoin_fixture(client: &Client) -> Result<NamecoinFixture> {
    let (resolver, pool_ids_by_slug) = default_pool_snapshot(client).await?;
    let source_id = get_source_id(client, NAMECOIN_SOURCE_CODE).await?;
    let parsed = parse_auxpow_fixture("500000-valid-parent")?;
    Ok((resolver, pool_ids_by_slug, source_id, parsed))
}

#[cfg(feature = "db-integration")]
pub fn namecoin_event_payload(
    parsed: &ParsedAuxpowBlock,
    resolver: &PoolResolver,
    pool_ids_by_slug: &HashMap<String, i64>,
    child_height: i32,
    proof: ClassificationProof,
    observed_at_epoch: i64,
) -> Result<MergeMiningEventPayload> {
    build_event_payload(
        parsed,
        Some(child_height),
        resolve_event_pools(parsed, resolver, pool_ids_by_slug),
        proof,
        observed_at_epoch,
    )
}

#[cfg(feature = "db-integration")]
pub async fn capture_test_payload(
    client: &mut Client,
    source_id: i64,
    classifier: &ConfiguredParentClassifier,
    payload: &mut MergeMiningEventPayload,
) -> Result<i64> {
    capture_in_txn(
        client,
        source_id,
        classifier,
        payload,
        "test",
        async |txn, sid, p| upsert_merge_mining_event(txn, sid, p).await,
    )
    .await
}

#[cfg(feature = "db-integration")]
pub struct NamecoinEventFixture {
    pub resolver: PoolResolver,
    pub pool_ids_by_slug: HashMap<String, i64>,
    pub source_id: i64,
    pub parsed: Box<ParsedAuxpowBlock>,
}

#[cfg(feature = "db-integration")]
pub struct InsertedNamecoinEvent {
    pub id: i64,
    pub parent_hash: Vec<u8>,
    pub header: Header,
}

#[cfg(feature = "db-integration")]
impl NamecoinEventFixture {
    pub async fn new(client: &Client) -> Result<Self> {
        let (resolver, pool_ids_by_slug, source_id, parsed) = namecoin_fixture(client).await?;
        Ok(Self {
            resolver,
            pool_ids_by_slug,
            source_id,
            parsed,
        })
    }

    pub fn payload(
        &self,
        child_height: i32,
        proof: ClassificationProof,
        observed_at_epoch: i64,
    ) -> Result<MergeMiningEventPayload> {
        namecoin_event_payload(
            &self.parsed,
            &self.resolver,
            &self.pool_ids_by_slug,
            child_height,
            proof,
            observed_at_epoch,
        )
    }

    pub async fn insert_event(
        &self,
        client: &Client,
        child_height: i32,
        proof: ClassificationProof,
        observed_at_epoch: i64,
    ) -> Result<InsertedNamecoinEvent> {
        let payload = self.payload(child_height, proof, observed_at_epoch)?;
        let id = upsert_merge_mining_event(client, self.source_id, &payload).await?;
        Ok(InsertedNamecoinEvent {
            id,
            parent_hash: self.parsed.parent_header.hash().to_byte_array().to_vec(),
            header: self.parsed.parent_header.header,
        })
    }

    pub async fn insert_event_with_header(
        &self,
        client: &Client,
        child_height: i32,
        child_hash_fill: u8,
        header: Header,
        proof: ClassificationProof,
        observed_at_epoch: i64,
    ) -> Result<InsertedNamecoinEvent> {
        let parent_hash = header.block_hash().to_byte_array().to_vec();
        let mut payload = self.payload(child_height, proof, observed_at_epoch)?;
        payload.child_block_hash = vec![child_hash_fill; 32];
        payload.btc_parent_header_hash = parent_hash.clone();
        payload.btc_parent_prev_header_hash = header.prev_blockhash.to_byte_array().to_vec();
        payload.btc_parent_header_bytes = serialize(&header);
        payload.btc_parent_header_time = header.time as i64;
        let id = upsert_merge_mining_event(client, self.source_id, &payload).await?;
        Ok(InsertedNamecoinEvent {
            id,
            parent_hash,
            header,
        })
    }
}

#[cfg(feature = "db-integration")]
pub fn canonical_parent_classification(
    header: &Header,
    height: i32,
    core_attested: bool,
) -> ParentClassification {
    ParentClassification {
        live_observed: core_attested,
        core_attested,
        ..scenario::canonical_verdict(header, height)
    }
}

#[cfg(feature = "db-integration")]
pub fn classified_proof(parent_kind: ParentKind, parent_height: i32) -> ClassificationProof {
    ClassificationProof {
        parent_kind: Some(parent_kind),
        parent_height: Some(parent_height),
        difficulty_epoch_ok: Some(true),
    }
}
