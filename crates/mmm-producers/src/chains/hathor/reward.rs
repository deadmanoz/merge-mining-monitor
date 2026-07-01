//! Hathor reward-output parsing from the persisted RFC 0006 funds graph.
//!
//! The sidecar stores `funds_graph = funds || graph` plus the accepted
//! `funds_graph_split`. Only the funds segment contains block outputs; the
//! graph segment is retained as audit context and must never be parsed as
//! output serialization.

use anyhow::{Result, ensure};
use bitcoin::base58;
use serde_json::{Value, json};

use mmm_capture::capture::{
    CHILD_COINBASE_OUTPUT_SOURCE, CHILD_PAYOUT_REGISTRY_SOURCE, EventPoolAttribution,
    PoolAttributionConfidence, PoolAttributionSide,
};
use mmm_capture::child_payout::{PoolIdentityLookup, pool_identity_lookup_key};

use crate::chains::hathor::address::{MAINNET_P2PKH_VERSION, MAINNET_P2SH_VERSION};

/// Pool-identity namespace under which decoded Hathor reward addresses are
/// keyed (shared by the registry seeding, capture, and reclassify replay).
pub const HATHOR_REWARD_ADDRESS_NAMESPACE: &str = "hathor_reward_address";

/// Hathor's `OP_GREATERTHAN_TIMESTAMP` opcode that closes a timelock prefix.
const OP_GREATERTHAN_TIMESTAMP: u8 = 0x6f;

/// Parsed Hathor block funds segment: the output list plus the funds/graph
/// length split it was decoded under.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HathorRewardParse {
    /// Funds-segment serialization version (big-endian leading u16).
    pub version: u16,
    pub outputs: Vec<HathorRewardOutput>,
    /// Length of the trailing graph segment (retained as audit context, never
    /// parsed as outputs).
    pub graph_len: usize,
    /// Length of the parsed funds segment (the accepted split offset).
    pub funds_len: usize,
}

impl HathorRewardParse {
    /// Distinct reward addresses in output order: only HTR (non-token,
    /// non-authority, standard-script) outputs qualify; deduplicated.
    pub fn reward_addresses(&self) -> Vec<String> {
        let mut addresses = Vec::new();
        for output in &self.outputs {
            if let Some(address) = output.reward_address()
                && !addresses.contains(address)
            {
                addresses.push(address.clone());
            }
        }
        addresses
    }

    /// All outputs (including skipped/non-reward ones) as the audit JSON array
    /// stored in the sidecar `reward_output_details`.
    pub fn output_details_json(&self) -> Value {
        Value::Array(
            self.outputs
                .iter()
                .map(HathorRewardOutput::to_audit_json)
                .collect(),
        )
    }

    /// The distinct reward addresses as the JSON array stored in the sidecar
    /// `reward_addresses`.
    pub fn reward_addresses_json(&self) -> Value {
        Value::Array(
            self.reward_addresses()
                .into_iter()
                .map(Value::String)
                .collect(),
        )
    }

    /// Child-side pool attributions, one per distinct reward address. A registry
    /// hit carries the resolved pool id (registry source, higher trust); an
    /// unmapped address is recorded address-only (coinbase-output source) so it
    /// stays a future-resolution candidate.
    pub fn reward_attributions(
        &self,
        identities: &PoolIdentityLookup,
    ) -> Vec<EventPoolAttribution> {
        let mut attributions = Vec::new();
        for address in self.reward_addresses() {
            let output_indexes = self
                .outputs
                .iter()
                .filter(|output| output.reward_address() == Some(&address))
                .map(|output| output.index)
                .collect::<Vec<_>>();
            let identity = identities
                .get(&pool_identity_lookup_key(
                    HATHOR_REWARD_ADDRESS_NAMESPACE,
                    &address,
                ))
                .copied();
            attributions.push(hathor_reward_address_attribution(
                address,
                identity,
                &output_indexes,
            ));
        }
        attributions
    }
}

