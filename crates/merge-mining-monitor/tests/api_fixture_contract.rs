use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use serde_json::Value;

#[test]
fn api_fixture_examples_are_listed_and_parse() {
    let dir = fixture_dir();
    let manifest = load_json(&dir.join("manifest.json"));
    assert_eq!(manifest["manifest_schema_version"], "manifest-v1");

    let listed = listed_fixture_files(&manifest);
    let actual = actual_fixture_files(&dir);
    assert_eq!(listed, actual, "manifest must list every API fixture");

    for file in listed {
        let fixture = load_json(&dir.join(&file));
        assert_fixture_envelope(&file, &fixture);
        if file == "sources.json" {
            assert_sources_fixture_contract(&fixture);
        }
        if file == "competitions.json" {
            assert_competitions_fixture_contract(&fixture);
        }
        if file == "version.json" {
            assert_version_fixture_contract(&fixture);
        }
        if file.starts_with("block-") {
            assert_block_fixture_contract(&file, &fixture);
        }
    }
}

fn assert_block_fixture_contract(file: &str, fixture: &Value) {
    let block = fixture["block"]
        .as_object()
        .unwrap_or_else(|| panic!("{file} must carry a block object"));
    assert!(
        block.contains_key("error_block_reason"),
        "{file} block must include nullable error_block_reason"
    );
    if block.get("kind").and_then(Value::as_str) == Some("error_block") {
        assert!(
            block["error_block_reason"]
                .as_str()
                .is_some_and(|reason| !reason.is_empty()),
            "{file} error block must carry a non-empty error_block_reason"
        );
        assert!(
            fixture["competition"].is_null(),
            "{file} error block must not carry a stale competition"
        );
        assert!(
            fixture["stale_branch"].is_null(),
            "{file} error block must not carry a stale branch"
        );
    } else {
        assert!(
            block["error_block_reason"].is_null(),
            "{file} non-error block must carry error_block_reason: null"
        );
    }
    let events = fixture["event_details"]
        .as_array()
        .unwrap_or_else(|| panic!("{file} must carry an event_details array"));
    for event in events {
        let object = event
            .as_object()
            .unwrap_or_else(|| panic!("{file} event detail must be an object"));
        for field in ["child_header_hex", "child_nbits"] {
            assert!(
                object.contains_key(field),
                "{file} event detail must include nullable {field}"
            );
        }
        if let Some(header) = event["child_header_hex"].as_str() {
            assert_lower_hex(header, 160, file, "child_header_hex");
        } else {
            assert!(
                event["child_header_hex"].is_null(),
                "{file} child_header_hex must be null or a string"
            );
        }
        if let Some(nbits) = event["child_nbits"].as_str() {
            assert_lower_hex(nbits, 8, file, "child_nbits");
        } else {
            assert!(
                event["child_nbits"].is_null(),
                "{file} child_nbits must be null or a string"
            );
        }
    }
}

fn assert_lower_hex(value: &str, len: usize, file: &str, field: &str) {
    assert_eq!(
        value.len(),
        len,
        "{file} {field} must contain exactly {len} hexadecimal characters"
    );
    assert!(
        value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)),
        "{file} {field} must be lowercase hexadecimal"
    );
}

/// The version fixture must track the workspace release: its top-level
/// version and newest release-note section both pin `CARGO_PKG_VERSION`, the
/// projection's own counters agree with the payload shape, and the whole
/// `version` + `release_notes` projection is deep-equal to what the code
/// actually serves (`mmm_api::version_payload_json`, which parses the
/// embedded `RELEASE_NOTES.md`). A release bump, a notes edit, or a parser
/// change therefore cannot land without regenerating this fixture (and, via
/// the shared manifest walk above, its manifest scenario).
fn assert_version_fixture_contract(fixture: &Value) {
    let workspace_version = env!("CARGO_PKG_VERSION");
    assert_eq!(
        fixture["version"].as_str(),
        Some(workspace_version),
        "version fixture must pin the workspace release version"
    );
    let notes = &fixture["release_notes"];
    let releases = notes["releases"]
        .as_array()
        .expect("version fixture must carry a releases array");
    assert_eq!(
        notes["release_count"].as_u64(),
        Some(releases.len() as u64),
        "release_count must match the releases array"
    );
    let newest = releases
        .first()
        .expect("version fixture must carry at least one release");
    assert_eq!(
        newest["version"].as_str(),
        Some(workspace_version),
        "the newest release-note section must be the released version"
    );
    assert_eq!(
        newest["item_count"].as_u64(),
        Some(newest["items"].as_array().expect("items array").len() as u64),
        "item_count must match the items array"
    );

    // Deep equality against the served projection: the fixture is a capture of
    // /api/v1/version minus the envelope, so `version` and the entire
    // `release_notes` subtree (dates and item prose included) must match the
    // code's own payload byte for byte.
    let served = mmm_api::version_payload_json();
    assert_eq!(
        fixture["version"], served["version"],
        "fixture version must equal the served payload"
    );
    assert_eq!(
        fixture["release_notes"], served["release_notes"],
        "fixture release_notes must equal the served projection; regenerate \
         the fixture from /api/v1/version after editing RELEASE_NOTES.md"
    );
}

