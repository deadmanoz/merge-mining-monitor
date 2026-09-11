-- 0020_add_qbit_source.sql
--
-- Adds Qbit as permanent source id 36 with a Live lifecycle. Qbit merge-mines
-- Bitcoin from its own genesis and is served by the shared bitcoind-family
-- producer, so later historical publication imports are additive rather than
-- an authoritative snapshot.
--
-- Fresh / reset databases receive this row from the regenerated 0002 seed.
-- Databases that already applied the earlier seed receive it here. The guards
-- reject either possible identity collision before writing, and the sequence
-- repair advances the next generated id without moving an already-ahead
-- identity sequence backwards.

DO $$
DECLARE
    qbit_id BIGINT;
    id_36_code TEXT;
    identity_sequence TEXT;
    identity_last BIGINT;
    identity_called BOOLEAN;
    max_source_id BIGINT;
    required_next BIGINT;
    current_next BIGINT;
BEGIN
    SELECT id INTO qbit_id FROM source WHERE code = 'auxpow:qbit';
    SELECT code INTO id_36_code FROM source WHERE id = 36;

    IF qbit_id IS NOT NULL AND qbit_id <> 36 THEN
        RAISE EXCEPTION
            'cannot add auxpow:qbit: expected source id 36, found %',
            qbit_id;
    END IF;
    IF id_36_code IS NOT NULL AND id_36_code <> 'auxpow:qbit' THEN
        RAISE EXCEPTION
            'cannot add auxpow:qbit: source id 36 belongs to %',
            id_36_code;
    END IF;

    IF qbit_id IS NULL THEN
        INSERT INTO source (id, code, kind, chain, instance, created_at)
        OVERRIDING SYSTEM VALUE
        VALUES (
            36,
            'auxpow:qbit',
            'auxpow',
            'qbit',
            NULL,
            extract(epoch from now())::bigint
        );
    ELSIF EXISTS (
        SELECT 1
        FROM source
        WHERE id = 36
          AND (
              kind <> 'auxpow'
              OR chain IS DISTINCT FROM 'qbit'
              OR instance IS NOT NULL
          )
    ) THEN
        RAISE EXCEPTION
            'cannot add auxpow:qbit: source id 36 has incompatible metadata';
    END IF;

    identity_sequence := pg_get_serial_sequence('source', 'id');
    IF identity_sequence IS NULL THEN
        RAISE EXCEPTION 'cannot add auxpow:qbit: source.id identity sequence is missing';
    END IF;

    EXECUTE format('SELECT last_value, is_called FROM %s', identity_sequence)
        INTO identity_last, identity_called;
    SELECT COALESCE(MAX(id), 0) INTO max_source_id FROM source;
    required_next := GREATEST(max_source_id + 1, 37);
    current_next := CASE
        WHEN identity_called THEN identity_last + 1
        ELSE identity_last
    END;
    IF current_next < required_next THEN
        PERFORM setval(identity_sequence::regclass, required_next - 1, true);
    END IF;
END;
$$;
