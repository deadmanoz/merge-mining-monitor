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
                "Bitcoin Core synced-tip lookup failed; not overriding the cached nBits verdict"
            );
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_fresh_tip_can_prove_a_height_fabricated() {
        assert_eq!(horizon_gate(Some(100), true, 244), HorizonGate::Hold);
        assert_eq!(horizon_gate(Some(100), true, 245), HorizonGate::FarFuture);
        assert_eq!(horizon_gate(Some(100), false, 245), HorizonGate::Hold);
        assert_eq!(horizon_gate(None, true, 245), HorizonGate::Hold);
    }
}
