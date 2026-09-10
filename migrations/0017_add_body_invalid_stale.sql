-- 0017_add_body_invalid_stale.sql
--
-- Adds the operator-imported body-invalid stale annotation table. A block in
-- this set remains an ordinary `kind='stale'` derived row: its header passed
-- every check the stale evidence profile can apply, but its complete block
-- body is known consensus-invalid from an independently observed full block
-- (the research overlay `data/error-blocks/body_invalid_stales.csv`, mirrored
-- here as `data/consensus/body_invalid_stales.csv`). The membership is a
-- display annotation only: it is joined at API projection time and never
-- consulted by parent classification, orphan-class derivation, source-health
-- counters, or reconciliation. In particular it must NOT promote a row to
-- `error_block` -- that kind is reserved for the research catalogue's offline
-- re-derivable violations, enforced by the 0008 CHECK constraints.
--
-- Loaded by `import-body-invalid-stales` from the pinned compact mirror.
-- Keyed by header hash in rust-bitcoin internal (to_byte_array) byte order so
-- it joins directly against `block.btc_header_hash`.

CREATE TABLE body_invalid_stale (
    hash          BYTEA   PRIMARY KEY CHECK (octet_length(hash) = 32),
    btc_height    INTEGER,
    rule          TEXT    NOT NULL,
    evidence_url  TEXT,
    source_label  TEXT    NOT NULL,
    imported_at   BIGINT  NOT NULL
);

COMMENT ON TABLE body_invalid_stale IS
  'Operator-imported annotation of stale Bitcoin blocks whose complete body is known consensus-invalid from external full-block evidence (research body-invalid stales overlay). Display annotation only: joined at API projection, never consulted by classification, orphan derivation, or reconciliation. Loaded by import-body-invalid-stales.';
COMMENT ON COLUMN body_invalid_stale.hash IS
  'Bitcoin block header hash in internal (to_byte_array) byte order, matching block.btc_header_hash.';
COMMENT ON COLUMN body_invalid_stale.btc_height IS
  'Overlay-reported Bitcoin height; advisory only, NULL when the source row omits it.';
COMMENT ON COLUMN body_invalid_stale.rule IS
  'Bitcoin Core reject family attested by the external full-body evidence (e.g. bad-blk-sigops). Not a research rules_violated token.';
COMMENT ON COLUMN body_invalid_stale.evidence_url IS
  'Public URL of the external full-body observation the invalidity claim rests on.';
COMMENT ON COLUMN body_invalid_stale.source_label IS
  'Provenance recorded at import (e.g. the mirror name plus pinned research commit), from the import-body-invalid-stales --source-label argument.';
COMMENT ON COLUMN body_invalid_stale.imported_at IS
  'Epoch seconds at which the row was last imported (rows are upserted in place on re-import).';
