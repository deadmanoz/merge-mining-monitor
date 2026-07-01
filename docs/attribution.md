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
| `data/pools/child-identities/hathor_reward_registry.json` | `hathor_reward_address` | Hathor child reward address. |
| `data/pools/child-identities/elastos_reward_address_registry.json` | `elastos_reward_address` | Elastos child reward address. |
| `data/pools/child-identities/elastos_minerinfo_registry.json` | `elastos_minerinfo` | Elastos self-declared minerinfo label. |

Registries are intentionally conservative. A child identifier is promoted when
it is stable enough to be useful and has evidence beyond a single coincidental
same-event Bitcoin parent tag. Ambiguous, rotating, or spoofable identifiers
stay unresolved.

## Reclassification

`just reclassify-pools` replays attribution from stored evidence and the
embedded registries. It is fill-null-only by default; pass `--overwrite` only
when intentionally re-attributing existing registry-backed rows.

Use `--only` or `--skip-*` flags to limit expensive phases when only one
registry changed.

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
