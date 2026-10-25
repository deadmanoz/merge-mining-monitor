//! Relationship signals: every pool a Bitcoin parent names, and whether they name more than one.
//!
//! A parent names pools through its coinbase tag and payout addresses, each child block's reward
//! address or coinbase tag, and the protocol accounts its coinbase declares. Each one is an
//! association, not proof of who mined, built or controls anything (`docs/attribution.md`).
//!
//! The rule is pure and generic over the pool type, so the API's block page and the read
//! model's stored `relationship_summary` apply exactly the same derivation.

use std::collections::{HashMap, HashSet};
use std::sync::OnceLock;

use bitcoin::consensus::deserialize;
use serde::Serialize;
use tracing::warn;

use crate::auxpow::{TxOut, output_addresses};
use crate::capture::{
    BTC_COINBASE_TAG_NAMESPACE, BTC_PAYOUT_ADDRESS_NAMESPACE,
    published_parent_coinbase_output_addresses,
};
use crate::pool_resolver::{ELASTOS_MINERINFO_NAMESPACE, PoolResolver};
use crate::protocol_commitment::{CORE_REWARD_ADDRESS_NAMESPACE, EXSAT_ACCOUNT_NAMESPACE};

const BITCOIN_COINBASE_TAG: &str = "bitcoin_coinbase_tag";
const BITCOIN_PAYOUT_ADDRESS: &str = "bitcoin_payout_address";
const CHILD_REWARD_ADDRESS: &str = "child_reward_address";
const CHILD_COINBASE_TAG: &str = "child_coinbase_tag";
const PROTOCOL_ACCOUNT: &str = "protocol_account";

const POOL_SNAPSHOT_BASIS: &str = "pool_snapshot";
const CHILD_REGISTRY_BASIS: &str = "child_registry";
const PROTOCOL_ACCOUNT_REGISTRY_BASIS: &str = "protocol_account_registry";

/// A pool as a caller represents it: the API's full pool object, or the read model's bare
/// `pool.id`.
pub trait SignalPool: Clone {
    /// The `pool.id` of a known pool; `None` for an unknown one.
    fn pool_id(&self) -> Option<i64>;
    /// The unknown pool.
    fn unknown() -> Self;
}

impl SignalPool for Option<i64> {
    fn pool_id(&self) -> Option<i64> {
        *self
    }

    fn unknown() -> Self {
        None
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct RelationshipSignals<P> {
    /// True when `pools` holds two or more distinct known pools.
    pub mixed: bool,
    /// The block's Bitcoin miner when known, else the first known signal's pool.
    pub reference_pool: P,
    /// The distinct known pools of `reference_pool` and every entry, reference first.
    pub pools: Vec<P>,
    pub entries: Vec<RelationshipSignal<P>>,
}

#[derive(Debug, Clone, Serialize)]
pub struct RelationshipSignal<P> {
    pub kind: &'static str,
    /// The child source for a child signal, else `null`.
    pub source: Option<String>,
    pub namespace: String,
    pub value: String,
    pub pool: P,
    pub basis: &'static str,
}

/// One stored copy of a parent coinbase: its scriptSig, and its outputs as consensus bytes or
/// as a publication's output text.
#[derive(Debug, Clone, Copy, Default)]
pub struct CoinbaseEvidence<'a> {
    pub script: Option<&'a [u8]>,
    pub outputs: Option<&'a [u8]>,
    pub outputs_text: Option<&'a str>,
}

impl CoinbaseEvidence<'_> {
    /// Structured outputs rank first, then output text, then a script alone; an empty copy has
    /// no rank. SQL callers that pick a copy in the database rank the same way.
    fn tier(&self) -> Option<u8> {
        if self.outputs.is_some() {
            Some(0)
        } else if self.outputs_text.is_some() {
            Some(1)
        } else {
            self.script.map(|_| 2)
        }
    }
}

/// The copy a parent's signals read among `(row id, copy)` pairs: the lowest tier, then the
/// lowest id, so a partial copy never shadows a more complete one.
pub fn choose_coinbase_evidence<'a>(
    copies: impl IntoIterator<Item = (i64, CoinbaseEvidence<'a>)>,
) -> Option<CoinbaseEvidence<'a>> {
    copies
        .into_iter()
        .filter_map(|(id, copy)| copy.tier().map(|tier| ((tier, id), copy)))
        .min_by_key(|(rank, _)| *rank)
        .map(|(_, copy)| copy)
}

