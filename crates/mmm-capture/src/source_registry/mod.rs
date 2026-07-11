//! Source Lifecycle Registry: the single definition of every
//! `source` row.
//!
//! Source id, code, chain, kind, and lifecycle all live here
//! exactly once; per-chain presentation (display config, by-line, modal help,
//! editorial profiles) lives in `data/sources/chain_profiles.json`. Everything else in
//! the repo is derived from this list or drift-checked against it:
//!
//! - API source-filter validation reads [`SOURCE_REGISTRY`] and [`SourceKind::ALL`].
//! - the producer-facing `*_SOURCE_CODE` constants are OWNED here; every caller
//!   imports them directly from this module.
//! - the source-seed SQL (`migrations/0002_seed_sources.sql`) and the frontend
//!   metadata (`www/js/source-registry.generated.js`) are GENERATED from
//!   [`SOURCE_REGISTRY`] by `gen-source-artifacts` (see [`generate`]).
//! - DB integration tests check the seeded `source` rows against
//!   [`SOURCE_REGISTRY`]; API fixture tests check that fixture source codes are
//!   registered.
//!
//! Source ids are explicit, permanent, and never reused. The array is kept in
//! ascending-id order for readability, but its position does not define identity.
//! Retired ids remain absent from the registry (currently id 32). See the unit
//! tests below.

#[cfg(any(test, feature = "artifact-generation"))]
pub mod generate;

// ---------------------------------------------------------------------------
// Vocabulary enums (the DB CHECK domains for `source`).
// ---------------------------------------------------------------------------

/// The `<kind>` segment of a source code, and the `source.kind` CHECK domain.
/// Distinct from the parent-classification kind in `api::normalize::ParentKind`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceKind {
    Auxpow,
    LiveChaintip,
}

impl SourceKind {
    /// The full source-kind vocabulary, in DB CHECK order.
    pub const ALL: [SourceKind; 2] = [SourceKind::Auxpow, SourceKind::LiveChaintip];

    /// The exact `source.kind` CHECK token, and the `<kind>` segment of a source
    /// code. The unit tests bind it: every registry `code` must decompose so
    /// segment 0 equals `kind.as_str()`. Emitted into the seed SQL and frontend
    /// generators and used by the API source-filter validation, so these strings
    /// are a persisted wire contract: never reword without a migration.
    pub const fn as_str(self) -> &'static str {
        match self {
            SourceKind::Auxpow => "auxpow",
            SourceKind::LiveChaintip => "live-chaintip",
        }
    }
}

/// The source-class axis. Registry-derived display metadata; `Live` covers the
/// active producers and the Bitcoin classifier; `Historical` covers the
/// recovered dead chains; `Partial` covers an ingestible recovered subset whose
/// full child chain remains unavailable; `Surveyed` covers a recovered and
/// reviewed chain that yielded no admissible Bitcoin evidence; `Catalogued`
/// covers chains known to have BTC-merge-mined but with no recovered data.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceLifecycle {
    Live,
    Historical,
    Partial,
    Surveyed,
    Catalogued,
}

impl SourceLifecycle {
    /// The lifecycle token emitted into the frontend `SOURCE_LIFECYCLE` dict,
    /// keyed by exact source code (not chain), so live and historical AuxPoW rows
    /// separate even though both use kind=auxpow. Consumed by `render_frontend_js`;
    /// the buildless www/ frontend reads these strings, so they are a wire value.
    pub const fn as_str(self) -> &'static str {
        match self {
            SourceLifecycle::Live => "live",
            SourceLifecycle::Historical => "historical",
            SourceLifecycle::Partial => "partial",
            SourceLifecycle::Surveyed => "surveyed",
            SourceLifecycle::Catalogued => "catalogued",
        }
    }
}

/// One source definition. `code` is `<kind>:<chain>[:instance]`;
/// the unit tests assert `code` decomposes to exactly `kind`/`chain`/`instance`.
#[derive(Debug, Clone, Copy)]
pub struct SourceDefinition {
    /// Permanent database identity. Values may have gaps and are never reused.
    pub id: i64,
    pub code: &'static str,
    pub chain: &'static str,
    pub instance: Option<&'static str>,
    pub kind: SourceKind,
    pub lifecycle: SourceLifecycle,
}

// ---------------------------------------------------------------------------
// Source codes. Every caller (producers, read-model,
// API, tests) imports these directly from `mmm_capture::source_registry`. These
// are the canonical `source.code` strings; changing a code here is a migration +
// a frontend regen + a SOURCE_REGISTRY row edit, never a literal swap.
// ---------------------------------------------------------------------------

