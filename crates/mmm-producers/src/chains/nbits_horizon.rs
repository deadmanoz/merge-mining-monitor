//! Shared beyond-horizon BTC nBits resolution for the public-RPC producers
//! (Elastos and Hathor).
//!
//! When the offline embedded nBits table cannot classify a parent (its BIP34
//! height is beyond the table horizon), the live producer asks Bitcoin Core for
//! the canonical nBits at that DAA epoch instead of cursor-pinning. The far-future
//! guard stays keyed on the CLAIMED BIP34 height (never the epoch start): rounding
//! to the epoch start would discard up to `DAA_EPOCH_INTERVAL - 1` blocks of
//! future-height distance and let a fabricated height inside an already-known
//! epoch pass as valid.
//!
//! Both producers route their `AboveTableHorizon` arm through here (the rule-of-two
//! extraction): the tricky height/tolerance ordering lives in the pure
//! [`horizon_gate`] (unit-tested directly), and [`resolve_horizon_nbits`] is the
//! thin async wrapper that never propagates a transient Core RPC error - every
//! Core failure is caught and mapped to [`HorizonOutcome::Hold`] (fail closed). The
//! producer turns that into a `TableHorizonHold`, a cursor-blocking abort the poller
//! retries each tick, so the cursor never advances past unresolved evidence; the row
//! resolves once Core can answer again, without an operator regenerating the table.

use bitcoin::CompactTarget;
use tracing::{debug, warn};

use mmm_bitcoin_core::ConfiguredParentClassifier;
use mmm_capture::nbits_table::{NbitsVerdict, classify_against_expected_nbits, daa_epoch_start};

/// Lag tolerance (in BTC blocks) between a parent's claimed BIP34 height and the
/// synced Bitcoin Core tip before the parent is treated as a fabricated far-future
/// height. Within tolerance we hold (Core may be lagging the real tip); beyond it
/// we revoke as non-BTC. Relocated from the Elastos slice (value preserved); now shared.
pub(crate) const FUTURE_BIP34_CORE_TIP_TOLERANCE: i32 = 144;

/// The pure decision a `synced_tip` + claimed `bip34_height` imply, with no I/O.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HorizonGate {
    /// Fail closed: no synced tip, or the claimed height is above tip but within
    /// tolerance (Core may be lagging). Hold the cursor.
    Hold,
    /// The claimed height is implausibly far beyond synced tip: a fabricated
    /// future height. Revoke as non-BTC.
    FarFuture,
    /// The claimed height is at or below synced tip: resolve the epoch nBits.
    Resolve,
}

/// The terminal outcome of resolving a beyond-horizon parent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HorizonOutcome {
    /// The parent nBits matches the Core-resolved epoch nBits: a genuine BTC parent.
    Valid,
    /// The parent nBits differs from BTC's at that epoch: a BCH/BSV contaminant.
    Contaminant,
    /// A fabricated far-future BIP34 height: revoke as non-BTC.
    FarFuture,
    /// Core could not answer (disabled / IBD / unreachable / lagging within
    /// tolerance / transient fetch error): fail closed, hold the cursor.
    Hold,
}

/// Pure height/tolerance gate. The far-future decision is keyed on the CLAIMED
/// `bip34_height`, never on the epoch start. `saturating_add` so a near-`i32::MAX`
/// tip cannot wrap and falsely clear the far-future gate. `tip_fresh` is whether
/// Core's synced tip is recent: a beyond-tolerance height is only treated as a
/// fabricated far-future height when the tip is fresh, because a stalled / isolated
/// node can report a synced-but-lagging tip against which a genuine beyond-horizon
/// parent would look fabricated. A stale tip holds instead of revoking.
pub(crate) fn horizon_gate(
    synced_tip: Option<i32>,
    tip_fresh: bool,
    bip34_height: i32,
) -> HorizonGate {
    let Some(tip) = synced_tip else {
        return HorizonGate::Hold;
    };
    if bip34_height > tip.saturating_add(FUTURE_BIP34_CORE_TIP_TOLERANCE) {
        if tip_fresh {
            HorizonGate::FarFuture
        } else {
            HorizonGate::Hold
        }
    } else if bip34_height > tip {
        HorizonGate::Hold
    } else {
        HorizonGate::Resolve
    }
}

