//! Fixture-driven Qbit extended-header parser and verification tests.
//!
//! Controls are mirrored verbatim from merge-mining-research `e3dc6d6`
//! (`tests/fixtures/qbit_controls.json` / `qbit_synthetic_parent.json`),
//! captured against Qbit revision `70fea84`. The four real mainnet proofs
//! are the byte-order guard: they cannot decode if the display-order /
//! internal-order fold convention is wrong. Mutation tests port the
//! reference suite's merkle-root repair technique so each negative is
//! rejected at the intended gate, asserted on that gate's error text.

use super::*;
use bitcoin::block::Version;
use serde::Deserialize;

#[derive(Deserialize)]
struct QbitControlsFile {
    controls: Vec<QbitControl>,
}

#[derive(Deserialize)]
struct QbitControl {
    height: u32,
    hash: String,
    header_hex: String,
    parent_hash: String,
    parent_self_pow: bool,
}

#[derive(Deserialize)]
struct QbitSyntheticFixture {
    height: u32,
    hash: String,
    header_hex: String,
    parent_hash: String,
}

fn controls() -> Vec<QbitControl> {
    serde_json::from_str::<QbitControlsFile>(include_str!(
        "../../../../fixtures/qbit/qbit_controls.json"
    ))
    .expect("parse qbit controls fixture")
    .controls
}

/// The one control whose embedded parent is a full-difficulty Bitcoin block
/// (child height 78058, BTC 966017).
fn positive_control() -> QbitControl {
    controls()
        .into_iter()
        .find(|control| control.parent_self_pow)
        .expect("positive control present")
}

fn positive_raw() -> Vec<u8> {
    hex::decode(positive_control().header_hex).expect("decode positive control hex")
}

fn parse_auxpow(raw: &[u8], height: u32) -> ParsedQbitAuxpow {
    match parse_qbit_extended_header(raw, height).expect("parse Qbit extended header") {
        ParsedQbitBlock::Auxpow(parsed) => *parsed,
        ParsedQbitBlock::Direct(_) => panic!("expected a merged proof"),
    }
}

/// The full error chain ({:#}) so per-gate assertions see context strings.
fn parse_err(raw: &[u8], height: u32) -> String {
    format!(
        "{:#}",
        parse_qbit_extended_header(raw, height).expect_err("mutation must be rejected")
    )
}

#[test]
fn controls_decode_and_reproduce_fixture_parent_pow() {
    for control in controls() {
        let raw = hex::decode(&control.header_hex).expect("decode control hex");
        let parsed = parse_auxpow(&raw, control.height);
        assert_eq!(parsed.child_header.hash().to_string(), control.hash);
        assert_eq!(parsed.parent_header.hash().to_string(), control.parent_hash);
        // Exactly the fixture-declared parent_self_pow values reproduce: true
        // only for the positive control, false for the genuine lower-work
        // parents (which are still VALID proofs against the child target).
        assert_eq!(
            validates_target(parsed.parent_header.hash(), parsed.parent_header.bits()),
            control.parent_self_pow,
            "parent_self_pow mismatch at child height {}",
            control.height
        );
        // auxpow_bytes is exactly the region after the pure child header.
        assert_eq!(parsed.auxpow_bytes.len(), raw.len() - 80);
        assert_eq!(parsed.auxpow_bytes.as_slice(), &raw[80..]);
    }
}

#[test]
fn positive_control_embeds_bitcoin_block_966017() {
    let control = positive_control();
    assert_eq!(control.height, 78_058);
    let parsed = parse_auxpow(&positive_raw(), control.height);
    // Hardcoded display hex pins byte order independently of the fixture.
    assert_eq!(
        parsed.parent_header.hash().to_string(),
        "0000000000000000000099c87c5d482e3aa11824a22c101c5f0a0f1b96d987a5"
    );
    assert_eq!(
        parse_bip34_height(&parsed.parent_coinbase_script),
        Some(966_017)
    );
}

