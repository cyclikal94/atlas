-- 1000 -> 1001: durable per-operation outcomes for device retirement and keyed session revocation.
-- Additive only; existing rows are untouched. Constraint names match the baseline dump.
CREATE TABLE operation_outcomes (
    account_id text NOT NULL,
    operation_id text NOT NULL,
    kind text NOT NULL,
    digest text NOT NULL,
    outcome text NOT NULL,
    created_at bigint NOT NULL,
    CONSTRAINT operation_outcomes_check CHECK (((outcome <> 'rejected_stale'::text) OR (kind = 'retire_device'::text))),
    CONSTRAINT operation_outcomes_kind_check CHECK ((kind = ANY (ARRAY['retire_device'::text, 'revoke_session'::text]))),
    CONSTRAINT operation_outcomes_outcome_check CHECK ((outcome = ANY (ARRAY['confirmed_applied'::text, 'rejected_stale'::text, 'superseded'::text]))),
    CONSTRAINT operation_outcomes_pkey PRIMARY KEY (account_id, operation_id),
    CONSTRAINT operation_outcomes_account_id_fkey FOREIGN KEY (account_id) REFERENCES accounts(id)
);
