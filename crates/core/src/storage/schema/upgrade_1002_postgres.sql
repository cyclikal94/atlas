-- 1001 -> 1002: BE-Q19 activation grants (approved-device-state component 4) and the OIDC
-- browser-attempt columns that issue them from a callback. Additive only; existing rows are
-- untouched. Constraint names match the baseline dump.
ALTER TABLE oidc_flows ADD COLUMN attempt_id text;
ALTER TABLE oidc_flows ADD COLUMN attempt_challenge text;

CREATE TABLE activation_grants (
    grant_hash text NOT NULL,
    grant_id text NOT NULL,
    account_id text NOT NULL,
    device_id text NOT NULL,
    auth_kind text NOT NULL,
    challenge_hash text NOT NULL,
    state text NOT NULL,
    session_id text,
    failed_verifiers bigint NOT NULL DEFAULT 0,
    created_at bigint NOT NULL,
    expires_at bigint NOT NULL,
    redeemed_at bigint,
    cancelled_at bigint,
    CONSTRAINT activation_grants_pkey PRIMARY KEY (grant_hash),
    CONSTRAINT activation_grants_challenge_hash_key UNIQUE (challenge_hash),
    CONSTRAINT activation_grants_auth_kind_check CHECK ((auth_kind = ANY (ARRAY['local'::text, 'oidc'::text]))),
    CONSTRAINT activation_grants_state_check CHECK ((state = ANY (ARRAY['issued'::text, 'redeemed'::text, 'cancelled'::text]))),
    CONSTRAINT activation_grants_account_id_fkey FOREIGN KEY (account_id) REFERENCES accounts(id)
);

CREATE UNIQUE INDEX activation_grant_identity ON activation_grants USING btree (grant_id);
CREATE INDEX activation_grants_expiry ON activation_grants USING btree (expires_at);
CREATE INDEX activation_grants_account_device ON activation_grants USING btree (account_id, device_id, state);
