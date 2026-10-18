//! Static per-chain specification table (`CHAINS`).

use crate::poller::PollerDefaults;
use mmm_capture::child_payout::{
    ChildPayoutParams, FRACTAL_CHILD_REWARD_PARAMS, NAMECOIN_CHILD_PAYOUT_PARAMS,
    QBIT_CHILD_PAYOUT_PARAMS, SYSCOIN_CHILD_PAYOUT_PARAMS,
};
use mmm_capture::source_registry::{
    ELASTOS_SOURCE_CODE, FRACTAL_SOURCE_CODE, HATHOR_SOURCE_CODE, NAMECOIN_SOURCE_CODE,
    QBIT_SOURCE_CODE, RSK_SOURCE_CODE, SYSCOIN_SOURCE_CODE, TERRACOIN_SOURCE_CODE,
};

/// Historical acquisition floor for the RSK live poller (bounded backfills may
/// still capture complete 80-byte parent headers below this height).
const RSK_ACQUISITION_FLOOR: i32 = 139_999;
const SYSCOIN_FIRST_AUXPOW_HEIGHT: i32 = 1_973;
const FRACTAL_FIRST_AUXPOW_HEIGHT: i32 = 1;
const HATHOR_FIRST_AUXPOW_HEIGHT: i32 = 0;
pub(crate) const HATHOR_DEFAULT_BACKFILL_START: i32 = 1_000_000;
const ELASTOS_FIRST_AUXPOW_HEIGHT: i32 = 177_000;
/// Fractal's exact merge-mined block version. Only blocks with this exact
/// nVersion carry a CAuxPow. The `0x20260100` Cadence class also sets the
/// generic `0x100` AuxPoW bit but is NOT merge-mined, so the bit check in
/// `parse_namecoin_block`/`parse_auxpow_header_blob` is insufficient as the
/// Fractal gate.
const FRACTAL_MERGE_MINED_VERSION: i32 = 0x2024_0100;
const QBIT_FIRST_AUXPOW_HEIGHT: i32 = 0;
/// Qbit mainnet genesis block hash (display order), pinned at height 0 because
/// the proof decoder authenticates no chain identity.
const QBIT_GENESIS_BLOCK_HASH: &str =
    "0000000000004d60aa5d46013991d0a0e2995d89ee98e53068ae196d763e79f2";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ChainId {
    Namecoin,
    Rsk,
    Syscoin,
    Fractal,
    Hathor,
    Elastos,
    Qbit,
    Terracoin,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RpcAuth {
    RequiredUserPass,
    OptionalUserPassOrCookie,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FetchStrategy {
    RawBlock {
        authentication: RawBlockAuthentication,
    },
    HeaderBlob {
        exact_version: i32,
    },
    QbitExtendedHeader {
        genesis_block_hash: &'static str,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RawBlockAuthentication {
    NodeValidated,
    StrictClassic {
        genesis_block_hash: &'static str,
        chain_id: u32,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaptureFailurePolicy {
    SkipMalformedProof,
    /// Persist a `capture_error` row before returning and hold live cursors /
    /// fail backfills over a range containing a malformed proof.
    HoldMalformedProof,
    /// Also retain returned operational failures, including failed replay heights.
    HoldAnyHeightFailure,
}

impl CaptureFailurePolicy {
    pub fn retains_errors(self) -> bool {
        self != Self::SkipMalformedProof
    }
}

/// Parse-time backfill range cap unless `<PREFIX>_ALLOW_LARGE_BACKFILL=1`.
#[derive(Debug, Clone, Copy)]
pub struct RangeCap {
    pub default_max: i32,
    pub note: &'static str,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RepairScope {
    Global,
    SourceScoped,
}

#[derive(Debug, Clone, Copy)]
pub struct FamilySpec {
    pub label: &'static str,
    pub auth: RpcAuth,
    pub fetch: FetchStrategy,
    pub raw_block_boolean_verbose: bool,
    pub child_payout: Option<ChildPayoutParams>,
    pub floor_warning: Option<&'static str>,
    pub repair_scope: RepairScope,
    pub failure_policy: CaptureFailurePolicy,
}

#[derive(Debug, Clone, Copy)]
pub struct ChainSpec {
    pub id: ChainId,
    pub slug: &'static str,
    pub display_name: &'static str,
    pub env_prefix: &'static str,
    pub source_code: &'static str,
    pub activation_floor: i32,
    pub poller: PollerDefaults,
    pub family: Option<FamilySpec>,
    pub backfill_range_cap: Option<RangeCap>,
}

pub static CHAINS: [ChainSpec; 8] = [
    ChainSpec {
        id: ChainId::Namecoin,
        slug: "namecoin",
        display_name: "Namecoin",
        env_prefix: "NAMECOIN",
        source_code: NAMECOIN_SOURCE_CODE,
        activation_floor: 0,
        poller: PollerDefaults {
            poll_interval_seconds: 30,
            batch_size: 100,
            reorg_depth: 0,
        },
        family: Some(FamilySpec {
            label: "Namecoin",
            auth: RpcAuth::RequiredUserPass,
            fetch: FetchStrategy::RawBlock {
                authentication: RawBlockAuthentication::NodeValidated,
            },
            raw_block_boolean_verbose: false,
            child_payout: Some(NAMECOIN_CHILD_PAYOUT_PARAMS),
            floor_warning: None,
            repair_scope: RepairScope::Global,
            failure_policy: CaptureFailurePolicy::SkipMalformedProof,
        }),
        backfill_range_cap: None,
    },
    ChainSpec {
        id: ChainId::Rsk,
        slug: "rsk",
        display_name: "RSK",
        env_prefix: "RSK",
        source_code: RSK_SOURCE_CODE,
        activation_floor: RSK_ACQUISITION_FLOOR,
        poller: PollerDefaults {
            poll_interval_seconds: 30,
            batch_size: 100,
            reorg_depth: 64,
        },
        family: None,
        backfill_range_cap: None,
    },
    ChainSpec {
        id: ChainId::Syscoin,
        slug: "syscoin",
        display_name: "Syscoin",
        env_prefix: "SYSCOIN",
        source_code: SYSCOIN_SOURCE_CODE,
        activation_floor: SYSCOIN_FIRST_AUXPOW_HEIGHT,
        poller: PollerDefaults {
            poll_interval_seconds: 30,
            batch_size: 100,
            reorg_depth: 0,
        },
        family: Some(FamilySpec {
            label: "Syscoin",
            auth: RpcAuth::OptionalUserPassOrCookie,
            fetch: FetchStrategy::RawBlock {
                authentication: RawBlockAuthentication::NodeValidated,
            },
            raw_block_boolean_verbose: false,
            child_payout: Some(SYSCOIN_CHILD_PAYOUT_PARAMS),
            floor_warning: Some(
                "start-height precedes Syscoin AuxPoW activation; earlier blocks parse as non-AuxPoW and will be skipped",
            ),
            repair_scope: RepairScope::SourceScoped,
            failure_policy: CaptureFailurePolicy::SkipMalformedProof,
        }),
        backfill_range_cap: None,
    },
    ChainSpec {
        id: ChainId::Fractal,
        slug: "fractal",
        display_name: "Fractal",
        env_prefix: "FRACTAL",
        source_code: FRACTAL_SOURCE_CODE,
        activation_floor: FRACTAL_FIRST_AUXPOW_HEIGHT,
        poller: PollerDefaults {
            poll_interval_seconds: 30,
            batch_size: 100,
            reorg_depth: 0,
        },
        family: Some(FamilySpec {
            label: "Fractal",
            auth: RpcAuth::OptionalUserPassOrCookie,
            fetch: FetchStrategy::HeaderBlob {
                exact_version: FRACTAL_MERGE_MINED_VERSION,
            },
            raw_block_boolean_verbose: false,
            child_payout: Some(FRACTAL_CHILD_REWARD_PARAMS),
            floor_warning: Some(
                "start-height precedes Fractal AuxPoW activation; earlier blocks are skipped",
            ),
            repair_scope: RepairScope::SourceScoped,
            failure_policy: CaptureFailurePolicy::SkipMalformedProof,
        }),
        backfill_range_cap: None,
    },
    ChainSpec {
        id: ChainId::Hathor,
        slug: "hathor",
        display_name: "Hathor",
        env_prefix: "HATHOR",
        source_code: HATHOR_SOURCE_CODE,
        activation_floor: HATHOR_FIRST_AUXPOW_HEIGHT,
        poller: PollerDefaults {
            poll_interval_seconds: 30,
            batch_size: 100,
            reorg_depth: 20,
        },
        family: None,
        backfill_range_cap: Some(RangeCap {
            default_max: 5_000,
            note: "the public REST API must not be swept",
        }),
    },
    ChainSpec {
        id: ChainId::Elastos,
        slug: "elastos",
        display_name: "Elastos",
        env_prefix: "ELASTOS",
        source_code: ELASTOS_SOURCE_CODE,
        activation_floor: ELASTOS_FIRST_AUXPOW_HEIGHT,
        poller: PollerDefaults {
            poll_interval_seconds: 30,
            batch_size: 100,
            reorg_depth: 0,
        },
        family: None,
        backfill_range_cap: Some(RangeCap {
            default_max: 50_000,
            note: "needed for a full 177000..tip backfill",
        }),
    },
    ChainSpec {
        id: ChainId::Qbit,
        slug: "qbit",
        display_name: "Qbit",
        env_prefix: "QBIT",
        source_code: QBIT_SOURCE_CODE,
        activation_floor: QBIT_FIRST_AUXPOW_HEIGHT,
        poller: PollerDefaults {
            poll_interval_seconds: 30,
            batch_size: 100,
            reorg_depth: 0,
        },
        family: Some(FamilySpec {
            label: "Qbit",
            auth: RpcAuth::OptionalUserPassOrCookie,
            fetch: FetchStrategy::QbitExtendedHeader {
                genesis_block_hash: QBIT_GENESIS_BLOCK_HASH,
            },
            raw_block_boolean_verbose: false,
            child_payout: Some(QBIT_CHILD_PAYOUT_PARAMS),
            floor_warning: None,
            repair_scope: RepairScope::SourceScoped,
            failure_policy: CaptureFailurePolicy::HoldMalformedProof,
        }),
        backfill_range_cap: None,
    },
    ChainSpec {
        id: ChainId::Terracoin,
        slug: "terracoin",
        display_name: "Terracoin",
        env_prefix: "TERRACOIN",
        source_code: TERRACOIN_SOURCE_CODE,
        activation_floor: 833_000,
        poller: PollerDefaults {
            poll_interval_seconds: 30,
            batch_size: 100,
            reorg_depth: 64,
        },
        family: Some(FamilySpec {
            label: "Terracoin",
            auth: RpcAuth::RequiredUserPass,
            fetch: FetchStrategy::RawBlock {
                authentication: RawBlockAuthentication::StrictClassic {
                    genesis_block_hash: "00000000804bbc6a621a9dbb564ce469f492e1ccf2d70f8a6b241e26a277afa2",
                    chain_id: 50,
                },
            },
            raw_block_boolean_verbose: true,
            child_payout: None,
            floor_warning: Some("start-height precedes Terracoin AuxPoW activation"),
            repair_scope: RepairScope::SourceScoped,
            failure_policy: CaptureFailurePolicy::HoldAnyHeightFailure,
        }),
        backfill_range_cap: None,
    },
];

pub fn by_id(id: ChainId) -> &'static ChainSpec {
    CHAINS
        .iter()
        .find(|spec| spec.id == id)
        .expect("every ChainId has a CHAINS row")
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;
    use mmm_capture::source_registry::{SOURCE_REGISTRY, SourceKind, SourceLifecycle};

    /// Every spec row binds to exactly the right source-registry definition,
    /// and (bidirectionally) every live AuxPoW registry definition has exactly
    /// one spec row, so a future live producer source cannot be registered
    /// without config metadata. Historical sources are excluded by the lifecycle
    /// filter by construction.
    #[test]
    fn chains_bind_bidirectionally_to_live_auxpow_sources() {
        for spec in &CHAINS {
            let def = SOURCE_REGISTRY
                .iter()
                .find(|def| def.code == spec.source_code)
                .unwrap_or_else(|| {
                    panic!(
                        "{}: source code {} not registered",
                        spec.slug, spec.source_code
                    )
                });
            assert_eq!(
                def.chain, spec.slug,
                "{}: registry chain != slug",
                spec.slug
            );
            assert_eq!(
                def.kind,
                SourceKind::Auxpow,
                "{}: registry kind is not auxpow",
                spec.slug
            );
            assert!(
                def.instance.is_none(),
                "{}: producer source must not carry an instance",
                spec.slug
            );
            assert_eq!(
                def.lifecycle,
                SourceLifecycle::Live,
                "{}: registry lifecycle is not live",
                spec.slug
            );
        }

        let spec_codes: HashSet<&str> = CHAINS.iter().map(|spec| spec.source_code).collect();
        for def in SOURCE_REGISTRY {
            if def.kind == SourceKind::Auxpow && def.lifecycle == SourceLifecycle::Live {
                assert!(
                    spec_codes.contains(def.code),
                    "live auxpow source {} has no ChainSpec row",
                    def.code
                );
            }
        }
    }

    #[test]
    fn chain_identities_are_unique() {
        let mut slugs = HashSet::new();
        let mut prefixes = HashSet::new();
        let mut codes = HashSet::new();
        for spec in &CHAINS {
            assert!(slugs.insert(spec.slug), "duplicate slug {}", spec.slug);
            assert!(
                prefixes.insert(spec.env_prefix),
                "duplicate env prefix {}",
                spec.env_prefix
            );
            assert!(
                codes.insert(spec.source_code),
                "duplicate source code {}",
                spec.source_code
            );
        }
    }

    /// The shared runner's malformed-proof behavior is per-spec data, so a new
    /// chain cannot silently change an incumbent's. Every chain that predates
    /// Qbit pins `SkipMalformedProof`; Qbit holds malformed proofs and
    /// Terracoin additionally retains operational height failures.
    #[test]
    fn new_sources_hold_the_interval_on_a_malformed_proof() {
        for spec in &CHAINS {
            let Some(family) = spec.family.as_ref() else {
                continue;
            };
            let expected = match spec.id {
                ChainId::Terracoin => CaptureFailurePolicy::HoldAnyHeightFailure,
                ChainId::Qbit => CaptureFailurePolicy::HoldMalformedProof,
                _ => CaptureFailurePolicy::SkipMalformedProof,
            };
            assert_eq!(
                family.raw_block_boolean_verbose,
                spec.id == ChainId::Terracoin
            );
            assert_eq!(
                family.failure_policy, expected,
                "{} malformed policy",
                spec.slug
            );
        }
    }

    #[test]
    fn qbit_is_a_family_chain_pinned_to_its_genesis() {
        let spec = by_id(ChainId::Qbit);
        assert_eq!(spec.source_code, QBIT_SOURCE_CODE);
        assert_eq!(spec.activation_floor, 0);
        assert_eq!(spec.env_prefix, "QBIT");
        assert!(spec.backfill_range_cap.is_none());
        let family = spec.family.as_ref().expect("Qbit is a family chain");
        assert_eq!(family.child_payout, Some(QBIT_CHILD_PAYOUT_PARAMS));
        match family.fetch {
            FetchStrategy::QbitExtendedHeader { genesis_block_hash } => assert_eq!(
                genesis_block_hash,
                "0000000000004d60aa5d46013991d0a0e2995d89ee98e53068ae196d763e79f2"
            ),
            other => panic!("Qbit must use the extended-header fetch, got {other:?}"),
        }
    }
}
