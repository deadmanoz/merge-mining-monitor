-- Hathor parent coinbase evidence now participates in strict BIP34 orphan
-- classification. Existing unknown parents may already carry the earlier weak
-- verdict, so schedule one full recheck through the durable Core-cache path.

UPDATE bitcoin_core_header_cache_state
   SET reclassification_needed = TRUE,
       orphan_recheck_needed = TRUE
 WHERE singleton;
