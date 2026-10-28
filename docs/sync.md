# Offline writes and synchronisation

Clients create UUID resource and operation IDs locally. Ordinary content edits and
progress can be queued offline. Account management, invitations, consent and permission
changes require a connection. Keep pending drafts separate from the server cache.
An offline create using implicit defaults carries the previously fetched defaults
revision; a changed revision requires review instead of silently sharing under new defaults.
`GET /defaults/snapshot` returns those defaults with the household membership their revision
covers from one database snapshot; capture that response, not separate reads, with a draft.

A sync request without a cursor builds an immutable authorised snapshot in SQL.
Snapshot items are separate rows, paged with opaque stored cursors that expire after
one hour. Finishing the snapshot yields a signed device-bound delta cursor. Delta
retention follows `ATLAS_SYNC_RETENTION_DAYS` (90 by default), not the snapshot timeout.

Changes publish atomically with domain writes and receipts. Batches carry notices;
response projections are re-authorised. Parents precede children on insertion and
children precede parents on removal. The per-device delivered-ID ledger ensures a
removal identifies only a resource delivered to that device. Snapshot and delta retries
must preserve operation and resource identity.

Content and policy versions are independent, and every reader receives both. A policy
change is delivered to each reader who keeps access as an upsert carrying the new
`policy_version`; a queued edit must send the revision captured with its draft, and is
rejected on replay if sharing has changed since (see [sharing](sharing.md)). Snapshot
pages created before contract 0.13.0 may still lack the revision for at most one hour;
refetch such a resource before editing it. Ordinary access changes deliver removals;
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

## Page size and limit

`limit` counts resources on a snapshot page and publication batches on a delta page. It
accepts 1 to 200 and defaults to 50; any other value is `422 invalid_value`, returned before
any snapshot, cursor or device registration is written. A snapshot page holds exactly one
batch of at most `limit` resources. A delta page holds at most `limit` whole publication
batches in revision order: `limit` never splits a batch. Follow `next_cursor` while
`has_more` is true to reach every pending batch.

The contract states the same bounds: the `limit` maximum and `SyncPage.batches.maxItems`
are both 200, and `Batch.changes.maxItems` is 800 (200 changed resources plus at most three
ancestors each). Before contract 0.30.0 the page bound read 80, so a correct response to a
`limit` of 81 to 200 with more than 80 pending batches failed a validator built from the
contract; the server's responses did not change. Raising the bound was chosen over lowering
the ceiling because lowering it would reject or truncate requests the server already
answers, and a batch count does not bound memory. By contract arithmetic a delta page at
the ceiling may carry 200 batches of up to 800 changes, each value at most 8 KiB. The
ceiling is a request bound, not a memory-safe size: a client that needs smaller responses
requests a smaller `limit`, and choosing it for a device is the client's decision.
