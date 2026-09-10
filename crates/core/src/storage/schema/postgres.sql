-- Current pre-release baseline. Incompatible experimental databases require an explicit reset.
CREATE FUNCTION atlas_resource_ancestors_insert() RETURNS trigger
    LANGUAGE plpgsql
    AS $$ BEGIN IF NEW.parent_id IS NOT NULL THEN INSERT INTO resource_ancestors SELECT NEW.id,NEW.parent_id,1 UNION ALL SELECT NEW.id,ancestor_id,depth+1 FROM resource_ancestors WHERE resource_id=NEW.parent_id; END IF; RETURN NEW; END $$;

CREATE FUNCTION atlas_resource_ancestors_move() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
BEGIN
 IF NEW.parent_id IS DISTINCT FROM OLD.parent_id THEN
   DELETE FROM resource_ancestors WHERE resource_id=NEW.id;
   INSERT INTO resource_ancestors SELECT NEW.id,NEW.parent_id,1 UNION ALL
   SELECT NEW.id,ancestor_id,depth+1 FROM resource_ancestors WHERE resource_id=NEW.parent_id;
 END IF;
 RETURN NEW;
END $$;

CREATE FUNCTION atlas_resource_parent_guard() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
BEGIN
 IF TG_OP='UPDATE' THEN
   IF (NEW.kind<>OLD.kind OR NEW.parent_id IS DISTINCT FROM OLD.parent_id) AND NOT (NEW.kind='field' AND OLD.kind='field' AND EXISTS(SELECT 1 FROM person_aliases a JOIN resources p ON p.id=a.canonical_id WHERE a.source_id=OLD.parent_id AND a.canonical_id=NEW.parent_id AND p.kind='person')) THEN
     RAISE EXCEPTION 'resource hierarchy is immutable';
   END IF;
 ELSIF NEW.parent_id IS NOT NULL THEN
   IF NOT EXISTS(SELECT 1 FROM resources p WHERE p.id=NEW.parent_id AND
     ((NEW.kind='field' AND p.kind IN ('person','task')) OR
      (NEW.kind='execution' AND p.kind='task') OR
      (NEW.kind='occurrence' AND p.kind='execution') OR
      (NEW.kind IN ('progress','review','reminder') AND p.kind='occurrence') OR (NEW.kind='event' AND p.kind='calendar_source'))) THEN
     RAISE EXCEPTION 'invalid resource parent';
   END IF;
 END IF;
 RETURN NEW;
END $$;

CREATE TABLE account_invitations (
    id text NOT NULL,
    token_hash text NOT NULL,
    issuer_id text,
    household_id text,
    created_at bigint NOT NULL,
    expires_at bigint NOT NULL,
    redeemed_by text,
    revoked bigint DEFAULT 0 NOT NULL,
    CONSTRAINT account_invitations_revoked_check CHECK ((revoked = ANY (ARRAY[(0)::bigint, (1)::bigint])))
);

CREATE TABLE accounts (
    id text NOT NULL,
    username text NOT NULL,
    password_hash text NOT NULL,
    access_epoch bigint DEFAULT 0 NOT NULL,
    sync_floor bigint DEFAULT 0 NOT NULL,
    primary_household_id text,
    preferences_version bigint DEFAULT 1 NOT NULL
);

CREATE TABLE anchored_occurrences (
    occurrence_id text NOT NULL,
    task_id text NOT NULL,
    anchor_key text NOT NULL,
    reference_id text NOT NULL,
    anchor_revision bigint NOT NULL
);

CREATE TABLE atlas_schema (
    version bigint NOT NULL
);

CREATE TABLE calendar_events (
    id text NOT NULL,
    source_id text NOT NULL,
    uid text NOT NULL,
    original text NOT NULL,
    data text NOT NULL,
    generation bigint NOT NULL
);

CREATE TABLE calendar_reviews (
    id text NOT NULL,
    occurrence_id text NOT NULL,
    reason text NOT NULL,
    reference_id text,
    source_revision bigint NOT NULL,
    resolved bigint DEFAULT 0 NOT NULL
);

CREATE TABLE calendar_sources (
    id text NOT NULL,
    connection text,
    timezone text NOT NULL,
    generation bigint DEFAULT 0 NOT NULL,
    committed_generation bigint DEFAULT 0 NOT NULL,
    lease_until bigint DEFAULT 0 NOT NULL,
    next_refresh bigint DEFAULT 0 NOT NULL,
    etag text,
    modified text,
    last_success bigint,
    health text DEFAULT 'pending'::text NOT NULL,
    coverage_start text,
    coverage_end text
);

CREATE TABLE completion_pending (
    occurrence_id text NOT NULL,
    next_check bigint DEFAULT 0 NOT NULL
);

CREATE TABLE completion_successors (
    predecessor_id text NOT NULL,
    successor_id text NOT NULL
);

CREATE TABLE default_templates (
    account_id text,
    household_id text,
    resource_kind text NOT NULL,
    template text NOT NULL,
    version bigint DEFAULT 1 NOT NULL,
    CONSTRAINT default_templates_check CHECK (((account_id IS NULL) <> (household_id IS NULL))),
    CONSTRAINT default_templates_resource_kind_check CHECK ((resource_kind = ANY (ARRAY['person'::text, 'field'::text, 'task'::text, 'list'::text, 'progress'::text])))
);

CREATE TABLE dependency_rules (
    occurrence_id text NOT NULL,
    strict bigint NOT NULL,
    CONSTRAINT dependency_rules_strict_check CHECK ((strict = ANY (ARRAY[(0)::bigint, (1)::bigint])))
);

CREATE TABLE external_identities (
    issuer text NOT NULL,
    subject text NOT NULL,
    account_id text NOT NULL
);

CREATE TABLE frozen_owner_visibility (
    resource_id text NOT NULL
);