#[test]
fn synthetic_parent_is_valid_proof_but_not_bitcoin_placement_evidence() {
    let fixture: QbitSyntheticFixture = serde_json::from_str(include_str!(
        "../../../../fixtures/qbit/qbit_synthetic_parent.json"
    ))
    .expect("parse synthetic parent fixture");
    let raw = hex::decode(&fixture.header_hex).expect("decode synthetic hex");
    let parsed = parse_auxpow(&raw, fixture.height);
    assert_eq!(parsed.child_header.hash().to_string(), fixture.hash);
    assert_eq!(parsed.parent_header.hash().to_string(), fixture.parent_hash);
    // The native synthetic parent coinbase opens with the 44-byte
    // merge-mining commitment, which must NOT be misread as a BIP34 height
    // push: this parent is not Bitcoin-placement evidence.
    assert_eq!(parse_bip34_height(&parsed.parent_coinbase_script), None);
}

#[test]
fn stored_proof_round_trips_and_classic_decoder_rejects_qbit_bytes() {
    let control = positive_control();
    let parsed = parse_auxpow(&positive_raw(), control.height);

    let detail = decode_qbit_auxpow_proof(&parsed.auxpow_bytes).expect("stored Qbit proof decodes");
    assert_eq!(detail.parent_header_hash, parsed.parent_header.hash());
    assert_eq!(
        detail.slot_index,
        u32::try_from(parsed.chain_branch.index).unwrap()
    );
    assert_eq!(detail.coinbase_branch.index, 0);
    assert_eq!(
        detail.coinbase_branch.siblings,
        parsed.coinbase_branch.hashes
    );
    assert_eq!(detail.blockchain_branch.index, detail.slot_index);
    assert_eq!(
        detail.blockchain_branch.siblings,
        parsed.chain_branch.hashes
    );

    // Formats are not interchangeable: the classic CAuxPow decoder must not
    // accept Qbit bytes, so API format selection stays explicit.
    assert_eq!(decode_auxpow_proof(&parsed.auxpow_bytes), None);
}

#[test]
fn qbit_decoder_rejects_classic_stored_proof_bytes() {
    // The reciprocal mutual-rejection direction, against a real classic
    // fixture: a Qbit decoder that accepted classic CAuxPow bytes would
    // otherwise pass every Qbit-side test.
    let raw = hex::decode(
        include_str!("../../../../fixtures/fractal/fb-1342257-getblockheader-auxpow.hex").trim(),
    )
    .expect("decode Fractal fixture hex");
    let classic = parse_auxpow_header_blob(&raw).expect("parse classic Fractal blob");
    assert!(decode_auxpow_proof(&classic.auxpow_bytes).is_some());
    assert_eq!(decode_qbit_auxpow_proof(&classic.auxpow_bytes), None);
}

/// Wire offsets of a Qbit extended header's AuxPoW regions, walked with the
/// production reader so mutations target exact byte positions.
struct QbitOffsets {
    tx_end: usize,
    parent_branch: MerkleBranch,
    parent_count_pos: usize,
    parent_index_pos: usize,
    chain_count_pos: usize,
    chain_branch: MerkleBranch,
    chain_index_pos: usize,
    parent_header_pos: usize,
}

fn offsets(raw: &[u8]) -> QbitOffsets {
    let mut reader = Reader::new(raw);
    reader.skip(80).unwrap();
    let (_, consumed) = deserialize_partial::<Transaction>(reader.remaining_slice()).unwrap();
    reader.skip(consumed).unwrap();
    let tx_end = reader.position();
    let parent_count_pos = reader.position();
    let parent_hashes = read_branch_hashes(&mut reader);
    let parent_index_pos = reader.position();
    let parent_index = reader.read_i32().unwrap();
    let chain_count_pos = reader.position();
    let chain_hashes = read_branch_hashes(&mut reader);
    let chain_index_pos = reader.position();
    let chain_index = reader.read_i32().unwrap();
    let parent_header_pos = reader.position();
    QbitOffsets {
        tx_end,
        parent_branch: MerkleBranch {
            hashes: parent_hashes,
            index: parent_index,
        },
        parent_count_pos,
        parent_index_pos,
        chain_count_pos,
        chain_branch: MerkleBranch {
            hashes: chain_hashes,
            index: chain_index,
        },
        chain_index_pos,
        parent_header_pos,
    }
}

