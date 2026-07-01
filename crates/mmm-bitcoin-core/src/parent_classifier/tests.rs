//! Classifier tests (moved verbatim; bodies byte-identical).

use super::core::{
    CoreHeaderSource, CoreRpcFuture, classify_core_canonical_header, classify_core_stale_header,
    classify_inferred_stale_with_competitor, core_height_to_i32, tip_is_fresh,
};
use super::*;
use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

fn assert_send<T: Send>(_: T) {}

#[derive(Clone)]
enum MockResult<T> {
    Ok(T),
    NotFound,
    Error,
}

impl<T> MockResult<T> {
    fn into_result(self) -> Result<T> {
        match self {
            Self::Ok(value) => Ok(value),
            Self::NotFound => Err(bitcoin_rpc::test_not_found_error()),
            Self::Error => Err(anyhow::anyhow!("mock transient rpc error")),
        }
    }
}

#[derive(Default)]
struct MockCoreHeaderSource {
    chain_status: Mutex<Option<MockResult<BitcoinCoreChainStatus>>>,
    verbose: Mutex<HashMap<BlockHash, MockResult<CoreHeaderStatus>>>,
    block_hashes: Mutex<HashMap<u64, MockResult<BlockHash>>>,
    headers: Mutex<HashMap<BlockHash, MockResult<ClassifiedHeader>>>,
    block_headers: Mutex<HashMap<BlockHash, MockResult<Header>>>,
    coinbases: Mutex<HashMap<BlockHash, MockResult<BitcoinCoreBlockCoinbase>>>,
    block_hash_calls: AtomicUsize,
    block_header_calls: AtomicUsize,
    coinbase_calls: AtomicUsize,
}

impl MockCoreHeaderSource {
    fn set_chain_status(&self, result: MockResult<BitcoinCoreChainStatus>) {
        *self.chain_status.lock().unwrap() = Some(result);
    }

    fn set_verbose(&self, hash: BlockHash, result: MockResult<CoreHeaderStatus>) {
        self.verbose.lock().unwrap().insert(hash, result);
    }

    fn set_block_hash(&self, height: u64, result: MockResult<BlockHash>) {
        self.block_hashes.lock().unwrap().insert(height, result);
    }

    fn set_header(&self, hash: BlockHash, result: MockResult<ClassifiedHeader>) {
        self.headers.lock().unwrap().insert(hash, result);
    }

    fn set_coinbase(&self, hash: BlockHash, result: MockResult<BitcoinCoreBlockCoinbase>) {
        self.coinbases.lock().unwrap().insert(hash, result);
    }

    fn set_block_header(&self, hash: BlockHash, result: MockResult<Header>) {
        self.block_headers.lock().unwrap().insert(hash, result);
    }

    fn block_hash_calls(&self) -> usize {
        self.block_hash_calls.load(Ordering::SeqCst)
    }

    fn block_header_calls(&self) -> usize {
        self.block_header_calls.load(Ordering::SeqCst)
    }

    fn coinbase_calls(&self) -> usize {
        self.coinbase_calls.load(Ordering::SeqCst)
    }
}

