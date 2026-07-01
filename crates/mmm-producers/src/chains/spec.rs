//! The static per-chain specification table.
//!
//! One `ChainSpec` row per live merge-mined producer chain. The row carries
//! the declared, reviewable facts a chain contributes to the shared producer
//! machinery: identity, env prefix, source code, activation floor, poller
//! defaults, and reorg policy. Behavior lives in the shared implementations
//! that CONSUME the spec; adding a Namecoin-family chain means adding a row
//! here (plus its `source_registry` entry), not cloning a module.

use crate::poller::PollerDefaults;
use mmm_capture::child_payout::{
    ChildPayoutParams, FRACTAL_CHILD_REWARD_PARAMS, NAMECOIN_CHILD_PAYOUT_PARAMS,
    SYSCOIN_CHILD_PAYOUT_PARAMS,
};
use mmm_capture::source_registry::{
    ELASTOS_SOURCE_CODE, FRACTAL_SOURCE_CODE, HATHOR_SOURCE_CODE, NAMECOIN_SOURCE_CODE,
    RSK_SOURCE_CODE, SYSCOIN_SOURCE_CODE,
};

/// RSKIP-92 activated at this RSK height; earlier blocks do not carry an
/// 80-byte BTC parent header and are skipped by the producer.
const RSK_FIRST_AUXPOW_HEIGHT: i32 = 139_999;
/// Current Syscoin chain-2 begins carrying AuxPoW evidence at this height.
const SYSCOIN_FIRST_AUXPOW_HEIGHT: i32 = 1_973;
/// Fractal Bitcoin merge-mines from height 1 (AuxPoW activation at mainnet
/// genesis); only the exact `0x20240100` Cadence class actually carries a
/// CAuxPow, the rest are skipped by the producer's version gate.
const FRACTAL_FIRST_AUXPOW_HEIGHT: i32 = 1;
/// Hathor's activation floor is 0, NOT its safe-default backfill start: the
/// `version != 3` capture gate makes any pre-activation height a harmless skip,
/// so an operator can replay earlier heights without the floor blocking them.
const HATHOR_FIRST_AUXPOW_HEIGHT: i32 = 0;
/// Documented safe default start for a Hathor backfill (merge-mining is ~100%
/// from ~1M; below this is mostly version-0 noise). A warn-below threshold, not
/// the activation floor.
pub(crate) const HATHOR_DEFAULT_BACKFILL_START: i32 = 1_000_000;
/// Elastos AuxPoW activation floor (the first post-dummy merge-mined block).
const ELASTOS_FIRST_AUXPOW_HEIGHT: i32 = 177_000;
/// Fractal's exact merge-mined block version. Only blocks with this exact
/// nVersion carry a CAuxPow. The `0x20260100` Cadence class also sets the
/// generic `0x100` AuxPoW bit but is NOT merge-mined, so the bit check in
/// `parse_namecoin_block`/`parse_auxpow_header_blob` is insufficient as the
/// Fractal gate.
const FRACTAL_MERGE_MINED_VERSION: i32 = 0x2024_0100;

/// Stable identity for a live producer chain. Consulted by registry dispatch
/// and spec lookup only; shared implementations branch on spec DATA, not on
/// this enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ChainId {
    Namecoin,
    Rsk,
    Syscoin,
    Fractal,
    Hathor,
    Elastos,
}

/// How a bitcoind-family chain authenticates its JSON-RPC endpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RpcAuth {
    /// `<PREFIX>_RPC_USER` and `<PREFIX>_RPC_PASSWORD` are independently
    /// required (each missing var is its own `<VAR> is required` error).
    RequiredUserPass,
    /// `<PREFIX>_RPC_USER`/`<PREFIX>_RPC_PASSWORD` are used when both are set
    /// (one without the other is a set-together error); otherwise
    /// `<PREFIX>_RPC_COOKIEFILE` may point at a Bitcoin Core-style cookie file
    /// containing `user:password`; otherwise unauthenticated.
    OptionalUserPassOrCookie,
}

