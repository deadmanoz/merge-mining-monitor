-- The Hathor sidecar's expected_btc_nbits recorded a Core-cache nBits lookup
-- at the parent coinbase height, taken while Hathor ran its own contamination
-- ladder. Every producer now shares the capture seam's Bitcoin-lineage gate,
-- which decides from the cache at capture time, so nothing writes or reads
-- the column. Apply through migrate-safe.sh, which backs up the database first.
ALTER TABLE hathor_merge_mining_evidence DROP COLUMN expected_btc_nbits;