fn read_branch_hashes(reader: &mut Reader<'_>) -> Vec<TxMerkleNode> {
    let count = reader.read_u8().unwrap() as usize;
    (0..count)
        .map(|_| TxMerkleNode::from_byte_array(reader.read_array::<32>().unwrap()))
        .collect()
}

fn marker_pos(raw: &[u8], tx_end: usize) -> usize {
    (80..tx_end - 4)
        .find(|&pos| raw[pos..pos + 4] == AUXPOW_MAGIC)
        .expect("marker present in the coinbase region")
}

/// Recompute the parent-header merkle root after a coinbase mutation (the
/// reference suite's `mutate_commitment` technique): the coinbase txid
/// changes with the scriptSig, so without this repair every commitment
/// mutation would fail at parent Merkle inclusion instead of the gate under
/// test.
fn repair_parent_merkle_root(raw: &mut [u8]) {
    let off = offsets(raw);
    let txid = sha256d::Hash::hash(&raw[80..off.tx_end]).to_byte_array();
    let root = fold_merkle_branch(txid, &off.parent_branch);
    raw[off.parent_header_pos + 36..off.parent_header_pos + 68].copy_from_slice(&root);
}

fn mutate_commitment(kind: &str) -> Vec<u8> {
    let mut raw = positive_raw();
    let off = offsets(&raw);
    let marker = marker_pos(&raw, off.tx_end);
    match kind {
        // Wire-order commitment: classic's convention, rejected by Qbit.
        "order" => raw[marker + 4..marker + 36].reverse(),
        "size" => raw[marker + 36] ^= 1,
        "nonce" => raw[marker + 40] ^= 1,
        other => panic!("unknown mutation {other}"),
    }
    repair_parent_merkle_root(&mut raw);
    raw
}

#[test]
fn commitment_rules_are_enforced_per_gate() {
    for (kind, reason) in [
        ("order", "display-order chain commitment missing"),
        ("size", "tree size"),
        ("nonce", "chain slot"),
    ] {
        let err = parse_err(&mutate_commitment(kind), 78_058);
        assert!(err.contains(reason), "{kind}: {err}");
    }
}

#[test]
fn parent_hash_must_meet_the_child_target() {
    // The four controls cannot prove this gate: a lower-work parent is still
    // a VALID proof, so deleting the comparison would leave every fixture
    // passing. Give the positive control a well-formed, within-pow-limit
    // child target of 1 (stricter than any real parent hash), repair the
    // chain commitment and parent merkle root for the new child header, and
    // require rejection at the parent-versus-child-target gate specifically.
    let mut raw = positive_raw();
    raw[72..76].copy_from_slice(&0x0300_0001u32.to_le_bytes());
    let off = offsets(&raw);
    let marker = marker_pos(&raw, off.tx_end);
    let leaf = sha256d::Hash::hash(&raw[..80]).to_byte_array();
    let mut display_root = fold_merkle_branch(leaf, &off.chain_branch);
    display_root.reverse();
    raw[marker + 4..marker + 36].copy_from_slice(&display_root);
    repair_parent_merkle_root(&mut raw);

    let err = parse_err(&raw, 78_058);
    assert!(err.contains("fails child header target"), "{err}");
}

#[test]
fn malformed_and_over_limit_child_nbits_fail_target_decoding() {
    // The earlier target-DECODING gate, distinguished from the
    // parent-versus-child-target comparison above: sign bit, over the
    // pow limit, and a mantissa that shifts to a zero target.
    for bits in [0x1D80_FFFFu32, 0x2000_FFFF, 0x0100_FFFF] {
        let mut raw = positive_raw();
        raw[72..76].copy_from_slice(&bits.to_le_bytes());
        let err = parse_err(&raw, 78_058);
        assert!(
            err.contains("invalid Qbit header target"),
            "{bits:08x}: {err}"
        );
    }
}

