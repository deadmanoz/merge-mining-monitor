//! Bitcoin coinbase OP_RETURN commitments for protocols that are not merge-mined child chains:
//! Core DAO delegated proof of work (`CORE`: a validator and a reward address) and exSat
//! (`EXSAT`: a synchronizer account). These are declarations by whoever built the coinbase:
//! they name protocol accounts, not who authored the block template.
//!
//! CORE follows Core DAO's relayer parser (coredao-org/btcpowermirror 0c9db0d,
//! `lightmirror/btclightmirrorv2.go` `ParsePowerParams`): it skips vout 0, needs at least 47
//! script bytes with `OP_RETURN` at 0, `CORE` at 2..6 and version 1 at 6, reads the validator at
//! 7..27, the reward address at 27..47 and a binding hash at 47..79 when present, and does not
//! check the push byte. exSat follows exsat-network/contract-of-consensus 543e74f
//! (`contracts/internal/utils.hpp` `get_op_return_eos_account`): one push covering the whole
//! script, `EXSAT` and version 1, then 1 to 12 account symbols not ending in `.`, at any vout.
//! Both parsers take the first match, and exSat's caller (`blksync.xsat` `find_miner`) also takes
//! an earlier output whose payout address is registered to a synchronizer, so which declaration
//! exSat credits cannot be decided from the coinbase alone. This decoder reports every recognized
//! output, marks the one Core DAO's parser reads, and accepts only the exact layout as `Valid`,
//! so a malformed declaration keeps its raw script and no identity.

use bitcoin::TxOut;
use bitcoin::consensus::{deserialize, encode};
use bitcoin::opcodes::all::{OP_PUSHDATA1, OP_PUSHDATA2, OP_PUSHDATA4};

/// Bump when a change alters what an existing script decodes to, so stored commitments are
/// rebuilt.
pub const DECODER_VERSION: i32 = 1;

