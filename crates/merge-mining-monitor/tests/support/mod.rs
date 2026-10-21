//! Shared helpers for this crate's integration-test binaries.

#![allow(dead_code)]

#[cfg(feature = "db-integration")]
use {
    anyhow::Result,
    bitcoin::{
        block::Header,
        consensus::{deserialize, serialize},
        hashes::Hash as _,
    },
    mmm_bitcoin_core::{ConfiguredParentClassifier, FakeParentClassifier, ParentClassification},
    mmm_capture::{
        auxpow::ParsedAuxpowBlock,
        auxpow::parse_bip34_height,
        btc_orphan::{BtcOrphanVerdict, classify_btc_orphan_with},
        capture::{
            ClassificationProof, MergeMiningEventPayload, ParentKind, ResolvedPoolAttributions,
            build_event_payload, resolve_event_pools_with_child_payout,
        },
        pool_resolver::PoolResolver,
        source_registry::NAMECOIN_SOURCE_CODE,
    },
    mmm_read_model::capture_in_txn,
    mmm_store::{
        CaptureWrite, get_source_id, load_bitcoin_core_nbits_table, upsert_merge_mining_event,
        upsert_pool_snapshot, write_capture,
    },
    std::collections::HashMap,
    tokio_postgres::Client,
};

#[cfg(feature = "db-integration")]
pub mod db;
#[cfg(feature = "db-integration")]
pub mod scenario;
#[cfg(feature = "db-integration")]
pub mod seed;

#[allow(unused_imports)]
#[cfg(feature = "test-support")]
pub use mmm_capture::test_support::{header_meeting_bits, parse_auxpow_fixture};

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
const BTC_400000_HEADER_HEX: &str = "0400000039fa821848781f027a2e6dfabbf6bda920d9ae61b63400030000000000000000ecae536a304042e3154be0e3e9a8220e5568c3433a9ab49ac4cbb74f8df8e8b0cc2acf569fb9061806652c27";

#[cfg(feature = "db-integration")]
const BTC_400000_COINBASE_SCRIPTSIG_HEX: &str = "03801a060004cc2acf560433c30f37085d4a39ad543b0c000a425720537570706f727420384d200a666973686572206a696e78696e092f425720506f6f6c2f";

#[cfg(feature = "db-integration")]
pub fn btc_400000_header() -> Result<Header> {
    Ok(deserialize(&hex::decode(BTC_400000_HEADER_HEX)?)?)
}

#[cfg(feature = "db-integration")]
pub fn btc_400000_coinbase_script() -> Result<Vec<u8>> {
    Ok(hex::decode(BTC_400000_COINBASE_SCRIPTSIG_HEX)?)
}

#[cfg(feature = "db-integration")]
pub async fn btc_400000_orphan_fixture(client: &Client) -> Result<(Header, Vec<u8>, &'static str)> {
    let header = btc_400000_header()?;
    let coinbase_script = btc_400000_coinbase_script()?;
    let nbits_table = load_bitcoin_core_nbits_table(client).await?;
    let relevance = match classify_btc_orphan_with(
        &nbits_table,
        i64::from(header.time),
        header.bits,
        parse_bip34_height(&coinbase_script),
    )
    .0
    {
        BtcOrphanVerdict::Strict => "strict_btc_orphan",
        BtcOrphanVerdict::Weak => "weak_btc_orphan",
        other => panic!("fixture must be strict/weak, got {other:?}"),
    };
    Ok((header, coinbase_script, relevance))
}

#[cfg(feature = "db-integration")]
pub fn absent_classifier(header: &Header) -> ConfiguredParentClassifier {
    ConfiguredParentClassifier::Fake(FakeParentClassifier::new(
        scenario::orphan_candidate_verdict(header),
    ))
}

#[cfg(feature = "db-integration")]
pub async fn default_pool_snapshot(client: &Client) -> Result<DefaultPoolSnapshot> {
    let resolver = PoolResolver::from_default_snapshot()?;
    let pool_ids_by_slug = upsert_pool_snapshot(client, resolver.snapshot()).await?;
    Ok((resolver, pool_ids_by_slug))
}

#[cfg(feature = "db-integration")]
pub async fn seed_synthetic_fixture_history(client: &Client) -> Result<()> {
    db::seed_synthetic_bitcoin_history(client, 800_002, 1_700_000_130, 0x207f_ffff).await
}