fn fixture_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/api")
}

fn load_json(path: &Path) -> Value {
    let body = fs::read_to_string(path)
        .unwrap_or_else(|err| panic!("failed to read {}: {err}", path.display()));
    serde_json::from_str(&body)
        .unwrap_or_else(|err| panic!("failed to parse {} as JSON: {err}", path.display()))
}

fn listed_fixture_files(manifest: &Value) -> BTreeSet<String> {
    manifest["fixtures"]
        .as_array()
        .expect("manifest.fixtures must be an array")
        .iter()
        .map(|row| {
            let file = string_field(row, "file").to_owned();
            assert_ne!(file, "manifest.json");
            assert!(!string_field(row, "endpoint_family").is_empty());
            assert!(!string_field(row, "scenario").is_empty());
            file
        })
        .collect()
}

fn actual_fixture_files(dir: &Path) -> BTreeSet<String> {
    fs::read_dir(dir)
        .unwrap_or_else(|err| panic!("failed to read {}: {err}", dir.display()))
        .map(|entry| {
            entry
                .expect("fixture dir entry")
                .file_name()
                .into_string()
                .expect("fixture filename must be UTF-8")
        })
        .filter(|file| file.ends_with(".json") && file != "manifest.json")
        .collect()
}

fn assert_fixture_envelope(file: &str, fixture: &Value) {
    assert!(
        fixture.is_object(),
        "{file} fixture must be a JSON object, not a scalar or array"
    );
    assert_eq!(
        fixture["schema_version"], "v1",
        "{file} must carry the v1 response envelope"
    );
    assert!(
        fixture["generated_at"].as_u64().is_some(),
        "{file} must carry numeric generated_at"
    );
    if file.starts_with("error-") {
        assert!(fixture["error"].is_object(), "{file} must carry error");
    }
}

