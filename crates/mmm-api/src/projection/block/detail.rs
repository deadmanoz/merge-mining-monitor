//! Block-detail rendering: commitments, coinbase tags, event details,
//! and RSK evidence.

use std::collections::{BTreeSet, HashMap};

use anyhow::{Context, Result, bail};
use bitcoin::hashes::Hash as _;
use serde_json::Value;

use super::super::ProjectionError;
use super::super::shared::{EventRow, display_child_block_hash, display_hash};
use super::loaders::BlockDetailRow;
use super::loaders::{EventDetailRow, RskEvidenceRow};
use super::{
    ApiProof, AuxBranchDetail, AuxMarkerProjection, AuxProofDetail, Commitment, EventDetail,
    EventPoolAttributions, HeaderProjection, RskEventDetail,
};
use crate::normalize::ParentKind;
use crate::query::kind_as_str;
use bitcoin::block::Header;
use bitcoin::consensus::encode::deserialize;
use mmm_capture::auxpow::evidence::{
    AuxMarker, decode_aux_marker, decode_auxpow_proof, extract_coinbase_tag,
};
use mmm_capture::pool_resolver::PoolResolver;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ChainFamily {
    /// Namecoin / Syscoin / Fractal: standard `fabe6d6d` AuxPoW marker.
    NamecoinFamily,
    /// RSK: RSKIP-92 midstate compression discards the coinbase (opaque).
    Rsk,
    /// Hathor: RFC 0006 `"Hath"` split-header form, never scanned for `fabe6d6d`.
    Hathor,
    Other,
}

/// Map a child chain slug to its AuxPoW `ChainFamily`. The single gate that
/// decides which chains get fabe6d6d marker scanning + slot/aux-proof decoding
/// (NamecoinFamily: namecoin/syscoin/fractal/elastos) vs opaque (rsk) vs
/// split-header (hathor). Adding a family chain is a new arm here, never a
/// cloned module (architecture rule).
pub(super) fn chain_family(chain: Option<&str>) -> ChainFamily {
    match chain {
        Some("namecoin") | Some("syscoin") | Some("fractal") | Some("elastos") => {
            ChainFamily::NamecoinFamily
        }
        Some("rsk") => ChainFamily::Rsk,
        Some("hathor") => ChainFamily::Hathor,
        _ => ChainFamily::Other,
    }
}

/// Reference AuxPoW chain id per child chain, **cite-or-null**. Namecoin's
/// `AUXPOW_CHAIN_ID` is 1 (the merged-mining spec's worked example confirms slot
/// derivation with `chain_id = 1`). Elastos's is 1224, verified against live
/// blocks. Syscoin / Fractal are not yet cited from their chainparams, so they
/// return `None` until a sourced constant is added. `slot_index` (decoded from
/// the proof) is the per-block datum used for verification.
pub(super) fn chain_id_for_chain(chain: &str) -> Option<u32> {
    match chain {
        "namecoin" => Some(1),
        "elastos" => Some(1224),
        _ => None,
    }
}

/// Project a decoded `AuxMarker` into the wire `AuxMarkerProjection`.
/// `aux_merkle_root` goes out display-order (rust-bitcoin `Display` reverses the
/// raw scriptSig bytes, constraint #5); `magic_present` is always true here
/// (only called once a marker decoded).
pub(super) fn marker_projection(marker: &AuxMarker) -> AuxMarkerProjection {
    AuxMarkerProjection {
        magic_present: true,
        // `Display` reverses to the standard display-order hex.
        aux_merkle_root: marker.aux_merkle_root.to_string(),
        merkle_size: marker.merkle_size,
        merkle_nonce: marker.merkle_nonce,
    }
}

