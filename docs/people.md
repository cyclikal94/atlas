# People

A person may be unlinked or explicitly associated with an Atlas account. No automatic
name/email matching links accounts. A visible person identity includes its display
name; individual contributions have independent policies. Sharing a contribution
requires authorised identity access too. Personal nicknames can be private fields.

## People, identity and contributions

People keep one visible basic name with independently controlled custom contributions.
Personal nicknames, notes, dates and wishlists use the existing field model. A field
requires visibility of its person's identity; it cannot expose an unnamed orphan note.

`GET /people/{id}/duplicates` suggests case/whitespace-normalised name matches among
visible identities. These are hints, never proof of identity. Pagination advances over
a bounded visible scan, so an empty result can still have `next_after`. Hidden names,
email addresses and fields are not searched. Account linking is always explicit.

`POST /people-commands` accepts the following online operations:

| Command | Behaviour |
| --- | --- |
| `reference_account` | Choose an existing account and a proposed person UUID. Return its canonical identity, creating it if needed. The referenced account owns the basic identity; the caller receives name access. Explicit identity exclusions are honoured. Existing contributions do not become visible merely by referencing the account. |
| `request_link` | The owner of an unlinked person asks an account holder to accept linking, recording the current content and policy versions. |
| `respond_request` | The recipient accepts or declines a pending request. |
| `cancel_request` | The sender cancels a pending request. |
| `merge` | Merge two identities owned by the caller using a current preview token. |
| `request_merge` | Request the other owner's approval where identities have different owners. Both must authorise the merge. |
| `rename_profile` | The linked account changes its canonical basic name using its current version. |
| `unlink` | Remove the account association without deleting contributions. |

Link acceptance transfers only the basic identity to the account holder. Existing
contribution ownership and audiences remain separate. If that account already has a
canonical person, acceptance merges into that identity instead of creating a second
profile. The account holder controls its name. New fields authored by someone else
exclude the subject by default; explicit field policies can override this, including
shared wishlist/about-me contributions. No account match is inferred silently.

`GET /people/requests` lists pending incoming requests. They expire after seven days;
at most 100 can be pending for one recipient. Expired proposal rows are collected in
chunks of 500, including terminal proposals once their original expiry passes. Minimal
request-ID tombstones and account-scoped operation results remain durable for replay;
cleanup must not recreate a handled action or reuse an old request identity. The response exposes the proposed basic
identity information, not internal preview tokens or private contribution state.
`GET /people/{id}` resolves an old merged ID to its current authorised canonical
identity and reports its account link.

## Merge privacy and offline identities

`POST /people/merge-preview` takes `source_id` and `target_id`. The caller must own at
least one and see both. It returns the two visible identities, visible field IDs,
a token, and whether another owner's approval is needed. It explicitly reports that
identity audiences combine and field policies freeze. Hidden field changes do not
alter the public token; their current audiences are preserved at commit time.

A merge keeps field IDs, values, ownership and birthday/task references. Source fields
move under the target, with a version change for the parent update. The source becomes
an archived alias and alias chains flatten. Offline field creation using an old parent
ID resolves to the canonical person; existing operation receipts remain valid. Merging
two account-linked identities is rejected. A linked identity retains its account's name.

Combining identity audiences could otherwise expose a contribution whose own grant was
previously blocked by its parent. To prevent this, merging and linking freeze each
existing field's **effective** read/edit audience into explicit account grants. Existing
household field grants consequently stop following future household membership until
someone deliberately replaces that field's policy. A previously blocked field owner
also stays blocked after the move; once the new parent is visible, that owner can
explicitly restore access through policy replacement. This behaviour favours preserving
current privacy over silently expanding it.

Merge/link operations are atomic and limited to a 200-resource scope. Sync publishes
only authorised projections/removals. Revocation still cannot erase a device's offline
cache until it reconnects. See [API and sync contract](api.md) and the [task API](tasks.md)
for the underlying history and permission rules.
