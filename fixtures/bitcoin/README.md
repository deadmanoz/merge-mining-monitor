# Bitcoin fixtures

`epoch-headers.json` is Bitcoin's retarget history: the height, time and nBits of every 2,016-block epoch boundary from genesis, plus the finalized tip at 967,961.
It is derived from Research's committed `data/bitcoin-epoch-reference/btc_epoch_headers.json` (a mempool.space Esplora snapshot; the source manifest is embedded) and feeds the Bitcoin lineage tests, which need real epoch bits rather than synthetic ones.

`coinbase-outputs-<height>.hex` are consensus-serialized coinbase outputs (`Vec<TxOut>`, the form `block.btc_coinbase_outputs` stores), fetched from Bitcoin Core, for the protocol-commitment decoder tests:

- 920,500 (`0000000000000000000118614f2638189bf4bfe5776318f7e4ef91dde16efa4c`): a Mining Squared block declaring AntPool's CORE reward address and exSat account `antpoole.sat`.
- 868,400 (`00000000000000000001b8bf94682a2bcf79b2442c91d6b82f8a7137a2449f0e`): an F2Pool block with the 10-symbol exSat account `f2pool.sat`.
- 853,776 (`000000000000000000030dbc110ac4bd1b57343b44f6e8c116c92313cce81fcf`): a Braiins Pool block whose CORE output nests a second `OP_RETURN 0x2d CORE 0x01` header.

`secpool-exsat-transition.json` holds the headers and raw coinbase transactions of two SecPool blocks, fetched from Bitcoin Core, for the protocol-commitment read-model tests.
Both declare SecPool's CORE reward address at vout 3 and resolve to SecPool through the embedded pool snapshot; 895,755 (`00000000000000000001718956300bcc55ebe00bb173371d5e8bcc2b86e35716`) is the first SecPool block that also declares the exSat account `antpoole.sat` (vout 4), which 895,743 (`00000000000000000001aa86b504345c002d99f06d3a8ffa58ec43d646222894`) does not.