/// How a bitcoind-family chain's AuxPoW proof bytes are fetched.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FetchStrategy {
    /// `getblock <hash> 0`: the raw block carries the CAuxPow inline
    /// (Namecoin, Syscoin); parsed by `parse_namecoin_block`.
    RawBlock,
    /// `getblockheader <hash> false true`: a `[child header][CAuxPow]` blob
    /// (Fractal). Only blobs whose child header version equals
    /// `exact_version` are merge-mined; every other class is skipped before
    /// any parse. Parsed by `parse_auxpow_header_blob`.
    HeaderBlob { exact_version: i32 },
}

/// A parse-time backfill range cap (a public/untrusted-endpoint foot-gun
/// guard): rejected at ARG-PARSE time, before any DB or RPC bootstrap, unless
/// `<PREFIX>_ALLOW_LARGE_BACKFILL=1`.
#[derive(Debug, Clone, Copy)]
pub struct RangeCap {
    /// Default for `<PREFIX>_MAX_BACKFILL_RANGE` (malformed values silently
    /// fall back to this; today's contract).
    pub default_max: i32,
    /// Tail note in the over-cap error message.
    pub note: &'static str,
}

/// Post-backfill read-model repair scope.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RepairScope {
    /// Repair scans all sources (historical Namecoin behavior).
    Global,
    /// Repair scans only this chain's own source.
    SourceScoped,
}

/// The bitcoind-family sharing contract: everything that distinguishes
/// Namecoin/Syscoin/Fractal from each other, as data consumed by ONE shared
/// client + capture + backfill implementation (`chains::bitcoind_rpc`,
/// `chains::auxpow_family`). Chains with genuinely divergent protocols (RSK,
/// Hathor, Elastos) carry `None` and keep real implementations.
#[derive(Debug, Clone, Copy)]
pub struct FamilySpec {
    /// Short label for RPC/parse error contexts and the capture-in-txn chain
    /// label ("Namecoin", "Syscoin", "Fractal").
    pub label: &'static str,
    /// How `config::bitcoind_rpc_config` resolves auth env (the per-chain
    /// error-string and evaluation-order contract).
    pub auth: RpcAuth,
    /// Which RPC call yields the CAuxPow bytes and how the capture path parses
    /// them.
    pub fetch: FetchStrategy,
    /// Child-payout reward params for value-side capture; `None` leaves the
    /// reward fields unpopulated.
    pub child_payout: Option<ChildPayoutParams>,
    /// Verbatim warning logged when a backfill starts below
    /// `activation_floor`; `None` means no warning (Namecoin, floor 0).
    pub floor_warning: Option<&'static str>,
    /// Whether post-backfill read-model repair scans all sources or only this
    /// chain's source (see [`RepairScope`]).
    pub repair_scope: RepairScope,
}

/// Whether `<PREFIX>_REORG_DEPTH` may configure a trailing rescan window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReorgPolicy {
    /// `<PREFIX>_REORG_DEPTH` overrides the default depth.
    EnvConfigurable,
    /// The chain is monotonic by construction: `reorg_depth` is hardcoded to 0
    /// and ANY present `<PREFIX>_REORG_DEPTH` (any value) is rejected, because
    /// a trailing rescan would re-capture a replaced same-height block as a
    /// NEW active row unless same-height reconciliation revokes the prior
    /// (a deferred follow-up). Exact current Elastos semantics.
    ForbiddenMonotonic,
}

