//! Fixture-driven parser and verification tests.

use super::*;
use bitcoin::CompactTarget;
use bitcoin::block::Version;
use bitcoin::consensus::{deserialize, serialize};
use bitcoin::hashes::sha256d;
use bitcoin::{Amount, Block};

use crate::child_payout::{FRACTAL_CHILD_REWARD_PARAMS, child_output_addresses};

fn parse_fractal_auxpow_header_blob() -> (Vec<u8>, ParsedAuxpowBlock) {
    let raw = hex::decode(
        include_str!("../../../../fixtures/fractal/fb-1342257-getblockheader-auxpow.hex").trim(),
    )
    .expect("decode Fractal fixture hex");
    let parsed = parse_auxpow_header_blob(&raw).expect("parse header+CAuxPow blob");
    (raw, parsed)
}

#[test]
fn parse_auxpow_header_blob_extracts_fractal_parent_header_and_tag() {
    // Real Fractal block (FB height 1,342,257, child version 0x20240100)
    // fetched via `getblockheader <hash> false true`: a [child header]
    // [CAuxPow] blob with NO trailing child transaction vector. Its embedded
    // BTC parent header is the chronologically-novel stale at BTC 928,455.
    let (_, parsed) = parse_fractal_auxpow_header_blob();

    assert_eq!(
        parsed.parent_header.hash().to_string(),
        "00000000000000000000398ead6702380e49e5e872d194a35cbd64c066a3d47f",
    );
    // No child tx vector in the blob -> child-coinbase fields are absent.
    assert_eq!(parsed.child_height, None);
    assert_eq!(parsed.child_coinbase_txid, None);
    assert_eq!(parsed.child_coinbase_script, None);
    assert!(parsed.child_coinbase_outputs.is_empty());
    // The BTC parent coinbase IS recovered (carries the pool scriptSig).
    assert!(!parsed.parent_coinbase_script.is_empty());

    assert_eq!(
        extract_coinbase_tag(&parsed.parent_coinbase_script),
        Some("\"_Di| MARA Made in USA |v04 )\\|S[".to_owned())
    );
}

#[test]
fn parse_and_attach_full_fractal_child_block_reward_identity() {
    // Real Fractal block (FB height 1,342,257) fetched as full raw child block
    // bytes. The public Mempool API for the same block reports coinbaseAddress
    // bc1qg4l3fvmsrnzuspuntv9yswwh7s58n08a59y3l7; the producer derives that
    // address from block bytes, not from explorer labels.
    let child_raw = include_bytes!("../../../../fixtures/fractal/fb-1342257-getblock-0.bin");
    let (_, mut parsed) = parse_fractal_auxpow_header_blob();
    let child = parse_child_block_coinbase(child_raw).expect("parse full child block coinbase");

    assert_eq!(
        child.child_header.hash().to_string(),
        "59801838b43b2d38a87b1d295c7c535b1d6cced64fe5cb89ddeb7ff8c92e7e0e",
    );
    assert_eq!(child.child_height, Some(1_342_257));
    assert_eq!(
        child.child_coinbase_txid.to_string(),
        "d284c656bee2c3b0d51a8b5613a88390459f1676d2a72ec45afa5dddac256d0c",
    );
    assert_eq!(
        child.child_coinbase_script,
        hex::decode("03317b1400").unwrap()
    );
    assert_eq!(
        child_output_addresses(&child.child_coinbase_outputs, FRACTAL_CHILD_REWARD_PARAMS),
        ["bc1qg4l3fvmsrnzuspuntv9yswwh7s58n08a59y3l7"]
    );

    attach_child_block_coinbase(&mut parsed, child).expect("attach child reward identity");
    assert_eq!(parsed.child_height, Some(1_342_257));
    assert_eq!(
        parsed.child_coinbase_txid.unwrap().to_string(),
        "d284c656bee2c3b0d51a8b5613a88390459f1676d2a72ec45afa5dddac256d0c",
    );
    assert_eq!(
        child_output_addresses(&parsed.child_coinbase_outputs, FRACTAL_CHILD_REWARD_PARAMS),
        ["bc1qg4l3fvmsrnzuspuntv9yswwh7s58n08a59y3l7"]
    );
    assert!(!parsed.auxpow_bytes.is_empty());
}