CREATE TABLE household_invitations (
    id text NOT NULL,
    household_id text NOT NULL,
    sender_id text NOT NULL,
    recipient_id text NOT NULL,
    status text NOT NULL,
    expires_at bigint NOT NULL,
    version bigint DEFAULT 1 NOT NULL,
    CONSTRAINT household_invitations_status_check CHECK ((status = ANY (ARRAY['pending'::text, 'accepted'::text, 'declined'::text, 'revoked'::text])))
);

CREATE TABLE household_memberships (
    household_id text NOT NULL,
    account_id text NOT NULL,
    role text NOT NULL,
    CONSTRAINT household_memberships_role_check CHECK ((role = ANY (ARRAY['manager'::text, 'member'::text])))
);

CREATE TABLE households (
    id text NOT NULL,
    name text NOT NULL,
    version bigint DEFAULT 1 NOT NULL
);

CREATE TABLE list_items (
    list_id text NOT NULL,
    task_id text NOT NULL
);

CREATE TABLE native_handoffs (
    code_hash text NOT NULL,
    account_id text NOT NULL,
    device_id text NOT NULL,
    challenge text NOT NULL,
    configuration_hash text NOT NULL,
    redirect_uri text NOT NULL,
    expires_at bigint NOT NULL
);

CREATE TABLE notification_subscriptions (
    id text NOT NULL,
    account_id text NOT NULL,
    device_id text NOT NULL,
    transport text NOT NULL,
    secret text NOT NULL,
    version bigint DEFAULT 1 NOT NULL,
    active bigint DEFAULT 1 NOT NULL
);

CREATE TABLE occurrence_dependencies (
    occurrence_id text NOT NULL,
    prerequisite_id text NOT NULL,
    CONSTRAINT occurrence_dependencies_check CHECK ((occurrence_id <> prerequisite_id))
);

CREATE TABLE occurrence_participants (
    occurrence_id text NOT NULL,
    account_id text NOT NULL,
    progress_id text NOT NULL,
    aggregate_consent bigint NOT NULL,
    excluded bigint DEFAULT 0 NOT NULL
);

CREATE TABLE oidc_flows (
    state_hash text NOT NULL,
    configuration_hash text NOT NULL,
    binding_hash text NOT NULL,
    nonce text NOT NULL,
    verifier text NOT NULL,
    device_id text NOT NULL,
    link_session_hash text,
    expires_at bigint NOT NULL,
    native_redirect text,
    native_challenge text,
    native_state text
);

CREATE TABLE people_request_ids (
    id text NOT NULL
);

CREATE TABLE people_requests (
    id text NOT NULL,
    sender_id text NOT NULL,
    recipient_id text NOT NULL,
    kind text NOT NULL,
    payload text NOT NULL,
    expires_at bigint NOT NULL,
    state text DEFAULT 'pending'::text NOT NULL
);

CREATE TABLE people_results (
    account_id text NOT NULL,
    operation_id text NOT NULL,
    person_id text NOT NULL
);

CREATE TABLE person_accounts (
    person_id text NOT NULL,
    account_id text NOT NULL
);

CREATE TABLE person_aliases (
    source_id text NOT NULL,
    canonical_id text NOT NULL
);

CREATE TABLE progress_entries (
    id text NOT NULL,
    progress_id text NOT NULL,
    sequence bigint NOT NULL,
    logical_order bigint NOT NULL,
    evidence text NOT NULL,
    replaces text,
    happened_at bigint NOT NULL,
    recorded_by text NOT NULL
);

CREATE TABLE receipts (
    account_id text NOT NULL,
    operation_id text NOT NULL,
    payload text NOT NULL,
    revision bigint NOT NULL,
    digest_version bigint DEFAULT 1 NOT NULL
);

CREATE TABLE reminder_deliveries (
    id text NOT NULL,
    reminder_id text NOT NULL,
    subscription_id text NOT NULL,
    revision text NOT NULL,
    due_at bigint NOT NULL,
    expires_at bigint NOT NULL,
    state text DEFAULT 'pending'::text NOT NULL,
    attempts bigint DEFAULT 0 NOT NULL,
    next_attempt bigint NOT NULL,
    lease_token text,
    lease_until bigint DEFAULT 0 NOT NULL
);

CREATE TABLE reminder_rules (
    id text NOT NULL,
    occurrence_id text NOT NULL,
    owner_id text NOT NULL,
    data text NOT NULL
);

CREATE TABLE resource_ancestors (
    resource_id text NOT NULL,
    ancestor_id text NOT NULL,
    depth bigint NOT NULL,
    CONSTRAINT resource_ancestors_depth_check CHECK (((depth >= 1) AND (depth <= 3)))
);

CREATE TABLE resource_exclusions (
    resource_id text NOT NULL,
    account_id text NOT NULL
);

CREATE TABLE resource_grants (
    resource_id text NOT NULL,
    account_id text NOT NULL,
    can_edit bigint NOT NULL,
    CONSTRAINT resource_grants_can_edit_check CHECK ((can_edit = ANY (ARRAY[(0)::bigint, (1)::bigint])))
);

CREATE TABLE resource_household_grants (
    resource_id text NOT NULL,
    household_id text NOT NULL,
    can_edit bigint NOT NULL,
    CONSTRAINT resource_household_grants_can_edit_check CHECK ((can_edit = ANY (ARRAY[(0)::bigint, (1)::bigint])))
);

