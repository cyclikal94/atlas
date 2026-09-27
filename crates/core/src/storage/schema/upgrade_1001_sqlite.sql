-- 1000 -> 1001: durable per-operation outcomes for device retirement and keyed session revocation.
-- Additive only; existing rows are untouched.
CREATE TABLE operation_outcomes (
 account_id TEXT NOT NULL REFERENCES accounts(id), operation_id TEXT NOT NULL,
 kind TEXT NOT NULL CHECK (kind IN ('retire_device','revoke_session')),
 digest TEXT NOT NULL,
 outcome TEXT NOT NULL CHECK (outcome IN ('confirmed_applied','rejected_stale','superseded')),
 created_at BIGINT NOT NULL,
 PRIMARY KEY(account_id,operation_id),
 CHECK (outcome <> 'rejected_stale' OR kind = 'retire_device')
);
