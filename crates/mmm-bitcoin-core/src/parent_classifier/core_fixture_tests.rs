//! Runs the PRODUCTION `ConfiguredParentClassifier` / `classify_parent`
//! against the scripted `core_fixture` server for the three shapes of parent
//! evidence Core-backed classification distinguishes: a header Core already
//! carries as canonical, one it carries as a losing (stale) sibling, and one
//! it has never seen at all (forcing the inferred-stale median-time-past
//! ancestor walk in `core.rs`'s `classify_inferred_stale`/
//! `median_time_past_passes`, around lines 299-393).
//!
//! Every case passes an empty `ParentPreflight` (`known_prev: None`), so the
//! classifier resolves the predecessor itself over RPC instead of a read-model
//! lookup; none of these tests need Postgres.
//!
//! The expected `RpcMetrics`/server-side counts documented on each assertion
//! were captured by running this suite once and reading back the actual
//! `RpcMetricsSnapshot` and `FixtureRequestCounts`, not derived from theory
//! alone; a future change to `core.rs`'s call sequence should update both the
//! code and this comment together.

use bitcoin::hashes::Hash as _;

use super::core_fixture::{self, FixtureChain};
use super::{BitcoinCoreParentClassifier, ConfiguredParentClassifier, ParentPreflight};
use mmm_capture::capture::ParentKind;

const BASE_TIME: u32 = 1_700_000_000;

fn classifier_for(fixture: &core_fixture::SpawnedFixture) -> ConfiguredParentClassifier {
    let url = format!("http://{}", fixture.addr);
    ConfiguredParentClassifier::BitcoinCore(
        BitcoinCoreParentClassifier::from_env_url(&url).expect("build fixture-backed classifier"),
    )
}

fn empty_preflight() -> ParentPreflight {
    ParentPreflight { known_prev: None }
}

#[tokio::test]
async fn classifies_a_known_canonical_parent() {
    let (chain, headers) = FixtureChain::mainline(11, BASE_TIME);
    let candidate = headers[5];
    let fixture = core_fixture::spawn(chain, 8);
    let classifier = classifier_for(&fixture);

    let result = classifier
        .classify_parent(&candidate, empty_preflight())
        .await
        .expect("classify canonical candidate");

    assert_eq!(result.kind, ParentKind::Canonical);
    assert_eq!(result.height, Some(5));
    assert!(result.core_attested);

    let snapshot = classifier
        .metrics()
        .expect("Core-backed classifier reports metrics")
        .snapshot();
    // One verbose getblockheader (confirmations/height for the candidate)
    // plus one getblock (its coinbase); the canonical branch returns
    // immediately without fetching a competitor.
    assert_eq!(snapshot.http_attempts, 2);
    assert_eq!(snapshot.rpc_elements, 2);
    assert_eq!(snapshot.retries, 0);
    assert_eq!(snapshot.failures, 0);
    assert_eq!(fixture.counts.getblockheader(), 1);
    assert_eq!(fixture.counts.getblock(), 1);
    assert_eq!(fixture.counts.getblockhash(), 0);
    assert_eq!(fixture.counts.getblockcount(), 0);
    assert_eq!(fixture.counts.getblockchaininfo(), 0);
}

#[tokio::test]
async fn classifies_a_stale_competitor_at_a_known_height() {
    let (mut chain, headers) = FixtureChain::mainline(11, BASE_TIME);
    // A losing sibling at height 8, forked off the real height-7 ancestor but
    // never adopted by Core as the main-chain block at height 8.
    let stale_candidate = core_fixture::header(headers[7].block_hash(), BASE_TIME + 8, 500_008);
    chain.insert_stale(8, stale_candidate);
    let fixture = core_fixture::spawn(chain, 8);
    let classifier = classifier_for(&fixture);

    let result = classifier
        .classify_parent(&stale_candidate, empty_preflight())
        .await
        .expect("classify stale candidate");

    assert_eq!(result.kind, ParentKind::Stale);
    assert_eq!(result.height, Some(8));
    assert_eq!(
        result.canonical_competitor_hash,
        Some(headers[8].block_hash().to_byte_array().to_vec())
    );
    assert!(result.core_attested);

    let snapshot = classifier
        .metrics()
        .expect("Core-backed classifier reports metrics")
        .snapshot();
    // Candidate: 1 verbose getblockheader (confirmations == -1) + 1 getblock
    // (its coinbase). Competitor at the same height: 1 getblockhash + 1
    // non-verbose getblockheader + 1 getblock (its coinbase). 5 total.
    assert_eq!(snapshot.http_attempts, 5);
    assert_eq!(snapshot.rpc_elements, 5);
    assert_eq!(snapshot.retries, 0);
    assert_eq!(snapshot.failures, 0);
    assert_eq!(fixture.counts.getblockheader(), 2);
    assert_eq!(fixture.counts.getblock(), 2);
    assert_eq!(fixture.counts.getblockhash(), 1);
    assert_eq!(fixture.counts.getblockcount(), 0);
    assert_eq!(fixture.counts.getblockchaininfo(), 0);
}

#[tokio::test]
async fn classifies_an_unknown_parent_via_the_median_time_past_ancestor_walk() {
    // Heights 0..=10 give the MTP walk its full eleven-header window; height
    // 11 doubles as the inferred-stale same-height competitor.
    let (chain, headers) = FixtureChain::mainline(11, BASE_TIME);
    // Never registered with the fixture (neither canonical nor stale), so
    // Core reports it "not found" and the classifier falls back to inferring
    // stale-ness from its known-canonical predecessor at height 10.
    let unknown_candidate = core_fixture::header(headers[10].block_hash(), BASE_TIME + 11, 999_999);
    let fixture = core_fixture::spawn(chain, 30);
    let classifier = classifier_for(&fixture);

    let result = classifier
        .classify_parent(&unknown_candidate, empty_preflight())
        .await
        .expect("classify unknown candidate via the MTP walk");

    assert_eq!(result.kind, ParentKind::Stale);
    assert_eq!(result.height, Some(11));
    // Inferred (not Core-indexed) stale evidence: proven via the predecessor
    // chain and MTP walk rather than Core's own stale-block index.
    assert!(!result.core_attested);
    assert!(!result.live_observed);

    let snapshot = classifier
        .metrics()
        .expect("Core-backed classifier reports metrics")
        .snapshot();
    // 1 verbose getblockheader for the candidate itself (not found, the sole
    // failure) + 1 verbose getblockheader for its declared predecessor
    // (canonical) + 2 (non-verbose getblockheader + getblock) fetching that
    // predecessor's full header/coinbase + 3 (getblockhash + non-verbose
    // getblockheader + getblock) fetching the same-height competitor + 11
    // sequential non-verbose getblockheader calls walking the MTP window.
    assert_eq!(snapshot.http_attempts, 18);
    assert_eq!(snapshot.rpc_elements, 18);
    assert_eq!(snapshot.retries, 0);
    assert_eq!(snapshot.failures, 1);
    // getblockheader: 2 verbose + 1 (predecessor hex) + 1 (competitor hex) + 11 (MTP walk) = 15.
    assert_eq!(fixture.counts.getblockheader(), 15);
    // getblock: predecessor coinbase + competitor coinbase = 2.
    assert_eq!(fixture.counts.getblock(), 2);
    assert_eq!(fixture.counts.getblockhash(), 1);
    assert_eq!(fixture.counts.getblockcount(), 0);
    assert_eq!(fixture.counts.getblockchaininfo(), 0);
}
