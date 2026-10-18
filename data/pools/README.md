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
- `child-identities/*.json` - per-chain child identity registries (`rsk_miner_registry.json`, `namecoin_payout_address_registry.json`, `syscoin_payout_address_registry.json`, `fractal_reward_address_registry.json`, `qbit_payout_address_registry.json`, `hathor_reward_registry.json`, `elastos_reward_address_registry.json`, `elastos_minerinfo_registry.json`), validated by `crates/mmm-capture/src/identity_registry.rs`. See `docs/attribution.md`.

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