const CORE_MAGIC: &[u8] = b"CORE";
const EXSAT_MAGIC: &[u8] = b"EXSAT";
const SUPPORTED_VERSION: u8 = 1;
/// Where both upstream parsers expect the magic: after `OP_RETURN` and a direct push byte.
const DIRECT_MAGIC_AT: usize = 2;
/// `OP_RETURN`, a 45-byte push, `CORE`, version 1.
const CORE_HEADER: &[u8] = b"\x6a\x2dCORE\x01";
/// The header, a 20-byte validator and a 20-byte reward address.
const CORE_SCRIPT_LEN: usize = CORE_HEADER.len() + 40;
const CORE_WITH_BINDING_HASH_LEN: usize = CORE_SCRIPT_LEN + 32;
const EXSAT_VERSION_AT: usize = DIRECT_MAGIC_AT + EXSAT_MAGIC.len();
const EXSAT_MAX_ACCOUNT_LEN: usize = 12;
const EXSAT_ALPHABET: &[u8; 32] = b"abcdefghijklmnopqrstuvwxyz12345.";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommitmentProtocol {
    Core,
    Exsat,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommitmentPayload {
    Core {
        validator: [u8; 20],
        reward_address: [u8; 20],
        binding_hash: Option<[u8; 32]>,
    },
    Exsat {
        account: String,
    },
}

/// How a recognized script departs from the exact layout; an upstream parser may still accept it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MalformedReason {
    /// The magic is not in a direct push at byte 2 of the expected length.
    WrongPushLength,
    /// A CORE push whose payload repeats the CORE header (five Braiins Pool blocks in 2024);
    /// Core DAO's parser reads header bytes as the validator.
    NestedHeader,
    TrailingBytes,
    Truncated,
    AccountByteOutOfRange,
    /// No account symbols, more than 12, or a trailing `.`.
    InvalidAccount,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DecodeStatus {
    Valid(CommitmentPayload),
    Malformed(MalformedReason),
    /// The version byte after the magic, when it is not 1.
    UnsupportedVersion(u8),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProtocolCommitment {
    pub vout: u32,
    pub protocol: CommitmentProtocol,
    pub script: Vec<u8>,
    pub status: DecodeStatus,
    /// For CORE, whether Core DAO's parser takes this output, whatever `status` says; `None` for
    /// exSat, whose choice the coinbase cannot decide.
    pub upstream_selected: Option<bool>,
}

/// Decode the CORE and exSat commitments among coinbase `outputs`, in output order.
pub fn decode_protocol_commitments(outputs: &[TxOut]) -> Vec<ProtocolCommitment> {
    let mut commitments: Vec<ProtocolCommitment> = outputs
        .iter()
        .enumerate()
        .filter_map(|(vout, output)| {
            if !output.script_pubkey.is_op_return() {
                return None;
            }
            let script = output.script_pubkey.as_bytes();
            let (protocol, version_at) = recognize(script)?;
            let status = match script.get(version_at) {
                None => DecodeStatus::Malformed(MalformedReason::Truncated),
                Some(&SUPPORTED_VERSION) => match protocol {
                    CommitmentProtocol::Core => decode_core(script, version_at),
                    CommitmentProtocol::Exsat => decode_exsat(script, version_at),
                }
                .map_or_else(DecodeStatus::Malformed, DecodeStatus::Valid),
                Some(&version) => DecodeStatus::UnsupportedVersion(version),
            };
            Some(ProtocolCommitment {
                vout: u32::try_from(vout).ok()?,
                protocol,
                script: script.to_vec(),
                status,
                upstream_selected: (protocol == CommitmentProtocol::Core).then_some(false),
            })
        })
        .collect();
    // Core DAO takes the first CORE output from vout 1 that passes its check.
    if let Some(selected) = commitments
        .iter_mut()
        .find(|c| c.upstream_selected.is_some() && c.vout > 0 && core_dao_reads(&c.script))
    {
        selected.upstream_selected = Some(true);
    }
    commitments
}

/// `ParsePowerParams`'s check, which ignores the push byte (the caller has checked `OP_RETURN`).
fn core_dao_reads(script: &[u8]) -> bool {
    script.len() >= CORE_SCRIPT_LEN
        && script[DIRECT_MAGIC_AT..CORE_HEADER.len()] == CORE_HEADER[DIRECT_MAGIC_AT..]
}

/// Decode consensus-serialized `Vec<TxOut>` coinbase outputs, as stored in Postgres.
pub fn decode_serialized_protocol_commitments(
    serialized: &[u8],
) -> Result<Vec<ProtocolCommitment>, encode::Error> {
    let outputs: Vec<TxOut> = deserialize(serialized)?;
    Ok(decode_protocol_commitments(&outputs))
}

/// The protocol whose magic starts the script's first push, and where its version byte is.
/// Byte 2 is checked first because Core DAO reads it whatever the push byte says.
fn recognize(script: &[u8]) -> Option<(CommitmentProtocol, usize)> {
    let pushdata_at = match script.get(1).copied()? {
        opcode if opcode == OP_PUSHDATA1.to_u8() => Some(3),
        opcode if opcode == OP_PUSHDATA2.to_u8() => Some(4),
        opcode if opcode == OP_PUSHDATA4.to_u8() => Some(6),
        _ => None,
    };
    std::iter::once(DIRECT_MAGIC_AT)
        .chain(pushdata_at)
        .find_map(|magic_at| {
            let rest = script.get(magic_at..)?;
            if rest.starts_with(EXSAT_MAGIC) {
                Some((CommitmentProtocol::Exsat, magic_at + EXSAT_MAGIC.len()))
            } else if rest.starts_with(CORE_MAGIC) {
                Some((CommitmentProtocol::Core, magic_at + CORE_MAGIC.len()))
            } else {
                None
            }
        })
}

fn decode_core(script: &[u8], version_at: usize) -> Result<CommitmentPayload, MalformedReason> {
    // The exact layout wins, so a validator that happens to begin with the header stays valid.
    let exact = script.starts_with(CORE_HEADER);
    let binding_hash = match script.len() {
        CORE_SCRIPT_LEN if exact => None,
        CORE_WITH_BINDING_HASH_LEN if exact => {
            Some(script[CORE_SCRIPT_LEN..].try_into().expect("32 bytes"))
        }
        _ if script[version_at + 1..].starts_with(CORE_HEADER) => {
            return Err(MalformedReason::NestedHeader);
        }
        _ if !exact => return Err(MalformedReason::WrongPushLength),
        len if len < CORE_SCRIPT_LEN => return Err(MalformedReason::Truncated),
        _ => return Err(MalformedReason::TrailingBytes),
    };
    Ok(CommitmentPayload::Core {
        validator: script[7..27].try_into().expect("20 bytes"),
        reward_address: script[27..47].try_into().expect("20 bytes"),
        binding_hash,
    })
}

fn decode_exsat(script: &[u8], version_at: usize) -> Result<CommitmentPayload, MalformedReason> {
    if version_at != EXSAT_VERSION_AT {
        return Err(MalformedReason::WrongPushLength);
    }
    let declared = usize::from(script[1]);
    if declared > script.len() - 2 {
        return Err(MalformedReason::Truncated);
    }
    if declared < script.len() - 2 {
        return Err(MalformedReason::TrailingBytes);
    }
    let symbols = &script[version_at + 1..];
    if symbols.is_empty() || symbols.len() > EXSAT_MAX_ACCOUNT_LEN {
        return Err(MalformedReason::InvalidAccount);
    }
    let account = symbols
        .iter()
        .map(|symbol| {
            EXSAT_ALPHABET
                .get(usize::from(*symbol))
                .copied()
                .map(char::from)
        })
        .collect::<Option<String>>()
        .ok_or(MalformedReason::AccountByteOutOfRange)?;
    if account.ends_with('.') {
        return Err(MalformedReason::InvalidAccount);
    }
    Ok(CommitmentPayload::Exsat { account })
}

#[cfg(test)]
mod tests {
    use bitcoin::ScriptBuf;
    use bitcoin::opcodes::all::OP_RETURN;

    use super::DecodeStatus::{Malformed, UnsupportedVersion, Valid};
    use super::MalformedReason::*;
    use super::*;

    const VALIDATOR: &str = "a21cbd3caa4fe89bccd1d716c92ce4533e4d4733";
    const ANTPOOL_REWARD: &str = "4e3ecda72cb7961caa4b541b1e322bcfe0b5a030";
    /// A valid exSat declaration of the account `ant`.
    const EXSAT_ANT: &str = "6a09455853415401000d13";

    /// Real coinbase outputs; `fixtures/bitcoin/README.md` describes each block.
    fn fixture_commitments(height: u32) -> Vec<ProtocolCommitment> {
        let hex_outputs = match height {
            920_500 => include_str!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../fixtures/bitcoin/coinbase-outputs-920500.hex"
            )),
            868_400 => include_str!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../fixtures/bitcoin/coinbase-outputs-868400.hex"
            )),
            853_776 => include_str!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../fixtures/bitcoin/coinbase-outputs-853776.hex"
            )),
            _ => unreachable!("no fixture for {height}"),
        };
        decode_serialized_protocol_commitments(&hex::decode(hex_outputs.trim()).unwrap()).unwrap()
    }

    fn core(reward_address: &str, binding_hash: Option<[u8; 32]>) -> DecodeStatus {
        Valid(CommitmentPayload::Core {
            validator: hex::decode(VALIDATOR).unwrap().try_into().unwrap(),
            reward_address: hex::decode(reward_address).unwrap().try_into().unwrap(),
            binding_hash,
        })
    }

    fn exsat(account: &str) -> DecodeStatus {
        let account = account.to_owned();
        Valid(CommitmentPayload::Exsat { account })
    }

    #[test]
    fn decodes_real_coinbase_outputs() {
        let f2pool_reward = "e7ec323813c943336c579e238228a8ebd096a7e5";
        let cases = [
            (
                920_500,
                vec![(2, core(ANTPOOL_REWARD, None)), (3, exsat("antpoole.sat"))],
            ),
            (
                868_400,
                vec![(3, core(f2pool_reward, None)), (4, exsat("f2pool.sat"))],
            ),
            (853_776, vec![(3, Malformed(NestedHeader))]),
        ];
        for (height, expected) in cases {
            let decoded = fixture_commitments(height)
                .into_iter()
                .map(|c| (c.vout, c.status));
            assert_eq!(decoded.collect::<Vec<_>>(), expected, "block {height}");
        }
    }

    fn classify_one(vout: usize, script_hex: &str) -> Option<DecodeStatus> {
        let mut outputs = vec![TxOut::NULL; vout + 1];
        outputs[vout].script_pubkey = ScriptBuf::from_bytes(hex::decode(script_hex).unwrap());
        decode_protocol_commitments(&outputs)
            .pop()
            .map(|c| c.status)
    }

    #[test]
    fn classifies_layout_edges() {
        let payload = format!("{VALIDATOR}{ANTPOOL_REWARD}");
        let core_script = format!("6a2d434f524501{payload}");
        // A valid validator that begins with the CORE header bytes.
        let header_validator = format!("6a2d434f524501{}", "00".repeat(13));
        let cases = [
            (
                format!("{core_script}{}", "11".repeat(32)),
                core(ANTPOOL_REWARD, Some([0x11; 32])),
            ),
            (format!("6a2d434f524502{payload}"), UnsupportedVersion(2)),
            (
                format!("6a4c2d434f524501{payload}"),
                Malformed(WrongPushLength),
            ),
            (core_script[..60].to_owned(), Malformed(Truncated)),
            (format!("{core_script}abcdef"), Malformed(TrailingBytes)),
            (
                format!("6a2d434f524501{header_validator}{ANTPOOL_REWARD}"),
                Valid(CommitmentPayload::Core {
                    validator: hex::decode(&header_validator).unwrap().try_into().unwrap(),
                    reward_address: hex::decode(ANTPOOL_REWARD).unwrap().try_into().unwrap(),
                    binding_hash: None,
                }),
            ),
            (EXSAT_ANT.to_owned(), exsat("ant")),
            (
                "6a0a45585341540100201312".to_owned(),
                Malformed(AccountByteOutOfRange),
            ),
            (
                "6a0a455853415401000d131f".to_owned(),
                Malformed(InvalidAccount),
            ),
            (
                format!("6a13455853415401{}", "00".repeat(13)),
                Malformed(InvalidAccount),
            ),
            ("6a0c455853415401000d13".to_owned(), Malformed(Truncated)),
            (
                "6a08455853415401000d13".to_owned(),
                Malformed(TrailingBytes),
            ),
        ];
        for (script, expected) in cases {
            assert_eq!(classify_one(1, &script), Some(expected), "{script}");
        }
        assert_eq!(classify_one(0, EXSAT_ANT), Some(exsat("ant")), "any vout");
    }

    #[test]
    fn marks_the_core_output_core_dao_reads() {
        let core_script = format!("6a2d434f524501{VALIDATOR}{ANTPOOL_REWARD}");
        let version_2 = format!("6a2d434f524502{VALIDATOR}{ANTPOOL_REWARD}");
        let outputs = [
            &core_script,
            &version_2,
            &core_script,
            &core_script,
            EXSAT_ANT,
        ]
        .map(|script| TxOut {
            script_pubkey: ScriptBuf::from_bytes(hex::decode(script).unwrap()),
            ..TxOut::NULL
        });
        let selected = decode_protocol_commitments(&outputs)
            .into_iter()
            .map(|c| c.upstream_selected);
        assert_eq!(
            selected.collect::<Vec<_>>(),
            [Some(false), Some(false), Some(true), Some(false), None],
            "vout 0 is kept but never selected"
        );
        let nested = &fixture_commitments(853_776)[0];
        assert_eq!(
            nested.upstream_selected,
            Some(true),
            "Core DAO reads the nested header bytes"
        );
    }

    /// Deterministic corpus: every truncation of the real commitment scripts, every byte of them
    /// replaced by boundary values, and seeded random `OP_RETURN` scripts up to 120 bytes.
    #[test]
    fn generated_corpus_never_panics() {
        let mut scripts: Vec<Vec<u8>> = Vec::new();
        for height in [920_500, 868_400, 853_776] {
            for commitment in fixture_commitments(height) {
                let script = commitment.script;
                scripts.extend((0..=script.len()).map(|len| script[..len].to_vec()));
                for position in 0..script.len() {
                    for value in [0x00, 0x01, 0x2d, 0x4c, 0x4d, 0x4e, 0x6a, 0xff] {
                        let mut mutated = script.clone();
                        mutated[position] = value;
                        scripts.push(mutated);
                    }
                }
            }
        }
        let mut state = 0x9e37_79b9_7f4a_7c15_u64;
        for len in 1..=120 {
            let mut script: Vec<u8> = (0..len)
                .map(|_| {
                    state ^= state << 13;
                    state ^= state >> 7;
                    state ^= state << 17;
                    state.to_le_bytes()[0]
                })
                .collect();
            script[0] = OP_RETURN.to_u8();
            scripts.push(script);
        }
        for script in scripts {
            let mut outputs = [TxOut::NULL, TxOut::NULL];
            outputs[1].script_pubkey = ScriptBuf::from_bytes(script);
            decode_protocol_commitments(&outputs);
        }
    }
}
