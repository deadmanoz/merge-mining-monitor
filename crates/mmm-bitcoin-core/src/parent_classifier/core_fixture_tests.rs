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
use super::{
    BitcoinCoreParentClassifier, BlockKind, ConfiguredParentClassifier, HeightSource,
    KnownBlockContext, ParentPreflight,
};
use mmm_capture::capture::ParentKind;

const BASE_TIME: u32 = 1_700_000_000;

#[tokio::test]
async fn strict_import_classification_rpc_budget() {
    use std::time::{Duration, Instant};

    let delay_ms = std::env::var("MMM_TEST_RPC_DELAY_MS")
        .map(|value| value.parse::<u64>().expect("integer RPC fixture delay"))
        .unwrap_or(0);
    // The aggregate preflight caches by parent hash. Exercise ten distinct
    // uncached parents, including the MTP-invalid result, with and without the
    // locally persisted predecessor that import_error_observation_decision uses.
    for known_prev in [true, false] {
        let (chain, headers) = FixtureChain::mainline(11, BASE_TIME);
        let fixture = core_fixture::spawn_with_delay(chain, 200, Duration::from_millis(delay_ms));
        let classifier = classifier_for(&fixture);
        let started = Instant::now();
        for index in 0..10 {
            let candidate = core_fixture::header(
                headers[10].block_hash(),
                if index == 0 {
                    BASE_TIME
                } else {
                    BASE_TIME + 11
                },
                900_000 + index,
            );
            let preflight = ParentPreflight {
                known_prev: known_prev.then_some(KnownBlockContext {
                    kind: BlockKind::Canonical,
                    btc_height: Some(10),
                    btc_height_source: Some(HeightSource::BitcoinCore),
                    canonical_competitor_hash: None,
                    core_attested: true,
                }),
            };
            let before = classifier.metrics().unwrap().snapshot();
            let result = classifier
                .classify_parent_strict(&candidate, preflight)
                .await
                .unwrap();
            assert_eq!(
                result.kind,
                if index == 0 {
                    ParentKind::ErrorBlock
                } else {
                    ParentKind::Stale
                }
            );
            let after = classifier.metrics().unwrap().snapshot();
            assert_eq!(
                after.http_attempts - before.http_attempts,
                if known_prev { 15 } else { 18 }
            );
            assert_eq!(
                after.rpc_elements - before.rpc_elements,
                if known_prev { 15 } else { 18 }
            );
        }
        let snapshot = classifier.metrics().unwrap().snapshot();
        assert_eq!(snapshot.retries, 0);
        assert_eq!(
            snapshot.failures, 10,
            "one expected Core not-found per parent"
        );
        assert_eq!(
            fixture.counts.getblockheader(),
            if known_prev { 130 } else { 150 }
        );
        assert_eq!(fixture.counts.getblockhash(), 10);
        assert_eq!(fixture.counts.getblock(), if known_prev { 10 } else { 20 });
        eprintln!(
            "strict import RPC receipt: parents=10 known_prev={known_prev} delay_ms={delay_ms} http_attempts={} rpc_elements={} retries={} wall_seconds={:.3}",
            snapshot.http_attempts,
            snapshot.rpc_elements,
            snapshot.retries,
            started.elapsed().as_secs_f64(),
        );
    }
}

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