impl CoreHeaderSource for MockCoreHeaderSource {
    fn get_chain_status(&self) -> CoreRpcFuture<'_, BitcoinCoreChainStatus> {
        Box::pin(async move {
            self.chain_status
                .lock()
                .unwrap()
                .clone()
                .unwrap_or(MockResult::Error)
                .into_result()
        })
    }

    fn get_block_hash(&self, height: u64) -> CoreRpcFuture<'_, BlockHash> {
        Box::pin(async move {
            self.block_hash_calls.fetch_add(1, Ordering::SeqCst);
            self.block_hashes
                .lock()
                .unwrap()
                .get(&height)
                .cloned()
                .unwrap_or(MockResult::NotFound)
                .into_result()
        })
    }

    fn get_header(&self, hash: BlockHash, _height: i32) -> CoreRpcFuture<'_, ClassifiedHeader> {
        Box::pin(async move {
            self.headers
                .lock()
                .unwrap()
                .get(&hash)
                .cloned()
                .unwrap_or(MockResult::NotFound)
                .into_result()
        })
    }

    fn get_block_header(&self, hash: BlockHash) -> CoreRpcFuture<'_, Header> {
        Box::pin(async move {
            self.block_header_calls.fetch_add(1, Ordering::SeqCst);
            self.block_headers
                .lock()
                .unwrap()
                .get(&hash)
                .cloned()
                .unwrap_or(MockResult::NotFound)
                .into_result()
        })
    }

    fn get_header_verbose(&self, hash: BlockHash) -> CoreRpcFuture<'_, CoreHeaderStatus> {
        Box::pin(async move {
            self.verbose
                .lock()
                .unwrap()
                .get(&hash)
                .cloned()
                .unwrap_or(MockResult::NotFound)
                .into_result()
        })
    }

    fn get_block_coinbase(&self, hash: BlockHash) -> CoreRpcFuture<'_, BitcoinCoreBlockCoinbase> {
        Box::pin(async move {
            self.coinbase_calls.fetch_add(1, Ordering::SeqCst);
            self.coinbases
                .lock()
                .unwrap()
                .get(&hash)
                .cloned()
                .unwrap_or(MockResult::NotFound)
                .into_result()
        })
    }
}

fn test_header(nonce: u32, bits: u32) -> Header {
    Header {
        version: bitcoin::block::Version::ONE,
        prev_blockhash: BlockHash::all_zeros(),
        merkle_root: bitcoin::TxMerkleNode::all_zeros(),
        time: 1,
        bits: CompactTarget::from_consensus(bits),
        nonce,
    }
}

fn classified_header(header: Header, height: i32) -> ClassifiedHeader {
    ClassifiedHeader {
        hash: header.block_hash().to_byte_array().to_vec(),
        prev_hash: header.prev_blockhash.to_byte_array().to_vec(),
        header,
        height,
        coinbase: None,
    }
}

#[test]
fn production_classifier_future_is_send() {
    let header = test_header(0, 0x207f_ffff);
    let disabled = ConfiguredParentClassifier::Disabled;
    assert_send(disabled.classify_parent(&header, ParentPreflight { known_prev: None }));

    let bitcoin_core = ConfiguredParentClassifier::BitcoinCore(
        BitcoinCoreParentClassifier::from_source(Arc::new(MockCoreHeaderSource::default())),
    );
    assert_send(bitcoin_core.classify_parent(&header, ParentPreflight { known_prev: None }));
}

#[tokio::test]
async fn disabled_classifier_returns_unknown() {
    let header = test_header(0, 0x207f_ffff);
    let result = ConfiguredParentClassifier::Disabled
        .classify_parent(&header, ParentPreflight { known_prev: None })
        .await
        .unwrap();
    assert_eq!(result.kind, ParentKind::Unknown);
    assert!(!result.core_attested);
}

#[tokio::test]
async fn configured_classifier_exposes_only_synced_core_tip_height() {
    let disabled_tip = ConfiguredParentClassifier::Disabled
        .synced_tip_height()
        .await
        .unwrap();
    assert_eq!(disabled_tip, None);

    let synced_source = Arc::new(MockCoreHeaderSource::default());
    synced_source.set_chain_status(MockResult::Ok(BitcoinCoreChainStatus {
        blocks: 953_305,
        headers: 953_305,
        initial_block_download: false,
        median_time: 0,
    }));
    let synced_tip = ConfiguredParentClassifier::BitcoinCore(
        BitcoinCoreParentClassifier::from_source(synced_source),
    )
    .synced_tip_height()
    .await
    .unwrap();
    assert_eq!(synced_tip, Some(953_305));

    let unsynced_source = Arc::new(MockCoreHeaderSource::default());
    unsynced_source.set_chain_status(MockResult::Ok(BitcoinCoreChainStatus {
        blocks: 953_304,
        headers: 953_305,
        initial_block_download: false,
        median_time: 0,
    }));
    let unsynced_tip = ConfiguredParentClassifier::BitcoinCore(
        BitcoinCoreParentClassifier::from_source(unsynced_source),
    )
    .synced_tip_height()
    .await
    .unwrap();
    assert_eq!(unsynced_tip, None);

    let fake_tip = ConfiguredParentClassifier::Fake(
        FakeParentClassifier::new(ParentClassification::unknown(&test_header(43, 0x207f_ffff)))
            .with_synced_tip_height(953_305),
    )
    .synced_tip_height()
    .await
    .unwrap();
    assert_eq!(fake_tip, Some(953_305));
}

