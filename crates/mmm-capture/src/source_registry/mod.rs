//! Source Lifecycle Registry: the single definition of every
//! `source` row.
//!
//! Source code, chain, kind, and lifecycle (Live, Historical, or Catalogued) all live here
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
//! The array ORDER is part of the contract: it fixes the seeded `source.id`
//! (id = registry index + 1), preserving the historical seed order
//! (namecoin, rsk, bitcoin, syscoin, fractal, hathor, elastos, then the 16
//! historical chains, then the 10 catalogued chains). See the unit tests below.

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
/// recovered dead chains; `Catalogued` covers chains known to have
/// BTC-merge-mined but with no recovered data (no producer, no evidence;
/// surfaced for completeness only).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceLifecycle {
    Live,
    Historical,
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
            SourceLifecycle::Catalogued => "catalogued",
        }
    }
}

/// One source definition. `code` is `<kind>:<chain>[:instance]`;
/// the unit tests assert `code` decomposes to exactly `kind`/`chain`/`instance`.
#[derive(Debug, Clone, Copy)]
pub struct SourceDefinition {
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
const fn live_auxpow(code: &'static str, chain: &'static str) -> SourceDefinition {
    SourceDefinition {
        code,
        chain,
        instance: None,
        kind: SourceKind::Auxpow,
        lifecycle: SourceLifecycle::Live,
    }
}

/// A historical (recovered, dead-chain) AuxPoW source.
const fn historical_auxpow(code: &'static str, chain: &'static str) -> SourceDefinition {
    SourceDefinition {
        code,
        chain,
        instance: None,
        kind: SourceKind::Auxpow,
        lifecycle: SourceLifecycle::Historical,
    }
}

/// A catalogued source: a chain known to have BTC-merge-mined but with no
/// recovered data in this monitor. No producer, no evidence; kind=auxpow is the
/// merge-mining catch-all (as for the live RSK/Hathor rows).
const fn catalogued_auxpow(code: &'static str, chain: &'static str) -> SourceDefinition {
    SourceDefinition {
        code,
        chain,
        instance: None,
        kind: SourceKind::Auxpow,
        lifecycle: SourceLifecycle::Catalogued,
    }
}

/// The source list. Array index + 1 is the seeded `source.id`, so
/// the order is a hard contract (asserted by tests + the seed/conformance path).
pub const SOURCE_REGISTRY: &[SourceDefinition] = &[
    // -- live (ids 1..7, preserving the historical seed order) --
    live_auxpow(NAMECOIN_SOURCE_CODE, "namecoin"),
    live_auxpow(RSK_SOURCE_CODE, "rsk"),
    SourceDefinition {
        code: BITCOIN_SOURCE_CODE,
        chain: "bitcoin",
        instance: Some("core"),
        kind: SourceKind::LiveChaintip,
        lifecycle: SourceLifecycle::Live,
    },
    live_auxpow(SYSCOIN_SOURCE_CODE, "syscoin"),
    live_auxpow(FRACTAL_SOURCE_CODE, "fractal"),
    live_auxpow(HATHOR_SOURCE_CODE, "hathor"),
    live_auxpow(ELASTOS_SOURCE_CODE, "elastos"),
    // -- historical (recovered) AuxPoW sources (ids 8..23) --
    historical_auxpow("auxpow:argentum", "argentum"),
    historical_auxpow("auxpow:bitcoin-vault", "bitcoin-vault"),
    historical_auxpow("auxpow:bitmark", "bitmark"),
    historical_auxpow("auxpow:coiledcoin", "coiledcoin"),
    historical_auxpow("auxpow:crown", "crown"),
    historical_auxpow("auxpow:devcoin", "devcoin"),
    historical_auxpow("auxpow:emercoin", "emercoin"),
    historical_auxpow("auxpow:geistgeld", "geistgeld"),
    historical_auxpow("auxpow:groupcoin", "groupcoin"),
    historical_auxpow("auxpow:huntercoin", "huntercoin"),
    historical_auxpow("auxpow:i0coin", "i0coin"),
    historical_auxpow("auxpow:ixcoin", "ixcoin"),
    historical_auxpow("auxpow:myriadcoin", "myriadcoin"),
    historical_auxpow("auxpow:terracoin", "terracoin"),
    historical_auxpow("auxpow:unobtanium", "unobtanium"),
    historical_auxpow("auxpow:xaya", "xaya"),
    // -- catalogued (known BTC-merge-mined, not recovered) sources (ids 24..33) --
    catalogued_auxpow("auxpow:vcash", "vcash"),
    catalogued_auxpow("auxpow:lyncoin", "lyncoin"),
    catalogued_auxpow("auxpow:jax-network", "jax-network"),
    catalogued_auxpow("auxpow:sixeleven", "sixeleven"),
    catalogued_auxpow("auxpow:blast", "blast"),
    catalogued_auxpow("auxpow:doichain", "doichain"),
    catalogued_auxpow("auxpow:fusioncoin", "fusioncoin"),
    catalogued_auxpow("auxpow:jincoin", "jincoin"),
    catalogued_auxpow("auxpow:mazacoin", "mazacoin"),
    catalogued_auxpow("auxpow:bitcoin-stash", "bitcoin-stash"),
    // -- post-baseline historical additions (id 34..) --
    // Appended at the very end (not in the historical group above) so existing
    // source ids 1..33 stay stable; inserting mid-array would renumber the
    // catalogued rows and need a baseline reset. Lifecycle is still Historical;
    // the frontend groups by lifecycle, not id order.
    historical_auxpow("auxpow:elcash", "elcash"),
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
    fn preserved_full_id_order() {
        // id = registry index + 1 for ALL 34 sources, so /sources ids are stable.
        // The first seven preserve the existing seed order (the Bitcoin source is id 3,
        // NOT the const-declaration order); the historical suffix is pinned too, so
        // a future reorder that regenerated the artifacts could not silently change
        // public source ids.
        let want = [
            "auxpow:namecoin",
            "auxpow:rsk",
            "live-chaintip:bitcoin:core",
            "auxpow:syscoin",
            "auxpow:fractal",
            "auxpow:hathor",
            "auxpow:elastos",
            "auxpow:argentum",
            "auxpow:bitcoin-vault",
            "auxpow:bitmark",
            "auxpow:coiledcoin",
            "auxpow:crown",
            "auxpow:devcoin",
            "auxpow:emercoin",
            "auxpow:geistgeld",
            "auxpow:groupcoin",
            "auxpow:huntercoin",
            "auxpow:i0coin",
            "auxpow:ixcoin",
            "auxpow:myriadcoin",
            "auxpow:terracoin",
            "auxpow:unobtanium",
            "auxpow:xaya",
            "auxpow:vcash",
            "auxpow:lyncoin",
            "auxpow:jax-network",
            "auxpow:sixeleven",
            "auxpow:blast",
            "auxpow:doichain",
            "auxpow:fusioncoin",
            "auxpow:jincoin",
            "auxpow:mazacoin",
            "auxpow:bitcoin-stash",
            "auxpow:elcash",
        ];
        let got: Vec<&str> = SOURCE_REGISTRY.iter().map(|s| s.code).collect();
        assert_eq!(got, want);
    }

    #[test]
    fn registry_has_seven_live_seventeen_historical_ten_catalogued() {
        assert_eq!(live().count(), 7);
        assert_eq!(historical().count(), 17);
        assert_eq!(catalogued().count(), 10);
        // Every historical entry is recovered AuxPoW evidence.
        for s in historical() {
            assert_eq!(s.kind, SourceKind::Auxpow, "{}", s.code);
        }
        // Every catalogued entry is AuxPoW (the merge-mining catch-all); they have
        // no producer and no recovered evidence.
        for s in catalogued() {
            assert_eq!(s.kind, SourceKind::Auxpow, "{}", s.code);
        }
    }

    #[test]
    fn kinds_matches_registered_source_vocabulary() {
        let kinds: Vec<&str> = SourceKind::ALL.iter().map(|kind| kind.as_str()).collect();
        assert_eq!(kinds, vec!["auxpow", "live-chaintip"]);
    }
}
