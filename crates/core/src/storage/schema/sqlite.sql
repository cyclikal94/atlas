-- Current pre-release baseline. Incompatible experimental databases require an explicit reset.
CREATE TABLE atlas_schema (version BIGINT PRIMARY KEY);

CREATE TABLE sync_clock (id BIGINT PRIMARY KEY, revision BIGINT NOT NULL, resource_count BIGINT NOT NULL DEFAULT 0);

CREATE TABLE accounts (
 id TEXT PRIMARY KEY, username TEXT NOT NULL UNIQUE, password_hash TEXT NOT NULL,
 access_epoch BIGINT NOT NULL DEFAULT 0
, sync_floor BIGINT NOT NULL DEFAULT 0, primary_household_id TEXT REFERENCES households(id), preferences_version BIGINT NOT NULL DEFAULT 1);

CREATE TABLE sessions (
 token_hash TEXT PRIMARY KEY, account_id TEXT NOT NULL REFERENCES accounts(id),
 device_id TEXT NOT NULL, expires_at BIGINT NOT NULL
, session_id TEXT NOT NULL DEFAULT '', created_at BIGINT NOT NULL DEFAULT 0, auth_kind TEXT NOT NULL DEFAULT 'local' CHECK (auth_kind IN ('local','oidc')));

CREATE TABLE resource_grants (
 resource_id TEXT NOT NULL REFERENCES resources(id), account_id TEXT NOT NULL REFERENCES accounts(id),
 can_edit BIGINT NOT NULL CHECK(can_edit IN (0,1)), PRIMARY KEY(resource_id,account_id)
);

CREATE TABLE receipts (
 account_id TEXT NOT NULL REFERENCES accounts(id), operation_id TEXT NOT NULL,
 payload TEXT NOT NULL, revision BIGINT NOT NULL, digest_version BIGINT NOT NULL DEFAULT 1, PRIMARY KEY(account_id,operation_id)
);

CREATE TABLE sync_batches (
 account_id TEXT NOT NULL REFERENCES accounts(id), revision BIGINT NOT NULL,
 payload TEXT NOT NULL, created_at BIGINT NOT NULL DEFAULT 0, PRIMARY KEY(account_id,revision)
);

CREATE TABLE sync_snapshots (
 id TEXT PRIMARY KEY, expires_at BIGINT NOT NULL
);

CREATE TABLE sync_cursors (
 token TEXT PRIMARY KEY, account_id TEXT NOT NULL REFERENCES accounts(id), device_id TEXT NOT NULL,
 epoch BIGINT NOT NULL, boundary BIGINT NOT NULL, snapshot_id TEXT NOT NULL REFERENCES sync_snapshots(id),
 position BIGINT NOT NULL, expires_at BIGINT NOT NULL
);

CREATE TABLE person_aliases (
 source_id TEXT PRIMARY KEY, canonical_id TEXT NOT NULL REFERENCES resources(id)
);

CREATE TABLE person_accounts (
 person_id TEXT PRIMARY KEY REFERENCES resources(id),
 account_id TEXT NOT NULL UNIQUE REFERENCES accounts(id)
);

CREATE TABLE frozen_owner_visibility (
 resource_id TEXT PRIMARY KEY REFERENCES resources(id)
);

CREATE TABLE snapshot_items (
 snapshot_id TEXT NOT NULL REFERENCES sync_snapshots(id) ON DELETE CASCADE,
 position BIGINT NOT NULL, id TEXT NOT NULL, kind TEXT NOT NULL, parent_id TEXT,
 label TEXT NOT NULL, value TEXT NOT NULL, version BIGINT NOT NULL,
 policy_version BIGINT, can_edit BIGINT NOT NULL, archived BIGINT NOT NULL DEFAULT 0,
 PRIMARY KEY(snapshot_id,position)
);

CREATE TABLE sync_devices (
 account_id TEXT NOT NULL REFERENCES accounts(id), device_id TEXT NOT NULL,
 last_seen BIGINT NOT NULL, cursor_key TEXT NOT NULL DEFAULT '', PRIMARY KEY(account_id,device_id)
);

