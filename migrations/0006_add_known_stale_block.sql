-- 0006_add_known_stale_block.sql
--
-- Adds the operator-imported known-stale membership table consulted at
-- `block.btc_orphan_class` derivation. A Bitcoin block that is a KNOWN STALE is
-- absent from Core's active chain by definition, so it passes the reconciler's
-- PoW + BIP34 + nBits + Core-absence checks and, without this membership, is
-- wrongly refined into `strict_btc_orphan` / `weak_btc_orphan`. Consulting this
-- set demotes such a header to `excluded`, matching the merge-mining-research
-- classifier's `known_stale_hash -> excluded` verdict (the shared source of
-- truth this classifier is ported from).
--
-- Loaded by `import-known-stales` from the upstream bitcoin-data/stale-blocks
-- dataset. Keyed by header hash in rust-bitcoin internal (to_byte_array) byte
-- order so it joins directly against `block.btc_header_hash` and
-- `merge_mining_event.btc_parent_header_hash`.

CREATE TABLE known_stale_block (
    hash          BYTEA   PRIMARY KEY CHECK (octet_length(hash) = 32),
    btc_height    INTEGER,
    source_label  TEXT    NOT NULL,
    imported_at   BIGINT  NOT NULL
);

COMMENT ON TABLE known_stale_block IS
  'Operator-imported membership of Bitcoin blocks known to be stale (upstream bitcoin-data/stale-blocks), consulted at btc_orphan_class derivation so a known stale is excluded, never labelled strict/weak. Loaded by import-known-stales.';
COMMENT ON COLUMN known_stale_block.hash IS
  'Bitcoin block header hash in internal (to_byte_array) byte order, matching block.btc_header_hash and merge_mining_event.btc_parent_header_hash.';
COMMENT ON COLUMN known_stale_block.btc_height IS
  'Upstream-reported stale height; advisory only, NULL when the source row omits it.';
COMMENT ON COLUMN known_stale_block.source_label IS
  'Provenance recorded at import (e.g. a dataset name plus commit), from the import-known-stales --source-label argument.';
COMMENT ON COLUMN known_stale_block.imported_at IS
  'Epoch seconds at which the row was first imported.';
