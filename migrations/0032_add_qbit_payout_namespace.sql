-- Qbit uses the existing child payout identity and attribution path. Widen only
-- the namespace check; no evidence rewrite or historical replay runs here.
ALTER TABLE pool_identity DROP CONSTRAINT pool_identity_namespace_check;
ALTER TABLE pool_identity ADD CONSTRAINT pool_identity_namespace_check CHECK (namespace IN (
    'rsk_miner_address',
    'namecoin_payout_address',
    'syscoin_payout_address',
    'fractal_reward_address',
    'hathor_reward_address',
    'elastos_reward_address',
    'elastos_minerinfo',
    'qbit_payout_address'
));