CREATE TABLE resources (
    id text NOT NULL,
    owner_id text NOT NULL,
    parent_id text,
    kind text NOT NULL,
    label text NOT NULL,
    value text NOT NULL,
    version bigint DEFAULT 1 NOT NULL,
    policy_version bigint DEFAULT 1 NOT NULL,
    archived bigint DEFAULT 0 NOT NULL,
    CONSTRAINT resource_parent CHECK ((((kind = ANY (ARRAY['person'::text, 'task'::text, 'list'::text, 'calendar_source'::text])) AND (parent_id IS NULL)) OR ((kind = ANY (ARRAY['field'::text, 'execution'::text, 'occurrence'::text, 'progress'::text, 'event'::text, 'review'::text, 'reminder'::text])) AND (parent_id IS NOT NULL)))),
    CONSTRAINT resources_archived_check CHECK ((archived = ANY (ARRAY[(0)::bigint, (1)::bigint]))),
    CONSTRAINT resources_kind_check CHECK ((kind = ANY (ARRAY['person'::text, 'field'::text, 'task'::text, 'execution'::text, 'occurrence'::text, 'progress'::text, 'list'::text, 'calendar_source'::text, 'event'::text, 'review'::text, 'reminder'::text])))
);

CREATE TABLE rota_consents (
    task_id text NOT NULL,
    account_id text NOT NULL,
    version bigint NOT NULL,
    accepted bigint NOT NULL,
    CONSTRAINT rota_consents_accepted_check CHECK ((accepted = ANY (ARRAY[(0)::bigint, (1)::bigint])))
);

CREATE TABLE sessions (
    token_hash text NOT NULL,
    account_id text NOT NULL,
    device_id text NOT NULL,
    expires_at bigint NOT NULL,
    session_id text DEFAULT ''::text NOT NULL,
    created_at bigint DEFAULT 0 NOT NULL,
    auth_kind text DEFAULT 'local'::text NOT NULL,
    CONSTRAINT sessions_auth_kind_check CHECK ((auth_kind = ANY (ARRAY['local'::text, 'oidc'::text])))
);

CREATE TABLE snapshot_items (
    snapshot_id text NOT NULL,
    "position" bigint NOT NULL,
    id text NOT NULL,
    kind text NOT NULL,
    parent_id text,
    label text NOT NULL,
    value text NOT NULL,
    version bigint NOT NULL,
    policy_version bigint,
    can_edit bigint NOT NULL,
    archived bigint DEFAULT 0 NOT NULL
);

CREATE TABLE sync_batches (
    account_id text NOT NULL,
    revision bigint NOT NULL,
    payload text NOT NULL,
    created_at bigint DEFAULT 0 NOT NULL
);

CREATE TABLE sync_clock (
    id bigint NOT NULL,
    revision bigint NOT NULL,
    resource_count bigint DEFAULT 0 NOT NULL
);

CREATE TABLE sync_cursors (
    token text NOT NULL,
    account_id text NOT NULL,
    device_id text NOT NULL,
    epoch bigint NOT NULL,
    boundary bigint NOT NULL,
    snapshot_id text NOT NULL,
    "position" bigint NOT NULL,
    expires_at bigint NOT NULL
);

CREATE TABLE sync_deliveries (
    account_id text NOT NULL,
    device_id text NOT NULL,
    resource_id text NOT NULL
);

CREATE TABLE sync_devices (
    account_id text NOT NULL,
    device_id text NOT NULL,
    last_seen bigint NOT NULL,
    cursor_key text DEFAULT ''::text NOT NULL
);

CREATE TABLE sync_snapshots (
    id text NOT NULL,
    expires_at bigint NOT NULL
);

CREATE TABLE task_anchors (
    task_id text NOT NULL,
    owner_id text NOT NULL,
    spec text NOT NULL,
    version bigint DEFAULT 1 NOT NULL,
    active bigint DEFAULT 1 NOT NULL
);

CREATE TABLE task_definitions (
    task_id text NOT NULL,
    revision bigint NOT NULL,
    definition text NOT NULL
);

CREATE TABLE task_enrolment_history (
    task_id text NOT NULL,
    account_id text NOT NULL,
    version bigint NOT NULL,
    effective_date text NOT NULL,
    active bigint NOT NULL,
    aggregate_consent bigint NOT NULL,
    policy text NOT NULL
);

CREATE TABLE task_enrolments (
    task_id text NOT NULL,
    account_id text NOT NULL,
    active bigint NOT NULL,
    aggregate_consent bigint NOT NULL,
    policy text NOT NULL,
    version bigint NOT NULL
);

CREATE TABLE task_occurrences (
    id text NOT NULL,
    task_id text NOT NULL,
    slot_key text NOT NULL,
    definition text NOT NULL,
    slot text NOT NULL,
    opens_at bigint,
    closes_at bigint,
    covered_by text,
    resolved bigint DEFAULT 0 NOT NULL
);

CREATE TABLE task_rotas (
    task_id text NOT NULL,
    version bigint NOT NULL,
    participants text NOT NULL,
    next_ordinal bigint DEFAULT 0 NOT NULL
);

CREATE TABLE tasks (
    task_id text NOT NULL,
    execution_id text NOT NULL,
    definition_revision bigint NOT NULL,
    last_date text,
    once_created bigint DEFAULT 0 NOT NULL
);

CREATE TABLE timer_sessions (
    id text NOT NULL,
    progress_id text NOT NULL,
    account_id text NOT NULL,
    started_at bigint NOT NULL,
    stopped_at bigint,
    version bigint NOT NULL,
    cancelled bigint DEFAULT 0 NOT NULL,
    CONSTRAINT timer_sessions_cancelled_check CHECK ((cancelled = ANY (ARRAY[(0)::bigint, (1)::bigint]))),
    CONSTRAINT timer_sessions_check CHECK (((stopped_at IS NULL) OR (stopped_at > started_at)))
);


ALTER TABLE ONLY account_invitations
    ADD CONSTRAINT account_invitations_pkey PRIMARY KEY (id);

ALTER TABLE ONLY account_invitations
    ADD CONSTRAINT account_invitations_token_hash_key UNIQUE (token_hash);

ALTER TABLE ONLY accounts
    ADD CONSTRAINT accounts_pkey PRIMARY KEY (id);

