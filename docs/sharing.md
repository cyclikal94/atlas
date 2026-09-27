# Sharing and households

Policies are shared infrastructure for people, tasks and future domains. Accounts can
belong to multiple households; the primary household supplies creation defaults.

## Household membership

`GET /api/experimental/v1/households` returns your households, your role, their members
and a version. `POST /management-commands` accepts up to twenty ordered online commands
with one account-wide `Idempotency-Key`. It commits membership, policy effects, sync
publication and the operation receipt atomically.

<!-- experimental-schema: ManagementCommands -->
```json
{
  "commands": [
    {
      "kind": "create_household",
      "id": "70000000-0000-4000-8000-000000000001",
      "name": "Home"
    }
  ]
}
```

The creator becomes a manager. The first household becomes the account's primary
household; joining additional households leaves that choice unchanged. Managers can
rename the household, invite existing accounts, revoke invitations, change roles and
remove members. Members can leave themselves. A last remaining manager cannot leave
or be demoted; promote another member first. Accounts can hold up to 32 memberships.

Invitations last seven days. `GET /invitations` returns the recipient's pending,
unexpired invitations. Only that recipient may accept or decline. A pending invitation
confers no household access. Managers can revoke it. Removing a member revokes that
member's unused invitations to the household, preventing reuse to rejoin. Invitations
currently target existing Atlas accounts; they are not account-registration links.

`GET /directory` lists account IDs and usernames. `ATLAS_DIRECTORY_ENABLED=false`
requires an exact `?username=...` lookup instead. Neither mode exposes private people
fields or email addresses. Looking someone up does not grant them resource access.

## Defaults and explicit choices

`GET /defaults` returns the resolved person/field/task/list/progress templates, primary household,
preferences version and an opaque defaults revision. Resolution is application →
primary household → personal. A per-record `initial_policy` overrides those defaults.
Templates are copied into a resource policy when it is created. Editing a template
never retroactively changes existing records.

Application defaults share new people with the primary household with edit permission;
without a primary household they are private. New free-text fields start private.
Household or personal templates may change either behaviour. Sharing a field still
requires every current recipient to have the person's identity, including its name.

`GET /defaults/templates` returns your stored templates and their versions; adding
`?household_id=...` reads that household's templates, if you are a member. A missing
template has version zero and inherits the next layer. `set_defaults` uses that
stored template version. A null template removes the override. Only managers can
change household defaults; everyone can change their personal defaults.

<!-- experimental-schema: ManagementCommands -->
```json
{
  "commands": [
    {
      "kind": "set_defaults",
      "household_id": null,
      "resource_kind": "field",
      "expected_version": 0,
      "template": { "kind": "primary_household", "edit": true }
    }
  ]
}
```

Queued creation using defaults must include the revision previously returned by
`GET /defaults`. If relevant templates, primary household or membership changed,
`defaults_changed` preserves the draft for review. Online creation may omit the guard
and accept current defaults. Retrying a committed operation returns its original
receipt even if defaults subsequently changed.

A private creation override takes effect within the creation transaction; it does not
briefly publish the record before making it private. Null or omitted `initial_policy`
uses defaults; an explicit policy with no grants is private:

<!-- experimental-schema: ContentCommands -->
```json
{
  "commands": [
    {
      "kind": "create_person",
      "id": "70000000-0000-4000-8000-000000000002",
      "name": "Morgan",
      "initial_policy": { "grants": [], "exclude_accounts": [] }
    }
  ]
}
```

Explicit creation with shared grants goes through `/access-commands`, the online
sharing route. Ordinary content edits and private creation overrides use `/commands`.
Both routes share the same account-wide operation-ID namespace. Primary-household
changes use `set_primary_household` with `preferences_version`; null clears the choice.

## Content writes and the sharing precondition

Every resource has a sharing revision, `policy_version`, which advances whenever its
owner changes who can see it. Every actor who can read a resource receives that number
in its projection, on sync pages, resource lists and person detail, not only the owner.
It is a counter and discloses no grantee or exclusion; `GET /policies/{id}` stays
owner-only.