#[tokio::test]
async fn epoch_nbits_fetches_header_only_and_memoizes() {
    let source = Arc::new(MockCoreHeaderSource::default());
    // `test_header` carries time = 1; bits is the Elastos live-stall epoch nBits.
    let header = test_header(7, 0x1702_40c3);
    let hash = header.block_hash();
    source.set_block_hash(955_584, MockResult::Ok(hash));
    source.set_block_header(hash, MockResult::Ok(header));
    let classifier = BitcoinCoreParentClassifier::from_source(source.clone());

    // A buried epoch (tip well past REORG_SAFE_DEPTH below the start) is memoized.
    let buried_tip = 956_000;
    let first = classifier.epoch_nbits(955_584, buried_tip).await.unwrap();
    assert_eq!(
        first,
        EpochNbits {
            nbits: 0x1702_40c3,
            header_time: 1,
        }
    );

    // A second lookup for the same epoch is a memo hit: no further Core calls.
    let second = classifier.epoch_nbits(955_584, buried_tip).await.unwrap();
    assert_eq!(second, first);

    assert_eq!(
        source.block_hash_calls(),
        1,
        "one getblockhash per epoch miss"
    );
    assert_eq!(
        source.block_header_calls(),
        1,
        "one header-only getblockheader per epoch miss"
    );
    assert_eq!(
        source.coinbase_calls(),
        0,
        "epoch nBits path must not fetch the coinbase / full block"
    );
}

#[tokio::test]
async fn epoch_nbits_does_not_cache_a_reorg_shallow_epoch() {
    let source = Arc::new(MockCoreHeaderSource::default());
    let header = test_header(8, 0x1702_40c3);
    let hash = header.block_hash();
    source.set_block_hash(955_584, MockResult::Ok(hash));
    source.set_block_header(hash, MockResult::Ok(header));
    let classifier = BitcoinCoreParentClassifier::from_source(source.clone());

    // The tip is only ~16 blocks past the epoch start, so the start is NOT buried
    // past REORG_SAFE_DEPTH: a reorg across the retarget boundary could still change
    // its nBits, so it is re-fetched each call rather than cached.
    let shallow_tip = 955_600;
    classifier.epoch_nbits(955_584, shallow_tip).await.unwrap();
    classifier.epoch_nbits(955_584, shallow_tip).await.unwrap();
    assert_eq!(
        source.block_hash_calls(),
        2,
        "a reorg-shallow epoch is re-fetched fresh, never cached"
    );
    assert_eq!(source.block_header_calls(), 2);
}

#[test]
fn core_chain_classification_handles_canonical_stale_and_self_match() {
    let header = test_header(1, 0x207f_ffff);
    let coinbase = BitcoinCoreBlockCoinbase {
        txid: vec![1; 32],
        script: b"/KnCMiner/".to_vec(),
        outputs: vec![2, 3],
    };
    let canonical = classify_core_canonical_header(&header, 720_000, Some(coinbase.clone()));
    assert_eq!(canonical.kind, ParentKind::Canonical);
    assert_eq!(canonical.height, Some(720_000));
    assert_eq!(canonical.height_source, Some(HeightSource::BitcoinCore));
    assert_eq!(canonical.difficulty_epoch_ok, Some(true));
    assert!(canonical.live_observed);
    assert!(canonical.core_attested);
    assert_eq!(canonical.coinbase, Some(coinbase));

    let competitor = classified_header(test_header(2, 0x207f_ffff), 720_000);
    let stale_coinbase = BitcoinCoreBlockCoinbase {
        txid: vec![4; 32],
        script: b"/Slush/".to_vec(),
        outputs: vec![5, 6],
    };
    let stale = classify_core_stale_header(
        &header,
        720_000,
        Some(competitor.clone()),
        Some(stale_coinbase.clone()),
    );
    assert_eq!(stale.kind, ParentKind::Stale);
    assert_eq!(stale.height, Some(720_000));
    assert_eq!(
        stale.canonical_competitor_hash,
        Some(competitor.hash.clone())
    );
    assert!(stale.live_observed);
    assert!(stale.core_attested);
    assert_eq!(stale.coinbase, Some(stale_coinbase));

    let self_match = classify_core_stale_header(
        &header,
        720_000,
        Some(classified_header(header, 720_000)),
        None,
    );
    assert_eq!(self_match.kind, ParentKind::Unknown);
    assert_eq!(self_match.canonical_competitor_hash, None);

    let missing_competitor = classify_core_stale_header(&header, 720_000, None, None);
    assert_eq!(missing_competitor.kind, ParentKind::Unknown);
}

