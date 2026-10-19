//! Read-model scenario builder for projection behavior tests, driving Bitcoin
//! Core spine blocks and child-chain evidence through production mutation entry points.

use std::collections::BTreeMap;

use anyhow::{Context, Result};
use bitcoin::block::Header;
use bitcoin::hashes::Hash as _;
use mmm_bitcoin_core::{
    ClassifiedHeader, ConfiguredParentClassifier, FakeParentClassifier, HeightSource,
    ParentClassification,
};
use mmm_capture::capture::{
    NormalizedEventEvidence, ParentKind, ResolvedPoolAttributions,
    build_event_payload_from_evidence,
};
use mmm_read_model::{
    CoreCanonicalWrite, capture_in_txn, revoke_merge_mining_event, write_core_canonical,
};
use mmm_store::{get_source_id, upsert_merge_mining_event_with_attributions};
use tokio_postgres::Client;

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
        coinbase_unavailable: false,
    };
    ParentClassification {
        canonical_competitor_header: Some(competitor),
        ..stale_verdict(header, height, competitor_hash)
    }
}

pub fn unknown_verdict(header: &Header) -> ParentClassification {
    ParentClassification::unknown(header)
}

pub fn orphan_candidate_verdict(header: &Header) -> ParentClassification {
    ParentClassification {
        core_absence_attested: true,
        ..ParentClassification::unknown(header)
    }
}

pub struct ChildEvidence {
    pub key: &'static str,
    pub source_code: &'static str,
    pub child_height: i32,
    pub child_block_hash: Vec<u8>,
    pub parent_header: Header,
    pub verdict: ParentClassification,
    pub observed_at: i64,
    pub parent_coinbase_script: Option<Vec<u8>>,
}

impl ChildEvidence {
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
        }
    }

    pub fn with_parent_coinbase_script(mut self, script: Vec<u8>) -> Self {
        self.parent_coinbase_script = Some(script);
        self
    }
}

enum Step {
    CoreCanonical { header: Header, height: i32 },
    Child(Box<ChildEvidence>),
}

#[derive(Default)]
pub struct Scenario {
    steps: Vec<Step>,
}

impl Scenario {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn core_canonical(mut self, header: Header, height: i32) -> Self {
        self.steps.push(Step::CoreCanonical { header, height });
        self
    }

    pub fn child_event(mut self, evidence: ChildEvidence) -> Self {
        self.steps.push(Step::Child(Box::new(evidence)));
        self
    }

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
                    let mut payload = build_event_payload_from_evidence(
                        NormalizedEventEvidence {
                            child_height: Some(evidence.child_height),
                            child_block_hash: Some(evidence.child_block_hash),
                            child_header_bytes: None,
                            child_block_time: Some(evidence.observed_at),
                            child_nbits: None,
                            child_prev_block_hash: None,
                            btc_parent_header: evidence.parent_header,
                            pow_validates_child_target: Some(true),
                            btc_parent_coinbase_txid: None,
                            btc_parent_coinbase_script: evidence.parent_coinbase_script,
                            btc_parent_coinbase_outputs: None,
                            btc_parent_coinbase_outputs_text: None,
                            btc_parent_coinbase_tx_bytes: None,
                            child_coinbase_txid: None,
                            child_coinbase_script: None,
                            child_coinbase_outputs: None,
                            aux_merkle_proof: None,
                        },
                        ResolvedPoolAttributions::default(),
                        proof,
                        evidence.observed_at,
                    )?;
                    let classifier = ConfiguredParentClassifier::Fake(FakeParentClassifier::new(
                        evidence.verdict,
                    ));
                    let event_id = super::written_event_id(
                        capture_in_txn(
                            client,
                            source_id,
                            &classifier,
                            &mut payload,
                            "scenario",
                            async |txn, sid, p| {
                                upsert_merge_mining_event_with_attributions(txn, sid, p).await
                            },
                        )
                        .await?,
                    )?;
                    event_ids.insert(evidence.key, event_id);
                }
            }
        }
        Ok(Applied { event_ids })
    }
}

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

pub async fn capture_child_event(client: &mut Client, evidence: ChildEvidence) -> Result<i64> {
    let key = evidence.key;
    Scenario::new()
        .child_event(evidence)
        .apply(client)
        .await?
        .event_id(key)
}

pub async fn revoke_event(client: &mut Client, event_id: i64, reason: &str) -> Result<()> {
    revoke_merge_mining_event(
        client,
        event_id,
        reason,
        &ConfiguredParentClassifier::Disabled,
    )
    .await
}