#[test]
fn parse_full_fractal_child_block_rejects_tampered_merkle_root() {
    let raw = include_bytes!("../../../../fixtures/fractal/fb-1342257-getblock-0.bin");
    let mut block: Block = deserialize(raw).expect("deserialize fixture block");
    block.txdata[0].output[0].value =
        Amount::from_sat(block.txdata[0].output[0].value.to_sat() + 1);

    let tampered = serialize(&block);
    let err = parse_child_block_coinbase(&tampered).unwrap_err();
    assert!(
        err.to_string()
            .contains("child block transaction merkle root does not match header")
    );
}

#[test]
fn parse_auxpow_header_blob_rejects_non_auxpow_header() {
    // An 80-byte header with no AuxPoW version bit is not a valid input.
    let mut header = [0u8; 80];
    header[0] = 0x01; // version=1, no VERSION_AUXPOW bit
    assert!(parse_auxpow_header_blob(&header).is_err());
}

#[test]
fn decode_aux_marker_reads_fields_in_display_order() {
    // Synthetic scriptSig: <prefix> || fabe6d6d || <32B root> || <size LE>
    // || <nonce LE> || <trailing>. Format modeled on the merged-mining spec's
    // Namecoin 823506 walkthrough (reference only, not test data).
    let root: [u8; 32] = [
        0x5a, 0x9c, 0x01, 0x98, 0xab, 0xca, 0x4e, 0x8c, 0x94, 0xa1, 0xce, 0x47, 0xb0, 0xf0, 0x9c,
        0xf8, 0x43, 0xe3, 0x3a, 0x6a, 0x4d, 0x3a, 0xaf, 0x6b, 0x92, 0x40, 0xb0, 0x80, 0x61, 0xd6,
        0x8a, 0x42,
    ];
    let mut script = vec![0x03, 0xab, 0x77, 0x0e, 0x19]; // arbitrary prefix
    script.extend_from_slice(&[0xfa, 0xbe, 0x6d, 0x6d]);
    script.extend_from_slice(&root);
    script.extend_from_slice(&16u32.to_le_bytes());
    script.extend_from_slice(&0u32.to_le_bytes());
    script.extend_from_slice(&[0x00, 0x00, 0xe6, 0x6c]); // pool extras

    let marker = decode_aux_marker(&script).expect("marker decodes");
    assert_eq!(marker.merkle_size, 16);
    assert_eq!(marker.merkle_nonce, 0);
    // Non-self-referential: the API string is the REVERSE of the inserted
    // wire bytes (the locked reversed/display hash order), not the raw hex.
    let mut reversed = root;
    reversed.reverse();
    assert_eq!(marker.aux_merkle_root.to_string(), hex::encode(reversed));
}

#[test]
fn read_merkle_branch_rejects_oversized_count_before_allocation() {
    // A CompactSize declaring ~4 billion entries with no following bytes must
    // error (count bounded against remaining), not attempt a ~128 GB
    // allocation. Guards the /block read path against a corrupt stored blob.
    let mut buf = vec![0xfe];
    buf.extend_from_slice(&0xffff_fff0u32.to_le_bytes());
    let mut reader = Reader::new(&buf);
    assert!(read_merkle_branch(&mut reader).is_err());
}

#[test]
fn decode_aux_marker_none_paths() {
    assert!(decode_aux_marker(&[0x01, 0x02, 0x03]).is_none()); // no magic
    let mut truncated = vec![0xfa, 0xbe, 0x6d, 0x6d];
    truncated.extend_from_slice(&[0u8; 10]); // magic but truncated after it
    assert!(decode_aux_marker(&truncated).is_none());
    assert!(decode_aux_marker(&[]).is_none()); // RSK-shaped: no coinbase
}

