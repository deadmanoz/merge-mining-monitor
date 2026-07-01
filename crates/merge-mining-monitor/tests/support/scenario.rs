//! Read-model scenario builder for projection behavior tests.
//!
//! Applies Bitcoin Core spine blocks and child-chain evidence through the
//! production mutation entry points, so derived block and proof rows stay
//! reconciler-owned. Keep using direct SQL seeds from
//! [`super::seed`] for table-layout tests such as pagination, filters, and
//! column handling.

use std::collections::BTreeMap;

use anyhow::{Context, Result};
use bitcoin::block::Header;
use bitcoin::hashes::Hash as _;
use mmm_bitcoin_core::{
    ClassifiedHeader, ConfiguredParentClassifier, FakeParentClassifier, HeightSource,
    ParentClassification,
};
use mmm_capture::capture::{
    EventPoolAttribution, NormalizedEventEvidence, ParentKind, PoolAttributionConfidence,
    PoolAttributionSide, ResolvedPoolAttributions, build_event_payload_from_evidence,
};
use mmm_read_model::{
    CoreCanonicalWrite, capture_in_txn, revoke_merge_mining_event, write_core_canonical,
};
use mmm_store::{get_source_id, upsert_merge_mining_event_with_attributions};
use serde_json::json;
use tokio_postgres::Client;

/// Verdict for a parent Core reports on its active chain.
pub fn canonical_verdict(header: &Header, height: i32) -> ParentClassification {
    ParentClassification {
        kind: ParentKind::Canonical,
        height: Some(height),
        height_source: Some(HeightSource::BitcoinCore),
        difficulty_epoch_ok: Some(true),
        live_observed: true,
        core_attested: true,
        ..ParentClassification::unknown(header)
    }
}

/// Verdict for a parent Core reports as a known stale block beaten by
/// `canonical_competitor_hash` at the same height.
pub fn stale_verdict(
    header: &Header,
    height: i32,
    canonical_competitor_hash: Vec<u8>,
) -> ParentClassification {
    ParentClassification {
        kind: ParentKind::Stale,
        height: Some(height),
        height_source: Some(HeightSource::BitcoinCore),
        canonical_competitor_hash: Some(canonical_competitor_hash),
        difficulty_epoch_ok: Some(true),
        live_observed: true,
        core_attested: true,
        ..ParentClassification::unknown(header)
    }
}

/// Verdict for a stale parent when the test also needs the synthesized
/// canonical competitor header persisted in the read model.
pub fn stale_verdict_with_competitor_header(
    header: &Header,
    height: i32,
    competitor_header: Header,
    competitor_hash: Vec<u8>,
) -> ParentClassification {
    let competitor = ClassifiedHeader {
        hash: competitor_hash.clone(),
        prev_hash: competitor_header.prev_blockhash.to_byte_array().to_vec(),
        header: competitor_header,
        height,
        coinbase: None,
    };
    ParentClassification {
        canonical_competitor_header: Some(competitor),
        ..stale_verdict(header, height, competitor_hash)
    }
}

/// Verdict for an unresolved parent (classifier disabled, transient error, or
/// genuinely never seen) - preserves any persisted orphan class.
pub fn unknown_verdict(header: &Header) -> ParentClassification {
    ParentClassification::unknown(header)
}

/// Verdict for a BTC-orphan candidate: Core was consulted and proved the
/// header absent, so the reconciler derives `block.btc_orphan_class`.
pub fn orphan_candidate_verdict(header: &Header) -> ParentClassification {
    ParentClassification {
        core_absence_attested: true,
        ..ParentClassification::unknown(header)
    }
}

/// One child-chain observation of a BTC parent, captured through the
/// production event write path under the given classifier verdict.
pub struct ChildEvidence {
    pub key: &'static str,
    pub source_code: &'static str,
    pub child_height: i32,
    pub child_block_hash: Vec<u8>,
    pub parent_header: Header,
    pub verdict: ParentClassification,
    pub observed_at: i64,
    pub parent_coinbase_script: Option<Vec<u8>>,
    pub parent_pool_id: Option<i64>,
}

impl ChildEvidence {
    /// The 32-byte child block hash mixes `child_height` into the leading
    /// bytes of a `child_seed` fill, so events are distinct per
    /// (source, height, seed) - the event key is
    /// (source_id, child_height, child_block_hash).
    pub fn new(
        key: &'static str,
        source_code: &'static str,
        child_height: i32,
        child_seed: u8,
        parent_header: Header,
        verdict: ParentClassification,
        observed_at: i64,
    ) -> Self {
        let mut child_block_hash = vec![child_seed; 32];
        child_block_hash[..4].copy_from_slice(&child_height.to_be_bytes());
        Self {
            key,
            source_code,
            child_height,
            child_block_hash,
            parent_header,
            verdict,
            observed_at,
            parent_coinbase_script: None,
            parent_pool_id: None,
        }
    }

    /// Attach a BTC parent coinbase scriptSig (e.g. carrying a pool tag) to
    /// the captured evidence.
    pub fn with_parent_coinbase_script(mut self, script: Vec<u8>) -> Self {
        self.parent_coinbase_script = Some(script);
        self
    }