ALTER TABLE ONLY accounts
    ADD CONSTRAINT accounts_username_key UNIQUE (username);

ALTER TABLE ONLY anchored_occurrences
    ADD CONSTRAINT anchored_occurrences_pkey PRIMARY KEY (occurrence_id);

ALTER TABLE ONLY anchored_occurrences
    ADD CONSTRAINT anchored_occurrences_task_id_anchor_key_key UNIQUE (task_id, anchor_key);

ALTER TABLE ONLY atlas_schema
    ADD CONSTRAINT atlas_schema_pkey PRIMARY KEY (version);

ALTER TABLE ONLY calendar_events
    ADD CONSTRAINT calendar_events_pkey PRIMARY KEY (id);

ALTER TABLE ONLY calendar_events
    ADD CONSTRAINT calendar_events_source_id_uid_original_key UNIQUE (source_id, uid, original);

ALTER TABLE ONLY calendar_reviews
    ADD CONSTRAINT calendar_reviews_occurrence_id_reason_source_revision_key UNIQUE (occurrence_id, reason, source_revision);

ALTER TABLE ONLY calendar_reviews
    ADD CONSTRAINT calendar_reviews_pkey PRIMARY KEY (id);

ALTER TABLE ONLY calendar_sources
    ADD CONSTRAINT calendar_sources_pkey PRIMARY KEY (id);

ALTER TABLE ONLY completion_pending
    ADD CONSTRAINT completion_pending_pkey PRIMARY KEY (occurrence_id);

ALTER TABLE ONLY completion_successors
    ADD CONSTRAINT completion_successors_pkey PRIMARY KEY (predecessor_id);

ALTER TABLE ONLY completion_successors
    ADD CONSTRAINT completion_successors_successor_id_key UNIQUE (successor_id);

ALTER TABLE ONLY dependency_rules
    ADD CONSTRAINT dependency_rules_pkey PRIMARY KEY (occurrence_id);

ALTER TABLE ONLY external_identities
    ADD CONSTRAINT external_identities_issuer_account_id_key UNIQUE (issuer, account_id);

ALTER TABLE ONLY external_identities
    ADD CONSTRAINT external_identities_pkey PRIMARY KEY (issuer, subject);

ALTER TABLE ONLY frozen_owner_visibility
    ADD CONSTRAINT frozen_owner_visibility_pkey PRIMARY KEY (resource_id);

ALTER TABLE ONLY household_invitations
    ADD CONSTRAINT household_invitations_pkey PRIMARY KEY (id);

ALTER TABLE ONLY household_memberships
    ADD CONSTRAINT household_memberships_pkey PRIMARY KEY (household_id, account_id);

ALTER TABLE ONLY households
    ADD CONSTRAINT households_pkey PRIMARY KEY (id);

ALTER TABLE ONLY list_items
    ADD CONSTRAINT list_items_pkey PRIMARY KEY (list_id, task_id);

ALTER TABLE ONLY native_handoffs
    ADD CONSTRAINT native_handoffs_pkey PRIMARY KEY (code_hash);

ALTER TABLE ONLY notification_subscriptions
    ADD CONSTRAINT notification_subscriptions_pkey PRIMARY KEY (id);

ALTER TABLE ONLY occurrence_dependencies
    ADD CONSTRAINT occurrence_dependencies_pkey PRIMARY KEY (occurrence_id, prerequisite_id);

ALTER TABLE ONLY occurrence_participants
    ADD CONSTRAINT occurrence_participants_pkey PRIMARY KEY (occurrence_id, account_id);

ALTER TABLE ONLY occurrence_participants
    ADD CONSTRAINT occurrence_participants_progress_id_key UNIQUE (progress_id);

ALTER TABLE ONLY oidc_flows
    ADD CONSTRAINT oidc_flows_pkey PRIMARY KEY (state_hash);

ALTER TABLE ONLY people_request_ids
    ADD CONSTRAINT people_request_ids_pkey PRIMARY KEY (id);

ALTER TABLE ONLY people_requests
    ADD CONSTRAINT people_requests_pkey PRIMARY KEY (id);

ALTER TABLE ONLY people_results
    ADD CONSTRAINT people_results_pkey PRIMARY KEY (account_id, operation_id);

ALTER TABLE ONLY person_accounts
    ADD CONSTRAINT person_accounts_account_id_key UNIQUE (account_id);

ALTER TABLE ONLY person_accounts
    ADD CONSTRAINT person_accounts_pkey PRIMARY KEY (person_id);

ALTER TABLE ONLY person_aliases
    ADD CONSTRAINT person_aliases_pkey PRIMARY KEY (source_id);

ALTER TABLE ONLY progress_entries
    ADD CONSTRAINT progress_entries_pkey PRIMARY KEY (id);

ALTER TABLE ONLY progress_entries
    ADD CONSTRAINT progress_entries_progress_id_sequence_key UNIQUE (progress_id, sequence);

ALTER TABLE ONLY progress_entries
    ADD CONSTRAINT progress_entries_replaces_key UNIQUE (replaces);

ALTER TABLE ONLY receipts
    ADD CONSTRAINT receipts_pkey PRIMARY KEY (account_id, operation_id);

ALTER TABLE ONLY reminder_deliveries
    ADD CONSTRAINT reminder_deliveries_pkey PRIMARY KEY (id);

ALTER TABLE ONLY reminder_deliveries
    ADD CONSTRAINT reminder_deliveries_reminder_id_subscription_id_revision_key UNIQUE (reminder_id, subscription_id, revision);

ALTER TABLE ONLY reminder_rules
    ADD CONSTRAINT reminder_rules_pkey PRIMARY KEY (id);

ALTER TABLE ONLY resource_ancestors
    ADD CONSTRAINT resource_ancestors_pkey PRIMARY KEY (resource_id, ancestor_id);