#[test]
fn extract_coinbase_tag_matches_frontend_ascii_rules() {
    assert_eq!(
        extract_coinbase_tag(b"\x00  ABCD  \xff  EFGH  \x01xyz"),
        Some("ABCD EFGH".to_owned())
    );
    assert_eq!(
        extract_coinbase_tag(b"\x00tag3\xff"),
        Some("tag3".to_owned())
    );
    assert_eq!(extract_coinbase_tag(b"\x00abc\xff\x01"), None);
}

#[test]
fn auxpow_blob_decoders_read_fractal_ca_auxpow_and_reject_full_blob() {
    let (raw, parsed) = parse_fractal_auxpow_header_blob();

    // The stored CAuxPow-only blob is `parsed.auxpow_bytes`.
    let summary = auxpow_blob_summary(&parsed.auxpow_bytes).expect("summary");
    assert_eq!(summary.parent_header_hash, parsed.parent_header.hash());
    assert_eq!(
        summary.slot_index,
        u32::try_from(parsed.proof.chain_branch.index).unwrap()
    );

    // Real-data smoke: the Fractal parent coinbase carries a decodable marker.
    assert!(decode_aux_marker(&parsed.parent_coinbase_script).is_some());

    // The full `[child header][CAuxPow]` blob must NOT be accepted here.
    assert!(auxpow_blob_summary(&raw).is_none());

    let detail = decode_auxpow_proof(&parsed.auxpow_bytes).expect("proof detail");

    assert_eq!(detail.slot_index, 0);
    assert_eq!(detail.parent_header_hash, parsed.parent_header.hash());
    // merkle_size == 1 here, so the chain (blockchain) branch has no siblings,
    // while the coinbase branch carries the parent-tree merkle path.
    assert!(detail.blockchain_branch.siblings.is_empty());
    assert_eq!(detail.blockchain_branch.index, 0);
    assert!(!detail.coinbase_branch.siblings.is_empty());
    // The full blob is still rejected by the same guard.
    assert!(decode_auxpow_proof(&raw).is_none());
}

#[test]
fn parses_bip34_height() {
    assert_eq!(parse_bip34_height(&[0x03, 0x40, 0x0d, 0x03]), Some(200_000));
    assert_eq!(parse_bip34_height(&[0x00]), None);
    assert_eq!(parse_bip34_height(&[0x05, 1, 2, 3, 4, 5]), None);
}

#[test]
fn pow_validates_target_accepts_meeting_hash_and_rejects_malformed_bits() {
    // Build a header hash equal to a known target (the easiest possible
    // target, mantissa 0x7fffff with exponent 0x20 = 32). Any hash whose
    // numeric value is <= target is accepted.
    let easiest_bits: u32 = 0x207f_ffff;
    let target = Target::from_compact(CompactTarget::from_consensus(easiest_bits));
    let meeting_header = header_meeting_target(easiest_bits);
    assert!(pow_validates_target(&meeting_header));
    assert!(validates_target(
        meeting_header.block_hash(),
        meeting_header.bits
    ));
    // Sanity-check the target is what we expect (not just a passthrough).
    assert!(target.is_met_by(meeting_header.block_hash()));

    // Valid Bitcoin genesis bits still reject a header hash above target.
    let hard_bits: u32 = 0x1d00_ffff;
    let failing_header = header_exceeding_target(hard_bits);
    assert!(!pow_validates_target(&failing_header));
    assert!(!validates_target(
        failing_header.block_hash(),
        failing_header.bits
    ));

    // Malformed compact targets are rejected before decoding.
    // sign bit set
    assert!(!pow_validates_target(&header_with_bits(0x0180_0000)));
    assert!(!validates_target(
        BlockHash::all_zeros(),
        CompactTarget::from_consensus(0x0180_0000)
    ));
    // zero mantissa
    assert!(!pow_validates_target(&header_with_bits(0x1d00_0000)));
    assert!(!validates_target(
        BlockHash::all_zeros(),
        CompactTarget::from_consensus(0x1d00_0000)
    ));
    // zero exponent
    assert!(!pow_validates_target(&header_with_bits(0x0000_ffff)));
    assert!(!validates_target(
        BlockHash::all_zeros(),
        CompactTarget::from_consensus(0x0000_ffff)
    ));
    // exponent > 32
    assert!(!pow_validates_target(&header_with_bits(0x2100_ffff)));
    assert!(!validates_target(
        BlockHash::all_zeros(),
        CompactTarget::from_consensus(0x2100_ffff)
    ));
}

