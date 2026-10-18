//! Chain-aware child payout address formatting for AuxPoW child outputs.
//!
//! Uses chain-specific base58/bech32 parameters rather than `bitcoin::Address`
//! so Namecoin and Syscoin outputs never collide with Bitcoin pool identities.

use std::collections::HashMap;

use bitcoin::bech32::{Hrp, segwit};
use bitcoin::{Script, TxOut, base58};
use serde::Deserialize;

use crate::source_registry::{
    FRACTAL_SOURCE_CODE, NAMECOIN_SOURCE_CODE, QBIT_SOURCE_CODE, SYSCOIN_SOURCE_CODE,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChildPayoutParams {
    pub namespace: &'static str,
    pub p2pkh_version: Option<u8>,
    pub p2sh_version: Option<u8>,
    pub bech32_hrp: &'static str,
    /// Restrict witness outputs to this (version, program length), when set.
    pub witness_program: Option<(u8, usize)>,
}

impl ChildPayoutParams {
    /// Shared by script formatting and curated-address validation.
    pub fn accepts_witness_program(self, version: u8, length: usize) -> bool {
        self.witness_program
            .is_none_or(|expected| expected == (version, length))
    }
}

pub const NAMECOIN_PAYOUT_ADDRESS_NAMESPACE: &str = "namecoin_payout_address";
pub const SYSCOIN_PAYOUT_ADDRESS_NAMESPACE: &str = "syscoin_payout_address";
pub const FRACTAL_REWARD_ADDRESS_NAMESPACE: &str = "fractal_reward_address";
/// Qbit native P2MR payout address, kept separate from Bitcoin parent evidence.
pub const QBIT_PAYOUT_ADDRESS_NAMESPACE: &str = "qbit_payout_address";

pub const NAMECOIN_CHILD_PAYOUT_PARAMS: ChildPayoutParams = ChildPayoutParams {
    namespace: NAMECOIN_PAYOUT_ADDRESS_NAMESPACE,
    p2pkh_version: Some(52),
    p2sh_version: Some(13),
    bech32_hrp: "nc",
    witness_program: None,
};

pub const SYSCOIN_CHILD_PAYOUT_PARAMS: ChildPayoutParams = ChildPayoutParams {
    namespace: SYSCOIN_PAYOUT_ADDRESS_NAMESPACE,
    p2pkh_version: Some(63),
    p2sh_version: Some(5),
    bech32_hrp: "sys",
    witness_program: None,
};

pub const FRACTAL_CHILD_REWARD_PARAMS: ChildPayoutParams = ChildPayoutParams {
    namespace: FRACTAL_REWARD_ADDRESS_NAMESPACE,
    p2pkh_version: Some(0),
    p2sh_version: Some(5),
    bech32_hrp: "bc",
    witness_program: None,
};

/// Qbit mainnet pays witness-v2, 32-byte P2MR programs under HRP `qb`.
pub const QBIT_CHILD_PAYOUT_PARAMS: ChildPayoutParams = ChildPayoutParams {
    namespace: QBIT_PAYOUT_ADDRESS_NAMESPACE,
    p2pkh_version: None,
    p2sh_version: None,
    bech32_hrp: "qb",
    witness_program: Some((2, 32)),
};

#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ChildPayoutConfidence {
    High,
    #[default]
    Medium,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PoolIdentityRef {
    pub pool_id: i64,
    pub pool_identity_id: i64,
    /// Used by payout attribution only; other identity paths keep their policy.
    pub payout_confidence: ChildPayoutConfidence,
}

pub type PoolIdentityLookup = HashMap<(String, String), PoolIdentityRef>;

pub fn params_for_source_code(source_code: &str) -> Option<ChildPayoutParams> {
    match source_code {
        NAMECOIN_SOURCE_CODE => Some(NAMECOIN_CHILD_PAYOUT_PARAMS),
        SYSCOIN_SOURCE_CODE => Some(SYSCOIN_CHILD_PAYOUT_PARAMS),
        FRACTAL_SOURCE_CODE => Some(FRACTAL_CHILD_REWARD_PARAMS),
        QBIT_SOURCE_CODE => Some(QBIT_CHILD_PAYOUT_PARAMS),
        _ => None,
    }
}

pub fn pool_identity_lookup_key(namespace: &str, identifier: &str) -> (String, String) {
    (namespace.to_owned(), identifier.to_owned())
}

/// Format standard outputs in a child coinbase as de-duplicated chain-native addresses.
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

/// Decode one standard output script into a chain-native address (`P2PKH` hash160 at `bytes[3..23]`,
/// `P2SH` hash160 at `bytes[2..22]`, or witness program at `bytes[2..]`).
pub fn format_child_script_address(script: &Script, params: ChildPayoutParams) -> Option<String> {
    let bytes = script.as_bytes();
    if script.is_p2pkh() {
        return params
            .p2pkh_version
            .map(|version| base58_address(version, &bytes[3..23]));
    }
    if script.is_p2sh() {
        return params
            .p2sh_version
            .map(|version| base58_address(version, &bytes[2..22]));
    }
    if let Some(version) = script.witness_version() {
        let program = &bytes[2..];
        if !params.accepts_witness_program(version.to_num(), program.len()) {
            return None;
        }
        let hrp = Hrp::parse(params.bech32_hrp).ok()?;
        return segwit::encode(hrp, version.to_fe(), program).ok();
    }
    None
}

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
    fn formats_native_qbit_p2mr_addresses_and_skips_unsupported_outputs() {
        // Independent native decodescript vectors, captured 2026-10-18.
        for (script, address) in [
            (
                "5220092f12207f98da2b526313dd72f21bce4ca5a7b301ca1a108c04a219557c17d1",
                "qb1zpyh3ygrlnrdzk5nrz0wh9usmeex2tfanq89p5yyvqj3pj4tuzlgs4hc5uh",
            ),
            (
                "5220435c094ddfe9d8b877e564b16210ddacfe7a43d4301547649a64deba3f5faddc",
                "qb1zgdwqjnwla8vtsal9vjckyyxa4nl85s75xq25wey6vn0t506l4hwqy0p2u9",
            ),
            (
                "5220419c3df36e130967cbae0f9e7cf57e7b44d1dc823bc078b08109467c4bd92f30",
                "qb1zgxwrmumwzvyk0jawp708eat70dzdrhyz80q83vypp9r8cj7e9ucqlwfe8r",
            ),
            (
                "5220b03468bcf890af33db25ad082d971ca912b29c68b3340451f368e0d2550dfbd4",
                "qb1zkq6x308cjzhn8ke945yzm9cu4yft98rgkv6qg50ndrsdy4gdl02qzvytm3",
            ),
        ] {
            let script = ScriptBuf::from_hex(script).unwrap();
            assert_eq!(
                format_child_script_address(&script, QBIT_CHILD_PAYOUT_PARAMS).as_deref(),
                Some(address)
            );
        }
        for script in [
            p2pkh_script([0; 20]),
            p2sh_script([0; 20]),
            ScriptBuf::from_bytes([vec![0x51, 0x20], vec![0; 32]].concat()),
            ScriptBuf::from_bytes([vec![0x52, 0x14], vec![0; 20]].concat()),
            ScriptBuf::from_bytes(vec![0x6a]),
        ] {
            assert_eq!(
                format_child_script_address(&script, QBIT_CHILD_PAYOUT_PARAMS),
                None
            );
        }
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
