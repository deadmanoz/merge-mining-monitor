-- 0018_add_rod_source.sql
--
-- Adds SpaceXpanse ROD as permanent source id 35. ROD is a live chain, but the
-- Monitor source is a sealed historical recovery snapshot and has no live
-- producer.
--
-- Fresh / reset databases receive this row from the regenerated 0002 seed.
-- Databases that already applied the earlier seed receive it here. The guards
-- reject either possible identity collision before writing, and the sequence
-- repair advances the next generated id without moving an already-ahead
-- identity sequence backwards.

DO $$
DECLARE
    rod_id BIGINT;
    id_35_code TEXT;
    identity_sequence TEXT;
    identity_last BIGINT;
    identity_called BOOLEAN;
    max_source_id BIGINT;
    required_next BIGINT;
    current_next BIGINT;
BEGIN
    SELECT id INTO rod_id FROM source WHERE code = 'auxpow:rod';
    SELECT code INTO id_35_code FROM source WHERE id = 35;

    IF rod_id IS NOT NULL AND rod_id <> 35 THEN
        RAISE EXCEPTION
            'cannot add auxpow:rod: expected source id 35, found %',
            rod_id;
    END IF;
    IF id_35_code IS NOT NULL AND id_35_code <> 'auxpow:rod' THEN
        RAISE EXCEPTION
            'cannot add auxpow:rod: source id 35 belongs to %',
            id_35_code;
    END IF;

    IF rod_id IS NULL THEN
        INSERT INTO source (id, code, kind, chain, instance, created_at)
        OVERRIDING SYSTEM VALUE
        VALUES (
            35,
            'auxpow:rod',
            'auxpow',
            'rod',
            NULL,
            extract(epoch from now())::bigint
        );
    ELSIF EXISTS (
        SELECT 1
        FROM source
        WHERE id = 35
          AND (
              kind <> 'auxpow'
              OR chain IS DISTINCT FROM 'rod'
              OR instance IS NOT NULL
          )
    ) THEN
        RAISE EXCEPTION
            'cannot add auxpow:rod: source id 35 has incompatible metadata';
    END IF;

    identity_sequence := pg_get_serial_sequence('source', 'id');
    IF identity_sequence IS NULL THEN
        RAISE EXCEPTION 'cannot add auxpow:rod: source.id identity sequence is missing';
    END IF;

    EXECUTE format('SELECT last_value, is_called FROM %s', identity_sequence)
        INTO identity_last, identity_called;
    SELECT COALESCE(MAX(id), 0) INTO max_source_id FROM source;
    required_next := GREATEST(max_source_id + 1, 36);
    current_next := CASE
        WHEN identity_called THEN identity_last + 1
        ELSE identity_last
    END;
    IF current_next < required_next THEN
        PERFORM setval(identity_sequence::regclass, required_next - 1, true);
    END IF;
END;
$$;