#[test]
fn validates_target_can_compare_one_header_hash_to_another_headers_bits() {
    let parent_bits: u32 = 0x1d00_ffff;
    let child_bits: u32 = 0x207f_ffff;
    let parent_header = header_failing_parent_meeting_child(parent_bits, child_bits);
    let child_header = header_exceeding_target(child_bits);

    assert!(validates_target(
        parent_header.block_hash(),
        child_header.bits
    ));
    assert!(!validates_target(
        parent_header.block_hash(),
        parent_header.bits
    ));
    assert!(!pow_validates_target(&child_header));
}

#[test]
fn wire_order_byte_convention_is_locked() {
    // A non-AuxPoW block is enough to lock the header-hash byte-order
    // convention: 80 raw header bytes, parsed.hash() should equal the raw
    // sha256d output (wire order), and to_string() should produce the
    // reversed-hex display form used by RPC and block explorers.
    let mut raw = [0u8; 80];
    // version=1 (no AuxPoW bit), arbitrary nonce-ish payload.
    raw[0] = 0x01;
    for (i, byte) in raw.iter_mut().enumerate().skip(4) {
        *byte = i as u8;
    }

    let parsed = parse_namecoin_block(&raw).unwrap();
    let header = match parsed {
        ParsedNamecoinBlock::NonAuxpow(header) => header,
        ParsedNamecoinBlock::Auxpow(_) => panic!("non-AuxPoW expected"),
    };

    let expected_wire = sha256d::Hash::hash(&raw).to_byte_array();
    assert_eq!(
        header.hash().to_byte_array(),
        expected_wire,
        "in-memory bytes must be wire (internal) order, NOT reversed"
    );

    // to_string() yields display order (reversed). Spot-check the first
    // display byte is the last wire byte rendered as two hex chars.
    let display = header.hash().to_string();
    let expected_first_pair = format!("{:02x}", expected_wire[31]);
    assert!(
        display.starts_with(&expected_first_pair),
        "Display impl must reverse to match RPC/explorer hex (got {display})"
    );
}

