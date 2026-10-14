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
| `respond_request` | The recipient accepts or declines a pending request. Accepting a merge request whose preview `requires_approval` must also echo `recipient_preview_token` from a fresh `GET /people/requests/{id}/merge-preview` read; a stale or missing token is a `409 conflict`. Ignored for `Link` requests. A supplied token is part of the operation's replay fingerprint, so retry a lost response with the same operation ID *and the same token*; an operation without a token replays exactly as it did before the token existed. |
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
identity and reports its account link. Audience, linkage and attribution summaries for
the People screens are separate reads; see [viewer-safe summaries](#viewer-safe-summaries).

`GET /people/requests/sent` lists this account's own sent requests, paginated and
across every state (`pending`/`accepted`/`declined`/`cancelled`/`expired`), not just
pending inbound ones. It is backed by a durable record mirrored alongside
`people_requests` that has no automatic age limit, so a sent request still appears
with its correct terminal state — including a computed `expired` — after the
operational row is later purged by the same seven-day cleanup described above. Each
row names its recipient (`recipient_id` and `recipient_username`), so a fresh device can
label it without any local record of what was sent. This is an ordinary authenticated
online read, not part of the sync/offline projection model.

## Viewer-safe summaries

Since API `0.29.0` (schema `1006`) three authenticated online reads describe people for
the People list, its filters and columns, the field table's "Who can see it" and the
detail "Shared with" rail, without the owner-only `GET /policies/{id}`:

| Read | Returns |
| --- | --- |
| `GET /people/summaries?scope=&archived=&after=&limit=` | `PersonSummaryPage`: one page (1–100 rows, default 50) of the caller's visible people in ascending id order and `next_after` |
| `GET /people/{id}/summary` | `PersonSummaryDetail`: one visible person's `PersonSummary` and its `SharedWith` rail |
| `GET /people/{id}/field-audiences?archived=&after=&limit=` | `FieldAudiencePage`: the person's fields the caller can see, each with its own sharing (`audience`) and who can actually see it (`effective`) |

Every response also carries the caller's primary household as `home` (or `null`) and
the caller's `preferences_version`, both from the response's own snapshot (see
freshness below).

A merged person's old id resolves to the canonical person, as on `GET /people/{id}`. A
person the caller cannot see and an id that names nothing are the same `404 not_found`.
None of the three has an owner-only branch, so none returns `403` because of a person's
sharing; a `403` can only come from credential transport, as on every protected read.

**Who added a person.** Each person created since schema `1006` records the account
that created it (`person_attributions`), separately from its current owner. Accepting a
link, a merge into a linked identity and `reference_account` all leave a person owned by
an account that did not create it, and unlinking keeps the transferred owner, so ownership
cannot answer the question. `create_person` records its caller; `reference_account`
records its caller when it creates the referenced account's identity and nothing when the
identity already exists; link, merge, unlink, rename and archive never change the record.
`added_by_you` is evaluated over the person **and every person merged into it**: linking a
person to an account that already has a profile merges the linked person into that
profile, and neither that nor any other merge erases the relationship. It is `yes` when the
caller added any of them, `no` (never naming who) when every one of them is known to have
been added by another account, and otherwise `unknown`. `shared_with_me` uses the same
rule, and list, detail and filters agree. Each person keeps exactly its own record (an
archived merged person, listed with `archived=true`, reports its own); nothing is copied
onto the canonical person or overwritten. The only extra thing a reader can infer is that
some merged person predates attribution (`unknown` rather than `no`), never who, which or
how many. No earlier record proves
who created an existing person (ownership may have moved, unlink deletes the link, and
receipts hold digests only), so the `1005 → 1006` upgrade records nobody and every older
person is `unknown`: make no attribution claim for it. Attribution grants no access and
changes no permission check.

**Disclosure.** The owner of a person or field receives the complete audience: every
direct grant, every household grant, the exclusion count and, in the rail, the member list
of each household the owner belongs to with excluded members marked, and the excluded
accounts. Every other reader receives `disclosure: "partial"`, always, whether or not the
owner has shared it more widely: the owner, the reader's own direct grant, and the
households the reader belongs to (name, version and size as in `GET /households`), never a
member list, another account's grant, an exclusion, a household the reader is not in, or a
count of anything hidden. A client says the owner may have shared it more widely. The owner
(the sharer) is disclosed to every reader, with the owner's own linked person when the
reader can see it. `member_count` is a household's size, not how many members can see the
person. `your_access` is `owner`, `edit` or `view` exactly as `can_edit`; a person linked
to another account is `view` for everyone but that account, even with an editable grant.

**Fields: sharing versus who can see it.** A field is visible only to someone who can also
see its person, so a field's own sharing is an upper bound on its audience, not the
audience. `FieldAudience.audience` describes the field's own sharing under the rules above:
for a field, `disclosure: "complete"` means only that the caller owns that configuration.
`FieldAudience.effective` says who can actually see it. Exact counts need every input to
that answer, so it is `complete` only when the caller owns both the field and its person
(and so may read both policies) **and** is a current member of every household the field
or its person is shared with (and so may read their membership, as in `GET /households`):
`account_count` is the other accounts that can see the field now and `limited_count` the
accounts its own sharing reaches (a direct grant or a member of a granted household, not
excluded and not the owner) that cannot, because they cannot see the person. Otherwise it is
`partial` with both counts `null`, always. That includes the owner of a field on someone
else's person (that person's audience belongs to its owner) and an owner of both who has
left a household that a retained grant on either still names: owning a policy does not
authorise reading the membership of a household the owner is not in, and a count derived
from it would reveal who joined or left. Nothing the caller cannot read changes a `partial`
response. The rule is conservative: it does not depend on whether the unreadable household
currently affects the answer, so a field shared only directly can be `partial` because its
person still names a household its owner has left. The owner still sees the field's own
configuration (`audience`), with that household's `name`, `version` and `member_count`
`null`, and can change it. Unknown, failed or offline is not `partial` and never "Only you".