pub const NAMECOIN_SOURCE_CODE: &str = "auxpow:namecoin";
pub const RSK_SOURCE_CODE: &str = "auxpow:rsk";
pub const SYSCOIN_SOURCE_CODE: &str = "auxpow:syscoin";
pub const FRACTAL_SOURCE_CODE: &str = "auxpow:fractal";
pub const HATHOR_SOURCE_CODE: &str = "auxpow:hathor";
pub const ELASTOS_SOURCE_CODE: &str = "auxpow:elastos";
pub const BITCOIN_SOURCE_CODE: &str = "live-chaintip:bitcoin:core";

/// A live AuxPoW producer source.
const fn live_auxpow(id: i64, code: &'static str, chain: &'static str) -> SourceDefinition {
    SourceDefinition {
        id,
        code,
        chain,
        instance: None,
        kind: SourceKind::Auxpow,
        lifecycle: SourceLifecycle::Live,
    }
}

/// A historical (recovered, dead-chain) AuxPoW source.
const fn historical_auxpow(id: i64, code: &'static str, chain: &'static str) -> SourceDefinition {
    SourceDefinition {
        id,
        code,
        chain,
        instance: None,
        kind: SourceKind::Auxpow,
        lifecycle: SourceLifecycle::Historical,
    }
}

/// An ingestible recovered subset whose full child chain is still unavailable.
const fn partial_auxpow(id: i64, code: &'static str, chain: &'static str) -> SourceDefinition {
    SourceDefinition {
        id,
        code,
        chain,
        instance: None,
        kind: SourceKind::Auxpow,
        lifecycle: SourceLifecycle::Partial,
    }
}

/// A recovered and reviewed chain with no admissible Bitcoin evidence rows.
const fn surveyed_auxpow(id: i64, code: &'static str, chain: &'static str) -> SourceDefinition {
    SourceDefinition {
        id,
        code,
        chain,
        instance: None,
        kind: SourceKind::Auxpow,
        lifecycle: SourceLifecycle::Surveyed,
    }
}

/// A catalogued source: a chain known to have BTC-merge-mined but with no
/// recovered data in this monitor. No producer, no evidence; kind=auxpow is the
/// merge-mining catch-all (as for the live RSK/Hathor rows).
const fn catalogued_auxpow(id: i64, code: &'static str, chain: &'static str) -> SourceDefinition {
    SourceDefinition {
        id,
        code,
        chain,
        instance: None,
        kind: SourceKind::Auxpow,
        lifecycle: SourceLifecycle::Catalogued,
    }
}

/// The source list, in ascending permanent-id order. Array position is not an id.
pub const SOURCE_REGISTRY: &[SourceDefinition] = &[
    // -- live (ids 1..7, preserving the historical seed identities) --
    live_auxpow(1, NAMECOIN_SOURCE_CODE, "namecoin"),
    live_auxpow(2, RSK_SOURCE_CODE, "rsk"),
    SourceDefinition {
        id: 3,
        code: BITCOIN_SOURCE_CODE,
        chain: "bitcoin",
        instance: Some("core"),
        kind: SourceKind::LiveChaintip,
        lifecycle: SourceLifecycle::Live,
    },
    live_auxpow(4, SYSCOIN_SOURCE_CODE, "syscoin"),
    live_auxpow(5, FRACTAL_SOURCE_CODE, "fractal"),
    live_auxpow(6, HATHOR_SOURCE_CODE, "hathor"),
    live_auxpow(7, ELASTOS_SOURCE_CODE, "elastos"),
    // -- historical (recovered) AuxPoW sources (ids 8..23) --
    historical_auxpow(8, "auxpow:argentum", "argentum"),
    historical_auxpow(9, "auxpow:bitcoin-vault", "bitcoin-vault"),
    historical_auxpow(10, "auxpow:bitmark", "bitmark"),
    historical_auxpow(11, "auxpow:coiledcoin", "coiledcoin"),
    historical_auxpow(12, "auxpow:crown", "crown"),
    historical_auxpow(13, "auxpow:devcoin", "devcoin"),
    historical_auxpow(14, "auxpow:emercoin", "emercoin"),
    historical_auxpow(15, "auxpow:geistgeld", "geistgeld"),
    historical_auxpow(16, "auxpow:groupcoin", "groupcoin"),
    historical_auxpow(17, "auxpow:huntercoin", "huntercoin"),
    historical_auxpow(18, "auxpow:i0coin", "i0coin"),
    historical_auxpow(19, "auxpow:ixcoin", "ixcoin"),
    historical_auxpow(20, "auxpow:myriadcoin", "myriadcoin"),
    historical_auxpow(21, "auxpow:terracoin", "terracoin"),
    historical_auxpow(22, "auxpow:unobtanium", "unobtanium"),
    historical_auxpow(23, "auxpow:xaya", "xaya"),
    // -- recovery candidates and recovered datasets --
    partial_auxpow(24, "auxpow:vcash", "vcash"),
    historical_auxpow(25, "auxpow:lyncoin", "lyncoin"),
    catalogued_auxpow(26, "auxpow:jax-network", "jax-network"),
    historical_auxpow(27, "auxpow:sixeleven", "sixeleven"),
    catalogued_auxpow(28, "auxpow:blast", "blast"),
    surveyed_auxpow(29, "auxpow:doichain", "doichain"),
    catalogued_auxpow(30, "auxpow:fusioncoin", "fusioncoin"),
    catalogued_auxpow(31, "auxpow:jincoin", "jincoin"),
    // id 32 is retired. Mazacoin was removed after its source audit found no AuxPoW.
    catalogued_auxpow(33, "auxpow:bitcoin-stash", "bitcoin-stash"),
    historical_auxpow(34, "auxpow:elcash", "elcash"),
];

