-- Body-invalid parents now belong to the reviewed error catalogue and its
-- authenticated child-observation publication. The display-only annotation
-- table has no remaining reader or writer. Apply through migrate-safe.sh,
-- which backs up the database before retiring the redundant annotation data.
-- Existing merge_mining_event rows and their provenance are unaffected.
DROP TABLE body_invalid_stale;
