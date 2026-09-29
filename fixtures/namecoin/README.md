# Namecoin Fixtures

These fixtures are small synthetic Namecoin-family raw blocks. They are stored
as raw `.bin` block bytes so tests exercise the same byte-level parser that the
live `getblock <hash> 0` poller uses.

Each `.bin` file has a sibling `.expected.json` sidecar with the fields asserted
by the parser and capture-payload tests.

The parent headers build on an all-zero prev and carry synthetic bits (the
regtest limit where they meet their own target), so against Bitcoin's real
retarget history they are not Bitcoin headers and the capture lineage gate
refuses them. The integration tests seed a synthetic Bitcoin history from that
all-zero genesis in which they are (`seed_synthetic_fixture_history`).

Cases:

- `019199-non-auxpow` - child header does not carry the AuxPoW bit and should
  be skipped without error.
- `500000-valid-parent` - parent header passes its own target and remains
  `unknown` without Bitcoin-chain proof.
- `500001-near-parent` - parent header fails its own target and is classified
  as `near`. It also fails the child header's aux target on purpose, so this
  synthetic fixture exercises `pow_validates_child_target = false`; real
  Namecoin-accepted AuxPoW blocks should not look like this.
- `500002-wrong-chain-parent` - a second parent header that passes its own
  target, at the next child height.
- `500003-malformed` - child header carries the AuxPoW bit but the payload is
  truncated; parsing fails and no event row should be written.
