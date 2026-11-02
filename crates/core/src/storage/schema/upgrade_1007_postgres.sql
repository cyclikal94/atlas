-- 1006 -> 1007: BE-PEOPLE-ACTIVITY People activity timeline. Adds `person_activity_events`, an
-- append-only, metadata-only record of supported People changes written in the same transaction as
-- the change, and `person_activity_coverage`, the moment recording began on this database. Kieran's
-- 1 November 2026 decisions: event metadata is kept for the lifetime of the person or field, and no
-- name, label, value or summary is stored (no column can hold one). Additive only: no existing row
-- is read, rewritten or dropped, and the step inserts no event. No pre-1007 record proves when or by
-- whom an earlier change was made, so the timeline honestly begins at this upgrade. Strict DDL (no
-- IF [NOT] EXISTS): a 1006 database that already has these tables is not a 1006 database and must
-- fail loudly. Creating empty tables locks nothing that exists.
CREATE TABLE person_activity_events (
    id text NOT NULL,
    revision bigint NOT NULL,
    ordinal bigint NOT NULL,
    person_id text NOT NULL,
    target_id text NOT NULL,
    event_type text NOT NULL,
    actor_id text NOT NULL,
    account_id text,
    merged_from text,
    label_changed bigint DEFAULT 0 NOT NULL,
    value_changed bigint DEFAULT 0 NOT NULL,
    occurred_at bigint NOT NULL,
    CONSTRAINT person_activity_events_event_type_check CHECK ((event_type = ANY (ARRAY['person_created'::text, 'person_renamed'::text, 'person_archived'::text, 'person_restored'::text, 'person_linked'::text, 'person_unlinked'::text, 'person_merged'::text, 'field_created'::text, 'field_edited'::text, 'field_archived'::text, 'field_restored'::text]))),
    CONSTRAINT person_activity_events_label_changed_check CHECK ((label_changed = ANY (ARRAY[(0)::bigint, (1)::bigint]))),
    CONSTRAINT person_activity_events_ordinal_check CHECK (((ordinal >= 0) AND (ordinal < 100))),
    CONSTRAINT person_activity_events_value_changed_check CHECK ((value_changed = ANY (ARRAY[(0)::bigint, (1)::bigint])))
);

CREATE TABLE person_activity_coverage (
    id bigint NOT NULL,
    started_at bigint NOT NULL,
    CONSTRAINT person_activity_coverage_id_check CHECK ((id = 1))
);

ALTER TABLE ONLY person_activity_coverage
    ADD CONSTRAINT person_activity_coverage_pkey PRIMARY KEY (id);

ALTER TABLE ONLY person_activity_events
    ADD CONSTRAINT person_activity_events_pkey PRIMARY KEY (id);

ALTER TABLE ONLY person_activity_events
    ADD CONSTRAINT person_activity_events_revision_ordinal_key UNIQUE (revision, ordinal);

CREATE INDEX person_activity_events_person ON person_activity_events USING btree (person_id, revision DESC, ordinal DESC);

CREATE INDEX person_activity_events_target ON person_activity_events USING btree (target_id);

ALTER TABLE ONLY person_activity_events
    ADD CONSTRAINT person_activity_events_account_id_fkey FOREIGN KEY (account_id) REFERENCES accounts(id);

ALTER TABLE ONLY person_activity_events
    ADD CONSTRAINT person_activity_events_actor_id_fkey FOREIGN KEY (actor_id) REFERENCES accounts(id);

ALTER TABLE ONLY person_activity_events
    ADD CONSTRAINT person_activity_events_merged_from_fkey FOREIGN KEY (merged_from) REFERENCES resources(id);

ALTER TABLE ONLY person_activity_events
    ADD CONSTRAINT person_activity_events_person_id_fkey FOREIGN KEY (person_id) REFERENCES resources(id);

ALTER TABLE ONLY person_activity_events
    ADD CONSTRAINT person_activity_events_target_id_fkey FOREIGN KEY (target_id) REFERENCES resources(id);

-- Whole seconds, truncated like SQLite's strftime('%s') and every event's `occurred_at`: a
-- bare cast of the fractional epoch rounds, and could put the coverage start up to half a second
-- in the future, after a change committed in the same second.
INSERT INTO person_activity_coverage(id, started_at) VALUES (1, CAST(floor(EXTRACT(EPOCH FROM now())) AS bigint));
