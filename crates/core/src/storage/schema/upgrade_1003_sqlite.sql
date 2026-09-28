-- 1002 -> 1003: BE-Q16 durable sent-request/invitation history. Additive only; existing
-- `people_requests`/`household_invitations` rows and their cleanup jobs are untouched.
-- Backfill mirrors every currently-live row so nothing already sent silently drops out
-- of history the first time it later expires or is cleaned up.
CREATE TABLE people_request_history (
 id TEXT PRIMARY KEY, sender_id TEXT NOT NULL REFERENCES accounts(id),
 recipient_id TEXT NOT NULL REFERENCES accounts(id), kind TEXT NOT NULL,
 payload TEXT NOT NULL, state TEXT NOT NULL, expires_at BIGINT NOT NULL,
 updated_at BIGINT NOT NULL
);

CREATE INDEX people_request_history_sender ON people_request_history(sender_id,id);

CREATE TABLE household_invitation_history (
 id TEXT PRIMARY KEY, household_id TEXT NOT NULL REFERENCES households(id),
 sender_id TEXT NOT NULL REFERENCES accounts(id),
 recipient_id TEXT NOT NULL REFERENCES accounts(id), status TEXT NOT NULL,
 expires_at BIGINT NOT NULL, version BIGINT NOT NULL, updated_at BIGINT NOT NULL
);

CREATE INDEX household_invitation_history_sender ON household_invitation_history(sender_id,id);

INSERT INTO people_request_history(id,sender_id,recipient_id,kind,payload,state,expires_at,updated_at)
SELECT id,sender_id,recipient_id,kind,payload,state,expires_at,expires_at FROM people_requests;

INSERT INTO household_invitation_history(id,household_id,sender_id,recipient_id,status,expires_at,version,updated_at)
SELECT id,household_id,sender_id,recipient_id,status,expires_at,version,expires_at FROM household_invitations;
