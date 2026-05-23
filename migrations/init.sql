-- Optional Postgres parameters store, shared across x402 services
-- (`spl-token-balance` and `aethervane`). Every reader filters by the
-- compile-time `service` constant of its crate, so one Postgres database
-- can back several Vercel deployments without prefixing key names.
--
-- Resolution order (per crate):
--   1. (service, endpoint, param_name) WHERE inactive=false AND in window
--   2. (service, '*',      param_name) WHERE inactive=false AND in window
--   3. process env var (when env_fallback is provided)
--   4. None — caller decides default
--
-- See migrations/CUTOVER.md for the exact SQL operators run when migrating
-- from the previous (single-column-keyed) schema.

CREATE TABLE IF NOT EXISTS parameters (
    id             BIGSERIAL PRIMARY KEY,
    service        TEXT NOT NULL,
    endpoint       TEXT NOT NULL DEFAULT '*',
    param_name     TEXT NOT NULL,
    param_value    TEXT NOT NULL,
    inactive       BOOLEAN NOT NULL DEFAULT FALSE,
    effective_from TIMESTAMPTZ,
    expires_at     TIMESTAMPTZ,
    created_at     TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at     TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE UNIQUE INDEX IF NOT EXISTS uniq_parameters_service_endpoint_param
    ON parameters(service, endpoint, param_name);

CREATE INDEX IF NOT EXISTS idx_parameters_service_active
    ON parameters(service, inactive);


-- Purchase orders ledger for the buy-spl-token endpoint.
-- Keyed on `payment_uid` to enforce idempotency across retries; the `state` column
-- drives the strict state machine pending_transfer -> transfer_landed ->
-- delivery_submitted -> completed (with `failed` as a terminal error state).

CREATE TABLE IF NOT EXISTS purchase_orders (
    payment_uid         TEXT PRIMARY KEY,
    state               TEXT NOT NULL
        CONSTRAINT purchase_orders_state_check
        CHECK (state IN (
            'pending_transfer',
            'transfer_landed',
            'delivery_submitted',
            'completed',
            'failed'
        )),
    transfer_signature  TEXT,
    evidence_url        TEXT,
    delivery_signature  TEXT,
    created_at          TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at          TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS idx_purchase_orders_state ON purchase_orders (state ASC);


-- Example seed rows (uncomment / adjust):
-- INSERT INTO parameters (service, endpoint, param_name, param_value) VALUES
--   ('spl-token-balance', '*',             'X402_NETWORK',          'solana:EtWTRABZaYq6iMfeYKouRu166VU2xqa1'),
--   ('spl-token-balance', '*',             'X402_PAY_TO',           'YourSolanaWalletBase58'),
--   ('spl-token-balance', 'check-balance', 'X402_PAYMENT_AMOUNT_USDC',   '0.05'),
--   ('spl-token-balance', 'check-balance', 'X402_ACCEPTS_JSON',     '[{"kind":"usdc","amountUi":"0.01"},{"kind":"sol","amountUi":"0.00001"}]'),
--   ('spl-token-balance', 'buy-spl-token', 'BUY_SPL_TOKEN_CATALOG_JSON',
--    '[{"mint":"...","decimals":6,"price_usdc_ui":"0.42","name":"Merry Xmas"}]')
-- ON CONFLICT (service, endpoint, param_name) DO UPDATE SET
--   param_value = EXCLUDED.param_value,
--   updated_at  = NOW();