CREATE TABLE sync_deliveries (
 account_id TEXT NOT NULL, device_id TEXT NOT NULL, resource_id TEXT NOT NULL,
 PRIMARY KEY(account_id,device_id,resource_id),
 FOREIGN KEY(account_id,device_id) REFERENCES sync_devices(account_id,device_id) ON DELETE CASCADE
);

CREATE TABLE households (
 id TEXT PRIMARY KEY, name TEXT NOT NULL, version BIGINT NOT NULL DEFAULT 1
);

CREATE TABLE household_memberships (
 household_id TEXT NOT NULL REFERENCES households(id),
 account_id TEXT NOT NULL REFERENCES accounts(id),
 role TEXT NOT NULL CHECK(role IN ('manager','member')),
 PRIMARY KEY(household_id,account_id)
);

CREATE TABLE household_invitations (
 id TEXT PRIMARY KEY, household_id TEXT NOT NULL REFERENCES households(id),
 sender_id TEXT NOT NULL REFERENCES accounts(id), recipient_id TEXT NOT NULL REFERENCES accounts(id),
 status TEXT NOT NULL CHECK(status IN ('pending','accepted','declined','revoked')),
 expires_at BIGINT NOT NULL, version BIGINT NOT NULL DEFAULT 1
);

CREATE TABLE resource_household_grants (
 resource_id TEXT NOT NULL REFERENCES resources(id), household_id TEXT NOT NULL REFERENCES households(id),
 can_edit BIGINT NOT NULL CHECK(can_edit IN (0,1)), PRIMARY KEY(resource_id,household_id)
);

CREATE TABLE resource_exclusions (
 resource_id TEXT NOT NULL REFERENCES resources(id), account_id TEXT NOT NULL REFERENCES accounts(id),
 PRIMARY KEY(resource_id,account_id)
);

CREATE TABLE external_identities (
    issuer TEXT NOT NULL,
    subject TEXT NOT NULL,
    account_id TEXT NOT NULL REFERENCES accounts(id),
    PRIMARY KEY (issuer, subject),
    UNIQUE (issuer, account_id)
);

CREATE TABLE oidc_flows (
    state_hash TEXT PRIMARY KEY,
    configuration_hash TEXT NOT NULL,
    binding_hash TEXT NOT NULL,
    nonce TEXT NOT NULL,
    verifier TEXT NOT NULL,
    device_id TEXT NOT NULL,
    link_session_hash TEXT,
    expires_at BIGINT NOT NULL
, native_redirect TEXT, native_challenge TEXT, native_state TEXT);

CREATE TABLE account_invitations (
    id TEXT PRIMARY KEY,
    token_hash TEXT NOT NULL UNIQUE,
    issuer_id TEXT REFERENCES accounts(id),
    household_id TEXT REFERENCES households(id),
    created_at BIGINT NOT NULL,
    expires_at BIGINT NOT NULL,
    redeemed_by TEXT REFERENCES accounts(id),
    revoked BIGINT NOT NULL DEFAULT 0 CHECK (revoked IN (0,1))
);

CREATE TABLE native_handoffs (
    code_hash TEXT PRIMARY KEY,
    account_id TEXT NOT NULL REFERENCES accounts(id),
    device_id TEXT NOT NULL,
    challenge TEXT NOT NULL,
    configuration_hash TEXT NOT NULL,
    redirect_uri TEXT NOT NULL,
    expires_at BIGINT NOT NULL
);

CREATE TABLE resource_ancestors (
 resource_id TEXT NOT NULL REFERENCES resources(id),
 ancestor_id TEXT NOT NULL REFERENCES resources(id),
 depth BIGINT NOT NULL CHECK(depth BETWEEN 1 AND 3),
 PRIMARY KEY(resource_id,ancestor_id)
);

