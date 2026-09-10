-- Hathor parent coinbase evidence now participates in strict BIP34 orphan
-- classification. Existing unknown parents may already carry the earlier weak
-- verdict, so schedule one full recheck through the durable Core-cache path.
-- Requires all Monitor processes stopped through activation of the new
-- classifier: an old binary can consume these flags without the Hathor rule.
-- Follow the stop/migrate/start acceptance in docs/operations.md.

UPDATE bitcoin_core_header_cache_state
   SET reclassification_needed = TRUE,
       orphan_recheck_needed = TRUE
 WHERE singleton;
