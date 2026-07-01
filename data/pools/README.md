# Pool Data

`current.json` is the embedded BTC pool registry used for live pool resolution.
It is generated reproducibly from the public
[bitcoin-data/mining-pools](https://github.com/bitcoin-data/mining-pools)
per-pool records and committed as a local runtime artifact so producers do not
depend on a live external attribution service.

The BTC pool registry resolves the *Bitcoin parent* pool, which every
merge-mined chain shares, so `current.json` has no chain coupling.

## Files

- `current.json` - the embedded snapshot (`schema_version: 1`). Embedded by
  `crates/mmm-capture/src/pool_resolver/btc.rs` via `include_str!`.
- `slug-map.json` - the pinned upstream-filename -> repo-slug remap. The slug is
  the stable DB key (`pool.slug` UNIQUE, `pool.id` GENERATED ALWAYS AS
  IDENTITY); an upstream filename rename must not silently mint a new slug. Only
  genuine remaps live here (`ocean -> ocean-xyz`, `braiins -> braiins-pool`);
  new pools default to their upstream filename stem.
- `child-identities/rsk_miner_registry.json` - the RSK miner-address identity
  registry (separate attribution path; see `docs/attribution.md`).
- `child-identities/namecoin_payout_address_registry.json` - the Namecoin payout
  address identity registry.
- `child-identities/syscoin_payout_address_registry.json` - the Syscoin payout
  address identity registry.
- `child-identities/fractal_reward_address_registry.json` - the Fractal reward
  address identity registry.
- `child-identities/hathor_reward_registry.json` - the Hathor reward-address identity registry.
  Entries are promoted only after local candidates are corroborated by external
  Hathor reward-output evidence and external Bitcoin coinbase tag evidence.
- `child-identities/elastos_reward_address_registry.json` - the Elastos reward
  address identity registry.
- `child-identities/elastos_minerinfo_registry.json` - the Elastos minerinfo
  identity registry. A label is promoted only when it is itself a documented
  Bitcoin coinbase signature for the pool the same-event BTC parent reconciled
  to (the identity is RPC-decoded, not AuxPoW-authenticated). See
  `docs/attribution.md`.

All child identity registries share the generic
validator in `crates/mmm-capture/src/identity_registry.rs` (schema version,
non-empty/whitespace, duplicate identifier, slug-canonical consistency); only the
per-chain identifier-format check differs.

## Regenerating

Regenerate with `just gen-pool-snapshot [POOLS_DIR] [-- ...args]`, e.g.

```
just gen-pool-snapshot path/to/mining-pools/pools --generated-at YYYY-MM-DD
```

Alternatively, set `MMM_POOLS_DIR` in local `.env` and omit the path:

```
just gen-pool-snapshot --generated-at YYYY-MM-DD
```

The generator is deterministic: given identical inputs and `--generated-at`, it
reproduces `current.json` byte-for-byte. It requires a clean upstream
`bitcoin-data/mining-pools` checkout. Regeneration prints a reviewable
added/removed/changed-slug diff. `--check` exits non-zero if `current.json`
drifted and writes nothing.

The BTC registry is derived from `bitcoin-data/mining-pools`, which is MIT
licensed. Keep the upstream license attribution when redistributing the data.

See `docs/attribution.md` for the slug-stability contract, regeneration
discipline, the cross-registry completeness invariant, and the offline
`reclassify-pools` historical re-resolution path.
