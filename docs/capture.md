# Capture Sources

Every source follows the same high-level contract: fetch child-chain evidence,
verify enough of it locally to make it safe to store, normalize it into
`merge_mining_event`, and let the read model derive Bitcoin tree state.
Child height, hash, header, time, and `nBits` are independent optional evidence;
unavailable values remain `NULL`.

## Capture And Classification Flow

<p align="center">
  <img src="img/capture-classification-flow.png" width="820" alt="Per-child-block capture flow from poller cursor selection through AuxPoW verification, event storage, pool attribution, Bitcoin proof-of-work checks, Core classification, and read-model reconciliation" />
</p>

For a live child block, the poller advances from cursor selection to source
fetching, AuxPoW parsing, event insertion, sidecar insertion, and pool
attribution. Parent classification then splits on three checks: Bitcoin target
validation first, a pinned consensus-invalid error-block catalogue second, then
Bitcoin Core placement. Target failures become `near`; a target-valid catalogue
match becomes `error_block` with its mechanically derived rejection reason;
Core-known parents become `canonical` or `stale`; Core-absent, non-catalogued
target-valid parents remain `unknown` until a later
`reclassify-unknown-parents` pass can upgrade them. BTC orphan status is a
later refinement of Core-absent `unknown`
parents, not a separate parent kind, and it is gated by the operator-imported
`known_stale_block` membership: a header catalogued as a known stale is
`excluded` from strict/weak orphan classification rather than overclaimed.

## Live Sources

| Source | Capture path | Notes |
|---|---|---|
| Namecoin | Core-style raw block RPC: `getblock <hash> 0`. | Namecoin-family AuxPoW parser. |
| Syscoin | Core-style raw block RPC: `getblock <hash> 0`. | Same shared parser as Namecoin, with Syscoin activation/version gates. |
| Fractal Bitcoin | `getblockheader <hash> false true` for `[header][CAuxPoW]`, plus child block data when needed. | Fractal raw blocks do not carry inline CAuxPoW. |
| RSK | Ethereum-style RSKj JSON-RPC for canonical blocks and uncles. | Stores RSK proof sidecar data and miner beneficiary identity. |
| Hathor | Public REST API plus Hathor RFC 0006 merged-mining reconstruction. | No self-hosted mainnet node assumption; reward outputs are parsed from persisted funds graph data. |

RSK replays may refine role and optional proof fields, but an existing sidecar's
block identity, height, miner, merge-mining hash, proof format, and any two
non-null optional proof values must remain compatible. A contradictory replay
fails the whole event transaction, including historical provenance.

Hathor live capture can promote a matching height-only historical observation
to exact child-hash identity in place. Such a hashless row is not considered a
superseded prior when its Bitcoin parent matches the validated live block.
| Elastos | JSON-RPC `getblockbyheight`. | Reconstructs the 84-byte child header and verifies the AuxPoW commitment. |
| Bitcoin Core | `sync-bitcoin-core`. | Writes canonical backbone headers and coinbase evidence for tree browsing. |

## Polling And Backfill

Live pollers use `poll_cursor`, not `MAX(child_height)`, as progress state.
Cursor seeding order is:

1. explicit `<PREFIX>_START_HEIGHT`
2. persisted cursor
3. `tip - reorg_depth`

Backfills are bounded, idempotent over event identity, and do not move the live
cursor. Use the `just poll-CHAIN` and `just backfill-CHAIN START END` recipes
for `namecoin`, `rsk`, `syscoin`, `fractal`, `hathor`, and `elastos`.

## Shared Producer Rules

- Namecoin-family chains should extend the shared chain spec and AuxPoW family
  path.
- Divergent chains may have their own module, but still write through shared
  store and read-model entry points.
- Producers write base evidence and sidecars only. They do not maintain
  `block`, `attestation_proof`, or `source_health` directly.
- A captured `child_block_time` is the child block's own stamp, whichever field
  the chain commits (header `nTime` for the Namecoin family, the RSK block
  timestamp, the Hathor transaction timestamp). It is the source's claimed
  timestamp, read from a header, an RPC response, or a publication column, with
  no build ever observed and, outside the verified paths, nothing tying it to
  the merge-mining commitment. It is not when the child block was broadcast, and
  it does not identify which Bitcoin job carried the template, since one
  unchanged template can be committed into many successive jobs. Whoever set it
  need not be the Bitcoin pool either. See
  "What `child_block_time` Means" in
  `docs/data-model.md` before reading it as a clock on the Bitcoin block.
- Live capture is additive. Historical and partial publication sources reconcile
  as authoritative snapshots through the shared source lifecycle, while live
  publication imports never remove live events.
- Long child-chain backfills may run with `BITCOIN_RPC_URL` unset, then upgrade
  the deferred `unknown` parents afterward with
  `just reclassify-unknown-parents`. Dataset imports are stricter: see
  `docs/historical-ingest.md`.