ALTER TABLE ONLY resource_exclusions
    ADD CONSTRAINT resource_exclusions_pkey PRIMARY KEY (resource_id, account_id);

ALTER TABLE ONLY resource_grants
    ADD CONSTRAINT resource_grants_pkey PRIMARY KEY (resource_id, account_id);

ALTER TABLE ONLY resource_household_grants
    ADD CONSTRAINT resource_household_grants_pkey PRIMARY KEY (resource_id, household_id);

ALTER TABLE ONLY resources
    ADD CONSTRAINT resources_pkey PRIMARY KEY (id);

ALTER TABLE ONLY rota_consents
    ADD CONSTRAINT rota_consents_pkey PRIMARY KEY (task_id, account_id);

ALTER TABLE ONLY sessions
    ADD CONSTRAINT sessions_pkey PRIMARY KEY (token_hash);

ALTER TABLE ONLY snapshot_items
    ADD CONSTRAINT snapshot_items_pkey PRIMARY KEY (snapshot_id, "position");

ALTER TABLE ONLY sync_batches
    ADD CONSTRAINT sync_batches_pkey PRIMARY KEY (account_id, revision);

ALTER TABLE ONLY sync_clock
    ADD CONSTRAINT sync_clock_pkey PRIMARY KEY (id);

ALTER TABLE ONLY sync_cursors
    ADD CONSTRAINT sync_cursors_pkey PRIMARY KEY (token);

ALTER TABLE ONLY sync_deliveries
    ADD CONSTRAINT sync_deliveries_pkey PRIMARY KEY (account_id, device_id, resource_id);

ALTER TABLE ONLY sync_devices
    ADD CONSTRAINT sync_devices_pkey PRIMARY KEY (account_id, device_id);

ALTER TABLE ONLY sync_snapshots
    ADD CONSTRAINT sync_snapshots_pkey PRIMARY KEY (id);

ALTER TABLE ONLY task_anchors
    ADD CONSTRAINT task_anchors_pkey PRIMARY KEY (task_id);

ALTER TABLE ONLY task_definitions
    ADD CONSTRAINT task_definitions_pkey PRIMARY KEY (task_id, revision);

ALTER TABLE ONLY task_enrolment_history
    ADD CONSTRAINT task_enrolment_history_pkey PRIMARY KEY (task_id, account_id, version);

ALTER TABLE ONLY task_enrolments
    ADD CONSTRAINT task_enrolments_pkey PRIMARY KEY (task_id, account_id);

ALTER TABLE ONLY task_occurrences
    ADD CONSTRAINT task_occurrences_pkey PRIMARY KEY (id);

ALTER TABLE ONLY task_occurrences
    ADD CONSTRAINT task_occurrences_task_id_slot_key_key UNIQUE (task_id, slot_key);

ALTER TABLE ONLY task_rotas
    ADD CONSTRAINT task_rotas_pkey PRIMARY KEY (task_id);

ALTER TABLE ONLY tasks
    ADD CONSTRAINT tasks_execution_id_key UNIQUE (execution_id);

ALTER TABLE ONLY tasks
    ADD CONSTRAINT tasks_pkey PRIMARY KEY (task_id);

ALTER TABLE ONLY timer_sessions
    ADD CONSTRAINT timer_sessions_pkey PRIMARY KEY (id);


CREATE INDEX account_invitation_expiry ON account_invitations USING btree (expires_at);

CREATE INDEX account_invitation_household ON account_invitations USING btree (household_id, expires_at);

CREATE INDEX account_invitation_issuer ON account_invitations USING btree (issuer_id, expires_at);

CREATE INDEX ancestors_descendants ON resource_ancestors USING btree (ancestor_id, resource_id);

CREATE INDEX anchor_reference ON anchored_occurrences USING btree (reference_id, task_id);

CREATE INDEX batches_expiry ON sync_batches USING btree (created_at, revision, account_id);

CREATE INDEX calendar_event_source ON calendar_events USING btree (source_id, id);

CREATE INDEX completion_pending_due ON completion_pending USING btree (next_check, occurrence_id);

CREATE INDEX cursors_device ON sync_cursors USING btree (account_id, device_id);

CREATE INDEX cursors_expiry ON sync_cursors USING btree (expires_at);

CREATE INDEX cursors_snapshot ON sync_cursors USING btree (snapshot_id);

CREATE UNIQUE INDEX defaults_account ON default_templates USING btree (account_id, resource_kind);

CREATE UNIQUE INDEX defaults_household ON default_templates USING btree (household_id, resource_kind);

CREATE INDEX deliveries_due ON reminder_deliveries USING btree (state, next_attempt, lease_until);

CREATE INDEX dependency_reverse ON occurrence_dependencies USING btree (prerequisite_id, occurrence_id);

CREATE INDEX devices_last_seen ON sync_devices USING btree (last_seen);

CREATE INDEX enrolment_history_date ON task_enrolment_history USING btree (task_id, effective_date, account_id);

CREATE INDEX grants_account ON resource_grants USING btree (account_id, resource_id);

CREATE INDEX household_grants_household ON resource_household_grants USING btree (household_id, resource_id);

CREATE INDEX invitations_recipient ON household_invitations USING btree (recipient_id, status, id);

CREATE INDEX lists_task ON list_items USING btree (task_id, list_id);

CREATE INDEX memberships_account ON household_memberships USING btree (account_id, household_id);

CREATE INDEX native_handoff_expiry ON native_handoffs USING btree (expires_at);

CREATE INDEX occurrences_task ON task_occurrences USING btree (task_id, id);

CREATE INDEX occurrences_window ON task_occurrences USING btree (opens_at, closes_at, id);

CREATE INDEX oidc_flow_expiry ON oidc_flows USING btree (expires_at);

