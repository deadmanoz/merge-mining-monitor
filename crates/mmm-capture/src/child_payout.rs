//! Chain-aware child payout address formatting for AuxPoW child outputs.
//!
//! These helpers intentionally do not use `bitcoin::Address`: Namecoin and
//! Syscoin have their own address parameters, and formatting child outputs as
//! Bitcoin addresses would create false pool identities.

use std::collections::HashMap;

use bitcoin::bech32::{Hrp, segwit};
use bitcoin::{Script, TxOut, base58};

use crate::source_registry::{FRACTAL_SOURCE_CODE, NAMECOIN_SOURCE_CODE, SYSCOIN_SOURCE_CODE};

/// Per-chain address-encoding parameters for formatting an AuxPoW child
/// coinbase output as a chain-native address (NOT a Bitcoin address). Carries
/// the base58check version bytes for P2PKH and P2SH and the bech32 HRP for
/// witness outputs, plus the `event_pool_attribution.namespace` under which a
/// matched payout address is keyed. One const per supported child chain;
/// resolved from a source code via [`params_for_source_code`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChildPayoutParams {
    pub namespace: &'static str,
    pub p2pkh_version: u8,
    pub p2sh_version: u8,
    pub bech32_hrp: &'static str,
}

/// `event_pool_attribution.namespace` for a Namecoin child coinbase payout
/// address. Persisted string key, bound to the matched address as
/// `(namespace, matched_value)`. Do not rename without a coordinated data
/// migration.
pub const NAMECOIN_PAYOUT_ADDRESS_NAMESPACE: &str = "namecoin_payout_address";
/// `event_pool_attribution.namespace` for a Syscoin child coinbase payout
/// address. Persisted string key; do not rename without a data migration.
pub const SYSCOIN_PAYOUT_ADDRESS_NAMESPACE: &str = "syscoin_payout_address";
/// `event_pool_attribution.namespace` for a Fractal Bitcoin child coinbase
/// reward address. Persisted string key; do not rename without a data
/// migration. (Fractal pays the reward to a standard bc1 address, hence the
/// reward rather than payout wording.)
pub const FRACTAL_REWARD_ADDRESS_NAMESPACE: &str = "fractal_reward_address";

/// Namecoin mainnet address parameters: base58 P2PKH version 52 (`M...`), P2SH
/// version 13, bech32 HRP `nc`. Verified against the formatting test vectors in
/// this file.
pub const NAMECOIN_CHILD_PAYOUT_PARAMS: ChildPayoutParams = ChildPayoutParams {
    namespace: NAMECOIN_PAYOUT_ADDRESS_NAMESPACE,
    p2pkh_version: 52,
    p2sh_version: 13,
    bech32_hrp: "nc",
};

/// Syscoin mainnet address parameters: base58 P2PKH version 63 (`S...`), P2SH
/// version 5 (`3...`), bech32 HRP `sys`. Verified against the formatting test
/// vectors in this file.
pub const SYSCOIN_CHILD_PAYOUT_PARAMS: ChildPayoutParams = ChildPayoutParams {
    namespace: SYSCOIN_PAYOUT_ADDRESS_NAMESPACE,
    p2pkh_version: 63,
    p2sh_version: 5,
    bech32_hrp: "sys",
};

/// Fractal Bitcoin address parameters, identical to Bitcoin mainnet: base58
/// P2PKH version 0, P2SH version 5, bech32 HRP `bc`. Fractal reuses Bitcoin's
/// address format, so reward addresses render as `bc1...`; verified against
/// public mempool block samples in this file's tests.
pub const FRACTAL_CHILD_REWARD_PARAMS: ChildPayoutParams = ChildPayoutParams {
    namespace: FRACTAL_REWARD_ADDRESS_NAMESPACE,
    p2pkh_version: 0,
    p2sh_version: 5,
    bech32_hrp: "bc",
};

/// Resolved pool identity for a child payout/reward address: the database
/// `pool_id` plus the specific `pool_identity_id` (the address-level identity
/// row). Looked up by `(namespace, identifier)` to attach a concrete pool to a
/// formatted child address.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PoolIdentityRef {
    pub pool_id: i64,
    pub pool_identity_id: i64,
}