/// Build the parent-level merge-mining commitment from a block's event rows,
/// with an explicit family priority (NamecoinFamily > Rsk > Hathor). The
/// representative row (which sets `format`, `parent_coinbase_txid`,
/// `parent_coinbase_script_hex`, and `marker`) is a single row, so the script and
/// the marker always agree: the first Namecoin-family row that decodes a marker
/// (ascending event id), else the first Namecoin-family row with `marker = None`.
pub(super) fn derive_commitment(rows: &[EventDetailRow]) -> Option<Commitment> {
    let is_family = |r: &&EventDetailRow, family: ChainFamily| {
        chain_family(r.source.chain.as_deref()) == family
    };

    if rows
        .iter()
        .any(|r| chain_family(r.source.chain.as_deref()) == ChainFamily::NamecoinFamily)
    {
        let mut namecoin: Vec<&EventDetailRow> = rows
            .iter()
            .filter(|r| is_family(r, ChainFamily::NamecoinFamily))
            .collect();
        namecoin.sort_by_key(|r| r.id);
        let decoded = namecoin.iter().find_map(|r| {
            r.btc_parent_coinbase_script
                .as_deref()
                .and_then(decode_aux_marker)
                .map(|marker| (*r, marker))
        });
        let (rep, marker) = match decoded {
            Some((row, marker)) => (row, Some(marker)),
            None => (*namecoin.first().expect("non-empty family"), None),
        };
        return Some(Commitment {
            format: "namecoin-aux",
            parent_coinbase_txid: rep
                .btc_parent_coinbase_txid
                .as_deref()
                .and_then(|bytes| display_hash(bytes).ok()),
            parent_coinbase_script_hex: rep.btc_parent_coinbase_script.as_deref().map(hex::encode),
            marker: marker.as_ref().map(marker_projection),
        });
    }
    if rows
        .iter()
        .any(|r| chain_family(r.source.chain.as_deref()) == ChainFamily::Rsk)
    {
        return Some(Commitment {
            format: "rsk-opaque",
            parent_coinbase_txid: None,
            parent_coinbase_script_hex: None,
            marker: None,
        });
    }
    if rows
        .iter()
        .any(|r| chain_family(r.source.chain.as_deref()) == ChainFamily::Hathor)
    {
        return Some(Commitment {
            format: "hathor-rfc0006",
            parent_coinbase_txid: None,
            parent_coinbase_script_hex: None,
            marker: None,
        });
    }
    None
}

/// Fallback `coinbase_tag`: extract printable tag runs from the commitment
/// representative's parent coinbase scriptSig (via `extract_coinbase_tag`).
/// Used when there is no Core-attested stored script (see
/// `coinbase_tag_from_core_block`, which wins for canonical+core_attested).
pub(super) fn coinbase_tag_from_commitment(commitment: Option<&Commitment>) -> Option<String> {
    let script_hex = commitment?.parent_coinbase_script_hex.as_deref()?;
    let script = hex::decode(script_hex).ok()?;
    extract_coinbase_tag(&script)
}

/// Preferred `coinbase_tag` for canonical Core-attested blocks: resolve the
/// stored `block.btc_coinbase_script` through the pool snapshot (pool name),
/// falling back to a normalized raw tag. Returns `None` for non-canonical or
/// non-core-attested rows, deferring to the commitment fallback. Reads stored
/// wire-order bytes.
pub(super) fn coinbase_tag_from_core_block(row: &BlockDetailRow) -> Option<String> {
    if row.kind != ParentKind::Canonical || !row.core_attested {
        return None;
    }
    let script = row.btc_coinbase_script.as_deref()?;
    PoolResolver::from_default_snapshot()
        .ok()
        .and_then(|resolver| {
            resolver
                .resolve_coinbase_script(script)
                .map(|pool_match| pool_match.matched_value.to_owned())
        })
        .or_else(|| extract_coinbase_tag(script).and_then(|tag| normalize_core_coinbase_tag(&tag)))
}

/// Trim surrounding slashes/whitespace from a raw Core coinbase tag, returning
/// `None` if nothing remains. The fallback path inside
/// `coinbase_tag_from_core_block` when the pool snapshot does not resolve the
/// script.
pub(super) fn normalize_core_coinbase_tag(tag: &str) -> Option<String> {
    let normalized = tag.trim_matches('/').trim();
    (!normalized.is_empty()).then(|| normalized.to_owned())
}