CREATE INDEX people_requests_expiry ON people_requests USING btree (expires_at, id);

CREATE INDEX people_requests_recipient ON people_requests USING btree (recipient_id, state, id);

CREATE INDEX person_alias_target ON person_aliases USING btree (canonical_id);

CREATE INDEX progress_stream ON progress_entries USING btree (progress_id, sequence);

CREATE INDEX resources_owner ON resources USING btree (owner_id, id);

CREATE INDEX resources_parent ON resources USING btree (parent_id, id);

CREATE INDEX session_account ON sessions USING btree (account_id);

CREATE UNIQUE INDEX session_identity ON sessions USING btree (session_id);

CREATE INDEX sessions_expiry ON sessions USING btree (expires_at);

CREATE INDEX snapshots_expiry ON sync_snapshots USING btree (expires_at);

CREATE INDEX subscriptions_account ON notification_subscriptions USING btree (account_id, id);

CREATE INDEX timer_account_times ON timer_sessions USING btree (account_id, started_at, stopped_at);

CREATE UNIQUE INDEX timer_active_account ON timer_sessions USING btree (account_id) WHERE ((stopped_at IS NULL) AND (cancelled = 0));

CREATE INDEX timer_progress ON timer_sessions USING btree (progress_id, id);

CREATE TRIGGER resource_ancestors_insert AFTER INSERT ON resources FOR EACH ROW EXECUTE FUNCTION atlas_resource_ancestors_insert();

CREATE TRIGGER resource_ancestors_move AFTER UPDATE OF parent_id ON resources FOR EACH ROW EXECUTE FUNCTION atlas_resource_ancestors_move();

CREATE TRIGGER resource_parent_guard BEFORE INSERT OR UPDATE OF kind, parent_id ON resources FOR EACH ROW EXECUTE FUNCTION atlas_resource_parent_guard();

ALTER TABLE ONLY account_invitations
    ADD CONSTRAINT account_invitations_household_id_fkey FOREIGN KEY (household_id) REFERENCES households(id);

ALTER TABLE ONLY account_invitations
    ADD CONSTRAINT account_invitations_issuer_id_fkey FOREIGN KEY (issuer_id) REFERENCES accounts(id);

ALTER TABLE ONLY account_invitations
    ADD CONSTRAINT account_invitations_redeemed_by_fkey FOREIGN KEY (redeemed_by) REFERENCES accounts(id);

ALTER TABLE ONLY accounts
    ADD CONSTRAINT accounts_primary_household_id_fkey FOREIGN KEY (primary_household_id) REFERENCES households(id);

ALTER TABLE ONLY anchored_occurrences
    ADD CONSTRAINT anchored_occurrences_occurrence_id_fkey FOREIGN KEY (occurrence_id) REFERENCES task_occurrences(id);

ALTER TABLE ONLY anchored_occurrences
    ADD CONSTRAINT anchored_occurrences_reference_id_fkey FOREIGN KEY (reference_id) REFERENCES resources(id);

ALTER TABLE ONLY anchored_occurrences
    ADD CONSTRAINT anchored_occurrences_task_id_fkey FOREIGN KEY (task_id) REFERENCES task_anchors(task_id);

ALTER TABLE ONLY calendar_events
    ADD CONSTRAINT calendar_events_id_fkey FOREIGN KEY (id) REFERENCES resources(id);

ALTER TABLE ONLY calendar_events
    ADD CONSTRAINT calendar_events_source_id_fkey FOREIGN KEY (source_id) REFERENCES calendar_sources(id);

ALTER TABLE ONLY calendar_reviews
    ADD CONSTRAINT calendar_reviews_id_fkey FOREIGN KEY (id) REFERENCES resources(id);

ALTER TABLE ONLY calendar_reviews
    ADD CONSTRAINT calendar_reviews_occurrence_id_fkey FOREIGN KEY (occurrence_id) REFERENCES task_occurrences(id);

ALTER TABLE ONLY calendar_reviews
    ADD CONSTRAINT calendar_reviews_reference_id_fkey FOREIGN KEY (reference_id) REFERENCES resources(id);

ALTER TABLE ONLY calendar_sources
    ADD CONSTRAINT calendar_sources_id_fkey FOREIGN KEY (id) REFERENCES resources(id);

ALTER TABLE ONLY completion_pending
    ADD CONSTRAINT completion_pending_occurrence_id_fkey FOREIGN KEY (occurrence_id) REFERENCES task_occurrences(id);

ALTER TABLE ONLY completion_successors
    ADD CONSTRAINT completion_successors_predecessor_id_fkey FOREIGN KEY (predecessor_id) REFERENCES task_occurrences(id);

ALTER TABLE ONLY completion_successors
    ADD CONSTRAINT completion_successors_successor_id_fkey FOREIGN KEY (successor_id) REFERENCES task_occurrences(id);

ALTER TABLE ONLY default_templates
    ADD CONSTRAINT default_templates_account_id_fkey FOREIGN KEY (account_id) REFERENCES accounts(id);

ALTER TABLE ONLY default_templates
    ADD CONSTRAINT default_templates_household_id_fkey FOREIGN KEY (household_id) REFERENCES households(id);

ALTER TABLE ONLY dependency_rules
    ADD CONSTRAINT dependency_rules_occurrence_id_fkey FOREIGN KEY (occurrence_id) REFERENCES task_occurrences(id);

ALTER TABLE ONLY external_identities
    ADD CONSTRAINT external_identities_account_id_fkey FOREIGN KEY (account_id) REFERENCES accounts(id);

ALTER TABLE ONLY frozen_owner_visibility
    ADD CONSTRAINT frozen_owner_visibility_resource_id_fkey FOREIGN KEY (resource_id) REFERENCES resources(id);

ALTER TABLE ONLY household_invitations
    ADD CONSTRAINT household_invitations_household_id_fkey FOREIGN KEY (household_id) REFERENCES households(id);

