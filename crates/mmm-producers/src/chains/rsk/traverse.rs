//! RSK canonical + uncle traversal: fetching height bundles from an RSK
//! block source, order-preserving and error-capturing, ready for the capture
//! writer in `super::capture`.

use anyhow::{Context, Result};
use futures::future::try_join_all;
use tracing::warn;

use crate::chains::rsk::rpc::{RskBlock, RskRpcClient, decode_quantity_i64};

#[allow(async_fn_in_trait)]
/// Async block-source trait over the RSK RPC client.
///
/// The bounded backfill prefetch pipeline ([`fetch_rsk_height_bundle`]) drives
/// this trait so a deterministic in-memory fake can exercise the fetch stage
/// without a live node. It stays crate-private; integration tests exercise the
/// public capture APIs rather than the traversal source abstraction.
///
/// `Clone` is a supertrait so the pipeline can clone the source per height
/// (`source.clone()`); `RskRpcClient` is a cheap `Arc`-backed clone.
//
// `async fn` in this trait yields non-`Send` futures, which is fine: the
// `buffered(K)` stream is polled within the single backfill task (never
// `tokio::spawn`ed across threads), so no `Send` bound is required. This mirrors
// the `ChainPoller` trait in `src/poller.rs`.
pub(crate) trait RskBlockSource: Clone {
    /// Canonical block at `height`, or `Ok(None)` when absent.
    async fn block_by_number(&self, height: i64) -> Result<Option<RskBlock>>;
    /// Uncle at canonical `height` and listed `idx`, or `Ok(None)` when the node
    /// no longer serves that listed uncle.
    async fn uncle_by_index(&self, height: i64, idx: i32) -> Result<Option<RskBlock>>;
}

impl RskBlockSource for RskRpcClient {
    async fn block_by_number(&self, height: i64) -> Result<Option<RskBlock>> {
        self.get_block_by_number(height).await
    }

    async fn uncle_by_index(&self, height: i64, idx: i32) -> Result<Option<RskBlock>> {
        self.get_uncle_by_block_number_and_index(height, idx).await
    }
}

/// One fetched uncle within a height bundle. The `result` preserves the listed
/// uncle's fetch outcome verbatim so the sequential write stage can mirror the
/// serial path: `Ok(Some)` is a fetched uncle block, `Ok(None)` is a listed
/// uncle the source returned `null` for (counted as malformed by the writer),
/// and `Err(..)` is a fetch error captured here rather than raised during the
/// fetch stage, so the writer can commit the canonical and prior uncles before
/// surfacing it.
#[derive(Debug)]
pub(crate) struct RskFetchedUncle {
    /// Listed uncle index on the canonical block, also the
    /// `eth_getUncleByBlockNumberAndIndex` argument.
    pub index: i32,
    /// Fetch outcome captured verbatim (see the type doc): `Ok(Some)` fetched,
    /// `Ok(None)` listed-but-null, `Err` deferred to the write stage.
    pub result: Result<Option<RskBlock>>,
}

/// Network-bound prefetch result for one RSK height: the canonical block (if
/// present) plus one [`RskFetchedUncle`] per uncle index listed on the
/// canonical block, in listed order. No DB access.
#[derive(Debug)]
pub(crate) struct RskHeightBundle {
    /// The canonical block, or `None` when the height has no canonical block.
    pub canonical: Option<RskBlock>,
    /// One entry per uncle listed on the canonical block, in listed order; empty
    /// when the canonical was absent or its number failed to decode to i32.
    pub uncles: Vec<RskFetchedUncle>,
}