/// Map from `(namespace, identifier)` to a resolved [`PoolIdentityRef`], built
/// once per replay from the pool-identity table (`mmm-store`) and consulted
/// while formatting child payout addresses. Key via [`pool_identity_lookup_key`]
/// so callers cannot drift on tuple construction.
pub type PoolIdentityLookup = HashMap<(String, String), PoolIdentityRef>;

/// Map a `source_registry` source code to its [`ChildPayoutParams`], or `None`
/// for chains with no child payout/reward attribution. The match arms key off
/// the canonical NAMECOIN/SYSCOIN/FRACTAL source codes, keeping this in step
/// with the source registry. The `None` arm is the explicit no-child-payout
/// signal, not an error.
pub fn params_for_source_code(source_code: &str) -> Option<ChildPayoutParams> {
    match source_code {
        NAMECOIN_SOURCE_CODE => Some(NAMECOIN_CHILD_PAYOUT_PARAMS),
        SYSCOIN_SOURCE_CODE => Some(SYSCOIN_CHILD_PAYOUT_PARAMS),
        FRACTAL_SOURCE_CODE => Some(FRACTAL_CHILD_REWARD_PARAMS),
        _ => None,
    }
}

/// Build the [`PoolIdentityLookup`] key `(namespace, identifier)` from borrowed
/// strs. The single canonical key constructor: callers (lookup builds in
/// mmm-store, consults in capture/producers) route through it so the tuple format
/// and ownership cannot drift between insert and get.
pub fn pool_identity_lookup_key(namespace: &str, identifier: &str) -> (String, String) {
    (namespace.to_owned(), identifier.to_owned())
}

/// Format every standard output in a child coinbase as a chain-native address
/// under `params`, de-duplicated and in first-seen order. Non-standard outputs
/// (OP_RETURN, bare scripts) yield no address and are skipped. Pure: decodes the
/// script template only, never touches the network.
pub fn child_output_addresses(outputs: &[TxOut], params: ChildPayoutParams) -> Vec<String> {
    let mut addresses = Vec::new();
    for output in outputs {
        if let Some(address) = format_child_script_address(&output.script_pubkey, params)
            && !addresses.contains(&address)
        {
            addresses.push(address);
        }
    }
    addresses
}

/// Decode one standard output script into a chain-native address, or `None` for
/// non-standard scripts. Byte layout is fixed by the standard templates: P2PKH
/// (`OP_DUP OP_HASH160 <0x14> hash160 OP_EQUALVERIFY OP_CHECKSIG`) takes the
/// 20-byte hash at `bytes[3..23]` and base58check-encodes it with
/// `p2pkh_version`; P2SH (`OP_HASH160 <0x14> hash160 OP_EQUAL`) takes
/// `bytes[2..22]` with `p2sh_version`; a witness program is `bytes[2..]` (after
/// the version opcode and push-length byte) bech32-encoded under `bech32_hrp` at
/// its witness version. The offsets assume the canonical templates that
/// rust-bitcoin's `is_p2pkh`/`is_p2sh`/`witness_version` already validate; do
/// not change them.
pub fn format_child_script_address(script: &Script, params: ChildPayoutParams) -> Option<String> {
    let bytes = script.as_bytes();
    if script.is_p2pkh() {
        return Some(base58_address(params.p2pkh_version, &bytes[3..23]));
    }
    if script.is_p2sh() {
        return Some(base58_address(params.p2sh_version, &bytes[2..22]));
    }
    if let Some(version) = script.witness_version() {
        let program = &bytes[2..];
        let hrp = Hrp::parse(params.bech32_hrp).ok()?;
        return segwit::encode(hrp, version.to_fe(), program).ok();
    }
    None
}

/// Base58check-encode a 20-byte hash with a one-byte version prefix: prepend
/// `version`, append `payload`, then `base58::encode_check` (which adds the
/// 4-byte double-SHA256 checksum). The version byte selects the chain/address
/// type (e.g. Namecoin P2PKH 52, Syscoin P2PKH 63); the input bytes are used
/// verbatim, never reversed.
fn base58_address(version: u8, payload: &[u8]) -> String {
    let mut prefixed = Vec::with_capacity(payload.len() + 1);
    prefixed.push(version);
    prefixed.extend_from_slice(payload);
    base58::encode_check(&prefixed)
}

#[cfg(test)]
mod tests {
    use bitcoin::blockdata::opcodes::all::OP_RETURN;
    use bitcoin::blockdata::script::Builder;
    use bitcoin::hashes::Hash as _;
    use bitcoin::script::PushBytesBuf;
    use bitcoin::{Amount, PubkeyHash, ScriptBuf, ScriptHash, WPubkeyHash};

