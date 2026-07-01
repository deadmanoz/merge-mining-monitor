//! Hathor RFC 0006 split-header AuxPoW reconstruction.
//!
//! Unlike the Namecoin family, Hathor does not attach a CAuxPow blob to an
//! 80-byte child header. Its `aux_pow` field stores only 36 + 12 of the 80 BTC
//! parent-header bytes; the merkle-root middle (offsets 36..68) is
//! RECONSTRUCTED from the parent coinbase plus a merkle path plus the Hathor
//! block's own funds/graph bytes (the `raw` prefix before `aux_pow`). The
//! commitment marker is `"Hath"` in a coinbase OP_RETURN, not `0xfabe6d6d`, so
//! this parses entirely in its own module (the `mmm_capture::auxpow` internals are
//! private and assume a CAuxPow blob) and never touches `auxpow.rs`.
//!
//! The reconstruction is self-verifying: the assembled header is accepted only
//! when its block hash equals the Hathor block hash the API reported, so a
//! wrong funds|graph split can never be accepted. Byte orders must match exactly
//! and were proven byte-exact against the live API and the research fixtures
//! (see `fixtures/hathor/`).

use anyhow::{Context, Result, bail, ensure};
use bitcoin::BlockHash;
use bitcoin::block::Header;
use bitcoin::consensus::deserialize;
use bitcoin::hashes::{Hash as _, sha256, sha256d};

/// The `"Hath"` merge-mining commitment magic that terminates `cb_head`.
const HATH_MAGIC: [u8; 4] = *b"Hath";

/// A real CAuxPow blob is ~1 KB; cap untrusted REST input well above that.
const MAX_AUX_POW_LEN: usize = 16 * 1024;

/// The funds+graph prefix is ~150 bytes in practice; bound the brute-force
/// search space against a hostile `raw`.
const MAX_FUNDS_GRAPH_LEN: usize = 64 * 1024;

/// The BTC coinbase merkle path is ~12-14 deep; reject absurd counts before
/// allocation. A depth this large is structurally impossible for a real proof.
const MAX_MERKLE_COUNT: usize = 32;

/// Parsed RFC 0006 split-header AuxPoW blob.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HathorAuxPow {
    /// BTC header bytes 0..36 (version + previous-block hash).
    head_36: [u8; 36],
    /// BTC parent coinbase up to and including the `"Hath"` marker; the 32-byte
    /// `aux_block_hash` is spliced immediately after this.
    cb_head: Vec<u8>,
    /// BTC parent coinbase after the spliced `aux_block_hash` to the end.
    cb_tail: Vec<u8>,
    /// Coinbase-to-merkle-root sibling hashes, stored display-order in the blob.
    merkle_path: Vec<[u8; 32]>,
    /// BTC header bytes 68..80 (nTime + nBits + nNonce).
    tail_12: [u8; 12],
}

impl HathorAuxPow {
    /// The BTC parent coinbase commitment marker terminates `cb_head`.
    #[cfg(test)]
    pub fn cb_head(&self) -> &[u8] {
        &self.cb_head
    }
}

/// A successful, self-verified reconstruction.
#[derive(Debug, Clone)]
pub struct HathorReconstruction {
    /// The reassembled 80-byte BTC parent header (hash equals the Hathor block).
    pub header: Header,
    /// The reassembled full BTC parent coinbase transaction bytes.
    pub full_coinbase: Vec<u8>,
    /// The funds|graph split offset (stored in the sidecar for O(1) re-verify).
    pub funds_graph_split: usize,
    /// Length of the funds+graph prefix in `raw` (i.e. the `aux_pow` start
    /// offset), so callers reuse it instead of re-scanning `raw` for `aux_pow`.
    pub funds_graph_len: usize,
}

/// Single SHA-256, internal byte order (the funds/graph leaf hash inputs).
fn sha(b: &[u8]) -> [u8; 32] {
    sha256::Hash::hash(b).to_byte_array()
}

/// Double SHA-256, internal byte order (the BTC merkle/coinbase hash primitive).
fn dsha(b: &[u8]) -> [u8; 32] {
    sha256d::Hash::hash(b).to_byte_array()
}