/// Sort and render `EventDetailRow`s into wire `EventDetail`s. Deterministic
/// order: confirmed_at, source code, child height, child hash, id (pins the
/// `event_details` array, block-*.json). chain_id/slot_index/aux_proof are
/// Namecoin-family-only and slot_index is gated on the decoded blob's embedded
/// parent header matching this event's own parent (a mismatched-but-parseable
/// blob never leaks another parent's slot, constraint #5). Decodes via
/// mmm_capture::auxpow::evidence.
pub(super) fn render_event_details(
    mut rows: Vec<EventDetailRow>,
    pool_attributions_by_event: &HashMap<i64, EventPoolAttributions>,
) -> Result<Vec<EventDetail>> {
    rows.sort_by(|a, b| {
        a.event_confirmed_at
            .cmp(&b.event_confirmed_at)
            .then_with(|| a.source.code.cmp(&b.source.code))
            .then_with(|| a.child_height.cmp(&b.child_height))
            .then_with(|| a.child_block_hash.cmp(&b.child_block_hash))
            .then_with(|| a.id.cmp(&b.id))
    });
    rows.into_iter()
        .map(|row| {
            // chain_id / slot_index are Namecoin-family-only. slot_index is gated
            // on the stored blob's embedded parent header matching this event's
            // own parent, so a parseable-but-mismatched blob never surfaces a slot
            // that belongs to a different parent.
            let family = chain_family(row.source.chain.as_deref());
            let (chain_id, slot_index, aux_proof) = if family == ChainFamily::NamecoinFamily {
                let chain_id = row.source.chain.as_deref().and_then(chain_id_for_chain);
                // Decode the CAuxPow blob once, gated on the embedded parent
                // header matching this event's own parent, then derive both the
                // quick-glance slot and the full branch breakdown from it.
                let proof = row
                    .aux_merkle_proof
                    .as_deref()
                    .and_then(decode_auxpow_proof)
                    .filter(|detail| {
                        detail.parent_header_hash.to_byte_array().as_slice()
                            == row.parent_hash.as_slice()
                    });
                let slot_index = proof.as_ref().map(|detail| detail.slot_index);
                let aux_proof = proof.map(|detail| AuxProofDetail {
                    hash_block: detail.hash_block.to_string(),
                    coinbase_branch: AuxBranchDetail {
                        index: detail.coinbase_branch.index,
                        siblings: detail
                            .coinbase_branch
                            .siblings
                            .iter()
                            .map(ToString::to_string)
                            .collect(),
                    },
                    blockchain_branch: AuxBranchDetail {
                        index: detail.blockchain_branch.index,
                        siblings: detail
                            .blockchain_branch
                            .siblings
                            .iter()
                            .map(ToString::to_string)
                            .collect(),
                    },
                });
                (chain_id, slot_index, aux_proof)
            } else {
                (None, None, None)
            };
            let rsk = row.rsk.map(render_rsk_detail);
            let child_block_hash =
                display_child_block_hash(&row.source.code, &row.child_block_hash)?;
            Ok(EventDetail {
                id: row.id,
                source: row.source.code,
                child_chain: row.source.chain,
                child_height: row.child_height,
                child_block_hash,
                child_block_time: row.child_block_time,
                btc_parent_header_hash: display_hash(&row.parent_hash)?,
                event_parent_kind: kind_as_str(row.kind),
                btc_parent_coinbase_txid: display_hash_opt(row.btc_parent_coinbase_txid)?,
                btc_parent_coinbase_script_hex: row.btc_parent_coinbase_script.map(hex::encode),
                btc_parent_coinbase_outputs_hex: row.btc_parent_coinbase_outputs.map(hex::encode),
                child_coinbase_txid: display_hash_opt(row.child_coinbase_txid)?,
                child_coinbase_script_hex: row.child_coinbase_script.map(hex::encode),
                aux_merkle_proof_hex: row.aux_merkle_proof.map(hex::encode),
                chain_id,
                slot_index,
                aux_proof,
                rsk,
                pow_validates_btc_target: row.pow_validates_btc_target,
                pow_validates_child_target: row.pow_validates_child_target,
                difficulty_epoch_ok: row.difficulty_epoch_ok,
                event_discovered_at: row.event_discovered_at,
                event_confirmed_at: row.event_confirmed_at,
                event_revoked_at: row.event_revoked_at,
                event_revocation_reason: row.event_revocation_reason,
                child_miner_pool: row.child_miner_pool,
                pool_attributions: pool_attributions_by_event
                    .get(&row.id)
                    .cloned()
                    .unwrap_or_default(),
            })
        })
        .collect()
}