// ---------------------------------------------------------------------------
// Helpers.
// ---------------------------------------------------------------------------

/// Look up a definition by exact source code.
pub fn by_code(code: &str) -> Option<&'static SourceDefinition> {
    SOURCE_REGISTRY.iter().find(|s| s.code == code)
}

/// The live sources (active producers + the Bitcoin classifier). Positive
/// `== Live` match so catalogued rows are excluded (a `!= Historical` predicate
/// would wrongly include them).
#[cfg(any(test, feature = "test-support"))]
pub fn live() -> impl Iterator<Item = &'static SourceDefinition> {
    SOURCE_REGISTRY
        .iter()
        .filter(|s| s.lifecycle == SourceLifecycle::Live)
}

/// The historical (recovered, dead-chain) sources.
#[cfg(any(test, feature = "test-support"))]
pub fn historical() -> impl Iterator<Item = &'static SourceDefinition> {
    SOURCE_REGISTRY
        .iter()
        .filter(|s| s.lifecycle == SourceLifecycle::Historical)
}

/// The catalogued (known BTC-merge-mined, not recovered) sources.
#[cfg(any(test, feature = "test-support"))]
pub fn catalogued() -> impl Iterator<Item = &'static SourceDefinition> {
    SOURCE_REGISTRY
        .iter()
        .filter(|s| s.lifecycle == SourceLifecycle::Catalogued)
}

/// The ingestible recovered subsets whose full child chain remains unavailable.
#[cfg(any(test, feature = "test-support"))]
pub fn partial() -> impl Iterator<Item = &'static SourceDefinition> {
    SOURCE_REGISTRY
        .iter()
        .filter(|s| s.lifecycle == SourceLifecycle::Partial)
}

