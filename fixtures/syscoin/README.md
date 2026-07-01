# Syscoin Fixtures

These are real Syscoin Core raw `getblock <hash> 0` fixtures fetched from a
self-hosted Syscoin Core node.

Cases:

- `1972.bin` - the block immediately before `SYSCOIN_FIRST_AUXPOW_HEIGHT`;
  parses as `NonAuxpow`, not malformed.
- `1973.bin` - the first AuxPoW block on the current chain; parses with the
  shared Namecoin-family parser and exposes child height `1973`.
- `2248408.bin` - a modern AuxPoW block sample with current trailing payload
  format; parses with the same shared parser and requires the producer's RPC
  height hint because the child coinbase does not expose a BIP34-style height.
