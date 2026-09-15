-- 0023_validate_child_displacement.sql
--
-- Validates the three child-displacement CHECK constraints that
-- `0022_add_child_displacement.sql` added NOT VALID. This is a separate file
-- because `scripts/migrate-safe.sh` runs each file in its own transaction:
-- VALIDATE CONSTRAINT takes only SHARE UPDATE EXCLUSIVE, so capture and API
-- reads continue during the scan, whereas a validated ADD CONSTRAINT would
-- have scanned the table under 0022's ACCESS EXCLUSIVE lock.
--
-- Every row that predates 0022 has both columns NULL and every row written
-- since has been checked on write, so validation cannot fail. Validating an
-- already-valid constraint is a no-op.

ALTER TABLE merge_mining_event
    VALIDATE CONSTRAINT chk_mme_child_displaced_by_len;
ALTER TABLE merge_mining_event
    VALIDATE CONSTRAINT chk_mme_child_displacement_pair;
ALTER TABLE merge_mining_event
    VALIDATE CONSTRAINT chk_mme_child_displaced_by_other_block;