CREATE TABLE "default_templates" (account_id TEXT REFERENCES accounts(id),household_id TEXT REFERENCES households(id),resource_kind TEXT NOT NULL CHECK(resource_kind IN ('person','field','task','list','progress')),template TEXT NOT NULL,version BIGINT NOT NULL DEFAULT 1,CHECK((account_id IS NULL) <> (household_id IS NULL)));

CREATE TABLE task_definitions (
 task_id TEXT NOT NULL REFERENCES resources(id), revision BIGINT NOT NULL,
 definition TEXT NOT NULL, PRIMARY KEY(task_id,revision)
);

CREATE TABLE tasks (
 task_id TEXT PRIMARY KEY REFERENCES resources(id), execution_id TEXT NOT NULL UNIQUE REFERENCES resources(id),
 definition_revision BIGINT NOT NULL, last_date TEXT, once_created BIGINT NOT NULL DEFAULT 0
);

CREATE TABLE task_enrolments (
 task_id TEXT NOT NULL REFERENCES tasks(task_id), account_id TEXT NOT NULL REFERENCES accounts(id),
 active BIGINT NOT NULL, aggregate_consent BIGINT NOT NULL, policy TEXT NOT NULL,
 version BIGINT NOT NULL, PRIMARY KEY(task_id,account_id)
);

CREATE TABLE task_occurrences (
 id TEXT PRIMARY KEY REFERENCES resources(id), task_id TEXT NOT NULL REFERENCES tasks(task_id),
 slot_key TEXT NOT NULL, definition TEXT NOT NULL, slot TEXT NOT NULL,
 opens_at BIGINT, closes_at BIGINT, covered_by TEXT REFERENCES task_occurrences(id),
 resolved BIGINT NOT NULL DEFAULT 0, UNIQUE(task_id,slot_key)
);

CREATE TABLE occurrence_participants (
 occurrence_id TEXT NOT NULL REFERENCES task_occurrences(id), account_id TEXT NOT NULL REFERENCES accounts(id),
 progress_id TEXT NOT NULL UNIQUE REFERENCES resources(id), aggregate_consent BIGINT NOT NULL,
 excluded BIGINT NOT NULL DEFAULT 0,
 PRIMARY KEY(occurrence_id,account_id)
);

CREATE TABLE progress_entries (
 id TEXT PRIMARY KEY, progress_id TEXT NOT NULL REFERENCES resources(id),
 sequence BIGINT NOT NULL, logical_order BIGINT NOT NULL, evidence TEXT NOT NULL, replaces TEXT UNIQUE REFERENCES progress_entries(id),
 happened_at BIGINT NOT NULL, recorded_by TEXT NOT NULL REFERENCES accounts(id),
 UNIQUE(progress_id,sequence)
);

CREATE TABLE list_items (
 list_id TEXT NOT NULL REFERENCES resources(id), task_id TEXT NOT NULL REFERENCES resources(id),
 PRIMARY KEY(list_id,task_id)
);


CREATE TABLE task_enrolment_history (
 task_id TEXT NOT NULL REFERENCES tasks(task_id), account_id TEXT NOT NULL REFERENCES accounts(id),
 version BIGINT NOT NULL, effective_date TEXT NOT NULL, active BIGINT NOT NULL,
 aggregate_consent BIGINT NOT NULL, policy TEXT NOT NULL,
 PRIMARY KEY(task_id,account_id,version)
);

CREATE TABLE "resources" (
 id TEXT PRIMARY KEY, owner_id TEXT NOT NULL REFERENCES accounts(id),
 parent_id TEXT REFERENCES "resources"(id),
 kind TEXT NOT NULL CHECK(kind IN ('person','field','task','execution','occurrence','progress','list','calendar_source','event','review','reminder')),
 label TEXT NOT NULL, value TEXT NOT NULL, version BIGINT NOT NULL DEFAULT 1,
 policy_version BIGINT NOT NULL DEFAULT 1,
 archived BIGINT NOT NULL DEFAULT 0 CHECK(archived IN (0,1)),
 CHECK ((kind IN ('person','task','list','calendar_source') AND parent_id IS NULL) OR
        (kind IN ('field','execution','occurrence','progress','event','review','reminder') AND parent_id IS NOT NULL))
);