A write that replaces existing authored text sends the revision its author last saw as
`expected_policy_version`, beside the content `expected_version`. That covers `edit`,
and `put_field` (updating an existing field), `revise_task` and `edit_list` on
`/task-commands`. The server compares it in the same transaction as the write, under the
gate that also serialises sharing changes, so a sharing change commits either wholly
before the write is checked or wholly after it. If the revision differs, whether the
audience was widened, narrowed or had its edit rights changed, the whole command is
rejected with `conflict`. Nothing is written: no content, version, sync batch or
receipt. Omitting the value, or sending `null`, is rejected in the same way whatever the
policy currently is, and owners are gated as well as collaborators.

<!-- experimental-schema: ContentCommands -->
```json
{
  "commands": [
    {
      "kind": "edit",
      "id": "70000000-0000-4000-8000-000000000003",
      "expected_version": 3,
      "expected_policy_version": 2,
      "label": "Hobby",
      "value": "Surfing"
    }
  ]
}
```

On `conflict` the client refetches the resource, shows its author the current sharing
and content, and submits again with the new revision. The operation ID may be reused,
because a rejected write leaves no receipt. A committed operation retried with an
identical body returns its original receipt even after sharing has since changed,
since receipts are consulted first. A different body under a committed operation ID is
still `operation_conflict`. An offline edit carries the revision captured with its draft,
so one made before a sharing change is rejected on replay and enters conflict review.

Both counters belong to one resource, so an edit is checked against exactly the resource
it names and is never re-addressed. An `edit` naming a merged person's old ID is
rejected with `conflict`: the old identity's counters can coincide with the canonical
person's although its audience differs, so the draft could otherwise be written, and
shown, under sharing its author never saw. Read the canonical person
(`GET /people/{id}` resolves the old ID), review the draft against it and submit with its
counters. A committed edit still replays under its original ID, as above. Field
creation under an old parent ID still resolves to the canonical person, because it
carries no counters.

Visibility and authority are decided first. An account that cannot see the resource
receives `not_found` and one that can see but not edit it receives `forbidden`, whatever
revision it sends, so the check reveals nothing about resources it cannot read.
Creating a resource sends no revision: `put_field` with a null `expected_version` must
omit it and is otherwise refused with `invalid_value`, since creation under defaults is
already guarded by the defaults revision.

The two versions stay independent. A content edit does not advance `policy_version`, so
a sharing change prepared before it still applies, and a sharing command carrying a
stale policy revision conflicts without touching content. A sharing change reaches
every reader who keeps access as an ordinary upsert, which is how their client learns
the new revision.

The precondition compares the resource's own policy, and does not cover every way an
audience can change. Household membership changes alter who can see every resource
shared with that household, and a change to a person's or task's policy limits or
extends the audience of the fields under it, but neither advances the field's own
counter. A client offering a sharing summary alongside an edit should refresh that
context as well. Other commands that do not replace authored text (for example
occurrence corrections, archiving, list membership and merges bound to a preview token)
keep their own version checks and are not gated by this revision.

## Resource policies and recovery

Owners can read `GET /policies/{id}` and use `replace_policy` to atomically replace
account grants, household grants and account exclusions. Each grant selects read or
edit. Exclusions apply to all audience grants; resource owners retain their own
management access. Read/edit permission does not imply policy-management permission.
The atomic batch direct `grant`/`revoke` commands remain available and affect only direct grants.
A direct grant does not override an exclusion or remove a household grant.

Household membership is evaluated live. Joining reveals existing household-shared
resources. Leaving removes that source of access while preserving independent direct
grants. A lost person identity also hides its fields, even when a field grant remains.
Sharing never reveals private sibling fields or their labels. A creator's ownership
persists if they leave a household; their existing household grants also persist until
explicitly changed.

Ordinary changes use incremental sync, including durable removals for data previously
delivered to that device. Access changes affecting over 200 resources instead advance
only the affected account's recovery floor. Its old cursor returns `resync_required`;
a new snapshot recovers the data in bounded pages. Unaffected owners do not have to
replace their cache. Pending drafts remain separate from the server cache.


Linked-account identity and merge rules are described in [people](people.md).