#[test]
fn inferred_stale_classification_checks_height_source_and_bits() {
    let header = test_header(10, 0x207f_ffff);
    let predecessor = classified_header(test_header(9, 0x207f_ffff), 719_999);
    let competitor = classified_header(test_header(11, 0x207f_ffff), 720_000);

    let inferred = classify_inferred_stale_with_competitor(
        &header,
        720_000,
        Some(predecessor.clone()),
        BlockKind::Canonical,
        Some(competitor.clone()),
    );
    assert_eq!(inferred.kind, ParentKind::Stale);
    assert_eq!(inferred.height, Some(720_000));
    assert_eq!(inferred.height_source, Some(HeightSource::PrevCanonical));
    assert_eq!(inferred.canonical_predecessor_header, Some(predecessor));
    assert_eq!(
        inferred.canonical_competitor_hash,
        Some(competitor.hash.clone())
    );
    assert_eq!(inferred.difficulty_epoch_ok, Some(true));
    assert!(!inferred.live_observed);
    assert!(!inferred.core_attested);

    let inferred_from_stale_prev = classify_inferred_stale_with_competitor(
        &header,
        720_000,
        None,
        BlockKind::Stale,
        Some(competitor),
    );
    assert_eq!(
        inferred_from_stale_prev.height_source,
        Some(HeightSource::PrevStale)
    );

    let mut mismatch_header = test_header(12, 0x1d00_ffff);
    mismatch_header.prev_blockhash = header.prev_blockhash;
    let mismatch_competitor = classified_header(mismatch_header, 720_000);
    let mismatch = classify_inferred_stale_with_competitor(
        &header,
        720_000,
        None,
        BlockKind::Canonical,
        Some(mismatch_competitor),
    );
    assert_eq!(mismatch.kind, ParentKind::Unknown);
    assert_eq!(mismatch.difficulty_epoch_ok, Some(false));

    let missing =
        classify_inferred_stale_with_competitor(&header, 720_000, None, BlockKind::Canonical, None);
    assert_eq!(missing.kind, ParentKind::Unknown);
}

#[test]
fn tip_is_fresh_within_the_max_age() {
    // A synced tip is fresh while its median time is within ~24h of now, and stale
    // once older (a lagging / isolated node), so the far-future guard holds instead
    // of revoking valid evidence against a stale tip.
    let now = 2_000_000_000;
    assert!(tip_is_fresh(now, now));
    assert!(tip_is_fresh(now - 86_400, now)); // exactly at the bound
    assert!(!tip_is_fresh(now - 86_401, now)); // just over the bound -> stale
    assert!(!tip_is_fresh(0, now)); // epoch-old (median_time unavailable) -> stale
    assert!(tip_is_fresh(now + 100, now)); // minor clock skew (future) is fresh
}

#[test]
fn core_height_overflow_is_rejected() {
    assert_eq!(core_height_to_i32(i32::MAX as i64).unwrap(), i32::MAX);
    assert!(core_height_to_i32(i32::MAX as i64 + 1).is_err());
}