/// One decoded funds-segment output, including the audit metadata for outputs
/// that do NOT qualify as a reward (token, authority, or nonstandard).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HathorRewardOutput {
    /// Position in the funds segment's output list.
    pub index: usize,
    pub value: i64,
    /// Raw `token_data` byte; the high bit is the authority flag, low 7 bits the
    /// token index.
    pub token_data: u8,
    pub authority: bool,
    pub token_index: u8,
    pub script_hex: String,
    /// `"P2PKH"`, `"P2SH"`, or `"nonstandard"`.
    pub script_type: &'static str,
    /// Decoded payout address, `None` for nonstandard scripts.
    pub address: Option<String>,
    /// Decoded timelock prefix, if present.
    pub timelock: Option<u32>,
    /// Why this output is NOT a reward (`authority_output`, `non_htr_token`,
    /// `nonstandard_script`); `None` means it qualifies.
    pub skipped_reason: Option<&'static str>,
}

impl HathorRewardOutput {
    /// The payout address iff this output is a clean HTR reward (token index 0,
    /// non-authority, standard script); `None` otherwise.
    fn reward_address(&self) -> Option<&String> {
        if self.token_data == 0 && !self.authority && self.skipped_reason.is_none() {
            self.address.as_ref()
        } else {
            None
        }
    }

    /// Full per-output record for the sidecar audit JSON (qualifying and
    /// skipped outputs alike).
    fn to_audit_json(&self) -> Value {
        json!({
            "output_index": self.index,
            "value": self.value,
            "token_data": self.token_data,
            "authority": self.authority,
            "token_index": self.token_index,
            "script_hex": self.script_hex,
            "script_type": self.script_type,
            "address": self.address,
            "timelock": self.timelock,
            "skipped_reason": self.skipped_reason,
        })
    }
}

/// Parse the funds segment (`funds_graph[..funds_graph_split]`) into outputs.
/// Only the funds segment is decoded; the graph tail is never read as output
/// data and the funds segment must be consumed exactly (trailing bytes error),
/// which is what stops a wrong split from parsing graph bytes as outputs.
pub fn parse_hathor_reward_outputs(
    funds_graph: &[u8],
    funds_graph_split: i32,
) -> Result<HathorRewardParse> {
    ensure!(
        funds_graph_split >= 0,
        "funds_graph_split must be non-negative"
    );
    let split = funds_graph_split as usize;
    ensure!(
        split <= funds_graph.len(),
        "funds_graph_split {split} exceeds funds_graph length {}",
        funds_graph.len()
    );
    let funds = &funds_graph[..split];
    ensure!(funds.len() >= 3, "Hathor funds segment is too short");

    let mut cursor = Cursor::new(funds);
    let version = cursor.read_u16()?;
    let output_count = usize::from(cursor.read_u8()?);
    let mut outputs = Vec::with_capacity(output_count);
    for index in 0..output_count {
        outputs.push(parse_output(index, &mut cursor)?);
    }
    ensure!(
        cursor.remaining() == 0,
        "Hathor funds segment has {} trailing bytes",
        cursor.remaining()
    );

    Ok(HathorRewardParse {
        version,
        outputs,
        graph_len: funds_graph.len() - split,
        funds_len: split,
    })
}

/// Decode one output: value, `token_data`, length-prefixed script. The
/// authority/token/nonstandard checks classify it and set `skipped_reason`
/// without dropping the output (it still appears in the audit JSON).
fn parse_output(index: usize, cursor: &mut Cursor<'_>) -> Result<HathorRewardOutput> {
    let value = cursor.read_output_value()?;
    let token_data = cursor.read_u8()?;
    let script_len = usize::from(cursor.read_u16()?);
    let script = cursor.take(script_len)?;
    let decoded = decode_hathor_script(script);
    let authority = (token_data & 0x80) != 0;
    let token_index = token_data & 0x7f;
    let skipped_reason = if authority {
        Some("authority_output")
    } else if token_index != 0 {
        Some("non_htr_token")
    } else if decoded.address.is_none() {
        Some("nonstandard_script")
    } else {
        None
    };

    Ok(HathorRewardOutput {
        index,
        value,
        token_data,
        authority,
        token_index,
        script_hex: hex::encode(script),
        script_type: decoded.script_type,
        address: decoded.address,
        timelock: decoded.timelock,
        skipped_reason,
    })
}

