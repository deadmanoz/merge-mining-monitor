use std::fs;

use bitcoin::block::Header;
use mmm_capture::auxpow::{ParsedNamecoinBlock, parse_namecoin_block};
use mmm_capture::capture::{ClassificationProof, ResolvedPoolAttributions, build_event_payload};
use mmm_capture::test_support::fixture_path;

#[test]
fn parses_real_syscoin_auxpow_fixtures() {
    for fixture in [
        AuxpowFixture {
            name: "1973",
            height: 1973,
            child_hash: "324c4b68958b3d392e4d7d6d0383e647206aa3a8243613247be4b17c16518d56",
            parent_hash: "000000000000000eb4b0724affd3cd367dbf70ad8245a6230471bb7f03352e9a",
            expected_child_height: Some(1973),
        },
        AuxpowFixture {
            name: "2248408",
            height: 2_248_408,
            child_hash: "6ddbfb6239223ad6aa9dac486a8062803de53d8dbbf0b921376c5e1a2cdd42ca",
            parent_hash: "000000000000000000035d693d77fd0c0912873aca0b7235ce91fd96bfeefab2",
            expected_child_height: None,
        },
    ] {
        let raw = load_raw_syscoin_fixture(fixture.name);
        let parsed = parse_namecoin_block(&raw).unwrap();
        let ParsedNamecoinBlock::Auxpow(parsed) = parsed else {
            panic!("{} should parse as AuxPoW", fixture.name);
        };

        assert_eq!(parsed.child_header.hash().to_string(), fixture.child_hash);
        assert_eq!(parsed.parent_header.hash().to_string(), fixture.parent_hash);
        assert_eq!(parsed.child_height, fixture.expected_child_height);
        assert_eq!(parsed.proof.chain_branch.hashes.len(), 4);
        assert_eq!(parsed.proof.chain_branch.index, 14);
        assert_eq!(
            parsed.child_header.consensus_bytes(),
            &raw[..Header::SIZE],
            "child header consensus serialization must round-trip fixture bytes"
        );
        assert_eq!(
            parsed.parent_header.consensus_bytes(),
            &parsed.auxpow_bytes[parsed.auxpow_bytes.len() - Header::SIZE..],
            "parent header consensus serialization must round-trip fixture bytes"
        );

        let event = build_event_payload(
            &parsed,
            Some(fixture.height),
            ResolvedPoolAttributions::default(),
            ClassificationProof::default(),
            1_700_000_000,
        )
        .unwrap();
        assert_eq!(event.child_height, fixture.height);
        assert_eq!(event.pow_validates_child_target, Some(true));
    }
}

#[test]
fn parses_real_pre_activation_syscoin_block_as_non_auxpow() {
    let raw = load_raw_syscoin_fixture("1972");
    let parsed = parse_namecoin_block(&raw).unwrap();
    let ParsedNamecoinBlock::NonAuxpow(header) = parsed else {
        panic!("1972 fixture should parse as non-AuxPoW");
    };

    assert_eq!(
        header.hash().to_string(),
        "00000008c161f0b7c4e4b1cb515b39ae0179130e6016fde62695acbe1cb243d1"
    );
}

#[derive(Debug, Clone, Copy)]
struct AuxpowFixture {
    name: &'static str,
    height: i32,
    child_hash: &'static str,
    parent_hash: &'static str,
    expected_child_height: Option<i32>,
}

fn load_raw_syscoin_fixture(name: &str) -> Vec<u8> {
    let path = fixture_path(&format!("syscoin/{name}"), "bin");
    fs::read(&path).unwrap_or_else(|err| panic!("failed to read fixture {name}: {err}"))
}