/// Resolve a parent whose BIP34 height is beyond the embedded nBits-table horizon
/// by asking Bitcoin Core for the canonical nBits at its DAA epoch. Fails closed to
/// [`HorizonOutcome::Hold`] on any Core error (never propagated to the tick).
pub(crate) async fn resolve_horizon_nbits(
    classifier: &ConfiguredParentClassifier,
    bip34_height: i32,
    actual_bits: CompactTarget,
) -> HorizonOutcome {
    let synced_tip = match classifier.synced_tip().await {
        Ok(tip) => tip,
        Err(err) => {
            warn!(
                bip34_height,
                error = %err,
                "Bitcoin Core synced-tip lookup failed; holding the nBits horizon"
            );
            return HorizonOutcome::Hold;
        }
    };
    // A `None` tip (disabled / IBD) holds; an unfresh tip never revokes (see gate).
    let (tip_height, tip_fresh) = match synced_tip {
        Some(tip) => (Some(tip.height), tip.fresh),
        None => (None, false),
    };
    match horizon_gate(tip_height, tip_fresh, bip34_height) {
        HorizonGate::Hold => HorizonOutcome::Hold,
        HorizonGate::FarFuture => HorizonOutcome::FarFuture,
        HorizonGate::Resolve => {
            // The gate returns Resolve only for `Some(tip) >= bip34_height`; the tip
            // gates the epoch-cache burial check (don't cache a reorg-shallow epoch).
            let tip = tip_height.expect("Resolve gate implies a synced tip");
            let epoch_start = daa_epoch_start(bip34_height);
            match classifier.epoch_nbits(epoch_start, tip).await {
                Ok(expected) => {
                    let outcome = match classify_against_expected_nbits(expected.nbits, actual_bits)
                    {
                        NbitsVerdict::Valid => HorizonOutcome::Valid,
                        // classify_against_expected_nbits only ever returns
                        // Valid/Contaminant; any non-Valid is a contaminant.
                        _ => HorizonOutcome::Contaminant,
                    };
                    debug!(
                        bip34_height,
                        epoch_start,
                        expected_nbits = format_args!("{:08x}", expected.nbits),
                        actual_nbits = format_args!("{:08x}", actual_bits.to_consensus()),
                        ?outcome,
                        "resolved beyond-horizon nBits from Bitcoin Core"
                    );
                    outcome
                }
                Err(err) => {
                    warn!(
                        bip34_height,
                        epoch_start,
                        error = %err,
                        "Bitcoin Core epoch nBits fetch failed; holding the nBits horizon"
                    );
                    HorizonOutcome::Hold
                }
            }
        }
    }
}