/// Fetch the canonical block at `height` and, when its RSK number decodes
/// cleanly, fetch each listed uncle. This is the parallelizable, DB-free stage
/// of the bounded backfill pipeline.
///
/// Only RPC transport/protocol errors on the canonical fetch propagate via `?`.
/// A null canonical is `Ok(RskHeightBundle { canonical: None, .. })`. A
/// malformed/overflowing canonical `number` is non-fatal (matching
/// [`process_rsk_height`]): keep `canonical: Some(block)`, skip uncle fetching
/// (empty `uncles`), and do NOT error. Each listed uncle's fetch outcome is
/// captured into [`RskFetchedUncle::result`] (including fetch errors) so the
/// write stage can preserve serial ordering. Uncles for one height are fetched
/// concurrently.
pub(crate) async fn fetch_rsk_height_bundle<S: RskBlockSource>(
    source: S,
    height: i64,
) -> Result<RskHeightBundle> {
    let canonical = match source
        .block_by_number(height)
        .await
        .with_context(|| format!("get RSK canonical block at height {height}"))?
    {
        Some(block) => block,
        None => {
            return Ok(RskHeightBundle {
                canonical: None,
                uncles: Vec::new(),
            });
        }
    };

    // The only hard requirement to walk uncles is decoding the canonical's RSK
    // height for the `eth_getUncleByBlockNumberAndIndex` call. A malformed or
    // i32-overflowing block.number must not abort the bounded backfill: keep the
    // canonical block and skip uncle traversal (matching `process_rsk_height`).
    let canonical_height_i64 = match decode_quantity_i64(&canonical.number) {
        Ok(n) => n,
        Err(err) => {
            warn!(
                rsk_hash = %canonical.hash,
                raw = %canonical.number,
                error = %err,
                "malformed RSK canonical block.number; skipping uncle traversal"
            );
            return Ok(RskHeightBundle {
                canonical: Some(canonical),
                uncles: Vec::new(),
            });
        }
    };
    if i32::try_from(canonical_height_i64).is_err() {
        warn!(
            rsk_hash = %canonical.hash,
            height = canonical_height_i64,
            "RSK canonical block number overflows i32; skipping uncle traversal"
        );
        return Ok(RskHeightBundle {
            canonical: Some(canonical),
            uncles: Vec::new(),
        });
    }

    // Fetch all listed uncles concurrently. Each fetch outcome is captured into
    // the bundle (errors included) rather than raised, so the write stage can
    // commit the canonical + prior uncles before surfacing an error.
    let uncle_fetches = canonical.uncles.iter().enumerate().map(|(index, _hash)| {
        let source = source.clone();
        let index = index as i32;
        async move {
            let result = source.uncle_by_index(canonical_height_i64, index).await;
            Ok::<RskFetchedUncle, anyhow::Error>(RskFetchedUncle { index, result })
        }
    });
    let uncles = try_join_all(uncle_fetches).await?;

    Ok(RskHeightBundle {
        canonical: Some(canonical),
        uncles,
    })
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;
    use crate::chains::rsk::test_fixtures::{
        KNOWN_MINER_HEX, header_meeting_bits, load_rsk_block_fixture, rsk_block_with,
    };

    /// What the fake source should return for one canonical block or uncle
    /// lookup: a present block, an explicit `null` (`Ok(None)`), or a fetch
    /// error captured verbatim.
    #[derive(Clone)]
    enum FakeResponse {
        Block(Box<RskBlock>),
        Null,
        Error(String),
    }

    impl FakeResponse {
        fn block(block: RskBlock) -> Self {
            FakeResponse::Block(Box::new(block))
        }

        fn to_result(&self) -> Result<Option<RskBlock>> {
            match self {
                FakeResponse::Block(b) => Ok(Some((**b).clone())),
                FakeResponse::Null => Ok(None),
                FakeResponse::Error(msg) => Err(anyhow::anyhow!(msg.clone())),
            }
        }
    }

    #[derive(Clone, Default)]
    struct FakeRskSource {
        canonical: HashMap<i64, FakeResponse>,
        uncles: HashMap<(i64, i32), FakeResponse>,
    }

    impl FakeRskSource {
        fn with_canonical(mut self, height: i64, response: FakeResponse) -> Self {
            self.canonical.insert(height, response);
            self
        }

        fn with_uncle(mut self, height: i64, idx: i32, response: FakeResponse) -> Self {
            self.uncles.insert((height, idx), response);
            self
        }
    }

    impl RskBlockSource for FakeRskSource {
        async fn block_by_number(&self, height: i64) -> Result<Option<RskBlock>> {
            self.canonical
                .get(&height)
                .map_or(Ok(None), FakeResponse::to_result)
        }

        async fn uncle_by_index(&self, height: i64, idx: i32) -> Result<Option<RskBlock>> {
            self.uncles
                .get(&(height, idx))
                .map_or(Ok(None), FakeResponse::to_result)
        }
    }

    fn block_on<F: std::future::Future>(future: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
            .block_on(future)
    }

    #[test]
    fn fetch_missing_height_yields_empty_bundle() {
        let source = FakeRskSource::default();
        let bundle = block_on(fetch_rsk_height_bundle(source, 200_000)).unwrap();
        assert!(bundle.canonical.is_none());
        assert!(bundle.uncles.is_empty());
    }

    #[test]
    fn fetch_canonical_no_uncles_yields_canonical_only() {
        let block = load_rsk_block_fixture("canonical-valid");
        let source =
            FakeRskSource::default().with_canonical(729_000, FakeResponse::block(block.clone()));
        let bundle = block_on(fetch_rsk_height_bundle(source, 729_000)).unwrap();
        assert_eq!(bundle.canonical.as_ref(), Some(&block));
        assert!(bundle.uncles.is_empty());
    }

    #[test]
    fn fetch_pre_rskip92_canonical_is_kept_in_bundle() {
        // Pre-RSKIP-92 / decode skips are downstream concerns; the fetch stage
        // keeps any present canonical block verbatim.
        let block = load_rsk_block_fixture("pre-rskip92");
        let source =
            FakeRskSource::default().with_canonical(100_000, FakeResponse::block(block.clone()));
        let bundle = block_on(fetch_rsk_height_bundle(source, 100_000)).unwrap();
        assert_eq!(bundle.canonical.as_ref(), Some(&block));
        assert!(bundle.uncles.is_empty());
    }

    #[test]
    fn fetch_keeps_canonical_with_malformed_evidence_fields() {
        // A malformed merge-mining field is a downstream `MalformedSkipped`, not
        // a fetch error: the bundle still carries the canonical block.
        let block = load_rsk_block_fixture("malformed-header");
        let source =
            FakeRskSource::default().with_canonical(729_002, FakeResponse::block(block.clone()));
        let bundle = block_on(fetch_rsk_height_bundle(source, 729_002)).unwrap();
        assert_eq!(bundle.canonical.as_ref(), Some(&block));
        assert!(bundle.uncles.is_empty());
    }

    #[test]
    fn fetch_multi_uncle_height_preserves_listed_order() {
        let uncle0 = load_rsk_block_fixture("uncle-valid");
        let uncle1 = load_rsk_block_fixture("uncle-second-miner");
        let canonical = load_rsk_block_fixture("canonical-with-uncles");
        let source = FakeRskSource::default()
            .with_canonical(729_001, FakeResponse::block(canonical))
            .with_uncle(729_001, 0, FakeResponse::block(uncle0.clone()))
            .with_uncle(729_001, 1, FakeResponse::block(uncle1.clone()));

        let bundle = block_on(fetch_rsk_height_bundle(source, 729_001)).unwrap();
        assert_eq!(bundle.uncles.len(), 2);
        assert_eq!(bundle.uncles[0].index, 0);
        assert_eq!(
            bundle.uncles[0].result.as_ref().unwrap().as_ref(),
            Some(&uncle0)
        );
        assert_eq!(bundle.uncles[1].index, 1);
        assert_eq!(
            bundle.uncles[1].result.as_ref().unwrap().as_ref(),
            Some(&uncle1)
        );
    }

    #[test]
    fn fetch_null_uncle_is_captured_as_ok_none() {
        let canonical = load_rsk_block_fixture("canonical-with-uncles");
        // The canonical lists an uncle, but the source returns null for it.
        let source = FakeRskSource::default()
            .with_canonical(729_001, FakeResponse::block(canonical))
            .with_uncle(729_001, 0, FakeResponse::Null);

        let bundle = block_on(fetch_rsk_height_bundle(source, 729_001)).unwrap();
        assert_eq!(bundle.uncles.len(), 2);
        assert_eq!(bundle.uncles[0].index, 0);
        assert!(matches!(bundle.uncles[0].result, Ok(None)));
        assert_eq!(bundle.uncles[1].index, 1);
        assert!(matches!(bundle.uncles[1].result, Ok(None)));
    }

    #[test]
    fn fetch_uncle_error_is_captured_not_raised() {
        let canonical = load_rsk_block_fixture("canonical-with-uncles");
        let source = FakeRskSource::default()
            .with_canonical(729_001, FakeResponse::block(canonical))
            .with_uncle(729_001, 0, FakeResponse::Error("boom".to_owned()));

        // The fetch stage MUST NOT raise the uncle error; it is captured.
        let bundle = block_on(fetch_rsk_height_bundle(source, 729_001))
            .expect("uncle fetch error must be captured, not raised");
        assert_eq!(bundle.uncles.len(), 2);
        let err = bundle.uncles[0].result.as_ref().unwrap_err();
        assert!(err.to_string().contains("boom"), "unexpected error: {err}");
        assert!(matches!(bundle.uncles[1].result, Ok(None)));
    }

    #[test]
    fn fetch_canonical_error_propagates() {
        // An RPC transport/protocol error on the canonical fetch is the only
        // failure that propagates via `?` from the fetch stage.
        let source = FakeRskSource::default()
            .with_canonical(729_000, FakeResponse::Error("rpc down".to_owned()));
        let err = block_on(fetch_rsk_height_bundle(source, 729_000)).unwrap_err();
        assert!(
            err.to_string()
                .contains("get RSK canonical block at height 729000"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn fetch_overflowing_canonical_number_keeps_canonical_skips_uncles() {
        let mut canonical = load_rsk_block_fixture("canonical-with-uncles");
        // i32::MAX + 1: the canonical RSK number overflows i32, so uncle
        // traversal is skipped, but the canonical block is kept and no error
        // is raised.
        canonical.number = "0x80000000".to_owned();
        let source = FakeRskSource::default()
            .with_canonical(729_001, FakeResponse::block(canonical.clone()))
            // This uncle must NOT be fetched because uncle traversal is skipped.
            .with_uncle(
                729_001,
                0,
                FakeResponse::Error("must not be fetched".to_owned()),
            );

        let bundle = block_on(fetch_rsk_height_bundle(source, 729_001))
            .expect("overflowing canonical number must be non-fatal");
        assert_eq!(bundle.canonical.as_ref(), Some(&canonical));
        assert!(bundle.uncles.is_empty());
    }

    #[test]
    fn fetch_malformed_canonical_number_keeps_canonical_skips_uncles() {
        let mut canonical = load_rsk_block_fixture("canonical-with-uncles");
        canonical.number = "0xnothex".to_owned();
        let source = FakeRskSource::default()
            .with_canonical(729_001, FakeResponse::block(canonical.clone()))
            .with_uncle(
                729_001,
                0,
                FakeResponse::Error("must not be fetched".to_owned()),
            );

        let bundle = block_on(fetch_rsk_height_bundle(source, 729_001))
            .expect("malformed canonical number must be non-fatal");
        assert_eq!(bundle.canonical.as_ref(), Some(&canonical));
        assert!(bundle.uncles.is_empty());
    }

    #[test]
    fn buffered_pipeline_yields_bundles_in_ascending_height_order() {
        use futures::StreamExt;

        // Each height has a present canonical whose `hash` encodes the height,
        // so we can assert the consumer sees them strictly ascending even though
        // the fetch stage runs concurrently.
        let mut source = FakeRskSource::default();
        for height in 200_000_i64..=200_009 {
            let block = rsk_block_with(
                height,
                1_700_000_000 + height,
                KNOWN_MINER_HEX,
                header_meeting_bits(0x207f_ffff),
                vec![],
            );
            source = source.with_canonical(height, FakeResponse::block(block));
        }

        let observed = block_on(async {
            let mut heights = Vec::new();
            let mut fetches = futures::stream::iter(200_000_i64..=200_009)
                .map(|height| fetch_rsk_height_bundle(source.clone(), height))
                .buffered(4);
            while let Some(bundle) = fetches.next().await {
                let canonical = bundle.unwrap().canonical.unwrap();
                heights.push(decode_quantity_i64(&canonical.number).unwrap());
            }
            heights
        });

        assert_eq!(observed, (200_000_i64..=200_009).collect::<Vec<_>>());
    }
}
