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