/// A deterministic non-merged header whose hash meets its own (easy,
/// within-pow-limit) target: grind the first satisfying nonce.
fn ground_direct_header() -> Header {
    let mut header = Header {
        version: Version::from_consensus(4),
        prev_blockhash: BlockHash::all_zeros(),
        merkle_root: TxMerkleNode::all_zeros(),
        time: 1_757_000_000,
        bits: CompactTarget::from_consensus(0x1f00_ffff),
        nonce: 0,
    };
    while !Target::from_compact(header.bits).is_met_by(header.block_hash()) {
        header.nonce += 1;
    }
    header
}

#[test]
fn direct_header_parses_as_behaviour_with_own_pow() {
    let header = ground_direct_header();
    let raw = serialize(&header);
    match parse_qbit_extended_header(&raw, 1).expect("parse direct header") {
        ParsedQbitBlock::Direct(parsed) => {
            assert_eq!(parsed.hash(), header.block_hash());
            assert_eq!(parsed.bits(), header.bits);
            assert_eq!(parsed.time(), header.time);
        }
        ParsedQbitBlock::Auxpow(_) => panic!("version 4 is non-merged"),
    }
}

#[test]
fn direct_header_rejects_trailing_bytes_and_failed_own_pow() {
    let header = ground_direct_header();
    let mut trailing = serialize(&header);
    trailing.push(0x00);
    assert!(parse_err(&trailing, 1).contains("trailing bytes on direct Qbit header"));

    // Grind the first FAILING nonce (deterministic; nonce 0 in practice).
    let mut failing = header;
    failing.nonce = 0;
    while Target::from_compact(failing.bits).is_met_by(failing.block_hash()) {
        failing.nonce += 1;
    }
    assert!(parse_err(&serialize(&failing), 1).contains("fails child target"));
}

#[test]
fn version_gate_matrix_matches_the_reference() {
    assert!(qbit_version_gate(0x2005_E100, 1).unwrap());
    assert!(!qbit_version_gate(0x3FFF_E004, 1).unwrap());
    assert!(!qbit_version_gate(4, 1).unwrap());
    assert!(!qbit_version_gate(1, 0).unwrap());
    for version in [
        0x4000_0100u32,
        0x0000_0100,
        0x2005_E300,
        0x2006_0100,
        1,
        3,
        0xA005_E100,
    ] {
        assert!(
            qbit_version_gate(version, 78_058).is_err(),
            "{version:#010x} must be rejected"
        );
    }
}

#[test]
fn spurious_classic_hash_block_field_is_rejected() {
    // Classic CAuxPow carries a 32-byte hashBlock between the coinbase tx
    // and the parent branch; Qbit's wire format has no such field.
    let raw = positive_raw();
    let off = offsets(&raw);
    let mut spliced = raw[..off.tx_end].to_vec();
    spliced.extend_from_slice(&[0u8; 32]);
    spliced.extend_from_slice(&raw[off.tx_end..]);
    assert!(parse_qbit_extended_header(&spliced, 78_058).is_err());
}

#[test]
fn wrong_chain_id_and_shapeless_auxpow_bit_versions_are_rejected() {
    for (version, reason) in [
        // Chain-id bits -> 48, shape and AuxPoW flag preserved.
        (0x2006_0100u32, "chain id"),
        // AuxPoW bit set without the 001 top-bit shape.
        (0x0000_0100, "non-canonical"),
        (0x4000_0100, "non-canonical"),
    ] {
        let mut raw = positive_raw();
        raw[0..4].copy_from_slice(&version.to_le_bytes());
        let err = parse_err(&raw, 78_058);
        assert!(err.contains(reason), "{version:#010x}: {err}");
    }
}

#[test]
fn out_of_bounds_branch_indices_are_rejected() {
    let raw = positive_raw();
    let off = offsets(&raw);
    for (pos, value, reason) in [
        (off.parent_index_pos, -1i32, "coinbase branch index"),
        (off.parent_index_pos, 1, "coinbase branch index"),
        (off.chain_index_pos, -1, "chain index"),
        (off.chain_index_pos, 1 << 30, "chain index"),
    ] {
        let mut mutated = raw.clone();
        mutated[pos..pos + 4].copy_from_slice(&value.to_le_bytes());
        let err = parse_err(&mutated, 78_058);
        assert!(err.contains(reason), "{value}@{pos}: {err}");
    }
}

