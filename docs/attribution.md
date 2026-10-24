# Attribution

Attribution answers two related questions:

1. Which Bitcoin mining pool appears in the parent coinbase?
2. Which child-chain reward, payout, or miner identity appears in the merge-mined child block?

The project keeps those facts separate.
Captured evidence records what a source said; registry data maps stable identifiers to pool identities later.

## BTC Pool Snapshot

`data/pools/current.json` is the embedded Bitcoin pool registry.
It is generated from the public [bitcoin-data/mining-pools](https://github.com/bitcoin-data/mining-pools) dataset and committed so capture and reclassification do not depend on a live attribution service.

The resolver matches:

- Bitcoin coinbase tags
- exact Bitcoin payout addresses

Slug stability matters because `pool.slug` is the stable public identity.
An upstream filename rename must not create a new pool slug accidentally.

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

Registries are intentionally conservative.
A child identifier is promoted when it is stable enough to be useful and has evidence beyond a single coincidental same-event Bitcoin parent tag.
Ambiguous, rotating, or spoofable identifiers stay unresolved.

Qbit child coinbase tags use the same embedded pool snapshot as the other Bitcoin-style child blocks.
Capture verifies the full child transaction merkle root and native coinbase height before storing its txid, script, and outputs.
A recognized tag records a child-side declaration independently of the Bitcoin parent pool; an unrecognized tag stays unresolved.
The Qbit payout registry also resolves authenticated child outputs with HRP `qb`, witness version 2 and a 32-byte program.
Unsupported output types are skipped.

Child payout registry entries may specify `confidence: "high"` or `"medium"`; omitting it preserves the existing medium rating.
This metadata is applied at bootstrap to matching, database-backed identities without additional queries or a confidence column; unknown payout addresses remain medium-confidence observations without a pool identity.
Matching child tag and payout rows remain separate evidence for the same pool, while conflicting known child pools project to Unknown.

## Protocol Account Associations

Some Bitcoin coinbases also carry `OP_RETURN` declarations for protocols that are not merge-mined child chains: Core DAO delegated proof of work (`CORE`: a validator and a reward address) and exSat (`EXSAT`: a synchronizer account).
The read model decodes them into `protocol_commitment` rows (see `docs/data-model.md`), keeping malformed and unsupported-version declarations with their raw script.

A declaration names a protocol account.
It does not establish who built the block template, who controls the account, or which account the protocol credited.
Core DAO validators serve many unrelated pools and are not mapped.
exSat can credit an earlier output whose payout address is registered to a synchronizer, so which declaration exSat credits cannot be decided from the coinbase, and `upstream_selected` is NULL for exSat; for CORE it marks the output Core DAO's relayer reads.
Realised rewards, claims and processing freshness are outside this layer.

| Registry | Namespace | Meaning |
|---|---|---|
| `data/pools/protocol-accounts/core_reward_address_registry.json` | `core_reward_address` | Declared CORE reward address (`0x` plus 40 lowercase hex). |
| `data/pools/protocol-accounts/exsat_account_registry.json` | `exsat_account` | Declared exSat synchronizer account. |

Every entry records its basis and dated evidence:

- `first_party_registration`: the protocol's own listing of the account for the pool, with its source and date. Every exSat entry comes from exSat's mainnet synchronizer configuration; `xsatsync.sat`, the exSat Foundation's own synchronizer, is not mapped.
- `empirical_association`: at least 20 Core-attested canonical blocks carry the identifier, and at least 90% of those whose miner resolves resolve to the mapped pool (payout-attributed blocks count for their pool), with block counts and first and last heights as of the registry's `generated_on` date. Seeding refuses an entry that falls below this rule, and conflicted identifiers stay unmapped.

Mappings are not time-bounded: an identifier seen changing hands is unmapped until time-bounded mappings exist.
The Bitcoin Core follower (`sync-bitcoin-core --follow`) seeds both namespaces at startup and makes them equal their registries, so removing an entry unmaps it at the next start.
No Bitcoin parent, child or event attribution reads these namespaces; a declared account's pool is resolved at read time.

A query of this shape counts canonical blocks per declared CORE reward address against the pool that mined them, which is what the empirical rule weighs:

```sql
SELECT s.account_identifier AS reward_address,
       declared.slug AS declared_account_pool,
       miner.slug AS bitcoin_miner_pool,
       count(*) AS blocks,
       min(b.btc_height) AS first_height,
       max(b.btc_height) AS last_height
  FROM protocol_commitment c
  JOIN protocol_commitment_script s ON s.id = c.script_id
  JOIN block b ON b.btc_header_hash = c.btc_header_hash
  LEFT JOIN pool miner ON miner.id = b.bitcoin_miner_pool_id
  LEFT JOIN pool_identity i
         ON (i.namespace, i.identifier) = (s.account_namespace, s.account_identifier)
  LEFT JOIN pool declared ON declared.id = i.pool_id
 WHERE s.protocol = 'core' AND s.decode_status = 'valid' AND c.upstream_selected
   AND b.kind = 'canonical' AND b.core_attested
 GROUP BY 1, 2, 3
 ORDER BY blocks DESC;
```

## Relationship Signals

A Bitcoin parent names pools in several independent places:
- its coinbase tag and payout addresses;
- each merge-mined child block's reward address or child coinbase tag;
- the protocol accounts its coinbase declares.

When those name different pools, the block was very likely built on another pool's jobs, for example a Binance Pool block whose RSK reward address is registered to AntPool. Attribution never compares them:
- the block miner takes a matching tag and ignores payout addresses;
- the display miner lets a known Bitcoin pool win;
- conflicting child pools collapse to Unknown.

The relationship-signals layer lists every one of these names with its pool. It is computed at read time from rows the API already reads (`/api/v1/block/:hash` `relationship_signals`, tree node `relationship`; see `docs/api-contract.md`).

- **Kinds.**
  - `bitcoin_coinbase_tag` and `bitcoin_payout_address` come from the pool snapshot, applied to one copy of the parent coinbase. That copy is the block row's complete Core coinbase; otherwise the event or observation with structured outputs, then with published output text, then with a script alone, lowest id first.
  - `child_reward_address` and `child_coinbase_tag` come from each child block's attribution rows, or a near observation's identities and coinbase script. A child coinbase tag is a label the child declares: a Bitcoin-style pool tag in its coinbase script, or Elastos minerinfo.
  - `protocol_account` comes from each valid CORE or exSat declaration.
- **Mixed.** A parent is mixed when its reference pool and its signals name two or more distinct known pools. The reference pool is the block miner when known, else the first known signal. Several signals naming one pool count once, and Unknown pools never count.
- **Unchanged attribution.** The block miner, the display miner and every attribution row stay as they are; the layer only reads them.
- **Wording.** A signal is an association, never proof of who mined the block, built its template or controls an address or account.

## Reclassification

`just reclassify-pools` replays attribution from stored evidence and the embedded registries (for example `just reclassify-pools --only main --source auxpow:qbit` to enrich stored child outputs in bounded pages without node RPC calls).
It is fill-null-only by default; pass `--overwrite` only when intentionally re-attributing existing registry-backed rows.

Historical Bitcoin-parent replay uses structured serialized outputs when a full coinbase transaction supplied their values.
It otherwise falls back to the publication's preserved output text, decoding raw Bitcoin scriptPubKeys and using published payout addresses without requiring a scriptSig marker.

Use `--only` or `--skip-*` flags to limit expensive phases when only one registry changed.
The phases are `rsk`, `main`, `hathor`, `elastos` and `observations`: the main and RSK phases re-attribute evidence events, and the `observations` phase re-resolves the pools of child blocks no event backs (nears and child-only rows) from their stored identities and coinbases, the near parent pool included.

The main scan counts eligible events once at startup, then reports completed events, rate and ETA through the shared progress reporter at 30-second intervals, with a final summary on completion or failure.
The initial total is an estimate while producers add evidence; no snapshot lock spans the replay.

## Operator Clusters

Child-chain payout addresses show that merge-mining operations can be more concentrated than Bitcoin pool branding suggests.
A Bitcoin pool may outsource or proxy merge-mined child-chain operations through another operator's endpoint.

The monitor treats this as an attribution nuance, not a contradiction:

- Bitcoin parent attribution remains the parent coinbase pool.
- Child identity attribution records the child-chain operator identity when it is stable and evidenced.
- Public UI/API consumers can distinguish pool brand, child reward identity, and source provenance.