/// Render an `RskEvidenceRow` into the wire `RskEventDetail`. Hashes/proofs are
/// hex-encoded in stored byte order (RSKIP-92 midstate form, NOT
/// Display-reversed, constraint #5); `uncle_parent_height` surfaces as
/// `uncle_referencing_height`.
pub(super) fn render_rsk_detail(row: RskEvidenceRow) -> RskEventDetail {
    RskEventDetail {
        block_hash: hex::encode(row.block_hash),
        height: row.height,
        is_uncle: row.is_uncle,
        uncle_index: row.uncle_index,
        miner_address: hex::encode(row.miner),
        pool_identity: row.pool_identity,
        merge_mining_hash: hex::encode(row.merge_mining_hash),
        merkle_proof_hex: row.merkle_proof.map(hex::encode),
        coinbase_tail_hex: row.coinbase_tail.map(hex::encode),
        proof_format: row.proof_format,
        uncle_referencing_height: row.uncle_parent_height,
    }
}

/// Deserialize 80 stored bytes into the wire `HeaderProjection`. prev_hash /
/// merkle_root go out display-order (rust-bitcoin Display reverses, constraint
/// #5); `bits` is 8-hex-digit compact. A malformed header is a 500 (internal,
/// not 404).
pub(super) fn header_projection(bytes: &[u8]) -> Result<HeaderProjection, ProjectionError> {
    let header: Header = deserialize(bytes).context("deserialize Bitcoin parent header")?;
    Ok(HeaderProjection {
        version: header.version.to_consensus(),
        prev_hash: header.prev_blockhash.to_string(),
        merkle_root: header.merkle_root.to_string(),
        time: header.time,
        bits: format!("{:08x}", header.bits.to_consensus()),
        nonce: header.nonce,
    })
}

/// Collect the de-duplicated, ascending event-id set from all auxpow proofs'
/// `contributing_event_ids` evidence. Non-auxpow proofs are skipped; an auxpow
/// proof missing the field is a hard error. This is the canonical/stale path's
/// event selector (feeds `load_event_details_by_ids`).
pub(super) fn contributing_event_ids(proofs: &[ApiProof]) -> Result<Vec<i64>> {
    let mut ids = BTreeSet::new();
    for proof in proofs {
        if proof.kind != "auxpow" {
            continue;
        }
        let Some(values) = proof
            .evidence
            .get("contributing_event_ids")
            .and_then(Value::as_array)
        else {
            bail!("auxpow proof evidence is missing contributing_event_ids");
        };
        for value in values {
            let id = value
                .as_i64()
                .context("auxpow contributing_event_ids member is not an i64")?;
            ids.insert(id);
        }
    }
    Ok(ids.into_iter().collect())
}

/// The first `contributing_event_ids` member of a proof's evidence, or `None`.
/// The secondary sort key in `load_proof_details_for_hash` (after source code)
/// that keeps the wire `proofs` order deterministic for equal-code proofs.
pub(super) fn first_contributing_id(evidence: &Value) -> Option<i64> {
    evidence
        .get("contributing_event_ids")
        .and_then(Value::as_array)
        .and_then(|values| values.first())
        .and_then(Value::as_i64)
}