    use super::*;

    fn p2pkh_script(hash: [u8; 20]) -> ScriptBuf {
        ScriptBuf::new_p2pkh(&PubkeyHash::from_slice(&hash).unwrap())
    }

    fn p2sh_script(hash: [u8; 20]) -> ScriptBuf {
        ScriptBuf::new_p2sh(&ScriptHash::from_slice(&hash).unwrap())
    }

    #[test]
    fn formats_namecoin_mainnet_standard_addresses() {
        assert_eq!(
            format_child_script_address(&p2pkh_script([0; 20]), NAMECOIN_CHILD_PAYOUT_PARAMS),
            Some("MvaNCeVyvP6ZXYFWGpKaDX9ujEQ418F7sm".to_owned())
        );
        assert_eq!(
            format_child_script_address(&p2sh_script([0x11; 20]), NAMECOIN_CHILD_PAYOUT_PARAMS),
            Some("6Fx5jVAN9MirDjMbikRJGe2M6Fy4SEUPeC".to_owned())
        );
        let witness = ScriptBuf::new_p2wpkh(&WPubkeyHash::from_slice(&[0; 20]).unwrap());
        assert_eq!(
            format_child_script_address(&witness, NAMECOIN_CHILD_PAYOUT_PARAMS),
            Some("nc1qqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqz4pnrn".to_owned())
        );
    }

    #[test]
    fn formats_syscoin_mainnet_standard_addresses() {
        assert_eq!(
            format_child_script_address(&p2pkh_script([0; 20]), SYSCOIN_CHILD_PAYOUT_PARAMS),
            Some("SMJ12qn9jNCCXJnTYRz5Yu9ZenERqvYwfg".to_owned())
        );
        assert_eq!(
            format_child_script_address(&p2sh_script([0x11; 20]), SYSCOIN_CHILD_PAYOUT_PARAMS),
            Some("33FFrcn4Tv1qgGEuXPkkPdr44DuWp3RzPo".to_owned())
        );
        let witness = ScriptBuf::new_p2wpkh(&WPubkeyHash::from_slice(&[0; 20]).unwrap());
        assert_eq!(
            format_child_script_address(&witness, SYSCOIN_CHILD_PAYOUT_PARAMS),
            Some("sys1qqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqdf0wuz".to_owned())
        );
    }

    #[test]
    fn formats_fractal_reward_addresses_like_public_mempool_samples() {
        let block_1342257 = ScriptBuf::new_p2wpkh(
            &WPubkeyHash::from_slice(
                &hex::decode("457f14b3701cc5c807935b0a4839d7f42879bcfd").unwrap(),
            )
            .unwrap(),
        );
        assert_eq!(
            format_child_script_address(&block_1342257, FRACTAL_CHILD_REWARD_PARAMS),
            Some("bc1qg4l3fvmsrnzuspuntv9yswwh7s58n08a59y3l7".to_owned())
        );

        let f2pool_sample = ScriptBuf::new_p2wpkh(
            &WPubkeyHash::from_slice(
                &hex::decode("0ac26fa8e79a2b8f6d1ab7c5c2adfd1e7d3b83e9").unwrap(),
            )
            .unwrap(),
        );
        assert_eq!(
            format_child_script_address(&f2pool_sample, FRACTAL_CHILD_REWARD_PARAMS),
            Some("bc1qptpxl288ng4c7mg6klzu9t0are7nhqlfmtmk9k".to_owned())
        );
    }

    #[test]
    fn skips_nonstandard_outputs_and_deduplicates_addresses() {
        let standard = TxOut {
            value: Amount::from_sat(1),
            script_pubkey: p2pkh_script([0; 20]),
        };
        let op_return = TxOut {
            value: Amount::from_sat(0),
            script_pubkey: Builder::new()
                .push_opcode(OP_RETURN)
                .push_slice(PushBytesBuf::try_from(vec![1, 2, 3]).unwrap())
                .into_script(),
        };
        let addresses = child_output_addresses(
            &[standard.clone(), op_return, standard],
            NAMECOIN_CHILD_PAYOUT_PARAMS,
        );
        assert_eq!(addresses, ["MvaNCeVyvP6ZXYFWGpKaDX9ujEQ418F7sm"]);
    }
}