/// Whether a parsed BIP34 height is a fabricated far-future claim against a FRESH
/// synced Core tip. This guards the IN-TABLE `Valid` path too: a fabricated height
/// inside a covered epoch whose nBits happens to match the embedded table would
/// otherwise be written without ever reaching [`resolve_horizon_nbits`]. It reuses
/// the same height/tolerance/freshness policy as [`horizon_gate`]. Returns `false`
/// (do NOT override the offline verdict) when Core is disabled, not synced, stale,
/// or unreachable: only a fresh tip can positively prove a height is fabricated, and
/// the offline nBits match is trusted otherwise (so offline runs still write).
pub(crate) async fn far_future_against_fresh_tip(
    classifier: &ConfiguredParentClassifier,
    bip34_height: i32,
) -> bool {
    match classifier.synced_tip().await {
        Ok(Some(tip)) => matches!(
            horizon_gate(Some(tip.height), tip.fresh, bip34_height),
            HorizonGate::FarFuture
        ),
        Ok(None) => false,
        Err(err) => {
            warn!(
                bip34_height,
                error = %err,
                "Bitcoin Core synced-tip lookup failed; not overriding the offline nBits verdict"
            );
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use bitcoin::block::Header;
    use bitcoin::hashes::Hash as _;
    use bitcoin::{BlockHash, TxMerkleNode};
    use mmm_bitcoin_core::{FakeParentClassifier, ParentClassification};

    use super::*;

    // The Elastos live-stall parent: claimed BIP34 height 955,585 in epoch 955,584,
    // with synced Core tip 955,609 (epoch 955,584 is therefore a known epoch).
    const TIP: i32 = 955_609;
    const FABRICATED: i32 = 957_599;

    fn test_header(nonce: u32, bits: u32) -> Header {
        Header {
            version: bitcoin::block::Version::ONE,
            prev_blockhash: BlockHash::all_zeros(),
            merkle_root: TxMerkleNode::all_zeros(),
            time: 1,
            bits: CompactTarget::from_consensus(bits),
            nonce,
        }
    }

    fn fake_classifier(fake: FakeParentClassifier) -> ConfiguredParentClassifier {
        ConfiguredParentClassifier::Fake(fake)
    }

    fn fake_parent_classifier() -> FakeParentClassifier {
        FakeParentClassifier::new(ParentClassification::unknown(&test_header(43, 0x207f_ffff)))
    }

    #[test]
    fn gate_resolves_height_at_or_below_tip() {
        assert_eq!(horizon_gate(Some(TIP), true, 955_585), HorizonGate::Resolve);
        assert_eq!(horizon_gate(Some(TIP), true, TIP), HorizonGate::Resolve);
        assert_eq!(horizon_gate(Some(TIP), true, 0), HorizonGate::Resolve);
    }

    #[test]
    fn gate_far_future_for_fabricated_height_inside_a_known_epoch() {
        // 957,599 sits in the SAME epoch as the tip (epoch start 955,584 <= tip),
        // so an epoch-keyed gate would wrongly resolve it. But the CLAIMED height is
        // far beyond tip + tolerance, so a FRESH-tip height-keyed gate flags FarFuture.
        assert_eq!(daa_epoch_start(FABRICATED), 955_584);
        assert!(daa_epoch_start(FABRICATED) <= TIP, "epoch is known to Core");
        assert_eq!(
            horizon_gate(Some(TIP), true, FABRICATED),
            HorizonGate::FarFuture
        );
    }

    #[test]
    fn gate_holds_far_future_when_the_tip_is_stale() {
        // Same beyond-tolerance height, but Core's tip is stale (lagging / isolated
        // node): hold rather than revoke a possibly-valid parent against a stale tip.
        assert_eq!(
            horizon_gate(Some(TIP), false, FABRICATED),
            HorizonGate::Hold
        );
    }

    #[test]
    fn gate_holds_for_height_above_tip_within_tolerance() {
        // Core may be lagging the real BTC tip: hold, do not accept or revoke.
        assert_eq!(horizon_gate(Some(TIP), true, TIP + 1), HorizonGate::Hold);
        assert_eq!(
            horizon_gate(Some(TIP), true, TIP + FUTURE_BIP34_CORE_TIP_TOLERANCE),
            HorizonGate::Hold
        );
        // Exactly one past tolerance flips to FarFuture (with a fresh tip).
        assert_eq!(
            horizon_gate(Some(TIP), true, TIP + FUTURE_BIP34_CORE_TIP_TOLERANCE + 1),
            HorizonGate::FarFuture
        );
    }

    #[test]
    fn gate_holds_without_a_synced_tip() {
        assert_eq!(horizon_gate(None, true, 955_585), HorizonGate::Hold);
        assert_eq!(horizon_gate(None, false, 955_585), HorizonGate::Hold);
    }

    #[test]
    fn gate_far_future_does_not_overflow_near_i32_max() {
        // saturating_add must not wrap a near-MAX tip into a small number.
        assert_eq!(
            horizon_gate(Some(i32::MAX), true, i32::MAX),
            HorizonGate::Resolve
        );
    }

    #[tokio::test]
    async fn resolver_holds_when_synced_tip_lookup_fails() {
        let classifier = fake_classifier(fake_parent_classifier().with_synced_tip_error());

        let outcome = resolve_horizon_nbits(
            &classifier,
            955_585,
            CompactTarget::from_consensus(0x1702_40c3),
        )
        .await;

        assert_eq!(outcome, HorizonOutcome::Hold);
    }

    #[tokio::test]
    async fn resolver_holds_when_tip_is_stale_for_far_future_claim() {
        let classifier = fake_classifier(fake_parent_classifier().with_stale_synced_tip(TIP));

        let outcome = resolve_horizon_nbits(
            &classifier,
            FABRICATED,
            CompactTarget::from_consensus(0x1702_40c3),
        )
        .await;

        assert_eq!(outcome, HorizonOutcome::Hold);
    }

    #[tokio::test]
    async fn resolver_holds_when_epoch_nbits_fetch_fails() {
        let classifier = fake_classifier(
            fake_parent_classifier()
                .with_synced_tip_height(TIP)
                .with_epoch_nbits_error(),
        );

        let outcome = resolve_horizon_nbits(
            &classifier,
            955_585,
            CompactTarget::from_consensus(0x1702_40c3),
        )
        .await;

        assert_eq!(outcome, HorizonOutcome::Hold);
    }

    #[tokio::test]
    async fn resolver_resolves_matching_epoch_nbits() {
        let classifier = fake_classifier(
            fake_parent_classifier()
                .with_synced_tip_height(TIP)
                .with_epoch_nbits(955_584, 0x1702_40c3, 1_782_525_607),
        );

        let outcome = resolve_horizon_nbits(
            &classifier,
            955_585,
            CompactTarget::from_consensus(0x1702_40c3),
        )
        .await;

        assert_eq!(outcome, HorizonOutcome::Valid);
    }

    #[tokio::test]
    async fn resolver_resolves_mismatched_epoch_nbits_as_contaminant() {
        let classifier = fake_classifier(
            fake_parent_classifier()
                .with_synced_tip_height(TIP)
                .with_epoch_nbits(955_584, 0x1702_40c3, 1_782_525_607),
        );

        let outcome = resolve_horizon_nbits(
            &classifier,
            955_585,
            CompactTarget::from_consensus(0x1a0f_ffff),
        )
        .await;

        assert_eq!(outcome, HorizonOutcome::Contaminant);
    }

    #[tokio::test]
    async fn resolver_resolves_fresh_far_future_claim_as_far_future() {
        let classifier = fake_classifier(fake_parent_classifier().with_synced_tip_height(TIP));

        let outcome = resolve_horizon_nbits(
            &classifier,
            FABRICATED,
            CompactTarget::from_consensus(0x1702_40c3),
        )
        .await;

        assert_eq!(outcome, HorizonOutcome::FarFuture);
    }

    #[tokio::test]
    async fn far_future_guard_does_not_override_when_synced_tip_lookup_fails() {
        let classifier = fake_classifier(fake_parent_classifier().with_synced_tip_error());

        assert!(
            !far_future_against_fresh_tip(&classifier, FABRICATED).await,
            "Core lookup failures must preserve the offline nBits verdict"
        );
    }

    #[tokio::test]
    async fn far_future_guard_does_not_override_without_a_synced_tip() {
        let classifier = fake_classifier(fake_parent_classifier());

        assert!(
            !far_future_against_fresh_tip(&classifier, FABRICATED).await,
            "missing Core tip must preserve the offline nBits verdict"
        );
    }

    #[tokio::test]
    async fn far_future_guard_does_not_override_when_tip_is_stale() {
        let classifier = fake_classifier(fake_parent_classifier().with_stale_synced_tip(500_000));

        assert!(
            !far_future_against_fresh_tip(&classifier, FABRICATED).await,
            "stale Core tip must preserve the offline nBits verdict"
        );
    }
}
