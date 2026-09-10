# Offline writes and synchronisation

Clients create UUID resource and operation IDs locally. Ordinary content edits and
progress can be queued offline. Account management, invitations, consent and permission
changes require a connection. Keep pending drafts separate from the server cache.
An offline create using implicit defaults carries the previously fetched defaults
revision; a changed revision requires review instead of silently sharing under new defaults.

A sync request without a cursor builds an immutable authorised snapshot in SQL.
Snapshot items are separate rows, paged with opaque stored cursors that expire after
one hour. Finishing the snapshot yields a signed device-bound delta cursor. Delta
retention follows `ATLAS_SYNC_RETENTION_DAYS` (90 by default), not the snapshot timeout.

Changes publish atomically with domain writes and receipts. Batches carry notices;
response projections are re-authorised. Parents precede children on insertion and
children precede parents on removal. The per-device delivered-ID ledger ensures a
removal identifies only a resource delivered to that device. Snapshot and delta retries
must preserve operation and resource identity.

Content and policy versions are independent. Ordinary access changes deliver removals;
large changes affecting more than 200 resources advance the affected account's recovery
floor. `resync_required` means replace the authorised cache from a new snapshot.
`access_changed` invalidates an in-progress snapshot. Neither response deletes pending
local drafts or means that the underlying task has ceased to exist.

Device retirement invalidates sessions, device cursors and delivery metadata while
preserving account-wide operation receipts. Expired metadata is collected in bounded
chunks. Request-ID reservations, aliases, task history and completed work have different
lifecycles and must not be deleted merely to reduce sync storage.

Revocation cannot remove information from a device that remains offline. On reconnect,
apply explicit removals or replace the authorised cache. A missing item in a daily view
or a filtered page is not a deletion signal. Sync process-crash, rollback, contention,
retention and privacy obligations are covered by [domain tests](testing.md).
