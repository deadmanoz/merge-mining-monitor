-- 0030_capture_failure_kind.sql
--
-- Transport and other per-height failures must remain visible during replay,
-- even when the monotonic cursor already lies beyond the failed height.
ALTER TABLE capture_error DROP CONSTRAINT capture_error_error_kind_check;
ALTER TABLE capture_error ADD CONSTRAINT capture_error_error_kind_check
    CHECK (error_kind IN ('malformed_auxpow_proof', 'height_capture_failed'));
