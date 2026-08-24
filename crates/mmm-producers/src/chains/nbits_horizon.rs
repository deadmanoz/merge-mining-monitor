//! Fresh-tip guard for implausible BIP34 height claims.
//!
//! nBits itself is classified from the persisted Core header cache. This small
//! guard prevents a fabricated height later in the current epoch from borrowing
//! that epoch's target when a fresh Core tip proves the claim impossible.

use tracing::warn;

use mmm_bitcoin_core::ConfiguredParentClassifier;

/// Lag tolerance, in Bitcoin blocks, before a claimed BIP34 height is treated
/// as fabricated rather than potentially ahead of a temporarily lagging node.
pub(crate) const FUTURE_BIP34_CORE_TIP_TOLERANCE: i32 = 144;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HorizonGate {
    Hold,
    FarFuture,
    WithinTip,
}

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
        HorizonGate::WithinTip
    }
}

/// Gate a claimed height against the persisted cache and a fresh Core tip.
///
/// `WithinTip` means the cache itself covers the claim. A Core tip that is
/// ahead of the cache does not authorize a write: the caller must refresh the
/// cache first, so every higher claim is either `Hold` or `FarFuture`.
pub(crate) async fn cached_horizon_gate(
    classifier: &ConfiguredParentClassifier,
    cached_horizon: i32,
    bip34_height: i32,
) -> HorizonGate {
    match classifier.synced_tip().await {
        Ok(Some(tip)) if tip.is_mainnet && tip.fresh => {
            match horizon_gate(Some(tip.height), tip.fresh, bip34_height) {
                HorizonGate::FarFuture => HorizonGate::FarFuture,
                HorizonGate::WithinTip if bip34_height <= cached_horizon => HorizonGate::WithinTip,
                HorizonGate::Hold | HorizonGate::WithinTip => HorizonGate::Hold,
            }
        }
        // A cache-covered claim remains usable when this optional fresh-tip
        // check is unavailable. The cached header itself came from a previously
        // fresh, mainnet Core observation; only a fresh contradictory tip may
        // demote it.
        Ok(Some(_)) | Ok(None) if bip34_height <= cached_horizon => HorizonGate::WithinTip,
        Ok(Some(_)) | Ok(None) => HorizonGate::Hold,
        Err(err) if bip34_height <= cached_horizon => {
            warn!(
                bip34_height,
                error = %err,
                "Bitcoin Core synced-tip lookup failed; retaining cache-covered height claim"
            );
            HorizonGate::WithinTip
        }
        Err(err) => {
            warn!(
                bip34_height,
                error = %err,
                "Bitcoin Core synced-tip lookup failed; holding Core-cache height claim"
            );
            HorizonGate::Hold
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_fresh_tip_can_prove_a_height_fabricated() {
        assert_eq!(horizon_gate(Some(100), true, 101), HorizonGate::Hold);
        assert_eq!(horizon_gate(Some(100), true, 244), HorizonGate::Hold);
        assert_eq!(horizon_gate(Some(100), true, 245), HorizonGate::FarFuture);
        assert_eq!(horizon_gate(Some(100), false, 245), HorizonGate::Hold);
        assert_eq!(horizon_gate(None, true, 245), HorizonGate::Hold);
    }

    #[tokio::test]
    async fn cached_horizon_holds_unobserved_and_unavailable_claims() {
        use bitcoin::Network;
        use mmm_bitcoin_core::{FakeParentClassifier, ParentClassification};

        let header = bitcoin::blockdata::constants::genesis_block(Network::Bitcoin).header;
        let current_epoch_claim = ConfiguredParentClassifier::Fake(
            FakeParentClassifier::new(ParentClassification::unknown(&header))
                .with_synced_tip_height(100),
        );
        assert_eq!(
            cached_horizon_gate(&current_epoch_claim, 100, 101).await,
            HorizonGate::Hold
        );

        let stale_cache = ConfiguredParentClassifier::Fake(
            FakeParentClassifier::new(ParentClassification::unknown(&header))
                .with_synced_tip_height(100),
        );
        assert_eq!(
            cached_horizon_gate(&stale_cache, 1_000, 245).await,
            HorizonGate::FarFuture,
            "a fresh Core tip rejects a fabricated claim even when stale cache rows cover it"
        );

        let unavailable = ConfiguredParentClassifier::Fake(
            FakeParentClassifier::new(ParentClassification::unknown(&header))
                .with_synced_tip_error(),
        );
        assert_eq!(
            cached_horizon_gate(&unavailable, 100, 101).await,
            HorizonGate::Hold
        );

        let non_mainnet = ConfiguredParentClassifier::Fake(
            FakeParentClassifier::new(ParentClassification::unknown(&header))
                .with_non_mainnet_synced_tip(100),
        );
        assert_eq!(
            cached_horizon_gate(&non_mainnet, 100, 245).await,
            HorizonGate::Hold
        );
    }
}
