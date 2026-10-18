//! Cross-repo drift check for `STRICT_BIP34_CHAINS` and `BIP34_HEIGHT` against
//! `merge-mining-research` (`MERGE_MINING_RESEARCH_DIR` or `../merge-mining-research`).
//! Skips cleanly when the research checkout is absent.

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

fn locate_research_checkout() -> Option<PathBuf> {
    if let Some(dir) = env::var_os(RESEARCH_DIR_ENV) {
        let path = PathBuf::from(dir);
        return path.is_dir().then_some(path);
    }
    let workspace_root = Path::new(env!("CARGO_MANIFEST_DIR")).parent()?.parent()?;
    let sibling = workspace_root.join("../merge-mining-research");
    sibling.is_dir().then_some(sibling)
}

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