#[test]
fn merkle_branch_round_trips() {
    // Build a synthetic AuxPoW block with non-zero hash_block and
    // non-empty merkle branches, then verify parse_namecoin_block
    // exposes them byte-for-byte. The real Namecoin fixtures all carry
    // zeroed hash_block and empty branches, so this is the only test
    // that exercises the non-trivial paths.
    let mut bytes = Vec::new();

    // Child header (80 bytes) with VERSION_AUXPOW bit set.
    let mut child_header = [0u8; 80];
    let child_version = VERSION_AUXPOW | 4;
    child_header[..4].copy_from_slice(&child_version.to_le_bytes());
    for (i, byte) in child_header.iter_mut().enumerate().skip(4) {
        *byte = (i as u8).wrapping_mul(3);
    }
    bytes.extend_from_slice(&child_header);

    // Parent coinbase transaction: version=1, single input (null
    // prevout + 1-byte script), single output (zero value, empty
    // script), locktime=0.
    bytes.extend_from_slice(&1i32.to_le_bytes()); // tx version
    bytes.push(0x01); // vin count = 1
    bytes.extend_from_slice(&[0u8; 32]); // prevout hash
    bytes.extend_from_slice(&0xffff_ffffu32.to_le_bytes()); // prevout index
    bytes.push(0x01); // script len
    bytes.push(0xaa); // script byte
    bytes.extend_from_slice(&0xffff_ffffu32.to_le_bytes()); // sequence
    bytes.push(0x01); // vout count = 1
    bytes.extend_from_slice(&0u64.to_le_bytes()); // value
    bytes.push(0x00); // script_pubkey len = 0
    bytes.extend_from_slice(&0u32.to_le_bytes()); // locktime

    // hash_block (non-zero, distinct byte pattern).
    let hash_block_wire: [u8; 32] = std::array::from_fn(|i| 0xa0 + i as u8);
    bytes.extend_from_slice(&hash_block_wire);

    // Coinbase merkle branch: two siblings, distinct byte patterns.
    bytes.push(0x02); // count = 2
    let cb_sibling_0: [u8; 32] = std::array::from_fn(|i| 0x10 + i as u8);
    let cb_sibling_1: [u8; 32] = std::array::from_fn(|i| 0x40 + i as u8);
    bytes.extend_from_slice(&cb_sibling_0);
    bytes.extend_from_slice(&cb_sibling_1);
    // Coinbase merkle index = 0 (canonical).
    bytes.extend_from_slice(&0i32.to_le_bytes());

    // Chain merkle branch: three siblings.
    bytes.push(0x03); // count = 3
    let ch_sibling_0: [u8; 32] = std::array::from_fn(|i| 0x60 + i as u8);
    let ch_sibling_1: [u8; 32] = std::array::from_fn(|i| 0x70 + i as u8);
    let ch_sibling_2: [u8; 32] = std::array::from_fn(|i| 0x80 + i as u8);
    bytes.extend_from_slice(&ch_sibling_0);
    bytes.extend_from_slice(&ch_sibling_1);
    bytes.extend_from_slice(&ch_sibling_2);
    // Chain merkle index = 5 (non-zero on purpose).
    bytes.extend_from_slice(&5i32.to_le_bytes());

    // Parent header (80 bytes, distinct pattern).
    let mut parent_header = [0u8; 80];
    parent_header[..4].copy_from_slice(&2i32.to_le_bytes());
    for (i, byte) in parent_header.iter_mut().enumerate().skip(4) {
        *byte = i as u8;
    }
    bytes.extend_from_slice(&parent_header);

    // Child transaction vector: 1 transaction, identical format to the
    // parent coinbase above.
    bytes.push(0x01); // child tx count = 1
    bytes.extend_from_slice(&1i32.to_le_bytes()); // tx version
    bytes.push(0x01); // vin count
    bytes.extend_from_slice(&[0u8; 32]);
    bytes.extend_from_slice(&0xffff_ffffu32.to_le_bytes());
    bytes.push(0x01);
    bytes.push(0xbb);
    bytes.extend_from_slice(&0xffff_ffffu32.to_le_bytes());
    bytes.push(0x01); // vout
    bytes.extend_from_slice(&0u64.to_le_bytes());
    bytes.push(0x00);
    bytes.extend_from_slice(&0u32.to_le_bytes());

    let parsed = parse_namecoin_block(&bytes).unwrap();
    let parsed = match parsed {
        ParsedNamecoinBlock::Auxpow(parsed) => parsed,
        ParsedNamecoinBlock::NonAuxpow(_) => panic!("AuxPoW expected"),
    };

    assert_eq!(parsed.proof.hash_block.to_byte_array(), hash_block_wire);
    assert_eq!(parsed.proof.coinbase_branch.hashes.len(), 2);
    assert_eq!(
        parsed.proof.coinbase_branch.hashes[0].to_byte_array(),
        cb_sibling_0
    );
    assert_eq!(
        parsed.proof.coinbase_branch.hashes[1].to_byte_array(),
        cb_sibling_1
    );
    assert_eq!(parsed.proof.coinbase_branch.index, 0);
    assert_eq!(parsed.proof.chain_branch.hashes.len(), 3);
    assert_eq!(
        parsed.proof.chain_branch.hashes[0].to_byte_array(),
        ch_sibling_0
    );
    assert_eq!(
        parsed.proof.chain_branch.hashes[1].to_byte_array(),
        ch_sibling_1
    );
    assert_eq!(
        parsed.proof.chain_branch.hashes[2].to_byte_array(),
        ch_sibling_2
    );
    assert_eq!(parsed.proof.chain_branch.index, 5);
    assert_eq!(parsed.parent_header.consensus_bytes(), parent_header);
}

