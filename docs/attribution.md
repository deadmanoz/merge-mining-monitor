# Attribution

Attribution answers two related questions:

1. Which Bitcoin mining pool appears in the parent coinbase?
2. Which child-chain reward, payout, or miner identity appears in the
   merge-mined child block?

The project keeps those facts separate. Captured evidence records what a source
said; registry data maps stable identifiers to pool identities later.

## BTC Pool Snapshot

`data/pools/current.json` is the embedded Bitcoin pool registry. It is generated
from the public [bitcoin-data/mining-pools](https://github.com/bitcoin-data/mining-pools)
dataset and committed so capture and reclassification do not depend on a live
attribution service.

The resolver matches:

- Bitcoin coinbase tags
- exact Bitcoin payout addresses

Slug stability matters because `pool.slug` is the stable public identity. An
upstream filename rename must not create a new pool slug accidentally.

## Child Identity Registries

Additional registries map chain-native identifiers to pools:

| Registry | Namespace | Meaning |
|---|---|---|
| `data/pools/child-identities/rsk_miner_registry.json` | `rsk_miner_address` | RSK miner beneficiary address. |
| `data/pools/child-identities/namecoin_payout_address_registry.json` | `namecoin_payout_address` | Namecoin child payout address. |
| `data/pools/child-identities/syscoin_payout_address_registry.json` | `syscoin_payout_address` | Syscoin child payout address. |
| `data/pools/child-identities/fractal_reward_address_registry.json` | `fractal_reward_address` | Fractal child reward address. |
| `data/pools/child-identities/qbit_payout_address_registry.json` | `qbit_payout_address` | Qbit native witness-v2 P2MR payout address. |
| `data/pools/child-identities/hathor_reward_registry.json` | `hathor_reward_address` | Hathor child reward address. |
| `data/pools/child-identities/elastos_reward_address_registry.json` | `elastos_reward_address` | Elastos child reward address. |
| `data/pools/child-identities/elastos_minerinfo_registry.json` | `elastos_minerinfo` | Elastos self-declared minerinfo label. |

Registries are intentionally conservative. A child identifier is promoted when
it is stable enough to be useful and has evidence beyond a single coincidental
same-event Bitcoin parent tag. Ambiguous, rotating, or spoofable identifiers
stay unresolved.

Qbit child coinbase tags use the same embedded pool snapshot as the other
Bitcoin-style child blocks. Capture verifies the full child transaction merkle
root and native coinbase height before storing its txid, script, and outputs.
A recognized tag records a child-side declaration independently of the Bitcoin
parent pool; an unrecognized tag stays unresolved. The Qbit payout registry
also resolves authenticated child outputs with HRP `qb`, witness version 2 and
a 32-byte program. Unsupported output types are skipped.

Child payout registry entries may specify `confidence: "high"` or `"medium"`;
omitting it preserves the existing medium rating. This metadata is applied at
bootstrap to matching, database-backed identities, without additional queries
or a confidence column. Unknown payout addresses remain medium-confidence
observations without a pool identity. Other identity mechanisms retain their
existing confidence policies.

The 2026-10-18 retained snapshot supports F2Pool at high confidence: all 4,482
observations with its exact payout script also carry F2Pool native child tags
and F2Pool Bitcoin-parent attribution. SECPOOL (1,245 observations) and Luxor
(1,070) have exclusive parent-pool correlations and remain medium confidence.
These are pool associations, not independent proof of wallet control. The
shared wallet spans six Bitcoin parent brands and remains unmapped. Matching
child tag and payout rows remain separate evidence for the same pool;
conflicting known child pools project to Unknown.

## Reclassification

`just reclassify-pools` replays attribution from stored evidence and the
embedded registries. It is fill-null-only by default; pass `--overwrite` only
when intentionally re-attributing existing registry-backed rows.

Historical Bitcoin-parent replay uses structured serialized outputs when a
full coinbase transaction supplied their values. It otherwise falls back to the
publication's preserved output text, decoding raw Bitcoin scriptPubKeys and
using published payout addresses without requiring a scriptSig marker.

Use `--only` or `--skip-*` flags to limit expensive phases when only one
registry changed.

After applying migration `0032` through the backup-first workflow, enrich
existing Qbit events with
`just reclassify-pools --only main --source auxpow:qbit`. This uses stored
child outputs in bounded pages and adds no per-event node RPC calls. It also
adds payout evidence to already tagged F2Pool events; a repeat writes no
unchanged attributions. Existing resolved payout rows retain the normal
`--overwrite` protection. The CLI's existing Core-cache startup is separate
from the offline replay loop. No native-block replay or one-off repair script
is required.

## Operator Clusters

Child-chain payout addresses show that merge-mining operations can be more
concentrated than Bitcoin pool branding suggests. A Bitcoin pool may outsource
or proxy merge-mined child-chain operations through another operator's endpoint.

The monitor treats this as an attribution nuance, not a contradiction:

- Bitcoin parent attribution remains the parent coinbase pool.
- Child identity attribution records the child-chain operator identity when it
  is stable and evidenced.
- Public UI/API consumers can distinguish pool brand, child reward identity, and
  source provenance.