    /// Attribute the BTC parent to an already-seeded pool id.
    pub fn with_parent_pool(mut self, pool_id: i64) -> Self {
        self.parent_pool_id = Some(pool_id);
        self
    }
}

enum Step {
    CoreCanonical { header: Header, height: i32 },
    Child(Box<ChildEvidence>),
}

/// Ordered scenario description; `apply` drives every step through the
/// production mutation entry points and returns the event ids by key.
#[derive(Default)]
pub struct Scenario {
    steps: Vec<Step>,
}

impl Scenario {
    pub fn new() -> Self {
        Self::default()
    }

    /// Append a Bitcoin Core spine block (canonical-by-Core observation),
    /// written via `write_core_canonical` with its cascade token consumed.
    pub fn core_canonical(mut self, header: Header, height: i32) -> Self {
        self.steps.push(Step::CoreCanonical { header, height });
        self
    }

    /// Append a child-chain evidence capture.
    pub fn child_event(mut self, evidence: ChildEvidence) -> Self {
        self.steps.push(Step::Child(Box::new(evidence)));
        self
    }

    /// Run every step in order through the production write paths.
    pub async fn apply(self, client: &mut Client) -> Result<Applied> {
        let mut event_ids = BTreeMap::new();
        for step in self.steps {
            match step {
                Step::CoreCanonical { header, height } => {
                    write_core_canonical(
                        client,
                        CoreCanonicalWrite {
                            header: &header,
                            height,
                            coinbase: None,
                        },
                        async |_txn| Ok(()),
                        "scenario core spine",
                    )
                    .await?
                    .cascade(client, &ConfiguredParentClassifier::Disabled)
                    .await?;
                }
                Step::Child(evidence) => {
                    let source_id = get_source_id(client, evidence.source_code).await?;
                    let proof = evidence.verdict.to_proof();
                    let attributions = evidence
                        .parent_pool_id
                        .filter(|_| {
                            evidence.source_code != mmm_capture::source_registry::RSK_SOURCE_CODE
                        })
                        .map(|pool_id| EventPoolAttribution {
                            side: PoolAttributionSide::BtcParent,
                            namespace: "btc_coinbase_tag",
                            match_kind: "test_seed",
                            matched_value: format!("test-pool-{pool_id}"),
                            pool_id: Some(pool_id),
                            pool_identity_id: None,
                            source: "test_seed",
                            confidence: PoolAttributionConfidence::High,
                            details: json!({}),
                        });
                    let mut payload = build_event_payload_from_evidence(
                        NormalizedEventEvidence {
                            child_height: evidence.child_height,
                            child_block_hash: evidence.child_block_hash,
                            child_block_time: evidence.observed_at,
                            btc_parent_header: evidence.parent_header,
                            pow_validates_child_target: Some(true),
                            btc_parent_coinbase_txid: None,
                            btc_parent_coinbase_script: evidence.parent_coinbase_script,
                            btc_parent_coinbase_outputs: None,
                            child_coinbase_txid: None,
                            child_coinbase_script: None,
                            child_coinbase_outputs: None,
                            aux_merkle_proof: None,
                        },
                        ResolvedPoolAttributions {
                            attributions: attributions.into_iter().collect(),
                        },
                        proof,
                        evidence.observed_at,
                    )?;
                    let classifier = ConfiguredParentClassifier::Fake(FakeParentClassifier::new(
                        evidence.verdict,
                    ));
                    let event_id = capture_in_txn(
                        client,
                        source_id,
                        &classifier,
                        &mut payload,
                        "scenario",
                        async |txn, sid, p| {
                            upsert_merge_mining_event_with_attributions(txn, sid, p).await
                        },
                    )
                    .await?;
                    event_ids.insert(evidence.key, event_id);
                }
            }
        }
        Ok(Applied { event_ids })
    }
}

/// Handle to the applied scenario: production event ids by evidence key.
pub struct Applied {
    pub event_ids: BTreeMap<&'static str, i64>,
}

impl Applied {
    pub fn event_id(&self, key: &str) -> Result<i64> {
        self.event_ids
            .get(key)
            .copied()
            .with_context(|| format!("scenario has no event keyed {key:?}"))
    }
}

/// Apply one child evidence capture through the same production mutation path as
/// [`Scenario::apply`], returning its event id.
pub async fn capture_child_event(client: &mut Client, evidence: ChildEvidence) -> Result<i64> {
    let key = evidence.key;
    Scenario::new()
        .child_event(evidence)
        .apply(client)
        .await?
        .event_id(key)
}

/// Revoke a captured event through the production mutation path (derived
/// rows rebuilt by the same cascade the CLI revoke command uses).
pub async fn revoke_event(client: &mut Client, event_id: i64, reason: &str) -> Result<()> {
    revoke_merge_mining_event(
        client,
        event_id,
        reason,
        &ConfiguredParentClassifier::Disabled,
    )
    .await
}
