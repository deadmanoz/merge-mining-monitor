# Product Brief

The monitor collects and presents evidence about Bitcoin stale blocks: valid
blocks the network mined but did not keep. Merge-mined (AuxPoW) child chains
embed Bitcoin parent headers in their own blocks, so they preserve independent
records of Bitcoin work, including work the Bitcoin chain itself discarded. The
product is aimed at researchers, mining analysts, and operators who want to see
which Bitcoin blocks lost a fork race, who mined them, and which child chains
preserved the proof.

## First Screen

The first screen is an analytical workspace that opens directly on the data,
not a marketing page:

- a windowed Bitcoin header tree for canonical, stale, unknown, and near
  parent headers;
- a control rail for classification and source filtering plus direct height
  and date/time lookup;
- a detail drawer for selected blocks and stale or orphan branches.

## Why Multiple AuxPoW Sources

Different child chains preserve different evidence shapes:

- Namecoin and Syscoin carry classic Namecoin-family AuxPoW with parent
  coinbase evidence.
- RSK commits to Bitcoin parent headers through Ethereum-style JSON-RPC
  payloads, adds uncle context, and attributes work to miner beneficiary
  addresses rather than payout scripts.
- Fractal Bitcoin is hybrid-mined: only one block in each three-block cycle is
  merge-mined, so skipped blocks are expected rather than missing evidence.
- Hathor commits through its own RFC 0006 merged-mining layout, served over a
  public REST API.
- Elastos reconstructs an 84-byte child header around its AuxPoW commitment.

The product contract is therefore multi-source from the start. A single-chain
model would hide real differences in child-chain target validation, pool
attribution, uncle context, opaque proof bytes, and partial merge-mined
coverage.

The same contract extends beyond the live producers. Recovered historical
datasets from dead or dormant merge-mined chains enter the same evidence path,
and merge-mined chains whose data has not been recovered are catalogued so the
census of AuxPoW chains stays visible.

## Why API Contracts Over CSV Semantics

The UI does not infer product meaning from exported CSV rows. The API contract
defines stable JSON objects, source identity, nullability, error envelopes, and
fixture scenarios, so the backend and the frontend can evolve independently
while preserving the same language for parent headers, event details, proofs,
pools, and stale branches.

## Interpretation Comes From Bitcoin Classification

Child-chain producers record what they can verify locally: `near` when the
embedded Bitcoin parent header fails Bitcoin target validation, and `unknown`
when it passes but lacks Bitcoin-chain proof. The `live-chaintip:bitcoin:core`
source is the Bitcoin Core classifier that turns captured evidence into
interpretation: canonical and stale verdicts, same-height competition, branch
context, and the strict/weak orphan refinement for valid blocks Bitcoin Core
has never seen.

## Multi-Block Stale Branches

Stale evidence is not always a one-block event. The UI and API represent branch
roots, tips, depth, members, and same-height canonical competitors, so
multi-block stale branches are first-class rather than a retrofit.

## RSK-Specific Product Language

RSK evidence needs explicit UI language because it differs from Namecoin:

- RSK rows may come from canonical RSK blocks or uncle blocks.
- RSK miner-address attribution is not the same as Namecoin parent coinbase or
  payout-address attribution.
- RSK proof bytes are opaque and use `proof_format = "rskj_rpc_opaque"`.
- RSK cannot populate Namecoin-only generic fields such as parent coinbase
  script, child coinbase script, AuxPoW proof bytes, or child target
  validation.

## Fractal-Specific Product Language

Fractal Bitcoin evidence needs explicit UI language because not every Fractal
block is merge-mined:

- Fractal Bitcoin mainnet launched on 2024-09-09.
- Fractal uses a hybrid mining design in which one block in each three-block
  cycle is Bitcoin-merge-mined and the other two are mined permissionlessly on
  Fractal.
- Fractal rows represent only blocks that carry the merge-mining proof; skipped
  non-merge-mined blocks are expected and should not be presented as source
  downtime or missing evidence.
- Fractal's raw AuxPoW proof path carries the child header and CAuxPoW, not the
  child transaction vector. Live capture and historical replay pair that proof
  with full child block bytes and record child reward attribution as
  `fractal_reward_address` rows.
- Unknown Fractal reward addresses remain visible as unresolved child-side
  attribution. Known reward addresses late-fill through `pool_identity`.

## Out Of Scope

The product does not define CSV export, RSS or SSE feeds, or runtime node
operations. Bitcoin Core integration currently covers backbone sync and parent
classification; a continuously-following live stale observer is future work.
