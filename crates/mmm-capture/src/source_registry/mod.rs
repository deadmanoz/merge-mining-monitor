//! Source Lifecycle Registry: canonical definition of every `source` row.
//!
//! Per-chain presentation and editorial profiles live in `data/sources/chain_profiles.json`.
//! Source ids are permanent and never reused (id 32 is retired).

#[cfg(any(test, feature = "artifact-generation"))]
pub mod generate;

/// The `<kind>` segment of a source code, and the `source.kind` CHECK domain.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceKind {
    Auxpow,
    LiveChaintip,
}

impl SourceKind {
    pub const ALL: [SourceKind; 2] = [SourceKind::Auxpow, SourceKind::LiveChaintip];

    pub const fn as_str(self) -> &'static str {
        match self {
            SourceKind::Auxpow => "auxpow",
            SourceKind::LiveChaintip => "live-chaintip",
        }
    }
}

/// Source lifecycle class emitted into `SOURCE_LIFECYCLE`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceLifecycle {
    Live,
    Historical,
    Partial,
    Surveyed,
    Catalogued,
}

impl SourceLifecycle {
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

/// Where a child chain serializes the target its Bitcoin parent must meet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChildTargetLocation {
    HeaderNbits,
    PowData,
}

/// One source definition (`code` is `<kind>:<chain>[:instance]`).
#[derive(Debug, Clone, Copy)]
pub struct SourceDefinition {
    pub id: i64,
    pub code: &'static str,
    pub chain: &'static str,
    pub instance: Option<&'static str>,
    pub kind: SourceKind,
    pub lifecycle: SourceLifecycle,
    pub child_target_location: ChildTargetLocation,
}

pub const NAMECOIN_SOURCE_CODE: &str = "auxpow:namecoin";
pub const RSK_SOURCE_CODE: &str = "auxpow:rsk";
pub const SYSCOIN_SOURCE_CODE: &str = "auxpow:syscoin";
pub const FRACTAL_SOURCE_CODE: &str = "auxpow:fractal";
pub const HATHOR_SOURCE_CODE: &str = "auxpow:hathor";
pub const ELASTOS_SOURCE_CODE: &str = "auxpow:elastos";
pub const TERRACOIN_SOURCE_CODE: &str = "auxpow:terracoin";
pub const QBIT_SOURCE_CODE: &str = "auxpow:qbit";
pub const BITCOIN_SOURCE_CODE: &str = "live-chaintip:bitcoin:core";

const fn live_auxpow(id: i64, code: &'static str, chain: &'static str) -> SourceDefinition {
    SourceDefinition {
        id,
        code,
        chain,
        instance: None,
        kind: SourceKind::Auxpow,
        lifecycle: SourceLifecycle::Live,
        child_target_location: ChildTargetLocation::HeaderNbits,
    }
}

const fn historical_auxpow(id: i64, code: &'static str, chain: &'static str) -> SourceDefinition {
    SourceDefinition {
        id,
        code,
        chain,
        instance: None,
        kind: SourceKind::Auxpow,
        lifecycle: SourceLifecycle::Historical,
        child_target_location: ChildTargetLocation::HeaderNbits,
    }
}

const fn historical_powdata_auxpow(
    id: i64,
    code: &'static str,
    chain: &'static str,
) -> SourceDefinition {
    SourceDefinition {
        child_target_location: ChildTargetLocation::PowData,
        ..historical_auxpow(id, code, chain)
    }
}

const fn partial_auxpow(id: i64, code: &'static str, chain: &'static str) -> SourceDefinition {
    SourceDefinition {
        id,
        code,
        chain,
        instance: None,
        kind: SourceKind::Auxpow,
        lifecycle: SourceLifecycle::Partial,
        child_target_location: ChildTargetLocation::HeaderNbits,
    }
}

const fn surveyed_auxpow(id: i64, code: &'static str, chain: &'static str) -> SourceDefinition {
    SourceDefinition {
        id,
        code,
        chain,
        instance: None,
        kind: SourceKind::Auxpow,
        lifecycle: SourceLifecycle::Surveyed,
        child_target_location: ChildTargetLocation::HeaderNbits,
    }
}

const fn catalogued_auxpow(id: i64, code: &'static str, chain: &'static str) -> SourceDefinition {
    SourceDefinition {
        id,
        code,
        chain,
        instance: None,
        kind: SourceKind::Auxpow,
        lifecycle: SourceLifecycle::Catalogued,
        child_target_location: ChildTargetLocation::HeaderNbits,
    }
}

/// The source list, in ascending permanent-id order.
pub const SOURCE_REGISTRY: &[SourceDefinition] = &[
    live_auxpow(1, NAMECOIN_SOURCE_CODE, "namecoin"),
    live_auxpow(2, RSK_SOURCE_CODE, "rsk"),
    SourceDefinition {
        id: 3,
        code: BITCOIN_SOURCE_CODE,
        chain: "bitcoin",
        instance: Some("core"),
        kind: SourceKind::LiveChaintip,
        lifecycle: SourceLifecycle::Live,
        child_target_location: ChildTargetLocation::HeaderNbits,
    },
    live_auxpow(4, SYSCOIN_SOURCE_CODE, "syscoin"),
    live_auxpow(5, FRACTAL_SOURCE_CODE, "fractal"),
    live_auxpow(6, HATHOR_SOURCE_CODE, "hathor"),
    live_auxpow(7, ELASTOS_SOURCE_CODE, "elastos"),
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
    live_auxpow(21, TERRACOIN_SOURCE_CODE, "terracoin"),
    historical_auxpow(22, "auxpow:unobtanium", "unobtanium"),
    historical_powdata_auxpow(23, "auxpow:xaya", "xaya"),
    partial_auxpow(24, "auxpow:vcash", "vcash"),
    historical_auxpow(25, "auxpow:lyncoin", "lyncoin"),
    catalogued_auxpow(26, "auxpow:jax-network", "jax-network"),
    historical_auxpow(27, "auxpow:sixeleven", "sixeleven"),
    catalogued_auxpow(28, "auxpow:blast", "blast"),
    surveyed_auxpow(29, "auxpow:doichain", "doichain"),
    catalogued_auxpow(30, "auxpow:fusioncoin", "fusioncoin"),
    catalogued_auxpow(31, "auxpow:jincoin", "jincoin"),
    // id 32 is retired (Mazacoin).
    catalogued_auxpow(33, "auxpow:bitcoin-stash", "bitcoin-stash"),
    historical_auxpow(34, "auxpow:elcash", "elcash"),
    historical_powdata_auxpow(35, "auxpow:rod", "rod"),
    live_auxpow(36, QBIT_SOURCE_CODE, "qbit"),
];

pub fn by_code(code: &str) -> Option<&'static SourceDefinition> {
    SOURCE_REGISTRY.iter().find(|s| s.code == code)
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
            (35, "auxpow:rod"),
            (36, "auxpow:qbit"),
        ];
        let got: Vec<(i64, &str)> = SOURCE_REGISTRY.iter().map(|s| (s.id, s.code)).collect();
        assert_eq!(got, want);
        assert!(by_code("auxpow:mazacoin").is_none());
    }

    #[test]
    fn kinds_matches_registered_source_vocabulary() {
        let kinds: Vec<&str> = SourceKind::ALL.iter().map(|kind| kind.as_str()).collect();
        assert_eq!(kinds, vec!["auxpow", "live-chaintip"]);
    }
}