ALTER TABLE ONLY household_invitations
    ADD CONSTRAINT household_invitations_recipient_id_fkey FOREIGN KEY (recipient_id) REFERENCES accounts(id);

ALTER TABLE ONLY household_invitations
    ADD CONSTRAINT household_invitations_sender_id_fkey FOREIGN KEY (sender_id) REFERENCES accounts(id);

ALTER TABLE ONLY household_memberships
    ADD CONSTRAINT household_memberships_account_id_fkey FOREIGN KEY (account_id) REFERENCES accounts(id);

ALTER TABLE ONLY household_memberships
    ADD CONSTRAINT household_memberships_household_id_fkey FOREIGN KEY (household_id) REFERENCES households(id);

ALTER TABLE ONLY list_items
    ADD CONSTRAINT list_items_list_id_fkey FOREIGN KEY (list_id) REFERENCES resources(id);

ALTER TABLE ONLY list_items
    ADD CONSTRAINT list_items_task_id_fkey FOREIGN KEY (task_id) REFERENCES resources(id);

ALTER TABLE ONLY native_handoffs
    ADD CONSTRAINT native_handoffs_account_id_fkey FOREIGN KEY (account_id) REFERENCES accounts(id);

ALTER TABLE ONLY notification_subscriptions
    ADD CONSTRAINT notification_subscriptions_account_id_fkey FOREIGN KEY (account_id) REFERENCES accounts(id);

ALTER TABLE ONLY occurrence_dependencies
    ADD CONSTRAINT occurrence_dependencies_occurrence_id_fkey FOREIGN KEY (occurrence_id) REFERENCES task_occurrences(id);

ALTER TABLE ONLY occurrence_dependencies
    ADD CONSTRAINT occurrence_dependencies_prerequisite_id_fkey FOREIGN KEY (prerequisite_id) REFERENCES task_occurrences(id);

ALTER TABLE ONLY occurrence_participants
    ADD CONSTRAINT occurrence_participants_account_id_fkey FOREIGN KEY (account_id) REFERENCES accounts(id);

ALTER TABLE ONLY occurrence_participants
    ADD CONSTRAINT occurrence_participants_occurrence_id_fkey FOREIGN KEY (occurrence_id) REFERENCES task_occurrences(id);

ALTER TABLE ONLY occurrence_participants
    ADD CONSTRAINT occurrence_participants_progress_id_fkey FOREIGN KEY (progress_id) REFERENCES resources(id);

ALTER TABLE ONLY people_requests
    ADD CONSTRAINT people_requests_recipient_id_fkey FOREIGN KEY (recipient_id) REFERENCES accounts(id);

ALTER TABLE ONLY people_requests
    ADD CONSTRAINT people_requests_sender_id_fkey FOREIGN KEY (sender_id) REFERENCES accounts(id);

ALTER TABLE ONLY people_results
    ADD CONSTRAINT people_results_account_id_fkey FOREIGN KEY (account_id) REFERENCES accounts(id);

ALTER TABLE ONLY person_accounts
    ADD CONSTRAINT person_accounts_account_id_fkey FOREIGN KEY (account_id) REFERENCES accounts(id);

ALTER TABLE ONLY person_accounts
    ADD CONSTRAINT person_accounts_person_id_fkey FOREIGN KEY (person_id) REFERENCES resources(id);

ALTER TABLE ONLY person_aliases
    ADD CONSTRAINT person_aliases_canonical_id_fkey FOREIGN KEY (canonical_id) REFERENCES resources(id);

ALTER TABLE ONLY progress_entries
    ADD CONSTRAINT progress_entries_progress_id_fkey FOREIGN KEY (progress_id) REFERENCES resources(id);

ALTER TABLE ONLY progress_entries
    ADD CONSTRAINT progress_entries_recorded_by_fkey FOREIGN KEY (recorded_by) REFERENCES accounts(id);

ALTER TABLE ONLY progress_entries
    ADD CONSTRAINT progress_entries_replaces_fkey FOREIGN KEY (replaces) REFERENCES progress_entries(id);

ALTER TABLE ONLY receipts
    ADD CONSTRAINT receipts_account_id_fkey FOREIGN KEY (account_id) REFERENCES accounts(id);

ALTER TABLE ONLY reminder_deliveries
    ADD CONSTRAINT reminder_deliveries_reminder_id_fkey FOREIGN KEY (reminder_id) REFERENCES reminder_rules(id);

ALTER TABLE ONLY reminder_deliveries
    ADD CONSTRAINT reminder_deliveries_subscription_id_fkey FOREIGN KEY (subscription_id) REFERENCES notification_subscriptions(id);

ALTER TABLE ONLY reminder_rules
    ADD CONSTRAINT reminder_rules_id_fkey FOREIGN KEY (id) REFERENCES resources(id);

ALTER TABLE ONLY reminder_rules
    ADD CONSTRAINT reminder_rules_occurrence_id_fkey FOREIGN KEY (occurrence_id) REFERENCES task_occurrences(id);

ALTER TABLE ONLY reminder_rules
    ADD CONSTRAINT reminder_rules_owner_id_fkey FOREIGN KEY (owner_id) REFERENCES accounts(id);

ALTER TABLE ONLY resource_ancestors
    ADD CONSTRAINT resource_ancestors_ancestor_id_fkey FOREIGN KEY (ancestor_id) REFERENCES resources(id);

ALTER TABLE ONLY resource_ancestors
    ADD CONSTRAINT resource_ancestors_resource_id_fkey FOREIGN KEY (resource_id) REFERENCES resources(id);

ALTER TABLE ONLY resource_exclusions
    ADD CONSTRAINT resource_exclusions_account_id_fkey FOREIGN KEY (account_id) REFERENCES accounts(id);

