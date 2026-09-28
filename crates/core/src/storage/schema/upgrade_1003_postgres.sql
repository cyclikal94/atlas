-- 1002 -> 1003: BE-Q16 durable sent-request/invitation history. Additive only; existing
-- `people_requests`/`household_invitations` rows and their cleanup jobs are untouched.
-- Backfill mirrors every currently-live row so nothing already sent silently drops out
-- of history the first time it later expires or is cleaned up.
CREATE TABLE people_request_history (
    id text NOT NULL,
    sender_id text NOT NULL,
    recipient_id text NOT NULL,
    kind text NOT NULL,
    payload text NOT NULL,
    state text NOT NULL,
    expires_at bigint NOT NULL,
    updated_at bigint NOT NULL,
    CONSTRAINT people_request_history_pkey PRIMARY KEY (id),
    CONSTRAINT people_request_history_sender_id_fkey FOREIGN KEY (sender_id) REFERENCES accounts(id),
    CONSTRAINT people_request_history_recipient_id_fkey FOREIGN KEY (recipient_id) REFERENCES accounts(id)
);

CREATE INDEX people_request_history_sender ON people_request_history USING btree (sender_id, id);

CREATE TABLE household_invitation_history (
    id text NOT NULL,
    household_id text NOT NULL,
    sender_id text NOT NULL,
    recipient_id text NOT NULL,
    status text NOT NULL,
    expires_at bigint NOT NULL,
    version bigint NOT NULL,
    updated_at bigint NOT NULL,
    CONSTRAINT household_invitation_history_pkey PRIMARY KEY (id),
    CONSTRAINT household_invitation_history_household_id_fkey FOREIGN KEY (household_id) REFERENCES households(id),
    CONSTRAINT household_invitation_history_sender_id_fkey FOREIGN KEY (sender_id) REFERENCES accounts(id),
    CONSTRAINT household_invitation_history_recipient_id_fkey FOREIGN KEY (recipient_id) REFERENCES accounts(id)
);

CREATE INDEX household_invitation_history_sender ON household_invitation_history USING btree (sender_id, id);

INSERT INTO people_request_history(id,sender_id,recipient_id,kind,payload,state,expires_at,updated_at)
SELECT id,sender_id,recipient_id,kind,payload,state,expires_at,expires_at FROM people_requests;

INSERT INTO household_invitation_history(id,household_id,sender_id,recipient_id,status,expires_at,version,updated_at)
SELECT id,household_id,sender_id,recipient_id,status,expires_at,version,expires_at FROM household_invitations;