/// Decode the RFC 0006 `aux_pow` binary layout, bounding every untrusted length
/// before allocation. Requires the blob to be consumed exactly.
fn parse_hathor_aux_pow(blob: &[u8]) -> Result<HathorAuxPow> {
    ensure!(
        blob.len() <= MAX_AUX_POW_LEN,
        "aux_pow blob too large: {} bytes",
        blob.len()
    );
    let mut cur = Cursor::new(blob);

    let head_36: [u8; 36] = cur.take(36)?.try_into().expect("36-byte slice");

    let cb_head_len = cur.read_varint()?;
    let cb_head = cur.take(cb_head_len)?.to_vec();

    let cb_tail_len = cur.read_varint()?;
    let cb_tail = cur.take(cb_tail_len)?.to_vec();

    let merkle_count = cur.read_varint()?;
    ensure!(
        merkle_count <= MAX_MERKLE_COUNT,
        "aux_pow merkle path too deep: {merkle_count}"
    );
    let mut merkle_path = Vec::with_capacity(merkle_count);
    for _ in 0..merkle_count {
        merkle_path.push(cur.take(32)?.try_into().expect("32-byte slice"));
    }

    let tail_12: [u8; 12] = cur.take(12)?.try_into().expect("12-byte slice");

    ensure!(
        cur.remaining() == 0,
        "aux_pow blob has {} trailing bytes",
        cur.remaining()
    );

    Ok(HathorAuxPow {
        head_36,
        cb_head,
        cb_tail,
        merkle_path,
        tail_12,
    })
}

/// The funds+graph bytes are the `raw` block prefix that precedes the `aux_pow`
/// subsequence. Returns that prefix, or an error if `aux_pow` is not found.
///
/// `find_subsequence` takes the first occurrence. A coincidental earlier match
/// would yield a too-short prefix and drop the block as malformed, never a wrong
/// accept: the caller re-derives the BTC header from this prefix and requires
/// `sha256d(header) == tx.hash`. The collision is astronomically unlikely because
/// `aux_pow` begins with the 36-byte high-entropy BTC version + prev-block hash.
fn funds_graph_from_raw<'a>(raw: &'a [u8], aux_pow: &[u8]) -> Result<&'a [u8]> {
    let idx = find_subsequence(raw, aux_pow)
        .context("aux_pow blob not found within the raw Hathor block")?;
    let prefix = &raw[..idx];
    ensure!(
        prefix.len() <= MAX_FUNDS_GRAPH_LEN,
        "funds+graph prefix too large: {} bytes",
        prefix.len()
    );
    ensure!(prefix.len() >= 2, "funds+graph prefix too small to split");
    Ok(prefix)
}

/// Require `cb_head` to end with the `"Hath"` commitment magic, immediately
/// before the spliced `aux_block_hash`. Covers both the OP_RETURN
/// (`6a 24 "Hath"`) and the reference-coordinator scriptSig variants.
fn verify_commitment_marker(cb_head: &[u8]) -> Result<()> {
    ensure!(
        cb_head.len() >= HATH_MAGIC.len()
            && cb_head[cb_head.len() - HATH_MAGIC.len()..] == HATH_MAGIC,
        "coinbase head does not end with the Hathor 'Hath' commitment marker"
    );
    Ok(())
}

/// Reconstruct the 80-byte BTC parent header per RFC 0006, brute-forcing the
/// funds|graph split and accepting the first split whose assembled header hashes
/// to `expected_block_hash` (the smoking-gun identity that makes this safe).
fn reconstruct_btc_header(
    aux: &HathorAuxPow,
    funds_graph: &[u8],
    expected_block_hash: BlockHash,
) -> Result<HathorReconstruction> {
    verify_commitment_marker(&aux.cb_head)?;
    ensure!(
        funds_graph.len() <= MAX_FUNDS_GRAPH_LEN,
        "funds+graph prefix too large: {} bytes",
        funds_graph.len()
    );

    for split in 2..funds_graph.len() {
        // aux_block_hash = sha256d(sha256(funds) || sha256(graph)), embedded in
        // display (byte-reversed) order.
        let mut pair = Vec::with_capacity(64);
        pair.extend_from_slice(&sha(&funds_graph[..split]));
        pair.extend_from_slice(&sha(&funds_graph[split..]));
        let mut abh = dsha(&pair);
        abh.reverse();

        let mut full_coinbase = Vec::with_capacity(aux.cb_head.len() + 32 + aux.cb_tail.len());
        full_coinbase.extend_from_slice(&aux.cb_head);
        full_coinbase.extend_from_slice(&abh);
        full_coinbase.extend_from_slice(&aux.cb_tail);

        // Fold the coinbase txid up the merkle path. The coinbase is index 0
        // (always the left leaf); siblings are stored display-order, so reverse
        // each before hashing.
        let mut cur = dsha(&full_coinbase);
        for sib in &aux.merkle_path {
            let mut node = Vec::with_capacity(64);
            node.extend_from_slice(&cur);
            let mut rsib = *sib;
            rsib.reverse();
            node.extend_from_slice(&rsib);
            cur = dsha(&node);
        }

        let mut header_bytes = Vec::with_capacity(80);
        header_bytes.extend_from_slice(&aux.head_36);
        header_bytes.extend_from_slice(&cur);
        header_bytes.extend_from_slice(&aux.tail_12);

        let header: Header = match deserialize(&header_bytes) {
            Ok(h) => h,
            Err(_) => continue,
        };
        if header.block_hash() == expected_block_hash {
            return Ok(HathorReconstruction {
                header,
                full_coinbase,
                funds_graph_split: split,
                funds_graph_len: funds_graph.len(),
            });
        }
    }

    bail!("no funds|graph split reconstructs to the expected Hathor block hash")
}

