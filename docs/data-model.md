# Data model

The authoritative DDL is in `crates/core/src/storage/schema/{sqlite,postgres}.sql`.
The schemas encode the same domain rules with engine-specific constraints/triggers.
This document explains ownership and relationships rather than duplicating every column.

| Tables | Responsibility |
| --- | --- |
| `accounts`, `sessions`, `external_identities`, `oidc_flows`, `native_handoffs`, `account_invitations` | Individual accounts, hashed session credentials, explicit provider links and one-use onboarding/authentication state |
| `resources` | UUID, owner, parent, kind, label, JSON data, independent content/policy versions and archive state |
| `resource_grants`, `resource_household_grants`, `resource_exclusions`, `resource_ancestors` | Direct/live-household visibility, exclusions and required ancestor visibility |
| `households`, `household_memberships`, `household_invitations`, `default_templates` | Membership, roles, invitations and creation templates |
| `person_accounts`, `person_aliases`, `frozen_owner_visibility` | Account-linked identity, authorised canonical resolution and merge-time visibility preservation |
| `people_requests`, `people_results`, `people_request_ids` | Consent proposals, replay results and permanent request UUID reservation |
| `tasks`, `task_definitions`, `task_enrolments`, `task_enrolment_history` | Task identity, execution definitions and versioned participation |
| `task_occurrences`, `occurrence_participants`, `progress_entries` | Stable scheduled instances, participant snapshots and correction-aware evidence |
| `list_items` | Authorised references to tasks; list visibility does not grant task visibility |
| `occurrence_dependencies`, `dependency_rules`, `timer_sessions` | Dependency decisions and durable timer state |
| `task_rotas`, `rota_consents`, `completion_pending`, `completion_successors` | Consented assignments and deduplicated completion-relative scheduling |
| `calendar_sources`, `calendar_events`, `task_anchors`, `anchored_occurrences`, `calendar_reviews` | Imported evidence, stable source identity, task binding and reviewable reconciliation |
| `reminder_rules`, `notification_subscriptions`, `reminder_deliveries` | Reminder ownership, encrypted destinations and claimed delivery attempts |
| `receipts`, `sync_clock`, `sync_batches` | Account-scoped operation digests, serial publication revision and content-free change notices |
| `sync_devices`, `sync_deliveries`, `sync_snapshots`, `snapshot_items`, `sync_cursors` | Device recovery state, delivered-ID ledger, immutable snapshot pages and opaque snapshot cursors |

Field data uses `resources::FieldValue`: text, exact quantity, date, URL, Boolean,
choices or checklist. There is no separate raw-field table or typed-field marker.
API `Projection.value` is native JSON; identity-only records use null. Storage JSON
is parsed at the projection boundary; malformed stored data fails explicitly.

Resource hierarchy triggers validate parents and maintain ancestor rows. A field can
move during an authorised person merge through the reserved alias relationship;
application code does not independently rebuild the same ancestor rows. Merges freeze
existing contribution policies before combining identity audiences, so ownership alone
does not expose previously private contributions to the new identity owner.

Receipts retain operation identity and a digest, not submitted private content. Request
payloads can expire while UUID reservations and replay results remain. Device retirement
cascades its delivered-ID ledger; task/history records are not retention metadata.
