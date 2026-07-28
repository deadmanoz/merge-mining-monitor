//! Cross-repo drift-sync test for the two BIP34 constants `mmm-capture` ports
//! from the merge-mining-research repo's Python classifier by hand.
//!
//! `mmm_capture::btc_orphan::STRICT_BIP34_CHAINS` and `BIP34_HEIGHT` are
//! deliberate hand-maintained ports of the research repo's
//! `BTC_COINBASE_SCRIPTSIG_CHAINS`
//! (`scripts/analysis/classify_btc_stale_relevance.py`) and `BIP34_HEIGHT`
//! (`src/stale_blocks_analysis/config.py`). The dual classifier (one Rust,
//! one Python) is permanent by design, so this test guards against silent
//! drift between the two hand-maintained copies rather than trying to
//! eliminate the duplication.
//!
//! The research checkout is located via `MERGE_MINING_RESEARCH_DIR`, falling
//! back to the sibling `../merge-mining-research` checkout relative to this
//! workspace's root. Neither is guaranteed to be present in every
//! environment this test runs in (CI, a contributor's machine), so the test
//! SKIPS cleanly when neither resolves to a directory. When the checkout IS
//! found, failing to locate either constant in the expected source is a hard
//! test failure: a silently-passing sync test would be worse than none.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use mmm_capture::btc_orphan::{BIP34_HEIGHT, STRICT_BIP34_CHAINS};

const RESEARCH_DIR_ENV: &str = "MERGE_MINING_RESEARCH_DIR";

#[test]
fn strict_bip34_chains_and_height_match_research_repo() {
    let Some(research_root) = locate_research_checkout() else {
        eprintln!(
            "skipping strict_bip34_chains_and_height_match_research_repo: neither ${RESEARCH_DIR_ENV} \
             nor the sibling ../merge-mining-research checkout was found"
        );
        return;
    };

    let classify_path = research_root.join("scripts/analysis/classify_btc_stale_relevance.py");
    let config_path = research_root.join("src/stale_blocks_analysis/config.py");

    let classify_source = fs::read_to_string(&classify_path)
        .unwrap_or_else(|err| panic!("read {}: {err}", classify_path.display()));
    let config_source = fs::read_to_string(&config_path)
        .unwrap_or_else(|err| panic!("read {}: {err}", config_path.display()));

    let research_chains =
        parse_btc_coinbase_scriptsig_chains(&classify_source).unwrap_or_else(|| {
            panic!(
                "could not find a BTC_COINBASE_SCRIPTSIG_CHAINS = {{ ... }} set literal in {}; \
             the research repo's constant format may have changed and this parser needs updating",
                classify_path.display()
            )
        });
    let research_bip34_height = parse_bip34_height(&config_source).unwrap_or_else(|| {
        panic!(
            "could not find a BIP34_HEIGHT = <int> assignment in {}; the research repo's \
             constant format may have changed and this parser needs updating",
            config_path.display()
        )
    });

    let mut mmm_chains: Vec<&str> = STRICT_BIP34_CHAINS.to_vec();
    mmm_chains.sort_unstable();
    let mut research_chains_sorted: Vec<&str> =
        research_chains.iter().map(String::as_str).collect();
    research_chains_sorted.sort_unstable();

    assert_eq!(
        mmm_chains,
        research_chains_sorted,
        "mmm_capture::btc_orphan::STRICT_BIP34_CHAINS has drifted from the research repo's \
         BTC_COINBASE_SCRIPTSIG_CHAINS ({})",
        classify_path.display()
    );
    assert_eq!(
        BIP34_HEIGHT,
        research_bip34_height,
        "mmm_capture::btc_orphan::BIP34_HEIGHT has drifted from the research repo's BIP34_HEIGHT ({})",
        config_path.display()
    );
}

/// `MERGE_MINING_RESEARCH_DIR` if it resolves to a directory, else the
/// sibling `../merge-mining-research` checkout relative to this workspace's
/// root if that resolves to a directory, else `None`. A `MERGE_MINING_RESEARCH_DIR`
/// pointing at a non-directory does NOT fall back to the sibling: an explicit
/// but wrong override should behave the same as "not found" for the skip
/// decision, since either way there is nothing to read.
fn locate_research_checkout() -> Option<PathBuf> {
    if let Some(dir) = env::var_os(RESEARCH_DIR_ENV) {
        let path = PathBuf::from(dir);
        return path.is_dir().then_some(path);
    }
    let workspace_root = Path::new(env!("CARGO_MANIFEST_DIR")).parent()?.parent()?;
    let sibling = workspace_root.join("../merge-mining-research");
    sibling.is_dir().then_some(sibling)
}

/// Parse the `BTC_COINBASE_SCRIPTSIG_CHAINS = { ... }` Python set literal:
/// simple line-based extraction of quoted strings between the `{` that opens
/// the assignment and the next top-level `}`. Deliberately naive (no real
/// Python parsing) since the source is a flat literal of quoted strings, one
/// per line, with no nesting; tolerant of blank lines, trailing commas, and
/// `#` comments. Returns `None` only when the assignment itself cannot be
/// found (a missing/renamed constant), never for an empty or malformed body
/// (those panic instead, per the "fail loudly" contract).
fn parse_btc_coinbase_scriptsig_chains(source: &str) -> Option<Vec<String>> {
    let name_at = source.find("BTC_COINBASE_SCRIPTSIG_CHAINS")?;
    let brace_open = source[name_at..].find('{')? + name_at;
    let brace_close = source[brace_open..].find('}')? + brace_open;
    let body = &source[brace_open + 1..brace_close];

    let mut chains = Vec::new();
    for raw_line in body.lines() {
        let line = raw_line.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        for entry in line.split(',') {
            let entry = entry.trim();
            if entry.is_empty() {
                continue;
            }
            let unquoted = entry
                .strip_prefix('"')
                .and_then(|s| s.strip_suffix('"'))
                .or_else(|| entry.strip_prefix('\'').and_then(|s| s.strip_suffix('\'')))
                .unwrap_or_else(|| {
                    panic!(
                        "unexpected token {entry:?} inside BTC_COINBASE_SCRIPTSIG_CHAINS \
                         (expected a quoted chain name)"
                    )
                });
            chains.push(unquoted.to_string());
        }
    }
    Some(chains)
}

/// Parse the literal `BIP34_HEIGHT = <int>` assignment. Python underscore
/// digit separators (e.g. `227_931`) are stripped before parsing.
fn parse_bip34_height(source: &str) -> Option<i32> {
    for raw_line in source.lines() {
        let line = raw_line.split('#').next().unwrap_or("").trim();
        let Some(rest) = line.strip_prefix("BIP34_HEIGHT") else {
            continue;
        };
        let Some(value) = rest.trim_start().strip_prefix('=') else {
            continue;
        };
        let digits: String = value.trim().chars().filter(|c| *c != '_').collect();
        return digits.parse().ok();
    }
    None
}