/// One live producer chain, as declared data.
#[derive(Debug, Clone, Copy)]
pub struct ChainSpec {
    /// Stable identity; the registry-dispatch and `by_id` lookup key.
    pub id: ChainId,
    /// Stable lowercase identity: subcommands are `poll-<slug>` /
    /// `backfill-<slug>`.
    pub slug: &'static str,
    /// Human display name for log lines and error contexts.
    pub display_name: &'static str,
    /// Env prefix for every chain-scoped variable (`<PREFIX>_RPC_URL`,
    /// `<PREFIX>_START_HEIGHT`, ...).
    pub env_prefix: &'static str,
    /// The chain's `source_registry` code (`auxpow:<slug>`); the conformance
    /// test asserts the full binding.
    pub source_code: &'static str,
    /// First height that can carry AuxPoW evidence; earlier heights are
    /// skipped (with a per-chain warning where the spec's backfill data says
    /// so).
    pub activation_floor: i32,
    /// Live-poll defaults; `<PREFIX>_*` env vars override each field.
    pub poller: PollerDefaults,
    /// Whether `<PREFIX>_REORG_DEPTH` may configure a trailing rescan window
    /// (see [`ReorgPolicy`]).
    pub reorg_policy: ReorgPolicy,
    /// `Some` for bitcoind-family chains served by the shared
    /// `chains::auxpow_family` implementation; `None` for divergent chains.
    pub family: Option<FamilySpec>,
    /// Parse-time backfill range cap (Hathor, Elastos); `None` = uncapped.
    pub backfill_range_cap: Option<RangeCap>,
}

/// The live producer chains, in the order `main.rs` historically listed their
/// subcommands.
pub static CHAINS: [ChainSpec; 6] = [
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
        reorg_policy: ReorgPolicy::EnvConfigurable,
        family: Some(FamilySpec {
            label: "Namecoin",
            auth: RpcAuth::RequiredUserPass,
            fetch: FetchStrategy::RawBlock,
            child_payout: Some(NAMECOIN_CHILD_PAYOUT_PARAMS),
            floor_warning: None,
            repair_scope: RepairScope::Global,
        }),
        backfill_range_cap: None,
    },
    ChainSpec {
        id: ChainId::Rsk,
        slug: "rsk",
        display_name: "RSK",
        env_prefix: "RSK",
        source_code: RSK_SOURCE_CODE,
        activation_floor: RSK_FIRST_AUXPOW_HEIGHT,
        poller: PollerDefaults {
            poll_interval_seconds: 30,
            batch_size: 100,
            reorg_depth: 64,
        },
        reorg_policy: ReorgPolicy::EnvConfigurable,
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
        reorg_policy: ReorgPolicy::EnvConfigurable,
        family: Some(FamilySpec {
            label: "Syscoin",
            auth: RpcAuth::OptionalUserPassOrCookie,
            fetch: FetchStrategy::RawBlock,
            child_payout: Some(SYSCOIN_CHILD_PAYOUT_PARAMS),
            floor_warning: Some(
                "start-height precedes Syscoin AuxPoW activation; earlier blocks parse as non-AuxPoW and will be skipped",
            ),
            repair_scope: RepairScope::SourceScoped,
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
        reorg_policy: ReorgPolicy::EnvConfigurable,
        family: Some(FamilySpec {
            label: "Fractal",
            auth: RpcAuth::OptionalUserPassOrCookie,
            fetch: FetchStrategy::HeaderBlob {
                exact_version: FRACTAL_MERGE_MINED_VERSION,
            },
            child_payout: Some(FRACTAL_CHILD_REWARD_PARAMS),
            floor_warning: Some(
                "start-height precedes Fractal AuxPoW activation; earlier blocks are skipped",
            ),
            repair_scope: RepairScope::SourceScoped,
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
        reorg_policy: ReorgPolicy::EnvConfigurable,
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
        reorg_policy: ReorgPolicy::ForbiddenMonotonic,
        family: None,
        backfill_range_cap: Some(RangeCap {
            default_max: 50_000,
            note: "needed for a full 177000..tip backfill",
        }),
    },
];

/// Look up the spec row for a chain id. Infallible: every `ChainId` is
/// guaranteed a `CHAINS` row (panics otherwise, which the bidirectional
/// conformance test rules out).
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

    #[test]
    fn by_id_returns_the_matching_row() {
        assert_eq!(by_id(ChainId::Namecoin).slug, "namecoin");
        assert_eq!(by_id(ChainId::Elastos).slug, "elastos");
        assert_eq!(
            by_id(ChainId::Rsk).activation_floor,
            RSK_FIRST_AUXPOW_HEIGHT
        );
    }
}
