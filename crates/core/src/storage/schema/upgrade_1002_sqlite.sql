-- 1001 -> 1002: BE-Q19 activation grants (approved-device-state component 4) and the OIDC
-- browser-attempt columns that issue them from a callback. Additive only; existing rows are
-- untouched.
ALTER TABLE oidc_flows ADD COLUMN attempt_id TEXT;
ALTER TABLE oidc_flows ADD COLUMN attempt_challenge TEXT;

CREATE TABLE activation_grants (
 grant_hash TEXT PRIMARY KEY, grant_id TEXT NOT NULL,
 account_id TEXT NOT NULL REFERENCES accounts(id), device_id TEXT NOT NULL,
 auth_kind TEXT NOT NULL CHECK (auth_kind IN ('local','oidc')),
 challenge_hash TEXT NOT NULL UNIQUE,
 state TEXT NOT NULL CHECK (state IN ('issued','redeemed','cancelled')),
 session_id TEXT, failed_verifiers BIGINT NOT NULL DEFAULT 0,
 created_at BIGINT NOT NULL, expires_at BIGINT NOT NULL,
 redeemed_at BIGINT, cancelled_at BIGINT
);

CREATE UNIQUE INDEX activation_grant_identity ON activation_grants(grant_id);
CREATE INDEX activation_grants_expiry ON activation_grants(expires_at);
CREATE INDEX activation_grants_account_device ON activation_grants(account_id,device_id,state);