#[tokio::test]
async fn bitcoin_core_classifier_uses_verbose_canonical_and_stale_paths() {
    let canonical_header = test_header(20, 0x207f_ffff);
    let canonical_source = Arc::new(MockCoreHeaderSource::default());
    canonical_source.set_verbose(
        canonical_header.block_hash(),
        MockResult::Ok(CoreHeaderStatus {
            confirmations: 5,
            height: 720_000,
        }),
    );
    let canonical_coinbase = BitcoinCoreBlockCoinbase {
        txid: vec![7; 32],
        script: b"/KnCMiner/".to_vec(),
        outputs: vec![],
    };
    canonical_source.set_coinbase(
        canonical_header.block_hash(),
        MockResult::Ok(canonical_coinbase.clone()),
    );
    let canonical = BitcoinCoreParentClassifier::from_source(canonical_source)
        .classify_parent(&canonical_header, ParentPreflight { known_prev: None })
        .await
        .unwrap();
    assert_eq!(canonical.kind, ParentKind::Canonical);
    assert_eq!(canonical.height, Some(720_000));
    assert!(canonical.core_attested);
    assert_eq!(canonical.coinbase, Some(canonical_coinbase));

    let stale_header = test_header(21, 0x207f_ffff);
    let competitor = classified_header(test_header(22, 0x207f_ffff), 720_001);
    let competitor_hash = BlockHash::from_slice(&competitor.hash).unwrap();
    let stale_source = Arc::new(MockCoreHeaderSource::default());
    stale_source.set_verbose(
        stale_header.block_hash(),
        MockResult::Ok(CoreHeaderStatus {
            confirmations: -1,
            height: 720_001,
        }),
    );
    stale_source.set_block_hash(720_001, MockResult::Ok(competitor_hash));
    stale_source.set_header(competitor_hash, MockResult::Ok(competitor.clone()));
    let stale = BitcoinCoreParentClassifier::from_source(stale_source)
        .classify_parent(&stale_header, ParentPreflight { known_prev: None })
        .await
        .unwrap();
    assert_eq!(stale.kind, ParentKind::Stale);
    assert_eq!(stale.height, Some(720_001));
    assert_eq!(stale.height_source, Some(HeightSource::BitcoinCore));
    assert_eq!(stale.canonical_competitor_hash, Some(competitor.hash));
}

#[tokio::test]
async fn bitcoin_core_classifier_infers_stale_after_not_found() {
    let header = test_header(30, 0x207f_ffff);
    let competitor = classified_header(test_header(31, 0x207f_ffff), 720_010);
    let competitor_hash = BlockHash::from_slice(&competitor.hash).unwrap();
    let source = Arc::new(MockCoreHeaderSource::default());
    source.set_block_hash(720_010, MockResult::Ok(competitor_hash));
    source.set_header(competitor_hash, MockResult::Ok(competitor.clone()));

    let from_preflight = BitcoinCoreParentClassifier::from_source(source.clone())
        .classify_parent(
            &header,
            ParentPreflight {
                known_prev: Some(KnownBlockContext {
                    kind: BlockKind::Canonical,
                    btc_height: Some(720_009),
                    btc_height_source: Some(HeightSource::BitcoinCore),
                    canonical_competitor_hash: None,
                    core_attested: true,
                }),
            },
        )
        .await
        .unwrap();
    assert_eq!(from_preflight.kind, ParentKind::Stale);
    assert_eq!(
        from_preflight.height_source,
        Some(HeightSource::PrevCanonical)
    );
    assert_eq!(
        from_preflight.canonical_competitor_hash,
        Some(competitor.hash.clone())
    );

    source.set_verbose(
        header.prev_blockhash,
        MockResult::Ok(CoreHeaderStatus {
            confirmations: 1,
            height: 720_009,
        }),
    );
    source.set_header(
        header.prev_blockhash,
        MockResult::Ok(classified_header(header, 720_009)),
    );
    let from_prev_lookup = BitcoinCoreParentClassifier::from_source(source)
        .classify_parent(&header, ParentPreflight { known_prev: None })
        .await
        .unwrap();
    assert_eq!(from_prev_lookup.kind, ParentKind::Stale);
    assert_eq!(
        from_prev_lookup.height_source,
        Some(HeightSource::PrevCanonical)
    );
    assert!(from_prev_lookup.canonical_predecessor_header.is_some());
}