#[test]
fn oversized_branch_counts_exceed_the_limit() {
    let raw = positive_raw();
    let off = offsets(&raw);
    // 32 > the parent cap of 31; 31 > the chain cap of 30; a multi-byte
    // CompactSize prefix (0xfd) is over every cap either way.
    for (pos, count) in [
        (off.parent_count_pos, 32u8),
        (off.chain_count_pos, 31),
        (off.parent_count_pos, 0xfd),
    ] {
        let mut mutated = raw.clone();
        mutated[pos] = count;
        let err = parse_err(&mutated, 78_058);
        assert!(err.contains("exceeds limit"), "{count}@{pos}: {err}");
    }
}

#[test]
fn exact_header_rejects_trailing_bytes_and_every_truncation() {
    let raw = positive_raw();
    let mut trailing = raw.clone();
    trailing.push(0x00);
    assert!(parse_err(&trailing, 78_058).contains("trailing bytes on Qbit AuxPoW header"));

    // Bounded-reader sweep: every strict prefix must fail.
    for end in 0..raw.len() {
        assert!(
            parse_qbit_extended_header(&raw[..end], 78_058).is_err(),
            "strict prefix of length {end} must be rejected"
        );
    }
}

#[test]
fn witness_encoded_coinbase_is_rejected() {
    // BIP144 re-encoding of the same coinbase: marker+flag after the tx
    // version, one empty witness stack before the locktime.
    let raw = positive_raw();
    let off = offsets(&raw);
    let mut encoded = raw[..84].to_vec();
    encoded.extend_from_slice(&[0x00, 0x01]);
    encoded.extend_from_slice(&raw[84..off.tx_end - 4]);
    encoded.push(0x00);
    encoded.extend_from_slice(&raw[off.tx_end - 4..]);
    assert!(parse_err(&encoded, 78_058).contains("non-witness encoding"));
}

/// The producer reads whole blocks, not exact extended headers. The prefix
/// reader must locate the exact header/proof region inside a full block, and
/// the parser must then accept that prefix while still rejecting the block.
#[test]
fn prefix_reader_splits_the_extended_header_out_of_a_full_block() {
    for control in controls() {
        let exact = hex::decode(&control.header_hex).expect("decode control hex");
        // A plausible block body: a one-transaction tx vector after the proof.
        let mut full_block = exact.clone();
        full_block.extend_from_slice(&[0x01, 0xde, 0xad, 0xbe, 0xef]);

        // The whole block is not an exact extended header.
        let err = parse_err(&full_block, control.height);
        assert!(
            err.contains("trailing bytes"),
            "full block must be rejected as an exact header: {err}"
        );

        let prefix = qbit_extended_header_prefix(&full_block, control.height)
            .expect("prefix reader locates the extended header");
        assert_eq!(prefix, exact.as_slice());
        let parsed = parse_auxpow(prefix, control.height);
        assert_eq!(parsed.child_header.hash().to_string(), control.hash);
        assert_eq!(parsed.parent_header.hash().to_string(), control.parent_hash);
    }
}

/// A directly mined block's extended header is just its 80-byte pure header,
/// so the prefix stops there and the block body is never reinterpreted.
#[test]
fn prefix_reader_stops_at_the_header_for_a_direct_block() {
    let header_only = serialize(&ground_direct_header());
    let mut raw = header_only.clone();
    raw.extend_from_slice(&[0x01, 0x00, 0x00, 0x00]);
    let prefix = qbit_extended_header_prefix(&raw, 1).expect("direct prefix is the header");
    assert_eq!(prefix, header_only.as_slice());
    assert!(matches!(
        parse_qbit_extended_header(prefix, 1).expect("direct header parses"),
        ParsedQbitBlock::Direct(_)
    ));
}