/// The competitions fixture pins the wire contract the delta view reads: the
/// documented ordering, the nullable delta, and source lists that agree with
/// what the tree and block projections report for the same block.
fn assert_competitions_fixture_contract(fixture: &Value) {
    let competitions = fixture["competitions"]
        .as_array()
        .expect("competitions fixture must carry a competitions array");
    assert!(
        competitions.len() >= 2,
        "competitions fixture must exercise more than one row"
    );

    // Ordering: ascending btc_height, then lexicographic stale_hash. The
    // fixture deliberately carries two stales at one height so the tie-break is
    // pinned, not merely implied.
    let keys = competitions
        .iter()
        .map(|row| {
            let height = row["btc_height"]
                .as_i64()
                .expect("btc_height must be numeric");
            let hash = row["stale_hash"]
                .as_str()
                .expect("stale_hash must be a string")
                .to_owned();
            (height, hash)
        })
        .collect::<Vec<_>>();
    let mut sorted = keys.clone();
    sorted.sort();
    assert_eq!(
        keys, sorted,
        "competitions must be ordered by btc_height then lexicographic stale_hash"
    );
    let duplicate_height = keys
        .windows(2)
        .any(|pair| pair[0].0 == pair[1].0 && pair[0].1 != pair[1].1);
    assert!(
        duplicate_height,
        "competitions fixture must include two stales at one height to pin the tie-break"
    );

    let mut saw_null_delta = false;
    let mut saw_bitcoin_source = false;
    let mut saw_auxpow_only = false;
    for row in competitions {
        // Absence of the key, not a null value: indexing a missing key yields
        // Value::Null, so an is_null() check would also accept a fixture that
        // added "canonical_hash": null and quietly broke the documented shape.
        assert!(
            !row.as_object()
                .expect("competition row is an object")
                .contains_key("canonical_hash"),
            "canonical_hash is deliberately not part of this payload"
        );
        assert!(
            row["stale_header_time"].as_i64().is_some(),
            "stale_header_time must be numeric"
        );
        for side in ["stale_bitcoin_miner_pool", "canonical_bitcoin_miner_pool"] {
            assert!(row[side].is_object(), "{side} must be a pool object");
            assert!(row[side]["known"].is_boolean(), "{side}.known must be bool");
        }
        let delta = &row["header_time_delta_s"];
        assert!(
            delta.is_null() || delta.as_i64().is_some(),
            "header_time_delta_s must be null or numeric"
        );
        saw_null_delta |= delta.is_null();

        let sources = row["sources"]
            .as_array()
            .expect("sources must be an array")
            .iter()
            .map(|value| value.as_str().expect("source code is a string").to_owned())
            .collect::<Vec<_>>();
        // Deliberately NOT asserting a non-empty list: a competition whose only
        // proof has been revoked is still a competition, and
        // competitions_sources_match_block_evidence_semantics pins that it is
        // served with sources: []. Requiring evidence here would contradict the
        // runtime contract and teach clients to assume something untrue.
        let mut sorted_sources = sources.clone();
        sorted_sources.sort();
        sorted_sources.dedup();
        assert_eq!(sources, sorted_sources, "sources must be sorted and unique");
        if sources
            .iter()
            .any(|code| code == "live-chaintip:bitcoin:core")
        {
            saw_bitcoin_source = true;
        } else if !sources.is_empty() {
            saw_auxpow_only = true;
        }
    }
    assert!(
        saw_null_delta,
        "fixture must pin a null header_time_delta_s (i32-overflow case)"
    );
    assert!(
        saw_bitcoin_source,
        "fixture must pin the synthetic live-chaintip:bitcoin:core source"
    );
    assert!(
        saw_auxpow_only,
        "fixture must pin an AuxPoW-only source list so the synthetic source is not assumed"
    );
}

fn assert_sources_fixture_contract(fixture: &Value) {
    let sources = fixture["sources"]
        .as_array()
        .expect("sources fixture must carry a sources array");
    for (code, mode) in [
        ("auxpow:lyncoin", "historical"),
        ("auxpow:sixeleven", "historical"),
        ("auxpow:vcash", "partial"),
        ("auxpow:doichain", "surveyed"),
        ("auxpow:bitcoin-stash", "catalogued"),
    ] {
        let source = sources
            .iter()
            .find(|source| source["code"] == code)
            .unwrap_or_else(|| panic!("sources fixture must include {code}"));
        assert_eq!(source["sync"]["mode"], mode);
        assert_eq!(source["sync"]["state"], mode);
        for field in [
            "progress_height",
            "progress_updated_at",
            "target_height",
            "latest_evidence_at",
            "error_code",
            "error_height",
        ] {
            assert!(
                source["sync"][field].is_null(),
                "{code} sync.{field} must be null"
            );
        }
    }
    for (code, expected_events, expected_last_seen) in [
        ("auxpow:vcash", 68, 1_659_809_588),
        ("auxpow:lyncoin", 11, 1_721_667_253),
        ("auxpow:sixeleven", 7, 1_536_793_971),
    ] {
        let source = sources
            .iter()
            .find(|source| source["code"] == code)
            .unwrap_or_else(|| panic!("sources fixture must include recovered {code}"));
        assert_eq!(source["counts"]["events"], expected_events);
        assert_eq!(source["counts"]["canonical"], expected_events);
        assert_eq!(source["counts"]["stale"], 0);
        assert_eq!(source["counts"]["error_block"], 0);
        assert_eq!(source["status"], "stale");
        assert_eq!(source["last_seen_at"], expected_last_seen);
    }
}

fn string_field<'a>(value: &'a Value, key: &str) -> &'a str {
    value[key]
        .as_str()
        .unwrap_or_else(|| panic!("{key} must be a string"))
}