#[tokio::test]
async fn core_absence_attested_only_on_candidate_not_found() {
    let header = test_header(40, 0x207f_ffff);

    // Disabled: Core never consulted -> not absence-attested.
    let disabled = ConfiguredParentClassifier::Disabled
        .classify_parent(&header, ParentPreflight { known_prev: None })
        .await
        .unwrap();
    assert_eq!(disabled.kind, ParentKind::Unknown);
    assert!(!disabled.core_absence_attested);

    // Candidate verbose lookup fails with a transient (non-not-found) error:
    // unknown, but NOT absence-attested (we do not know it is absent).
    let transient_source = Arc::new(MockCoreHeaderSource::default());
    transient_source.set_verbose(header.block_hash(), MockResult::Error);
    let transient = BitcoinCoreParentClassifier::from_source(transient_source)
        .classify_parent(&header, ParentPreflight { known_prev: None })
        .await
        .unwrap();
    assert_eq!(transient.kind, ParentKind::Unknown);
    assert!(!transient.core_absence_attested);

    // Candidate not-found + predecessor not-found: attested absent.
    let absent =
        BitcoinCoreParentClassifier::from_source(Arc::new(MockCoreHeaderSource::default()))
            .classify_parent(&header, ParentPreflight { known_prev: None })
            .await
            .unwrap();
    assert_eq!(absent.kind, ParentKind::Unknown);
    assert!(absent.core_absence_attested);

    // Candidate not-found + predecessor transient error: still attested,
    // because the candidate itself was proven absent.
    let prev_err_source = Arc::new(MockCoreHeaderSource::default());
    prev_err_source.set_verbose(header.prev_blockhash, MockResult::Error);
    let prev_err = BitcoinCoreParentClassifier::from_source(prev_err_source)
        .classify_parent(&header, ParentPreflight { known_prev: None })
        .await
        .unwrap();
    assert_eq!(prev_err.kind, ParentKind::Unknown);
    assert!(prev_err.core_absence_attested);

    // Candidate not-found + inferred-stale path returns unknown for a missing
    // competitor: attested (the candidate was absent).
    let canonical_prev = || KnownBlockContext {
        kind: BlockKind::Canonical,
        btc_height: Some(720_000),
        btc_height_source: Some(HeightSource::BitcoinCore),
        canonical_competitor_hash: None,
        core_attested: true,
    };
    let missing_comp =
        BitcoinCoreParentClassifier::from_source(Arc::new(MockCoreHeaderSource::default()))
            .classify_parent(
                &header,
                ParentPreflight {
                    known_prev: Some(canonical_prev()),
                },
            )
            .await
            .unwrap();
    assert_eq!(missing_comp.kind, ParentKind::Unknown);
    assert!(missing_comp.core_absence_attested);

    // Candidate not-found + inferred-stale path returns unknown for a
    // competitor whose nBits mismatches: attested.
    let mismatch_header = test_header(41, 0x207f_ffff);
    let mut competitor_header = test_header(42, 0x1d00_ffff);
    competitor_header.prev_blockhash = mismatch_header.prev_blockhash;
    let competitor = classified_header(competitor_header, 720_001);
    let competitor_hash = BlockHash::from_slice(&competitor.hash).unwrap();
    let mismatch_source = Arc::new(MockCoreHeaderSource::default());
    mismatch_source.set_block_hash(720_001, MockResult::Ok(competitor_hash));
    mismatch_source.set_header(competitor_hash, MockResult::Ok(competitor));
    let mismatch = BitcoinCoreParentClassifier::from_source(mismatch_source)
        .classify_parent(
            &mismatch_header,
            ParentPreflight {
                known_prev: Some(canonical_prev()),
            },
        )
        .await
        .unwrap();
    assert_eq!(mismatch.kind, ParentKind::Unknown);
    assert!(mismatch.core_absence_attested);
}