/// Convenience entry point: parse `aux_pow`, derive the funds+graph prefix from
/// `raw`, and reconstruct + self-verify the BTC parent header in one call.
pub fn reconstruct_from_blobs(
    raw: &[u8],
    aux_pow: &[u8],
    expected_block_hash: BlockHash,
) -> Result<(HathorAuxPow, HathorReconstruction)> {
    let aux = parse_hathor_aux_pow(aux_pow)?;
    let funds_graph = funds_graph_from_raw(raw, aux_pow)?;
    let reconstruction = reconstruct_btc_header(&aux, funds_graph, expected_block_hash)?;
    Ok((aux, reconstruction))
}

/// Index of the first occurrence of `needle` in `haystack`, or `None`. First
/// occurrence is intentional: a too-early match only ever fails the downstream
/// hash identity (a malformed skip), never a wrong accept (see
/// [`funds_graph_from_raw`]).
fn find_subsequence(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || needle.len() > haystack.len() {
        return None;
    }
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

/// Forward-only byte cursor over an untrusted blob, bounds-checking every read
/// so a truncated `aux_pow` errors instead of panicking.
struct Cursor<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    /// Take exactly `n` bytes, erroring if the blob ends early.
    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        ensure!(
            self.pos
                .checked_add(n)
                .is_some_and(|end| end <= self.buf.len()),
            "unexpected end of aux_pow blob"
        );
        let slice = &self.buf[self.pos..self.pos + n];
        self.pos += n;
        Ok(slice)
    }

    /// Take one byte.
    fn read_u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }

    /// Read a Bitcoin-style compact-size varint (0xFD/0xFE/0xFF prefixes for
    /// u16/u32/u64), erroring if the value exceeds `usize`.
    fn read_varint(&mut self) -> Result<usize> {
        let first = self.read_u8()?;
        let value = match first {
            0xFF => u64::from_le_bytes(self.take(8)?.try_into().expect("8-byte slice")),
            0xFE => u32::from_le_bytes(self.take(4)?.try_into().expect("4-byte slice")) as u64,
            0xFD => u16::from_le_bytes(self.take(2)?.try_into().expect("2-byte slice")) as u64,
            other => other as u64,
        };
        usize::try_from(value).context("aux_pow varint exceeds usize")
    }

    /// Bytes not yet consumed; the parser requires this to be 0 at the end
    /// (the blob must be consumed exactly).
    fn remaining(&self) -> usize {
        self.buf.len() - self.pos
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    /// Each committed fixture carries the live `raw`/`aux_pow` bytes plus the
    /// expected reconstruction outputs (verified against the research CSV).
    struct Fixture {
        json: &'static str,
    }

    const FIXTURES: &[Fixture] = &[
        Fixture {
            json: include_str!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../fixtures/hathor/1971823.json"
            )),
        },
        Fixture {
            json: include_str!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../fixtures/hathor/2773476.json"
            )),
        },
        Fixture {
            json: include_str!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../fixtures/hathor/3261131.json"
            )),
        },
        Fixture {
            json: include_str!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../fixtures/hathor/4482931.json"
            )),
        },
        Fixture {
            json: include_str!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../fixtures/hathor/5249647.json"
            )),
        },
        Fixture {
            json: include_str!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../fixtures/hathor/6342295.json"
            )),
        },
    ];

    fn field<'a>(json: &'a serde_json::Value, key: &str) -> &'a str {
        json.get(key).unwrap().as_str().unwrap()
    }

    #[test]
    fn all_validated_stale_fixtures_reconstruct_byte_exact() {
        for fx in FIXTURES {
            let j: serde_json::Value = serde_json::from_str(fx.json).unwrap();
            let raw = hex::decode(field(&j, "raw_hex")).unwrap();
            let aux_pow = hex::decode(field(&j, "aux_pow_hex")).unwrap();
            let expected = BlockHash::from_str(field(&j, "tx_id")).unwrap();

            let (aux, recon) = reconstruct_from_blobs(&raw, &aux_pow, expected).unwrap();

            // Header is byte-identical to the validated CSV header.
            assert_eq!(
                bitcoin::consensus::serialize(&recon.header),
                hex::decode(field(&j, "expected_btc_header_hex")).unwrap(),
                "header mismatch for {}",
                j["hathor_height"]
            );
            // The smoking-gun identity holds and the split matches.
            assert_eq!(recon.header.block_hash(), expected);
            assert_eq!(
                recon.funds_graph_split as u64,
                j["expected_funds_graph_split"].as_u64().unwrap()
            );
            // The reconstructed prev hash matches the CSV.
            assert_eq!(
                recon.header.prev_blockhash,
                BlockHash::from_str(field(&j, "expected_btc_prev_hash")).unwrap()
            );
            // The reconstructed parent satisfies BTC-difficulty PoW.
            assert!(mmm_capture::auxpow::pow_validates_target(&recon.header));
            // The coinbase carries the Hathor commitment marker.
            verify_commitment_marker(aux.cb_head()).unwrap();
        }
    }

    fn first_fixture() -> (Vec<u8>, Vec<u8>, BlockHash) {
        let j: serde_json::Value = serde_json::from_str(FIXTURES[0].json).unwrap();
        (
            hex::decode(field(&j, "raw_hex")).unwrap(),
            hex::decode(field(&j, "aux_pow_hex")).unwrap(),
            BlockHash::from_str(field(&j, "tx_id")).unwrap(),
        )
    }

    #[test]
    fn altered_commitment_marker_is_rejected() {
        let (raw, aux_pow, expected) = first_fixture();
        let mut aux = parse_hathor_aux_pow(&aux_pow).unwrap();
        // Corrupt the trailing "Hath" magic.
        let n = aux.cb_head.len();
        aux.cb_head[n - 1] ^= 0xFF;
        let funds_graph = funds_graph_from_raw(&raw, &aux_pow).unwrap();
        assert!(reconstruct_btc_header(&aux, funds_graph, expected).is_err());
    }

    #[test]
    fn flipped_merkle_sibling_breaks_identity() {
        let (raw, aux_pow, expected) = first_fixture();
        let mut aux = parse_hathor_aux_pow(&aux_pow).unwrap();
        assert!(!aux.merkle_path.is_empty());
        aux.merkle_path[0][0] ^= 0x01;
        let funds_graph = funds_graph_from_raw(&raw, &aux_pow).unwrap();
        assert!(reconstruct_btc_header(&aux, funds_graph, expected).is_err());
    }

    #[test]
    fn wrong_expected_hash_finds_no_split() {
        let (raw, aux_pow, _expected) = first_fixture();
        let aux = parse_hathor_aux_pow(&aux_pow).unwrap();
        let funds_graph = funds_graph_from_raw(&raw, &aux_pow).unwrap();
        let wrong =
            BlockHash::from_str("00000000000000000000000000000000000000000000000000000000deadbeef")
                .unwrap();
        assert!(reconstruct_btc_header(&aux, funds_graph, wrong).is_err());
    }

    #[test]
    fn truncated_blob_is_rejected() {
        let (_raw, aux_pow, _expected) = first_fixture();
        assert!(parse_hathor_aux_pow(&aux_pow[..aux_pow.len() - 4]).is_err());
        assert!(parse_hathor_aux_pow(&aux_pow[..10]).is_err());
    }

    #[test]
    fn oversize_merkle_count_is_rejected_before_allocation() {
        // 36-byte head, empty cb_head, empty cb_tail, then a varint merkle_count
        // of 0xFFFFFFFF (way over the cap) - must be rejected without allocating.
        let mut blob = vec![0u8; 36];
        blob.push(0x00); // cb_head_len = 0
        blob.push(0x00); // cb_tail_len = 0
        blob.push(0xFE); // varint: u32 follows
        blob.extend_from_slice(&u32::to_le_bytes(0xFFFF_FFFF));
        assert!(parse_hathor_aux_pow(&blob).is_err());
    }

    #[test]
    fn aux_pow_absent_from_raw_is_rejected() {
        let (_raw, aux_pow, _expected) = first_fixture();
        let unrelated = vec![0xABu8; 200];
        assert!(funds_graph_from_raw(&unrelated, &aux_pow).is_err());
    }
}
