-- 1006 -> 1007: BE-PEOPLE-ACTIVITY People activity timeline. Adds `person_activity_events`, an
-- append-only, metadata-only record of supported People changes written in the same transaction as
-- the change, and `person_activity_coverage`, the moment recording began on this database. Kieran's
-- 1 November 2026 decisions: event metadata is kept for the lifetime of the person or field, and no
-- name, label, value or summary is stored (no column can hold one). Additive only: no existing row
-- is read, rewritten or dropped, and the step inserts no event. No pre-1007 record proves when or by
-- whom an earlier change was made, so the timeline honestly begins at this upgrade. Strict DDL (no
-- IF [NOT] EXISTS): a 1006 database that already has these tables is not a 1006 database and must
-- fail loudly.
CREATE TABLE person_activity_events (
 id TEXT PRIMARY KEY,
 revision BIGINT NOT NULL,
 ordinal BIGINT NOT NULL CHECK (ordinal >= 0 AND ordinal < 100),
 person_id TEXT NOT NULL REFERENCES resources(id),
 target_id TEXT NOT NULL REFERENCES resources(id),
 event_type TEXT NOT NULL CHECK (event_type IN ('person_created','person_renamed','person_archived','person_restored','person_linked','person_unlinked','person_merged','field_created','field_edited','field_archived','field_restored')),
 actor_id TEXT NOT NULL REFERENCES accounts(id),
 account_id TEXT REFERENCES accounts(id),
 merged_from TEXT REFERENCES resources(id),
 label_changed BIGINT NOT NULL DEFAULT 0 CHECK (label_changed IN (0,1)),
 value_changed BIGINT NOT NULL DEFAULT 0 CHECK (value_changed IN (0,1)),
 occurred_at BIGINT NOT NULL,
 UNIQUE (revision, ordinal)
);
CREATE INDEX person_activity_events_person ON person_activity_events (person_id, revision DESC, ordinal DESC);
CREATE INDEX person_activity_events_target ON person_activity_events (target_id);
CREATE TABLE person_activity_coverage (
 id BIGINT PRIMARY KEY CHECK (id = 1),
 started_at BIGINT NOT NULL
);
INSERT INTO person_activity_coverage(id,started_at) VALUES (1, CAST(strftime('%s','now') AS INTEGER));