ALTER TABLE ONLY resource_exclusions
    ADD CONSTRAINT resource_exclusions_resource_id_fkey FOREIGN KEY (resource_id) REFERENCES resources(id);

ALTER TABLE ONLY resource_grants
    ADD CONSTRAINT resource_grants_account_id_fkey FOREIGN KEY (account_id) REFERENCES accounts(id);

ALTER TABLE ONLY resource_grants
    ADD CONSTRAINT resource_grants_resource_id_fkey FOREIGN KEY (resource_id) REFERENCES resources(id);

ALTER TABLE ONLY resource_household_grants
    ADD CONSTRAINT resource_household_grants_household_id_fkey FOREIGN KEY (household_id) REFERENCES households(id);

ALTER TABLE ONLY resource_household_grants
    ADD CONSTRAINT resource_household_grants_resource_id_fkey FOREIGN KEY (resource_id) REFERENCES resources(id);

ALTER TABLE ONLY resources
    ADD CONSTRAINT resources_owner_id_fkey FOREIGN KEY (owner_id) REFERENCES accounts(id);

ALTER TABLE ONLY resources
    ADD CONSTRAINT resources_parent_id_fkey FOREIGN KEY (parent_id) REFERENCES resources(id);

ALTER TABLE ONLY rota_consents
    ADD CONSTRAINT rota_consents_account_id_fkey FOREIGN KEY (account_id) REFERENCES accounts(id);

ALTER TABLE ONLY rota_consents
    ADD CONSTRAINT rota_consents_task_id_fkey FOREIGN KEY (task_id) REFERENCES tasks(task_id);

ALTER TABLE ONLY sessions
    ADD CONSTRAINT sessions_account_id_fkey FOREIGN KEY (account_id) REFERENCES accounts(id);

ALTER TABLE ONLY snapshot_items
    ADD CONSTRAINT snapshot_items_snapshot_id_fkey FOREIGN KEY (snapshot_id) REFERENCES sync_snapshots(id) ON DELETE CASCADE;

ALTER TABLE ONLY sync_batches
    ADD CONSTRAINT sync_batches_account_id_fkey FOREIGN KEY (account_id) REFERENCES accounts(id);

ALTER TABLE ONLY sync_cursors
    ADD CONSTRAINT sync_cursors_account_id_fkey FOREIGN KEY (account_id) REFERENCES accounts(id);

ALTER TABLE ONLY sync_cursors
    ADD CONSTRAINT sync_cursors_snapshot_id_fkey FOREIGN KEY (snapshot_id) REFERENCES sync_snapshots(id);

ALTER TABLE ONLY sync_deliveries
    ADD CONSTRAINT sync_deliveries_account_id_device_id_fkey FOREIGN KEY (account_id, device_id) REFERENCES sync_devices(account_id, device_id) ON DELETE CASCADE;

ALTER TABLE ONLY sync_devices
    ADD CONSTRAINT sync_devices_account_id_fkey FOREIGN KEY (account_id) REFERENCES accounts(id);

ALTER TABLE ONLY task_anchors
    ADD CONSTRAINT task_anchors_owner_id_fkey FOREIGN KEY (owner_id) REFERENCES accounts(id);

ALTER TABLE ONLY task_anchors
    ADD CONSTRAINT task_anchors_task_id_fkey FOREIGN KEY (task_id) REFERENCES tasks(task_id);

ALTER TABLE ONLY task_definitions
    ADD CONSTRAINT task_definitions_task_id_fkey FOREIGN KEY (task_id) REFERENCES resources(id);

ALTER TABLE ONLY task_enrolment_history
    ADD CONSTRAINT task_enrolment_history_account_id_fkey FOREIGN KEY (account_id) REFERENCES accounts(id);

ALTER TABLE ONLY task_enrolment_history
    ADD CONSTRAINT task_enrolment_history_task_id_fkey FOREIGN KEY (task_id) REFERENCES tasks(task_id);

ALTER TABLE ONLY task_enrolments
    ADD CONSTRAINT task_enrolments_account_id_fkey FOREIGN KEY (account_id) REFERENCES accounts(id);

ALTER TABLE ONLY task_enrolments
    ADD CONSTRAINT task_enrolments_task_id_fkey FOREIGN KEY (task_id) REFERENCES tasks(task_id);

ALTER TABLE ONLY task_occurrences
    ADD CONSTRAINT task_occurrences_covered_by_fkey FOREIGN KEY (covered_by) REFERENCES task_occurrences(id);

ALTER TABLE ONLY task_occurrences
    ADD CONSTRAINT task_occurrences_id_fkey FOREIGN KEY (id) REFERENCES resources(id);

ALTER TABLE ONLY task_occurrences
    ADD CONSTRAINT task_occurrences_task_id_fkey FOREIGN KEY (task_id) REFERENCES tasks(task_id);

ALTER TABLE ONLY task_rotas
    ADD CONSTRAINT task_rotas_task_id_fkey FOREIGN KEY (task_id) REFERENCES tasks(task_id);

ALTER TABLE ONLY tasks
    ADD CONSTRAINT tasks_execution_id_fkey FOREIGN KEY (execution_id) REFERENCES resources(id);

ALTER TABLE ONLY tasks
    ADD CONSTRAINT tasks_task_id_fkey FOREIGN KEY (task_id) REFERENCES resources(id);

ALTER TABLE ONLY timer_sessions
    ADD CONSTRAINT timer_sessions_account_id_fkey FOREIGN KEY (account_id) REFERENCES accounts(id);

ALTER TABLE ONLY timer_sessions
    ADD CONSTRAINT timer_sessions_progress_id_fkey FOREIGN KEY (progress_id) REFERENCES resources(id);



INSERT INTO sync_clock(id,revision,resource_count) VALUES(1,0,0);
INSERT INTO atlas_schema(version) VALUES(1000);