/// Adapt an `EventDetailRow` down to the neutral `EventRow` consumed by the
/// shared `source_summary_for_block` materializer (the api->materialize boundary).
/// Intentionally lossy: only the summary-relevant fields are retained.
pub(super) fn event_row_from_detail(row: &EventDetailRow) -> EventRow {
    EventRow {
        id: row.id,
        source: row.source.clone(),
        child_height: row.child_height,
        parent_hash: row.parent_hash.clone(),
        prev_hash: row.prev_hash.clone(),
        header_time: row.parent_header_time,
        kind: row.kind,
        pow_validates_btc_target: row.pow_validates_btc_target,
        child_miner_pool: row.child_miner_pool.clone(),
    }
}

/// Display-hex an optional stored hash (None passes through, Some is converted
/// via `display_hash`, constraint #5). Small adapter used for the optional
/// coinbase txid fields in `render_event_details`.
pub(super) fn display_hash_opt(bytes: Option<Vec<u8>>) -> Result<Option<String>> {
    bytes.map(|bytes| display_hash(&bytes)).transpose()
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::*;
    use crate::projection::block::SourceRef;
    use crate::projection::shared::{SourceRecord, unknown_pool};

    #[test]
    fn elastos_is_namecoin_family_with_chain_id_1224() {
        // Block-detail projection wiring: Elastos is a standard fabe6d6d CAuxPow
        // chain, so it must project the parent commitment / slot_index / aux_proof
        // (NamecoinFamily) and the cited chain id 1224 (verified vs live blocks).
        assert_eq!(chain_family(Some("elastos")), ChainFamily::NamecoinFamily);
        assert_eq!(chain_id_for_chain("elastos"), Some(1224));
    }

    #[test]
    fn contributing_event_ids_only_require_auxpow_evidence_shape() {
        let auxpow = api_proof("auxpow", json!({ "contributing_event_ids": [3, 1, 3] }));
        let observation = api_proof("live-chaintip", json!({ "first_seen_by": "node-a" }));

        let ids = contributing_event_ids(&[observation, auxpow]).unwrap();

        assert_eq!(ids, vec![1, 3]);
    }

    #[test]
    fn contributing_event_ids_reject_malformed_auxpow_evidence() {
        let proof = api_proof("auxpow", json!({ "other": [] }));

        let err = contributing_event_ids(&[proof]).unwrap_err();

        assert!(
            err.to_string()
                .contains("auxpow proof evidence is missing contributing_event_ids")
        );
    }

    fn api_proof(kind: &str, evidence: Value) -> ApiProof {
        ApiProof {
            kind: kind.to_owned(),
            source: SourceRef {
                id: 1,
                code: "auxpow:namecoin".to_owned(),
                kind: "auxpow".to_owned(),
                chain: Some("namecoin".to_owned()),
                instance: None,
            },
            discovered_at: 0,
            confirmed_at: 0,
            revoked_at: None,
            revocation_reason: None,
            pow_validates_btc_target: true,
            evidence,
        }
    }

    fn event_row(id: i64, chain: &str) -> EventDetailRow {
        EventDetailRow {
            id,
            source: SourceRecord {
                id,
                code: format!("auxpow:{chain}"),
                kind: "auxpow".to_owned(),
                chain: Some(chain.to_owned()),
            },
            child_height: 1,
            child_block_hash: vec![0u8; 32],
            child_block_time: 1_600_000_000,
            parent_hash: vec![0u8; 32],
            prev_hash: vec![0u8; 32],
            parent_header_bytes: vec![0u8; 80],
            parent_header_time: 1_600_000_000,
            kind: ParentKind::Unknown,
            btc_parent_coinbase_txid: None,
            btc_parent_coinbase_script: None,
            btc_parent_coinbase_outputs: None,
            child_coinbase_txid: None,
            child_coinbase_script: None,
            aux_merkle_proof: None,
            pow_validates_btc_target: true,
            pow_validates_child_target: None,
            difficulty_epoch_ok: None,
            event_discovered_at: 1_600_000_000,
            event_confirmed_at: 1_600_000_000,
            event_revoked_at: None,
            event_revocation_reason: None,
            child_miner_pool: unknown_pool(),
            rsk: None,
        }
    }

    fn synthetic_marker_script() -> Vec<u8> {
        let mut script = vec![0x03, 0xab, 0x77, 0x0e]; // arbitrary prefix
        script.extend_from_slice(&[0xfa, 0xbe, 0x6d, 0x6d]);
        script.extend_from_slice(&[0x11u8; 32]); // aux_merkle_root
        script.extend_from_slice(&8u32.to_le_bytes()); // merkle_size
        script.extend_from_slice(&3u32.to_le_bytes()); // merkle_nonce
        script
    }

    #[test]
    fn commitment_hathor_is_gated_from_namecoin_marker() {
        // Hathor's reconstructed coinbase can contain fabe6d6d; it must NOT be
        // scanned, so the commitment stays hathor-rfc0006 with a null marker.
        let mut h = event_row(1, "hathor");
        h.btc_parent_coinbase_script = Some(synthetic_marker_script());
        let c = derive_commitment(&[h]).expect("commitment");
        assert_eq!(c.format, "hathor-rfc0006");
        assert!(c.marker.is_none());
    }

    #[test]
    fn commitment_namecoin_no_marker_keeps_namecoin_aux() {
        let mut n = event_row(1, "namecoin");
        n.btc_parent_coinbase_script = Some(vec![0x03, 0xe5, 0xf5, 0x05]); // placeholder
        let c = derive_commitment(&[n]).expect("commitment");
        assert_eq!(c.format, "namecoin-aux");
        assert!(c.marker.is_none());
    }

    #[test]
    fn commitment_mixed_namecoin_and_rsk_is_namecoin_aux() {
        let mut n = event_row(2, "namecoin");
        n.btc_parent_coinbase_script = Some(vec![0x03, 0xe5, 0xf5, 0x05]);
        let r = event_row(1, "rsk"); // lower id, but family priority beats id
        let c = derive_commitment(&[r, n]).expect("commitment");
        assert_eq!(c.format, "namecoin-aux");
        assert!(c.marker.is_none());
    }

    #[test]
    fn commitment_namecoin_with_marker_decodes() {
        let mut n = event_row(1, "namecoin");
        n.btc_parent_coinbase_script = Some(synthetic_marker_script());
        let c = derive_commitment(&[n]).expect("commitment");
        assert_eq!(c.format, "namecoin-aux");
        let marker = c.marker.expect("marker");
        assert!(marker.magic_present);
        assert_eq!(marker.merkle_size, 8);
        assert_eq!(marker.merkle_nonce, 3);
    }

    #[test]
    fn slot_index_is_gated_on_parent_hash_match() {
        let raw = hex::decode(
            include_str!("../../../../../fixtures/fractal/fb-1342257-getblockheader-auxpow.hex")
                .trim(),
        )
        .expect("decode Fractal fixture hex");
        let parsed = mmm_capture::auxpow::parse_auxpow_header_blob(&raw).expect("parse blob");
        let parent_hash = parsed.parent_header.hash().to_byte_array().to_vec();

        // Matching parent hash -> slot_index surfaces; fractal chain_id is uncited.
        let mut ok = event_row(1, "fractal");
        ok.aux_merkle_proof = Some(parsed.auxpow_bytes.clone());
        ok.parent_hash = parent_hash;
        let rendered = render_event_details(vec![ok], &HashMap::new()).expect("render");
        assert!(rendered[0].slot_index.is_some());
        assert_eq!(rendered[0].chain_id, None);

        // Mismatched parent hash -> slot_index None (coherence guard fires).
        let mut bad = event_row(1, "fractal");
        bad.aux_merkle_proof = Some(parsed.auxpow_bytes);
        bad.parent_hash = vec![0xffu8; 32];
        let rendered = render_event_details(vec![bad], &HashMap::new()).expect("render");
        assert!(rendered[0].slot_index.is_none());
    }
}