/// A signal whose pool is already resolved, or named by a pool-snapshot slug that the caller
/// resolves to its `pool` row once per batch.
#[derive(Debug, Clone)]
pub struct SignalDraft<P> {
    kind: &'static str,
    source: Option<String>,
    namespace: String,
    value: String,
    pool: DraftPool<P>,
    basis: &'static str,
}

#[derive(Debug, Clone)]
enum DraftPool<P> {
    Known(P),
    Snapshot(String),
}

impl<P: SignalPool> SignalDraft<P> {
    /// A child attribution row or identity: a label the child coinbase declares (a legacy
    /// child-script tag or Elastos minerinfo), else the child block's reward or payout address.
    pub fn child(source: &str, namespace: &str, value: &str, pool: &P) -> Self {
        let (kind, basis) = match namespace {
            BTC_COINBASE_TAG_NAMESPACE => (CHILD_COINBASE_TAG, POOL_SNAPSHOT_BASIS),
            ELASTOS_MINERINFO_NAMESPACE => (CHILD_COINBASE_TAG, CHILD_REGISTRY_BASIS),
            _ => (CHILD_REWARD_ADDRESS, CHILD_REGISTRY_BASIS),
        };
        Self {
            kind,
            source: Some(source.to_owned()),
            namespace: namespace.to_owned(),
            value: value.to_owned(),
            pool: DraftPool::Known(pool.clone()),
            basis,
        }
    }

    fn snapshot(
        kind: &'static str,
        source: Option<&str>,
        namespace: &str,
        value: &str,
        slug: &str,
    ) -> Self {
        Self {
            kind,
            source: source.map(str::to_owned),
            namespace: namespace.to_owned(),
            value: value.to_owned(),
            pool: DraftPool::Snapshot(slug.to_owned()),
            basis: POOL_SNAPSHOT_BASIS,
        }
    }

    /// The pool a draft already carries; `None` for a snapshot slug still to resolve.
    pub fn known_pool(&self) -> Option<&P> {
        match &self.pool {
            DraftPool::Known(pool) => Some(pool),
            DraftPool::Snapshot(_) => None,
        }
    }

    /// The pool-snapshot slug a draft names; `None` once its pool is known.
    pub fn snapshot_slug(&self) -> Option<&str> {
        match &self.pool {
            DraftPool::Known(_) => None,
            DraftPool::Snapshot(slug) => Some(slug),
        }
    }

    fn resolve(self, pools_by_slug: &HashMap<String, P>) -> RelationshipSignal<P> {
        let pool = match self.pool {
            DraftPool::Known(pool) => pool,
            // A snapshot slug the `pool` table lacks resolves to no pool, as at capture.
            DraftPool::Snapshot(slug) => {
                pools_by_slug.get(&slug).cloned().unwrap_or_else(P::unknown)
            }
        };
        RelationshipSignal {
            kind: self.kind,
            source: self.source,
            namespace: self.namespace,
            value: self.value,
            pool,
            basis: self.basis,
        }
    }
}

/// A CORE reward address or exSat account the coinbase declares, keyed as `pool_identity` keys
/// it; only a valid declaration carries an account.
pub fn protocol_account_draft<P: SignalPool>(
    protocol: &str,
    account: Option<&str>,
    pool: &P,
) -> Option<SignalDraft<P>> {
    let namespace = match protocol {
        "core" => CORE_REWARD_ADDRESS_NAMESPACE,
        "exsat" => EXSAT_ACCOUNT_NAMESPACE,
        _ => return None,
    };
    Some(SignalDraft {
        kind: PROTOCOL_ACCOUNT,
        source: None,
        namespace: namespace.to_owned(),
        value: account?.to_owned(),
        pool: DraftPool::Known(pool.clone()),
        basis: PROTOCOL_ACCOUNT_REGISTRY_BASIS,
    })
}

/// The pool resolver over the compiled-in snapshot, built once per process; `None` (logged)
/// when the snapshot does not load.
pub fn default_pool_resolver() -> Option<&'static PoolResolver> {
    static RESOLVER: OnceLock<Option<PoolResolver>> = OnceLock::new();
    RESOLVER
        .get_or_init(|| {
            PoolResolver::from_default_snapshot()
                .inspect_err(|err| warn!(error = %err, "pool snapshot failed to load"))
                .ok()
        })
        .as_ref()
}

