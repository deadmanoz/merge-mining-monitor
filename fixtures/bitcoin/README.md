# Bitcoin fixtures

`epoch-headers.json` is Bitcoin's retarget history: the height, time and nBits
of every 2,016-block epoch boundary from genesis, plus the finalized tip at
967,961. It is derived from Research's committed
`data/bitcoin-epoch-reference/btc_epoch_headers.json` (a mempool.space
Esplora snapshot; the source manifest is embedded) and feeds the Bitcoin
lineage tests, which need real epoch bits rather than synthetic ones.