CREATE TABLE calendar_sources (
 id TEXT PRIMARY KEY REFERENCES resources(id), connection TEXT,
 timezone TEXT NOT NULL, generation BIGINT NOT NULL DEFAULT 0,
 committed_generation BIGINT NOT NULL DEFAULT 0, lease_until BIGINT NOT NULL DEFAULT 0,
 next_refresh BIGINT NOT NULL DEFAULT 0, etag TEXT, modified TEXT,
 last_success BIGINT, health TEXT NOT NULL DEFAULT 'pending',
 coverage_start TEXT, coverage_end TEXT
);

CREATE TABLE calendar_events (
 id TEXT PRIMARY KEY REFERENCES resources(id), source_id TEXT NOT NULL REFERENCES calendar_sources(id),
 uid TEXT NOT NULL, original TEXT NOT NULL, data TEXT NOT NULL,
 generation BIGINT NOT NULL, UNIQUE(source_id,uid,original)
);

CREATE TABLE task_anchors (
 task_id TEXT PRIMARY KEY REFERENCES tasks(task_id), owner_id TEXT NOT NULL REFERENCES accounts(id),
 spec TEXT NOT NULL, version BIGINT NOT NULL DEFAULT 1, active BIGINT NOT NULL DEFAULT 1
);

CREATE TABLE anchored_occurrences (
 occurrence_id TEXT PRIMARY KEY REFERENCES task_occurrences(id), task_id TEXT NOT NULL REFERENCES task_anchors(task_id),
 anchor_key TEXT NOT NULL, reference_id TEXT NOT NULL REFERENCES resources(id),
 anchor_revision BIGINT NOT NULL, UNIQUE(task_id,anchor_key)
);

CREATE TABLE calendar_reviews (
 id TEXT PRIMARY KEY REFERENCES resources(id), occurrence_id TEXT NOT NULL REFERENCES task_occurrences(id),
 reason TEXT NOT NULL, reference_id TEXT REFERENCES resources(id), source_revision BIGINT NOT NULL,
 resolved BIGINT NOT NULL DEFAULT 0, UNIQUE(occurrence_id,reason,source_revision)
);

CREATE TABLE reminder_rules (
 id TEXT PRIMARY KEY REFERENCES resources(id), occurrence_id TEXT NOT NULL REFERENCES task_occurrences(id),
 owner_id TEXT NOT NULL REFERENCES accounts(id), data TEXT NOT NULL
);

CREATE TABLE notification_subscriptions (
 id TEXT PRIMARY KEY, account_id TEXT NOT NULL REFERENCES accounts(id), device_id TEXT NOT NULL,
 transport TEXT NOT NULL, secret TEXT NOT NULL, version BIGINT NOT NULL DEFAULT 1,
 active BIGINT NOT NULL DEFAULT 1
);

CREATE TABLE reminder_deliveries (
 id TEXT PRIMARY KEY, reminder_id TEXT NOT NULL REFERENCES reminder_rules(id),
 subscription_id TEXT NOT NULL REFERENCES notification_subscriptions(id),
 revision TEXT NOT NULL, due_at BIGINT NOT NULL, expires_at BIGINT NOT NULL,
 state TEXT NOT NULL DEFAULT 'pending', attempts BIGINT NOT NULL DEFAULT 0,
 next_attempt BIGINT NOT NULL, lease_token TEXT, lease_until BIGINT NOT NULL DEFAULT 0,
 UNIQUE(reminder_id,subscription_id,revision)
);

CREATE TABLE occurrence_dependencies (
 occurrence_id TEXT NOT NULL REFERENCES task_occurrences(id),
 prerequisite_id TEXT NOT NULL REFERENCES task_occurrences(id),
 PRIMARY KEY(occurrence_id, prerequisite_id),
 CHECK(occurrence_id <> prerequisite_id)
);

CREATE TABLE dependency_rules (
 occurrence_id TEXT PRIMARY KEY REFERENCES task_occurrences(id),
 strict BIGINT NOT NULL CHECK(strict IN (0,1))
);