/// The parent coinbase's tag and every payout address the pool snapshot recognizes.
pub fn bitcoin_drafts<P: SignalPool>(
    coinbase: Option<CoinbaseEvidence<'_>>,
) -> Vec<SignalDraft<P>> {
    match (coinbase, default_pool_resolver()) {
        (Some(coinbase), Some(resolver)) => bitcoin_drafts_with(resolver, coinbase),
        _ => Vec::new(),
    }
}

/// A near observation's child coinbase tag, resolved as capture resolves the legacy
/// child-script tag (`resolve_child_pool_attribution_from_script`).
pub fn child_coinbase_tag_draft<P: SignalPool>(
    source: &str,
    script: Option<&[u8]>,
) -> Option<SignalDraft<P>> {
    child_coinbase_tag_draft_with(default_pool_resolver()?, source, script?)
}

fn bitcoin_drafts_with<P: SignalPool>(
    resolver: &PoolResolver,
    coinbase: CoinbaseEvidence<'_>,
) -> Vec<SignalDraft<P>> {
    let mut drafts = Vec::new();
    if let Some(tag) = coinbase
        .script
        .and_then(|script| resolver.resolve_coinbase_script(script))
    {
        drafts.push(SignalDraft::snapshot(
            BITCOIN_COINBASE_TAG,
            None,
            BTC_COINBASE_TAG_NAMESPACE,
            tag.matched_value,
            &tag.pool.slug,
        ));
    }
    for address in payout_addresses(coinbase) {
        if let Some(payout) = resolver.resolve_payout_address(&address) {
            drafts.push(SignalDraft::snapshot(
                BITCOIN_PAYOUT_ADDRESS,
                None,
                BTC_PAYOUT_ADDRESS_NAMESPACE,
                &address,
                &payout.pool.slug,
            ));
        }
    }
    drafts
}

fn child_coinbase_tag_draft_with<P: SignalPool>(
    resolver: &PoolResolver,
    source: &str,
    script: &[u8],
) -> Option<SignalDraft<P>> {
    let tag = resolver.resolve_coinbase_script(script)?;
    Some(SignalDraft::snapshot(
        CHILD_COINBASE_TAG,
        Some(source),
        BTC_COINBASE_TAG_NAMESPACE,
        tag.matched_value,
        &tag.pool.slug,
    ))
}

/// Payout addresses from the structured outputs, else from the published output text. Outputs
/// that do not deserialize fall back to the text rather than failing the caller; [`finish`]
/// lists a repeated address once.
fn payout_addresses(coinbase: CoinbaseEvidence<'_>) -> Vec<String> {
    if let Some(bytes) = coinbase.outputs {
        match deserialize::<Vec<TxOut>>(bytes) {
            Ok(outputs) => return output_addresses(&outputs),
            Err(err) => {
                warn!(error = %err, "parent coinbase outputs do not deserialize; using output text")
            }
        }
    }
    coinbase
        .outputs_text
        .map(published_parent_coinbase_output_addresses)
        .unwrap_or_default()
}

