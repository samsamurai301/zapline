-- Money is stored in integer cents. The CHECK is the last line of defence:
-- the service never lets a balance go negative, but if a bug ever tried,
-- the database would refuse the write (and zapline answers 422).
CREATE TABLE accounts (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    owner TEXT NOT NULL UNIQUE,
    opening_balance_cents BIGINT NOT NULL CHECK (opening_balance_cents >= 0),
    balance_cents BIGINT NOT NULL CHECK (balance_cents >= 0)
);

-- One row per committed transfer. `idempotency_key` makes a retried request
-- (same key) return the first result instead of moving the money twice.
CREATE TABLE transfers (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    from_id BIGINT NOT NULL REFERENCES accounts (id),
    to_id BIGINT NOT NULL REFERENCES accounts (id),
    amount_cents BIGINT NOT NULL CHECK (amount_cents > 0),
    idempotency_key TEXT UNIQUE,
    created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE INDEX transfers_from ON transfers (from_id);
CREATE INDEX transfers_to ON transfers (to_id);