CREATE TABLE timer_sessions (
 id TEXT PRIMARY KEY,
 progress_id TEXT NOT NULL REFERENCES resources(id),
 account_id TEXT NOT NULL REFERENCES accounts(id),
 started_at BIGINT NOT NULL, stopped_at BIGINT,
 version BIGINT NOT NULL, cancelled BIGINT NOT NULL DEFAULT 0 CHECK(cancelled IN (0,1)),
 CHECK(stopped_at IS NULL OR stopped_at>started_at)
);

CREATE TABLE completion_successors (
 predecessor_id TEXT PRIMARY KEY REFERENCES task_occurrences(id),
 successor_id TEXT NOT NULL UNIQUE REFERENCES task_occurrences(id)
);

CREATE TABLE task_rotas (
 task_id TEXT PRIMARY KEY REFERENCES tasks(task_id),
 version BIGINT NOT NULL, participants TEXT NOT NULL,
 next_ordinal BIGINT NOT NULL DEFAULT 0
);

CREATE TABLE rota_consents (
 task_id TEXT NOT NULL REFERENCES tasks(task_id),
 account_id TEXT NOT NULL REFERENCES accounts(id),
 version BIGINT NOT NULL, accepted BIGINT NOT NULL CHECK(accepted IN (0,1)),
 PRIMARY KEY(task_id,account_id)
);

CREATE TABLE completion_pending (
 occurrence_id TEXT PRIMARY KEY REFERENCES task_occurrences(id),
 next_check BIGINT NOT NULL DEFAULT 0
);

CREATE TABLE people_requests (
 id TEXT PRIMARY KEY, sender_id TEXT NOT NULL REFERENCES accounts(id),
 recipient_id TEXT NOT NULL REFERENCES accounts(id), kind TEXT NOT NULL,
 payload TEXT NOT NULL, expires_at BIGINT NOT NULL, state TEXT NOT NULL DEFAULT 'pending'
);

CREATE TABLE people_results (
 account_id TEXT NOT NULL REFERENCES accounts(id), operation_id TEXT NOT NULL,
 person_id TEXT NOT NULL, PRIMARY KEY(account_id,operation_id)
);

CREATE TABLE people_request_ids (id TEXT PRIMARY KEY);

CREATE INDEX person_alias_target ON person_aliases(canonical_id);

CREATE INDEX grants_account ON resource_grants(account_id,resource_id);

CREATE INDEX cursors_expiry ON sync_cursors(expires_at);

CREATE INDEX snapshots_expiry ON sync_snapshots(expires_at);

CREATE INDEX sessions_expiry ON sessions(expires_at);

CREATE INDEX memberships_account ON household_memberships(account_id,household_id);

CREATE INDEX invitations_recipient ON household_invitations(recipient_id,status,id);

CREATE INDEX household_grants_household ON resource_household_grants(household_id,resource_id);

CREATE INDEX oidc_flow_expiry ON oidc_flows(expires_at);

CREATE UNIQUE INDEX session_identity ON sessions(session_id);

CREATE INDEX session_account ON sessions(account_id);

CREATE INDEX account_invitation_issuer ON account_invitations(issuer_id,expires_at);

CREATE INDEX account_invitation_household ON account_invitations(household_id,expires_at);

CREATE INDEX account_invitation_expiry ON account_invitations(expires_at);

CREATE INDEX native_handoff_expiry ON native_handoffs(expires_at);

CREATE INDEX devices_last_seen ON sync_devices(last_seen);

CREATE INDEX cursors_snapshot ON sync_cursors(snapshot_id);

CREATE INDEX cursors_device ON sync_cursors(account_id,device_id);

CREATE INDEX batches_expiry ON sync_batches(created_at,revision,account_id);

CREATE INDEX ancestors_descendants ON resource_ancestors(ancestor_id,resource_id);

CREATE UNIQUE INDEX defaults_account ON default_templates(account_id,resource_kind);