fn elastos_360062_fixture() -> (ParsedHeader, Vec<u8>, BlockHash) {
    let child_bytes: [u8; Header::SIZE] = hex::decode(
        include_str!("../../../../fixtures/elastos/ela-360062-child-header.hex").trim(),
    )
    .expect("decode child header hex")
    .try_into()
    .expect("80-byte child header prefix");
    let child_header = parse_header(child_bytes).expect("parse child header prefix");
    let auxpow =
        hex::decode(include_str!("../../../../fixtures/elastos/ela-360062-auxpow.hex").trim())
            .expect("decode auxpow hex");
    let child_hash = "427b83fd71e7494601d841736c91b85a04251b41d029dda4a12d7ba8a1cd1b9b"
        .parse::<BlockHash>()
        .expect("parse child block hash");
    (child_header, auxpow, child_hash)
}

#[test]
fn auxpow_expected_index_matches_live_elastos_slot() {
    // ELA 360062 parent coinbase nonce 2_677_055_472, chain id 1224, 4-deep
    // chain branch -> slot 6 (verified against the live node).
    assert_eq!(
        auxpow_expected_index(2_677_055_472, ELASTOS_AUXPOW_CHAIN_ID, 4),
        6
    );
}

#[test]
fn parse_elastos_auxpow_extracts_stale_parent_and_consumes_blob() {
    let (child_header, auxpow, _) = elastos_360062_fixture();
    let parsed = parse_elastos_auxpow(child_header, &auxpow).expect("parse Elastos auxpow");
    // The embedded BTC parent header is the known stale.
    assert_eq!(
        parsed.parent_header.hash().to_string(),
        "0000000000000000000528b13b09f842b428401ca02f44be163868a8e719bb69",
    );
    // CAuxPow-only: child coinbase absent; stored proof is the consumed blob.
    assert_eq!(parsed.child_coinbase_txid, None);
    assert!(parsed.child_coinbase_outputs.is_empty());
    assert_eq!(parsed.auxpow_bytes, auxpow);
    // The stored proof re-parses (it is exactly the CAuxPow region).
    assert!(decode_auxpow_proof(&parsed.auxpow_bytes).is_some());
}

#[test]
fn parse_elastos_auxpow_rejects_trailing_and_oversize() {
    let (child_header, auxpow, _) = elastos_360062_fixture();
    let mut trailing = auxpow.clone();
    trailing.push(0x00);
    assert!(parse_elastos_auxpow(child_header.clone(), &trailing).is_err());
    assert!(parse_elastos_auxpow(child_header, &vec![0u8; MAX_ELASTOS_AUXPOW_BYTES + 1]).is_err());
}