/// The script-type classification of a single output script.
#[derive(Debug, Clone, PartialEq, Eq)]
struct DecodedScript {
    script_type: &'static str,
    address: Option<String>,
    timelock: Option<u32>,
}

/// Classify a Hathor output script (after stripping any timelock prefix) as
/// P2PKH / P2SH (deriving the base58check address) or nonstandard.
fn decode_hathor_script(script: &[u8]) -> DecodedScript {
    let (script, timelock) = strip_timelock(script);
    if script.len() == 25
        && script[0] == 0x76
        && script[1] == 0xa9
        && script[2] == 0x14
        && script[23] == 0x88
        && script[24] == 0xac
    {
        return DecodedScript {
            script_type: "P2PKH",
            address: Some(base58_address(MAINNET_P2PKH_VERSION, &script[3..23])),
            timelock,
        };
    }
    if script.len() == 23 && script[0] == 0xa9 && script[1] == 0x14 && script[22] == 0x87 {
        return DecodedScript {
            script_type: "P2SH",
            address: Some(base58_address(MAINNET_P2SH_VERSION, &script[2..22])),
            timelock,
        };
    }
    DecodedScript {
        script_type: "nonstandard",
        address: None,
        timelock,
    }
}

/// Strip a leading Hathor timelock prefix (a 4-byte big-endian timestamp
/// pushed then `OP_GREATERTHAN_TIMESTAMP`, in either direct-push or `OP_PUSHDATA1`
/// form), returning the remaining script and the decoded timelock.
fn strip_timelock(script: &[u8]) -> (&[u8], Option<u32>) {
    if script.len() >= 6 && script[0] == 0x04 && script[5] == OP_GREATERTHAN_TIMESTAMP {
        let timelock = u32::from_be_bytes(script[1..5].try_into().expect("4-byte slice"));
        return (&script[6..], Some(timelock));
    }
    if script.len() >= 7
        && script[0] == 0x4c
        && script[1] == 0x04
        && script[6] == OP_GREATERTHAN_TIMESTAMP
    {
        let timelock = u32::from_be_bytes(script[2..6].try_into().expect("4-byte slice"));
        return (&script[7..], Some(timelock));
    }
    (script, None)
}

/// base58check-encode a `version || payload` (20-byte hash) into a Hathor
/// mainnet address string.
fn base58_address(version: u8, payload: &[u8]) -> String {
    let mut prefixed = Vec::with_capacity(payload.len() + 1);
    prefixed.push(version);
    prefixed.extend_from_slice(payload);
    base58::encode_check(&prefixed)
}

/// Build the child-block attribution for one reward address. A registry hit
/// (`identity.is_some()`) sets the resolved pool id and the registry source; an
/// unmapped address records the value only, under the coinbase-output source, so
/// it remains a candidate for a later registry update. Always Medium confidence.
fn hathor_reward_address_attribution(
    address: String,
    identity: Option<mmm_capture::child_payout::PoolIdentityRef>,
    output_indexes: &[usize],
) -> EventPoolAttribution {
    let mapped = identity.is_some();
    EventPoolAttribution {
        side: PoolAttributionSide::ChildBlock,
        namespace: HATHOR_REWARD_ADDRESS_NAMESPACE,
        match_kind: "reward_address",
        matched_value: address,
        pool_id: identity.map(|identity| identity.pool_id),
        pool_identity_id: identity.map(|identity| identity.pool_identity_id),
        source: if mapped {
            CHILD_PAYOUT_REGISTRY_SOURCE
        } else {
            CHILD_COINBASE_OUTPUT_SOURCE
        },
        confidence: PoolAttributionConfidence::Medium,
        details: json!({
            "address_source": "hathor_funds_graph",
            "sidecar": "hathor_merge_mining_evidence.funds_graph",
            "output_indexes": output_indexes,
        }),
    }
}

