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

## Bounded delivery

From API `0.30.0` a client may send `max_changes` (an integer from 4 to 800) on `GET /sync`
to bound the number of `Change` items in one response, counted across its batches with
ancestors included. Without it every response is exactly what the server returned before.
Send it only when `GET /health` reports `0.30.0` or later; an older server rejects the
unknown parameter with `400 malformed_request`. A value outside 4–800 is `422
invalid_value`; a value that is not a non-negative integer, or a repeated parameter, is
`400 malformed_request`.

A snapshot page then holds at most `min(limit, max_changes)` resources; snapshot cursors,
their one-hour expiry and `access_changed` are unchanged. A delta page holds whole batches
in revision order while they fit, and at most `limit` of them. A batch larger than the bound
on its own is split into **segments**: each response carries one piece, alone in its page,
with the batch's `revision` and `segment: {index, last}`. Indices start at 0 and increase by
one; exactly one piece is `last`. A batch that fits is never split.

A revision's notices are ordered once: its upserts (resources visible now, parents first)
then its removals (resources no longer visible that were sent to this device, children
first), as for a whole batch. Each piece is the next consecutive slice of that sequence,
re-authorised at its own request. Every piece carries the visible ancestors of its upserts
before them, even if an earlier piece carried them, and they count towards the bound. A
resource hidden before its slice is omitted; if it was sent earlier, this revision's removal
pass or a later batch removes it. Nothing is disclosed about a resource the reader cannot
see and was never sent: the continuation carries a position in the revision's notice list,
masked so that it reveals neither a resource identifier nor how many notices were skipped.

After a non-final piece, `next_cursor` is a continuation and `has_more` is `true`. After the
last piece it is an ordinary delta cursor at that revision, and `has_more` reports later
batches. A continuation is bound to the account, device and revision and follows delta
retention; a tampered, foreign, expired, retired-device or pruned continuation is `410
resync_required`, like a delta cursor. Access-epoch changes do not invalidate it: access
changes arrive incrementally, as for deltas. Re-sending any cursor at an unchanged state
returns the same piece. A continuation may be sent with a different bound; without one it
returns the rest of the revision in one response, still marked as the last segment.

Clients stage pieces and apply a revision only when its `last` piece is stored, so the live
cache never shows part of a revision. Persist each piece with its response's `next_cursor`
in one account-owned transaction; discard a duplicate index; on a gap, `resync_required` or
`access_changed`, discard the staged pieces. Drafts and the outbox are never touched. If a
piece cannot be processed, retry the same cursor with a smaller bound, down to 4: one changed
resource and its at most three ancestors. The bound is structural. The server makes no
memory claim, applies no default bound and has no byte budget; the client chooses its
starting value. Each piece reflects access at its own request time, so a reassembled revision
may combine content read at slightly different moments; later changes arrive as later
batches. Publication, receipts, the delivery ledger, retention, the recovery floor and device
registration are unchanged.