**Filters.** `scope` is evaluated in the database over the caller's whole visible set,
then paginated, so following `next_after` to the end is complete at each read:

| `scope` | Includes |
| --- | --- |
| `all` (default) | Every visible person |
| `in_home` | People shared with the caller's primary household, while the caller belongs to it. With no primary household it is empty and `home` is `null` |
| `shared_with_me` | People owned by another account and not known to have been added by the caller; `unknown` attribution is included |
| `not_linked` | People without an account link |

`archived=true` lists only archived people, including merged aliases. `field_count` counts
the non-archived fields the caller can see. A row whose visibility or filter membership
changes while a client pages may be missed or appear newly; re-read after changes.

**Consistency and freshness.** Each call reads one database snapshot (PostgreSQL
`REPEATABLE READ`; SQLite's first read fixes the WAL snapshot), so a row's audience,
versions, field count, `home` and `preferences_version` belong together. Revocation
applies to the next call: the person leaves every scope and the `{id}` reads return `404`.
A summary is identity-bound display data, never an access grant, and is online-only: it is
not part of `/sync` and is `Cache-Control: private, no-store`. It is as fresh as its
snapshot and no fresher. Its inputs, and how a client learns that one changed:

| Input | Change signal |
| --- | --- |
| The person's label, archive, linkage, owner, attribution (merges) and sharing | The synced person projection's `version`/`policy_version` changes, or it is removed |
| `field_count` and field audiences | Any synced field projection of that person is added, changed or removed |
| `owner.person` | The synced projection of the owner's linked person changes, appears or is removed |
| Household names, versions, sizes and the caller's memberships | `GET /households` versions or membership |
| The primary household (`home`, every `in_home`) | `preferences_version` (`GET /defaults` and every summary response). A change, from any device, publishes nothing to `/sync` |

A client re-reads the summaries it shows on any change to **any** synced person or field
projection (not only the listed rows), after any command receipt, after reconnecting or
re-authenticating, when `GET /households` or `preferences_version` changes, and, while a
summary is shown online, at least every 60 seconds and whenever the view becomes visible
again, because household and primary household changes made by another device or member
publish nothing to this device. A summary shown online may therefore be up to that
interval behind such a change; never present it as live.

Responses can arrive out of order. For each account, access epoch and query, apply only
the response to the latest request; an invalidation supersedes every earlier request, and
a traversal that is invalidated restarts from its first page instead of mixing pages. Never
apply a response whose `preferences_version` is lower than one already applied in the same
epoch, nor replace a row with one whose `version` or `policy_version` is lower. Drop every
late response for another identity or epoch, keep pages in memory only, and show a summary
as unavailable offline rather than showing an old one.

A household member's view of a person shared with their primary household:

<!-- experimental-schema: PersonSummary -->
```json
{"id": "6f1c2a4e-8b2d-4c55-9a77-3f0e1d2c4b5a", "label": "Mo", "version": 1, "policy_version": 1, "archived": false, "linkage": "none", "linked_account_id": null, "owner": {"account_id": "0d6c1b9e-2f3a-4e5d-8c7b-1a2b3c4d5e6f", "username": "alice", "you": false, "person": null}, "added_by_you": "no", "your_access": "view", "field_count": 1, "audience": {"disclosure": "partial", "in_home": true, "households": [{"id": "9a8b7c6d-5e4f-4a3b-8c2d-1e0f9a8b7c6d", "can_edit": false, "you_are_member": true, "name": "Atlas Home", "version": 3, "member_count": 2}], "your_direct_grant": null, "direct_grant_count": null, "excluded_count": null}}
```

The same caller's rail, and In Home for a caller with no primary household:

<!-- experimental-schema: SharedWith -->
```json
{"disclosure": "partial", "owner": {"account_id": "0d6c1b9e-2f3a-4e5d-8c7b-1a2b3c4d5e6f", "username": "alice", "you": false, "person": null}, "accounts": [], "households": [{"id": "9a8b7c6d-5e4f-4a3b-8c2d-1e0f9a8b7c6d", "can_edit": false, "you_are_member": true, "name": "Atlas Home", "version": 3, "member_count": 2, "members": null}], "excluded_accounts": null}
```

<!-- experimental-schema: PersonSummaryPage -->
```json
{"items": [], "next_after": null, "home": null, "preferences_version": 2}
```

The owner of a field and its person, where one Home member cannot see the person:

<!-- experimental-schema: FieldAudience -->
```json
{"id": "3c2b1a09-8f7e-4d6c-9b5a-4f3e2d1c0b9a", "version": 1, "policy_version": 1, "archived": false, "owner_is_you": true, "your_access": "owner", "audience": {"disclosure": "complete", "in_home": true, "households": [{"id": "9a8b7c6d-5e4f-4a3b-8c2d-1e0f9a8b7c6d", "can_edit": false, "you_are_member": true, "name": "Atlas Home", "version": 3, "member_count": 3}], "your_direct_grant": null, "direct_grant_count": 0, "excluded_count": 0}, "effective": {"disclosure": "complete", "account_count": 1, "limited_count": 1}}
```

The same owner after leaving Atlas Home, which the field's retained grant still names: its
own configuration is complete, the household is redacted and effective access is `partial`:

<!-- experimental-schema: FieldAudience -->
```json
{"id": "3c2b1a09-8f7e-4d6c-9b5a-4f3e2d1c0b9a", "version": 1, "policy_version": 1, "archived": false, "owner_is_you": true, "your_access": "owner", "audience": {"disclosure": "complete", "in_home": false, "households": [{"id": "9a8b7c6d-5e4f-4a3b-8c2d-1e0f9a8b7c6d", "can_edit": false, "you_are_member": false, "name": null, "version": null, "member_count": null}], "your_direct_grant": null, "direct_grant_count": 0, "excluded_count": 0}, "effective": {"disclosure": "partial", "account_count": null, "limited_count": null}}
```

A field shared directly with one account, on a person still shared with a household its
owner has left (only the person depends on that household):

<!-- experimental-schema: FieldAudience -->
```json
{"id": "5e4d3c2b-1a09-4f8e-9d7c-6b5a4f3e2d1c", "version": 1, "policy_version": 1, "archived": false, "owner_is_you": true, "your_access": "owner", "audience": {"disclosure": "complete", "in_home": false, "households": [], "your_direct_grant": null, "direct_grant_count": 1, "excluded_count": 0}, "effective": {"disclosure": "partial", "account_count": null, "limited_count": null}}
```

## Merge privacy and offline identities

`POST /people/merge-preview` takes `source_id` and `target_id`. The caller must own at
least one and see both. It returns the two visible identities, visible field IDs,
a token, and whether another owner's approval is needed. It explicitly reports that
identity audiences combine and field policies freeze. Hidden field changes do not
alter the public token; their current audiences are preserved at commit time.

`GET /people/requests/{id}/merge-preview` is the equivalent read for the *recipient*
of a pending merge request they did not initiate: it does not require the caller to
independently pass `POST /people/merge-preview`'s "own at least one and see both"
check for the identity they do not own. An identity or field the recipient cannot
currently see is simply omitted from the response, never a permission error that
would itself disclose its existence. `stale: true` means the sender's own view of
the merge has changed since the request was made — recomputed and compared against
the `preview_token` captured at request-creation time — but the read still succeeds
with the recipient's current-state preview. The request no longer existing, having
been withdrawn, or already handled is `404 not_found`; having expired but not yet
swept is `410 invitation_expired`, distinguishing the two. Accepting the request
must echo this read's `token` back as `recipient_preview_token` (see the
`respond_request` table above); this is the acceptance-time check ensuring the
recipient did not act on a preview computed under now-stale visibility.

A merge keeps field IDs, values, ownership and birthday/task references. Source fields
move under the target, with a version change for the parent update. The source becomes
an archived alias and alias chains flatten. Offline field creation using an old parent
ID resolves to the canonical person; existing operation receipts remain valid. An `edit`
of the person under its old ID is refused with `conflict`, because the version and sharing
revision it carries were read from the old identity; read the canonical person and
resubmit. Merging two account-linked identities is rejected. A linked identity retains
its account's name.

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
