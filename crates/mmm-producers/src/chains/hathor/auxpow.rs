//! Hathor RFC 0006 split-header AuxPoW reconstruction.
//!
//! Hathor's `aux_pow` field stores only bytes `0..36` and `68..80` of the 80-byte
//! BTC parent header; the merkle root (`36..68`) is reconstructed from the parent
//! coinbase (`"Hath"` commitment), merkle path, and the child block's `funds || graph`
//! prefix, accepted only when the assembled header hashes to the Hathor block hash.

use anyhow::{Context, Result, bail, ensure};
use bitcoin::BlockHash;
use bitcoin::block::Header;
use bitcoin::consensus::deserialize;
use bitcoin::hashes::{Hash as _, sha256, sha256d};

const HATH_MAGIC: [u8; 4] = *b"Hath";
const MAX_AUX_POW_LEN: usize = 16 * 1024;
const MAX_FUNDS_GRAPH_LEN: usize = 64 * 1024;
const MAX_MERKLE_COUNT: usize = 32;

#[derive(Debug, Clone, PartialEq, Eq)]
struct HathorAuxPow {
    head_36: [u8; 36],
    /// BTC parent coinbase up to and including `"Hath"`, before the 32-byte `aux_block_hash`.
    cb_head: Vec<u8>,
    cb_tail: Vec<u8>,
    /// Coinbase-to-merkle-root sibling hashes, stored display-order in the blob.
    merkle_path: Vec<[u8; 32]>,
    tail_12: [u8; 12],
}

#[derive(Debug, Clone)]
pub struct HathorReconstruction {
    pub header: Header,
    pub full_coinbase: Vec<u8>,
    pub funds_graph_split: usize,
    pub funds_graph_len: usize,
}

fn sha(b: &[u8]) -> [u8; 32] {
    sha256::Hash::hash(b).to_byte_array()
}

fn dsha(b: &[u8]) -> [u8; 32] {
    sha256d::Hash::hash(b).to_byte_array()
}

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

/// Extract the `funds || graph` prefix preceding `aux_pow` in `raw`.
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

fn verify_commitment_marker(cb_head: &[u8]) -> Result<()> {
    ensure!(
        cb_head.len() >= HATH_MAGIC.len()
            && cb_head[cb_head.len() - HATH_MAGIC.len()..] == HATH_MAGIC,
        "coinbase head does not end with the Hathor 'Hath' commitment marker"
    );
    Ok(())
}

/// Reconstruct the 80-byte BTC parent header per RFC 0006, accepting the first
/// `funds | graph` split whose assembled header hashes to `expected_block_hash`.
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
        let Some((header, full_coinbase)) = assemble_btc_header(aux, funds_graph, split) else {
            continue;
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

/// Assemble the BTC parent header and full coinbase for one `funds | graph` split:
/// `aux_block_hash = sha256d(sha256(funds) || sha256(graph))` (display-reversed)
/// spliced into the coinbase and folded up the merkle path.
fn assemble_btc_header(
    aux: &HathorAuxPow,
    funds_graph: &[u8],
    split: usize,
) -> Option<(Header, Vec<u8>)> {
    let mut pair = Vec::with_capacity(64);
    pair.extend_from_slice(&sha(&funds_graph[..split]));
    pair.extend_from_slice(&sha(&funds_graph[split..]));
    let mut abh = dsha(&pair);
    abh.reverse();

    let mut full_coinbase = Vec::with_capacity(aux.cb_head.len() + 32 + aux.cb_tail.len());
    full_coinbase.extend_from_slice(&aux.cb_head);
    full_coinbase.extend_from_slice(&abh);
    full_coinbase.extend_from_slice(&aux.cb_tail);

    // Siblings are stored display-order; reverse each before hashing.
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
    deserialize::<Header>(&header_bytes)
        .ok()
        .map(|header| (header, full_coinbase))
}

/// Test helper: rewrite the declared graph weight and reassemble a self-consistent header.
#[cfg(any(test, feature = "db-integration"))]
pub fn forge_with_weight(
    raw: &[u8],
    aux_pow: &[u8],
    funds_graph_split: usize,
    weight: f64,
) -> Result<(Vec<u8>, BlockHash)> {
    let aux = parse_hathor_aux_pow(aux_pow)?;
    let mut funds_graph = funds_graph_from_raw(raw, aux_pow)?.to_vec();
    ensure!(
        funds_graph.len() >= funds_graph_split + 8,
        "the split leaves no room for a weight"
    );
    funds_graph[funds_graph_split..funds_graph_split + 8].copy_from_slice(&weight.to_be_bytes());
    let (header, _coinbase) = assemble_btc_header(&aux, &funds_graph, funds_graph_split)
        .context("the forged bytes do not assemble a header")?;
    let hash = header.block_hash();
    funds_graph.extend_from_slice(aux_pow);
    Ok((funds_graph, hash))
}

pub fn reconstruct_from_blobs(
    raw: &[u8],
    aux_pow: &[u8],
    expected_block_hash: BlockHash,
) -> Result<HathorReconstruction> {
    let aux = parse_hathor_aux_pow(aux_pow)?;
    let funds_graph = funds_graph_from_raw(raw, aux_pow)?;
    reconstruct_btc_header(&aux, funds_graph, expected_block_hash)
}

fn find_subsequence(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || needle.len() > haystack.len() {
        return None;
    }
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

struct Cursor<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

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

    fn read_u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }

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

            let recon = reconstruct_from_blobs(&raw, &aux_pow, expected).unwrap();

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
        let funds_graph = funds_graph_from_raw(&raw, &aux_pow).unwrap();
        let split = reconstruct_btc_header(&aux, funds_graph, expected)
            .unwrap()
            .funds_graph_split;
        // Corrupt the trailing "Hath" magic, then report the hash of the header
        // that corrupted coinbase assembles to: the hash identity alone would
        // accept this response, so only the marker check can reject it.
        let n = aux.cb_head.len();
        aux.cb_head[n - 1] ^= 0xFF;
        let (forged, _) = assemble_btc_header(&aux, funds_graph, split).unwrap();
        assert!(reconstruct_btc_header(&aux, funds_graph, forged.block_hash()).is_err());
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