/// Recovered and reviewed sources with no admissible Bitcoin evidence rows.
#[cfg(any(test, feature = "test-support"))]
pub fn surveyed() -> impl Iterator<Item = &'static SourceDefinition> {
    SOURCE_REGISTRY
        .iter()
        .filter(|s| s.lifecycle == SourceLifecycle::Surveyed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    #[test]
    fn codes_are_unique() {
        let set: BTreeSet<&str> = SOURCE_REGISTRY.iter().map(|s| s.code).collect();
        assert_eq!(set.len(), SOURCE_REGISTRY.len(), "duplicate source code");
    }

    #[test]
    fn ids_are_unique_positive_and_sorted() {
        let ids: Vec<i64> = SOURCE_REGISTRY.iter().map(|s| s.id).collect();
        let set: BTreeSet<i64> = ids.iter().copied().collect();
        assert_eq!(set.len(), SOURCE_REGISTRY.len(), "duplicate source id");
        assert!(ids.iter().all(|id| *id > 0), "source ids must be positive");
        assert!(ids.windows(2).all(|pair| pair[0] < pair[1]));
    }

    #[test]
    fn code_decomposes_to_its_structured_fields() {
        // Guards against a code drifting from its own kind/chain/instance within a
        // registry row (mirrors the fixture contract's decomposition invariant).
        for s in SOURCE_REGISTRY {
            let parts: Vec<&str> = s.code.split(':').collect();
            assert!(
                parts.len() == 2 || parts.len() == 3,
                "{} must be <kind>:<chain>[:instance]",
                s.code
            );
            assert_eq!(parts[0], s.kind.as_str(), "{} kind segment", s.code);
            assert_eq!(parts[1], s.chain, "{} chain segment", s.code);
            match (parts.get(2), s.instance) {
                (Some(seg), Some(inst)) => assert_eq!(*seg, inst, "{} instance segment", s.code),
                (None, None) => {}
                _ => panic!("{} instance segment vs field mismatch", s.code),
            }
        }
    }

    #[test]
    fn segments_are_lowercase_kebab() {
        for s in SOURCE_REGISTRY {
            for part in s.code.split(':') {
                assert!(!part.is_empty(), "{} has an empty segment", s.code);
                let mut chars = part.chars();
                let first = chars.next().unwrap();
                assert!(
                    first.is_ascii_lowercase(),
                    "{} segment must start [a-z]",
                    s.code
                );
                assert!(
                    part.chars()
                        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'),
                    "{} segment has invalid char",
                    s.code
                );
                assert!(
                    !part.ends_with('-') && !part.contains("--"),
                    "{} bad hyphen",
                    s.code
                );
            }
        }
    }

    #[test]
    fn preserved_full_ids_and_codes() {
        // Every persisted identity is pinned explicitly. Gaps are intentional and
        // retired ids must never be reused.
        let want = [
            (1, "auxpow:namecoin"),
            (2, "auxpow:rsk"),
            (3, "live-chaintip:bitcoin:core"),
            (4, "auxpow:syscoin"),
            (5, "auxpow:fractal"),
            (6, "auxpow:hathor"),
            (7, "auxpow:elastos"),
            (8, "auxpow:argentum"),
            (9, "auxpow:bitcoin-vault"),
            (10, "auxpow:bitmark"),
            (11, "auxpow:coiledcoin"),
            (12, "auxpow:crown"),
            (13, "auxpow:devcoin"),
            (14, "auxpow:emercoin"),
            (15, "auxpow:geistgeld"),
            (16, "auxpow:groupcoin"),
            (17, "auxpow:huntercoin"),
            (18, "auxpow:i0coin"),
            (19, "auxpow:ixcoin"),
            (20, "auxpow:myriadcoin"),
            (21, "auxpow:terracoin"),
            (22, "auxpow:unobtanium"),
            (23, "auxpow:xaya"),
            (24, "auxpow:vcash"),
            (25, "auxpow:lyncoin"),
            (26, "auxpow:jax-network"),
            (27, "auxpow:sixeleven"),
            (28, "auxpow:blast"),
            (29, "auxpow:doichain"),
            (30, "auxpow:fusioncoin"),
            (31, "auxpow:jincoin"),
            (33, "auxpow:bitcoin-stash"),
            (34, "auxpow:elcash"),
        ];
        let got: Vec<(i64, &str)> = SOURCE_REGISTRY.iter().map(|s| (s.id, s.code)).collect();
        assert_eq!(got, want);
        assert!(by_code("auxpow:mazacoin").is_none());
    }

    #[test]
    fn registry_lifecycle_counts_match_recovery_state() {
        assert_eq!(live().count(), 7);
        assert_eq!(historical().count(), 19);
        assert_eq!(partial().count(), 1);
        assert_eq!(surveyed().count(), 1);
        assert_eq!(catalogued().count(), 5);
        assert_eq!(SOURCE_REGISTRY.len(), 33);
        // Every historical entry is recovered AuxPoW evidence.
        for s in historical() {
            assert_eq!(s.kind, SourceKind::Auxpow, "{}", s.code);
        }
        // Every catalogued entry is AuxPoW (the merge-mining catch-all); they have
        // no producer and no recovered evidence.
        for s in catalogued() {
            assert_eq!(s.kind, SourceKind::Auxpow, "{}", s.code);
        }
        assert_eq!(partial().next().unwrap().code, "auxpow:vcash");
        assert_eq!(surveyed().next().unwrap().code, "auxpow:doichain");
        assert_eq!(
            by_code("auxpow:lyncoin").unwrap().lifecycle,
            SourceLifecycle::Historical
        );
        assert_eq!(
            by_code("auxpow:sixeleven").unwrap().lifecycle,
            SourceLifecycle::Historical
        );
    }

    #[test]
    fn kinds_matches_registered_source_vocabulary() {
        let kinds: Vec<&str> = SourceKind::ALL.iter().map(|kind| kind.as_str()).collect();
        assert_eq!(kinds, vec!["auxpow", "live-chaintip"]);
    }
}
