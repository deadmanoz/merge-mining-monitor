# Qbit fixtures

`qbit_controls.json` and `qbit_synthetic_parent.json` retain the Research
extended-header controls. The full native blocks `78058.hex` and
`135331.hex` were read from the retained self-hosted mainnet archive with
`qbit-cli getblock HASH 0` on 2026-10-17 (America/Los_Angeles).

Block 78058 has hash
`f2c1b37777ac0407ece840495347d3481d998b411bfd8796308d387526a10f42`;
its prefix matches the existing Research control, and its Bitcoin parent falls
within the integration tests' retained epoch history. Block 135331 is the
reported missing child-miner example; its independently RPC-checked identity
and coinbase fields are in `135331.json`.

The native blocks `2450.hex` (Luxor payout), `7887.hex` (unmapped shared wallet)
and `8197.hex` (SECPOOL payout) were read with the same raw RPC on 2026-10-18.
Their JSON sidecars record independently decoded native hashes, txids and
reward outputs. They cover payout capture and stored-evidence replay alongside
F2Pool's existing block 78058.

The full blocks test child coinbase authentication, tag and payout attribution.
The tests retain the proof bytes exactly and authenticate the transaction body
against the child merkle root. Recorded payout addresses are evidence, not
pool ownership mappings.
