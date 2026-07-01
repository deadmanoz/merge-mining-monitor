# RSK Fixtures

These fixtures are RSKj JSON-RPC block `result` objects shaped like
`eth_getBlockByNumber(..., false)` and
`eth_getUncleByBlockNumberAndIndex(...)` responses. Numeric fields stay as
`0x`-prefixed quantities, and merge-mining evidence fields stay as the opaque
`0x`-prefixed byte strings returned by RSKj.

Cases:

- `canonical-valid` - canonical block with a known miner and a Bitcoin parent
  header that satisfies its own target, so capture classifies it as `unknown`
  without Bitcoin-chain proof.
- `canonical-near` - canonical block with an unknown miner and a Bitcoin parent
  header that fails its own target, so capture classifies it as `near`.
- `uncle-valid` - uncle/ommer block used to verify stored uncle context.
- `canonical-with-uncles` - canonical block listing two uncles for traversal and
  backfill-order tests.
- `uncle-second-miner` - second-miner uncle/canonical sample used for traversal
  and pool-identity progression tests.
- `pre-rskip92` - block with a non-80-byte merge-mining header sentinel.
- `malformed-header` - block whose merge-mining header field is invalid hex.