#[test]
fn verify_auxpow_commitment_accepts_real_elastos_block_and_rejects_corruptions() {
    let (child_header, auxpow, child_hash) = elastos_360062_fixture();
    let parsed = parse_elastos_auxpow(child_header, &auxpow).expect("parse");
    verify_auxpow_commitment(&parsed, child_hash, ELASTOS_AUXPOW_CHAIN_ID)
        .expect("real Elastos commitment must verify");

    // Wrong child hash -> chain fold fails.
    let wrong_child = "0000000000000000000000000000000000000000000000000000000000000001"
        .parse::<BlockHash>()
        .unwrap();
    assert!(verify_auxpow_commitment(&parsed, wrong_child, ELASTOS_AUXPOW_CHAIN_ID).is_err());

    // The chain-fold leaf orientation must match exactly: the byte-REVERSED child
    // hash (i.e. the standard wire-order-leaf interpretation) must NOT verify.
    // This pins Elastos's display-order-leaf deviation from standard AuxPoW; the
    // positive assertion above accepts the real ELA 360062 block only with the
    // in-fold reversal, and this asserts the opposite orientation is rejected.
    let mut reversed = child_hash.to_byte_array();
    reversed.reverse();
    let reversed_child = BlockHash::from_byte_array(reversed);
    assert!(
        verify_auxpow_commitment(&parsed, reversed_child, ELASTOS_AUXPOW_CHAIN_ID).is_err(),
        "the wire-order-leaf orientation must be rejected; Elastos folds the reversed leaf"
    );

    // Wrong chain id -> deterministic slot mismatch.
    assert!(verify_auxpow_commitment(&parsed, child_hash, 1).is_err());

    // Flip a coinbase-branch sibling -> coinbase fold misses the parent root.
    let mut flipped = parsed.clone();
    let mut sib = flipped.proof.coinbase_branch.hashes[0].to_byte_array();
    sib[0] ^= 0xff;
    flipped.proof.coinbase_branch.hashes[0] = TxMerkleNode::from_byte_array(sib);
    assert!(verify_auxpow_commitment(&flipped, child_hash, ELASTOS_AUXPOW_CHAIN_ID).is_err());

    // Locate the single marker once for scriptSig tampering.
    let magic_pos = parsed
        .parent_coinbase_script
        .windows(4)
        .position(|w| w == AUXPOW_MAGIC)
        .unwrap();

    // Tamper the committed aux merkle root -> chain fold target wrong.
    let mut bad_root = parsed.clone();
    bad_root.parent_coinbase_script[magic_pos + 4] ^= 0xff;
    assert!(verify_auxpow_commitment(&bad_root, child_hash, ELASTOS_AUXPOW_CHAIN_ID).is_err());

    // Corrupt the tree-size field -> size != 1<<branch_len.
    let mut bad_size = parsed.clone();
    bad_size.parent_coinbase_script[magic_pos + 4 + 32] ^= 0x01;
    assert!(verify_auxpow_commitment(&bad_size, child_hash, ELASTOS_AUXPOW_CHAIN_ID).is_err());

    // A second fabe6d6d marker -> rejected (exactly-one rule).
    let mut dup = parsed.clone();
    dup.parent_coinbase_script.extend_from_slice(&AUXPOW_MAGIC);
    assert!(verify_auxpow_commitment(&dup, child_hash, ELASTOS_AUXPOW_CHAIN_ID).is_err());

    // A chain branch >= 32 deep is rejected before any fold (u32 tree-size bound).
    let mut deep = parsed.clone();
    while deep.proof.chain_branch.hashes.len() < 32 {
        deep.proof
            .chain_branch
            .hashes
            .push(TxMerkleNode::all_zeros());
    }
    assert!(verify_auxpow_commitment(&deep, child_hash, ELASTOS_AUXPOW_CHAIN_ID).is_err());
}

fn header_with_bits(bits: u32) -> Header {
    Header {
        version: Version::ONE,
        prev_blockhash: BlockHash::all_zeros(),
        merkle_root: TxMerkleNode::all_zeros(),
        time: 0,
        bits: CompactTarget::from_consensus(bits),
        nonce: 0,
    }
}

fn header_meeting_target(bits: u32) -> Header {
    let target = Target::from_compact(CompactTarget::from_consensus(bits));
    let mut header = header_with_bits(bits);
    loop {
        if target.is_met_by(header.block_hash()) {
            return header;
        }
        header.nonce = header.nonce.checked_add(1).unwrap();
    }
}

fn header_exceeding_target(bits: u32) -> Header {
    let target = Target::from_compact(CompactTarget::from_consensus(bits));
    let mut header = header_with_bits(bits);
    loop {
        if !target.is_met_by(header.block_hash()) {
            return header;
        }
        header.nonce = header.nonce.checked_add(1).unwrap();
    }
}

fn header_failing_parent_meeting_child(parent_bits: u32, child_bits: u32) -> Header {
    let parent_target = Target::from_compact(CompactTarget::from_consensus(parent_bits));
    let child_target = Target::from_compact(CompactTarget::from_consensus(child_bits));
    let mut header = header_with_bits(parent_bits);
    loop {
        let hash = header.block_hash();
        if !parent_target.is_met_by(hash) && child_target.is_met_by(hash) {
            return header;
        }
        header.nonce = header.nonce.checked_add(1).unwrap();
    }
}
