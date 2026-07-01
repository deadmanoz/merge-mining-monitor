# Namecoin Fixtures

These fixtures are small synthetic Namecoin-family raw blocks. They are stored
as raw `.bin` block bytes so tests exercise the same byte-level parser that the
live `getblock <hash> 0` poller uses.

Each `.bin` file has a sibling `.expected.json` sidecar with the fields asserted
by the parser and capture-payload tests.

Cases:

- `019199-non-auxpow` - child header does not carry the AuxPoW bit and should
  be skipped without error.
- `500000-valid-parent` - parent header passes its own target and remains
  `unknown` without Bitcoin-chain proof.
- `500001-near-parent` - parent header fails its own target and is classified
  as `near`. It also fails the child header's aux target on purpose, so this
  synthetic fixture exercises `pow_validates_child_target = false`; real
  Namecoin-accepted AuxPoW blocks should not look like this.
- `500002-wrong-chain-parent` - parent header passes its own target but carries
  fixture-provided `difficulty_epoch_ok = false`.
- `500003-malformed` - child header carries the AuxPoW bit but the payload is
  truncated; parsing fails and no event row should be written.
