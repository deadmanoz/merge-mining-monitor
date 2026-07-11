-- 0004_remove_mazacoin_source.sql
--
-- Retires permanent source id 32 after the Mazacoin source audit found no
-- AuxPoW consensus path. Fresh databases omit the row in 0002; this guarded
-- migration removes it from databases that applied the earlier baseline.
-- It also verifies that the historical 0003 insertion converged on Elcash id 34
-- before making any changes.
-- Evidence-bearing rows are never deleted. Operational and derived state may
-- be removed only after both evidence guards pass.

DO $$
DECLARE
    mazacoin_id BIGINT;
    id_32_code TEXT;
    elcash_id BIGINT;
    id_34_code TEXT;
BEGIN
    SELECT id INTO elcash_id FROM source WHERE code = 'auxpow:elcash';
    SELECT code INTO id_34_code FROM source WHERE id = 34;
    IF elcash_id IS DISTINCT FROM 34
       OR id_34_code IS DISTINCT FROM 'auxpow:elcash' THEN
        RAISE EXCEPTION
            'source identity mismatch: expected auxpow:elcash at id 34, found code id % and id 34 code %',
            elcash_id, id_34_code;
    END IF;

    SELECT code INTO id_32_code FROM source WHERE id = 32;
    IF id_32_code IS NOT NULL AND id_32_code <> 'auxpow:mazacoin' THEN
        RAISE EXCEPTION
            'cannot retire source id 32: it belongs to %, not auxpow:mazacoin',
            id_32_code;
    END IF;

    SELECT id INTO mazacoin_id FROM source WHERE code = 'auxpow:mazacoin';
    IF mazacoin_id IS NOT NULL THEN
        IF mazacoin_id <> 32 THEN
            RAISE EXCEPTION
                'cannot retire auxpow:mazacoin: expected source id 32, found %',
                mazacoin_id;
        END IF;

        IF EXISTS (SELECT 1 FROM merge_mining_event WHERE source_id = mazacoin_id) THEN
            RAISE EXCEPTION
                'cannot retire auxpow:mazacoin: merge_mining_event evidence exists';
        END IF;
        IF EXISTS (SELECT 1 FROM attestation_proof WHERE source_id = mazacoin_id) THEN
            RAISE EXCEPTION
                'cannot retire auxpow:mazacoin: attestation_proof evidence exists';
        END IF;

        DELETE FROM source_health WHERE source_id = mazacoin_id;
        DELETE FROM poll_cursor WHERE source_id = mazacoin_id;
        DELETE FROM poll_pending_reconcile WHERE source_id = mazacoin_id;
        DELETE FROM bitcoin_core_sync_state WHERE source_id = mazacoin_id;
        DELETE FROM source WHERE id = mazacoin_id;
    END IF;
END;
$$;