/// The signals with their pools, each once, and their summary against `bitcoin_miner_pool` (a
/// known miner counts even when no coinbase bytes are stored, since it derives from them).
/// `pools_by_slug` resolves the drafts' snapshot slugs.
pub fn finish<P: SignalPool>(
    bitcoin_miner_pool: &P,
    drafts: Vec<SignalDraft<P>>,
    pools_by_slug: &HashMap<String, P>,
) -> RelationshipSignals<P> {
    let mut seen = HashSet::new();
    let entries = drafts
        .into_iter()
        .filter(|draft| {
            seen.insert((
                draft.kind,
                draft.source.clone(),
                draft.namespace.clone(),
                draft.value.clone(),
            ))
        })
        .map(|draft| draft.resolve(pools_by_slug))
        .collect::<Vec<_>>();
    let mut pools: Vec<P> = Vec::new();
    for pool in std::iter::once(bitcoin_miner_pool).chain(entries.iter().map(|entry| &entry.pool)) {
        if let Some(id) = pool.pool_id()
            && !pools.iter().any(|seen| seen.pool_id() == Some(id))
        {
            pools.push(pool.clone());
        }
    }
    RelationshipSignals {
        mixed: pools.len() > 1,
        reference_pool: pools.first().cloned().unwrap_or_else(P::unknown),
        pools,
        entries,
    }
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use bitcoin::consensus::serialize;
    use bitcoin::{Address, Amount, Network};

    use super::*;

    const ALPHA_PAYOUT: &str = "1A1zP1eP5QGefi2DMPTfTL5SLmv7DivfNa";
    const BETA_PAYOUT: &str = "1BvBMSEYstWetqTFn5Au4m4GFg7xJaNVN2";

    type TestPool = Option<i64>;

    const ALPHA: TestPool = Some(1);
    const BETA: TestPool = Some(2);

    fn resolver() -> PoolResolver {
        PoolResolver::from_json_str(&format!(
            r#"{{
                "schema_version": 1, "generated_at": "2026-10-24", "scope": "test",
                "source": {{ "name": "test" }},
                "pools": [
                    {{ "slug": "alpha", "canonical_name": "Alpha", "coinbase_tags": ["/Alpha/"],
                       "payout_addresses": ["{ALPHA_PAYOUT}"] }},
                    {{ "slug": "beta", "canonical_name": "Beta", "coinbase_tags": ["/Beta/"],
                       "payout_addresses": ["{BETA_PAYOUT}"] }}
                ]
            }}"#
        ))
        .unwrap()
    }

    fn pools_by_slug() -> HashMap<String, TestPool> {
        HashMap::from([("alpha".to_owned(), ALPHA), ("beta".to_owned(), BETA)])
    }

    fn outputs_paying(addresses: &[&str]) -> Vec<u8> {
        let outputs = addresses
            .iter()
            .map(|address| TxOut {
                value: Amount::from_sat(1),
                script_pubkey: Address::from_str(address)
                    .unwrap()
                    .require_network(Network::Bitcoin)
                    .unwrap()
                    .script_pubkey(),
            })
            .collect::<Vec<_>>();
        serialize(&outputs)
    }

    fn summarize(
        miner: &TestPool,
        drafts: Vec<SignalDraft<TestPool>>,
    ) -> RelationshipSignals<TestPool> {
        finish(miner, drafts, &pools_by_slug())
    }

    fn kinds(signals: &RelationshipSignals<TestPool>) -> Vec<&'static str> {
        signals.entries.iter().map(|entry| entry.kind).collect()
    }

    fn pool_ids(signals: &RelationshipSignals<TestPool>) -> Vec<Option<i64>> {
        signals.pools.iter().map(SignalPool::pool_id).collect()
    }

    #[test]
    fn a_tag_and_a_payout_naming_different_pools_are_mixed() {
        let outputs = outputs_paying(&[BETA_PAYOUT]);
        let coinbase = CoinbaseEvidence {
            script: Some(b"\x03/Alpha/"),
            outputs: Some(&outputs),
            outputs_text: None,
        };
        let signals = summarize(&ALPHA, bitcoin_drafts_with(&resolver(), coinbase));
        assert_eq!(
            kinds(&signals),
            [BITCOIN_COINBASE_TAG, BITCOIN_PAYOUT_ADDRESS]
        );
        assert_eq!(pool_ids(&signals), [Some(1), Some(2)]);
        assert!(signals.mixed);
    }

    #[test]
    fn child_and_declared_signals_naming_another_pool_are_mixed() {
        let rsk = SignalDraft::child("auxpow:rsk", "rsk_miner_address", "aa", &BETA);
        let signals = summarize(&ALPHA, vec![rsk]);
        assert_eq!(signals.entries[0].kind, CHILD_REWARD_ADDRESS);
        assert!(signals.mixed);

        let label = SignalDraft::child("auxpow:elastos", "elastos_minerinfo", "e", &BETA);
        let signals = summarize(&ALPHA, vec![label]);
        let entry = &signals.entries[0];
        assert_eq!(
            (entry.kind, entry.basis),
            (CHILD_COINBASE_TAG, CHILD_REGISTRY_BASIS)
        );
        assert!(signals.mixed);

        let exsat = protocol_account_draft("exsat", Some("beta.sat"), &BETA).unwrap();
        let signals = summarize(&ALPHA, vec![exsat]);
        assert_eq!(signals.entries[0].basis, PROTOCOL_ACCOUNT_REGISTRY_BASIS);
        assert!(signals.mixed);
    }

    #[test]
    fn signals_naming_one_pool_or_unknown_pools_are_not_mixed() {
        let outputs = outputs_paying(&[ALPHA_PAYOUT]);
        let coinbase = CoinbaseEvidence {
            script: Some(b"/Alpha/"),
            outputs: Some(&outputs),
            outputs_text: None,
        };
        let mut drafts = bitcoin_drafts_with(&resolver(), coinbase);
        drafts.push(SignalDraft::child(
            "auxpow:rsk",
            "rsk_miner_address",
            "aa",
            &ALPHA,
        ));
        drafts.push(SignalDraft::child(
            "auxpow:namecoin",
            "namecoin_payout_address",
            "n",
            &None,
        ));
        let signals = summarize(&ALPHA, drafts);
        assert_eq!(signals.entries.len(), 4);
        assert_eq!(pool_ids(&signals), [Some(1)]);
        assert!(!signals.mixed);

        let unknown_only = SignalDraft::child("auxpow:rsk", "rsk_miner_address", "aa", &None);
        let signals = summarize(&None, vec![unknown_only]);
        assert!(
            signals.pools.is_empty()
                && !signals.mixed
                && signals.reference_pool.pool_id().is_none()
        );
    }

    #[test]
    fn a_known_miner_without_coinbase_bytes_is_the_reference() {
        assert!(
            bitcoin_drafts_with::<TestPool>(&resolver(), CoinbaseEvidence::default()).is_empty()
        );
        let rsk = SignalDraft::child("auxpow:rsk", "rsk_miner_address", "aa", &BETA);
        let signals = summarize(&ALPHA, vec![rsk]);
        assert_eq!(signals.reference_pool, ALPHA);
        assert!(signals.mixed);
    }

    #[test]
    fn payouts_come_from_output_text_when_structured_outputs_are_absent_or_undecodable() {
        let text = format!("{BETA_PAYOUT}:625000000;OP_RETURN aa21a9ed");
        let script_only = CoinbaseEvidence {
            script: Some(b"/Alpha/"),
            ..CoinbaseEvidence::default()
        };
        assert_eq!(
            kinds(&summarize(
                &None,
                bitcoin_drafts_with(&resolver(), script_only)
            )),
            [BITCOIN_COINBASE_TAG]
        );

        for outputs in [None, Some(&b"\xff\x00"[..])] {
            let coinbase = CoinbaseEvidence {
                script: Some(b"/Alpha/"),
                outputs,
                outputs_text: Some(&text),
            };
            let signals = summarize(&None, bitcoin_drafts_with(&resolver(), coinbase));
            assert_eq!(
                kinds(&signals),
                [BITCOIN_COINBASE_TAG, BITCOIN_PAYOUT_ADDRESS]
            );
            assert_eq!(signals.entries[1].value, BETA_PAYOUT);
            assert!(signals.mixed);
        }
    }

    #[test]
    fn a_near_child_tag_and_payout_identity_naming_different_pools_are_mixed() {
        let tag = child_coinbase_tag_draft_with(&resolver(), "auxpow:syscoin", b"/Alpha/").unwrap();
        let payout = SignalDraft::child("auxpow:syscoin", "syscoin_payout_address", "s", &BETA);
        let signals = summarize(&None, vec![tag, payout]);
        assert_eq!(kinds(&signals), [CHILD_COINBASE_TAG, CHILD_REWARD_ADDRESS]);
        assert!(signals.mixed);
    }

    #[test]
    fn the_most_complete_copy_wins_then_the_lowest_id() {
        let script_only = CoinbaseEvidence {
            script: Some(b"low"),
            ..CoinbaseEvidence::default()
        };
        let with_outputs = CoinbaseEvidence {
            script: Some(b"high"),
            outputs: Some(b"o"),
            outputs_text: None,
        };
        let with_text = CoinbaseEvidence {
            outputs_text: Some("t"),
            ..with_outputs
        };
        let rsk = CoinbaseEvidence::default();
        let chosen = |copies: Vec<(i64, CoinbaseEvidence<'static>)>| {
            choose_coinbase_evidence(copies).and_then(|copy| copy.script)
        };
        assert_eq!(
            chosen(vec![(1, script_only), (2, with_outputs), (3, rsk)]),
            Some(&b"high"[..])
        );
        let lower_outputs = CoinbaseEvidence {
            script: Some(b"lower"),
            ..with_outputs
        };
        assert_eq!(
            chosen(vec![(5, with_text), (4, lower_outputs)]),
            Some(&b"lower"[..])
        );
        assert_eq!(chosen(vec![(1, rsk)]), None);
    }
}