#[cfg(feature = "db-integration")]
pub async fn namecoin_fixture(client: &Client) -> Result<NamecoinFixture> {
    seed_synthetic_fixture_history(client).await?;
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
        resolve_event_pools_with_child_payout(parsed, resolver, pool_ids_by_slug, None, None),
        proof,
        observed_at_epoch,
    )
}

/// Capture a payload through the production path and return the event it wrote.
#[cfg(feature = "db-integration")]
pub async fn capture_test_payload(
    client: &mut Client,
    source_id: i64,
    classifier: &ConfiguredParentClassifier,
    payload: &mut MergeMiningEventPayload,
) -> Result<i64> {
    written_event_id(capture_test_outcome(client, source_id, classifier, payload).await?)
}

/// Capture a payload through the production path and return its outcome.
#[cfg(feature = "db-integration")]
pub async fn capture_test_outcome(
    client: &mut Client,
    source_id: i64,
    classifier: &ConfiguredParentClassifier,
    payload: &mut MergeMiningEventPayload,
) -> Result<mmm_read_model::CaptureOutcome> {
    capture_in_txn(
        client,
        source_id,
        classifier,
        payload,
        "test",
        async |txn, sid, p| write_capture(txn, sid, p).await,
    )
    .await
}

#[cfg(feature = "db-integration")]
pub fn written_event_id(outcome: mmm_read_model::CaptureOutcome) -> Result<i64> {
    match outcome {
        mmm_read_model::CaptureOutcome::Written(write) => event_id_of(write),
        refused => anyhow::bail!("the lineage gate refused the fixture parent: {refused:?}"),
    }
}

/// The event a capture wrote; a near records only its observation.
pub fn event_id_of(write: CaptureWrite) -> Result<i64> {
    match write {
        CaptureWrite::Event(written) => Ok(written.event_id),
        CaptureWrite::Near { .. } => {
            anyhow::bail!("the fixture parent is a near, which records no event")
        }
    }
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
        let id = upsert_merge_mining_event(client, self.source_id, &payload)
            .await?
            .event_id;
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
        payload.child_block_hash = Some(vec![child_hash_fill; 32]);
        payload.btc_parent_header_hash = parent_hash.clone();
        payload.btc_parent_prev_header_hash = header.prev_blockhash.to_byte_array().to_vec();
        payload.btc_parent_header_bytes = serialize(&header);
        payload.btc_parent_header_time = header.time as i64;
        let id = upsert_merge_mining_event(client, self.source_id, &payload)
            .await?
            .event_id;
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
        provisional: false,
    }
}

#[cfg(feature = "db-integration")]
pub fn exact_observation(
    fixture: &str,
    height: i32,
    hash: [u8; 32],
    observed_at: i64,
) -> Result<MergeMiningEventPayload> {
    let parsed = parse_auxpow_fixture(fixture)?;
    let mut payload = build_event_payload(
        &parsed,
        Some(height),
        ResolvedPoolAttributions::default(),
        ClassificationProof::default(),
        observed_at,
    )?;
    payload.child_block_hash = Some(hash.to_vec());
    payload.child_header_bytes = None;
    payload.child_nbits = None;
    payload.pow_validates_child_target = None;
    Ok(payload)
}

/// [`exact_observation`] of the `500001-near-parent` fixture as unknown evidence. Its parent
/// misses Bitcoin's target, so a near has no event; store tests that need an event on a
/// second parent use it in that role.
#[cfg(feature = "db-integration")]
pub fn second_parent_event(
    height: i32,
    hash: [u8; 32],
    observed_at: i64,
) -> Result<MergeMiningEventPayload> {
    let mut payload = exact_observation("500001-near-parent", height, hash, observed_at)?;
    payload.btc_parent_kind = ParentKind::Unknown;
    payload.pow_validates_btc_target = true;
    Ok(payload)
}

/// [`mmm_store::write_capture`] in its own committed transaction, as capture writes.
#[cfg(feature = "db-integration")]
pub async fn write_capture_committed(
    client: &mut tokio_postgres::Client,
    source_id: i64,
    payload: &MergeMiningEventPayload,
) -> Result<CaptureWrite> {
    let txn = client.transaction().await?;
    let write = mmm_store::write_capture(&txn, source_id, payload).await?;
    txn.commit().await?;
    Ok(write)
}