/// Forward-only big-endian cursor over the funds segment, bounds-checking every
/// read so truncated input errors instead of panicking.
struct Cursor<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    /// Take one byte.
    fn read_u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }

    /// Take a big-endian u16.
    fn read_u16(&mut self) -> Result<u16> {
        Ok(u16::from_be_bytes(
            self.take(2)?.try_into().expect("2-byte slice"),
        ))
    }

    /// Decode a Hathor output value: a 4-byte big-endian u32 when the high bit
    /// of the first byte is clear, otherwise an 8-byte big-endian value stored
    /// negated (so it must be negative and `i64::MIN` overflows on negation).
    fn read_output_value(&mut self) -> Result<i64> {
        let first = *self
            .buf
            .get(self.pos)
            .ok_or_else(|| anyhow::anyhow!("truncated Hathor output value"))?;
        if first & 0x80 == 0 {
            let value = u32::from_be_bytes(self.take(4)?.try_into().expect("4-byte slice"));
            return Ok(i64::from(value));
        }
        let encoded = i64::from_be_bytes(self.take(8)?.try_into().expect("8-byte slice"));
        ensure!(
            encoded < 0,
            "8-byte Hathor output value must be negative-encoded"
        );
        encoded
            .checked_neg()
            .ok_or_else(|| anyhow::anyhow!("8-byte Hathor output value overflows i64"))
    }

    /// Take exactly `n` bytes, erroring if the segment ends early.
    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        ensure!(
            self.pos
                .checked_add(n)
                .is_some_and(|end| end <= self.buf.len()),
            "truncated Hathor funds segment"
        );
        let start = self.pos;
        self.pos += n;
        Ok(&self.buf[start..self.pos])
    }

    /// Bytes not yet consumed; the parser requires this to be 0 at the end.
    fn remaining(&self) -> usize {
        self.buf.len() - self.pos
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture_funds_graph(name: &str) -> (Vec<u8>, i32) {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .join(name);
        let value: serde_json::Value =
            serde_json::from_str(std::fs::read_to_string(path).unwrap().as_str()).unwrap();
        let raw = hex::decode(value["raw_hex"].as_str().unwrap()).unwrap();
        let aux_pow = hex::decode(value["aux_pow_hex"].as_str().unwrap()).unwrap();
        let idx = raw
            .windows(aux_pow.len())
            .position(|window| window == aux_pow)
            .unwrap();
        (
            raw[..idx].to_vec(),
            value["expected_funds_graph_split"].as_i64().unwrap() as i32,
        )
    }

    #[test]
    fn parses_rest_checked_p2pkh_reward_from_fixture() {
        let (funds_graph, split) = fixture_funds_graph("fixtures/hathor/1971823.json");
        let parsed = parse_hathor_reward_outputs(&funds_graph, split).unwrap();
        assert_eq!(parsed.version, 3);
        assert_eq!(parsed.graph_len, funds_graph.len() - split as usize);
        assert_eq!(
            parsed.reward_addresses(),
            ["HV3iKMJpuZpktXwpoBxKEUetG6NS3zfXje"]
        );
        assert_eq!(parsed.outputs.len(), 1);
        let output = &parsed.outputs[0];
        assert_eq!(output.value, 3200);
        assert_eq!(output.token_data, 0);
        assert_eq!(output.script_type, "P2PKH");
        assert_eq!(output.timelock, None);
        assert_eq!(output.skipped_reason, None);
        assert_eq!(
            output.script_hex,
            "76a914f71b92054d1cdebfef315b71fadaa84b8dfcc9fc88ac"
        );
    }

    #[test]
    fn parses_later_rest_checked_reward_from_fixture() {
        let (funds_graph, split) = fixture_funds_graph("fixtures/hathor/4482931.json");
        let parsed = parse_hathor_reward_outputs(&funds_graph, split).unwrap();
        assert_eq!(
            parsed.reward_addresses(),
            ["HH5As5aLtzFkcbmbXZmE65wSd22GqPWq2T"]
        );
        assert_eq!(parsed.outputs[0].value, 800);
    }

    #[test]
    fn honors_funds_graph_split_and_rejects_graph_tail_as_funds() {
        let (funds_graph, split) = fixture_funds_graph("fixtures/hathor/1971823.json");
        parse_hathor_reward_outputs(&funds_graph, split).unwrap();
        let err = parse_hathor_reward_outputs(&funds_graph, funds_graph.len() as i32)
            .expect_err("graph tail must not parse as output data");
        assert!(err.to_string().contains("trailing bytes"));
    }

    #[test]
    fn rejects_minimum_negative_encoded_output_value() {
        let funds = [
            0x00, 0x03, 0x01, // version 3, one output
            0x80, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        ];
        let err = parse_hathor_reward_outputs(&funds, funds.len() as i32)
            .expect_err("minimum negative value cannot be converted safely");
        assert!(err.to_string().contains("overflows i64"));
    }

    #[test]
    fn audits_multiple_token_timelock_and_nonstandard_outputs() {
        let mut funds = vec![0x00, 0x03, 0x03];
        append_output(&mut funds, 1, 0, &p2pkh_script([0; 20], None));
        append_output(
            &mut funds,
            2,
            1,
            &p2pkh_script([1; 20], Some(1_700_000_000)),
        );
        append_output(&mut funds, 3, 0, &[0x6a, 0x01, 0x01]);
        let mut funds_graph = funds.clone();
        funds_graph.extend_from_slice(&[0xaa, 0xbb, 0xcc]);

        let parsed = parse_hathor_reward_outputs(&funds_graph, funds.len() as i32).unwrap();
        assert_eq!(
            parsed.reward_addresses(),
            ["H6X8PLvXQDY3iLaTynKkQ1tUBBJjSZSf23"]
        );

        let details = parsed.output_details_json();
        assert_eq!(details[0]["output_index"], 0);
        assert_eq!(details[0]["value"], 1);
        assert_eq!(details[0]["address"], "H6X8PLvXQDY3iLaTynKkQ1tUBBJjSZSf23");
        assert_eq!(details[0]["skipped_reason"], Value::Null);
        assert_eq!(details[1]["token_data"], 1);
        assert_eq!(details[1]["token_index"], 1);
        assert_eq!(details[1]["timelock"], 1_700_000_000u64);
        assert_eq!(details[1]["skipped_reason"], "non_htr_token");
        assert_eq!(details[2]["script_type"], "nonstandard");
        assert_eq!(details[2]["skipped_reason"], "nonstandard_script");
    }

    fn p2pkh_script(hash: [u8; 20], timelock: Option<u32>) -> Vec<u8> {
        let mut script = Vec::new();
        if let Some(timelock) = timelock {
            script.push(0x04);
            script.extend_from_slice(&timelock.to_be_bytes());
            script.push(OP_GREATERTHAN_TIMESTAMP);
        }
        script.extend_from_slice(&[0x76, 0xa9, 0x14]);
        script.extend_from_slice(&hash);
        script.extend_from_slice(&[0x88, 0xac]);
        script
    }

    fn append_output(buf: &mut Vec<u8>, value: u32, token_data: u8, script: &[u8]) {
        buf.extend_from_slice(&value.to_be_bytes());
        buf.push(token_data);
        buf.extend_from_slice(&(script.len() as u16).to_be_bytes());
        buf.extend_from_slice(script);
    }
}