CREATE UNIQUE INDEX defaults_household ON default_templates(household_id,resource_kind);

CREATE INDEX occurrences_task ON task_occurrences(task_id,id);

CREATE INDEX occurrences_window ON task_occurrences(opens_at,closes_at,id);

CREATE INDEX progress_stream ON progress_entries(progress_id,sequence);

CREATE INDEX lists_task ON list_items(task_id,list_id);

CREATE INDEX enrolment_history_date ON task_enrolment_history(task_id,effective_date,account_id);

CREATE INDEX resources_owner ON resources(owner_id,id);

CREATE INDEX resources_parent ON resources(parent_id,id);

CREATE INDEX calendar_event_source ON calendar_events(source_id,id);

CREATE INDEX anchor_reference ON anchored_occurrences(reference_id,task_id);

CREATE INDEX subscriptions_account ON notification_subscriptions(account_id,id);

CREATE INDEX deliveries_due ON reminder_deliveries(state,next_attempt,lease_until);

CREATE INDEX dependency_reverse ON occurrence_dependencies(prerequisite_id, occurrence_id);

CREATE UNIQUE INDEX timer_active_account ON timer_sessions(account_id) WHERE stopped_at IS NULL AND cancelled=0;

CREATE INDEX timer_progress ON timer_sessions(progress_id,id);

CREATE INDEX timer_account_times ON timer_sessions(account_id,started_at,stopped_at);

CREATE INDEX completion_pending_due ON completion_pending(next_check,occurrence_id);

CREATE INDEX people_requests_recipient ON people_requests(recipient_id,state,id);

CREATE INDEX people_requests_expiry ON people_requests(expires_at,id);

CREATE TRIGGER resource_parent_insert BEFORE INSERT ON resources WHEN NEW.parent_id IS NOT NULL
BEGIN
 SELECT CASE WHEN NOT EXISTS(SELECT 1 FROM resources p WHERE p.id=NEW.parent_id AND
   ((NEW.kind='field' AND p.kind IN ('person','task')) OR
    (NEW.kind='execution' AND p.kind='task') OR
    (NEW.kind='occurrence' AND p.kind='execution') OR
    (NEW.kind IN ('progress','review','reminder') AND p.kind='occurrence') OR (NEW.kind='event' AND p.kind='calendar_source')))
 THEN RAISE(ABORT,'invalid resource parent') END;
END;

CREATE TRIGGER resource_ancestors_insert AFTER INSERT ON resources WHEN NEW.parent_id IS NOT NULL BEGIN INSERT INTO resource_ancestors SELECT NEW.id,NEW.parent_id,1 UNION ALL SELECT NEW.id,ancestor_id,depth+1 FROM resource_ancestors WHERE resource_id=NEW.parent_id; END;

CREATE TRIGGER resource_parent_update BEFORE UPDATE OF kind,parent_id ON resources
WHEN NEW.kind<>OLD.kind OR NEW.parent_id IS NOT OLD.parent_id
BEGIN
 SELECT CASE WHEN NOT (NEW.kind='field' AND OLD.kind='field' AND EXISTS(
   SELECT 1 FROM person_aliases a JOIN resources p ON p.id=a.canonical_id
   WHERE a.source_id=OLD.parent_id AND a.canonical_id=NEW.parent_id AND p.kind='person'))
 THEN RAISE(ABORT,'resource hierarchy is immutable') END;
END;

CREATE TRIGGER resource_ancestors_move AFTER UPDATE OF parent_id ON resources
WHEN NEW.parent_id IS NOT OLD.parent_id
BEGIN
 DELETE FROM resource_ancestors WHERE resource_id=NEW.id;
 INSERT INTO resource_ancestors SELECT NEW.id,NEW.parent_id,1 UNION ALL
 SELECT NEW.id,ancestor_id,depth+1 FROM resource_ancestors WHERE resource_id=NEW.parent_id;
END;
INSERT INTO sync_clock(id,revision,resource_count) VALUES(1,0,0);
INSERT INTO atlas_schema(version) VALUES(1000);
