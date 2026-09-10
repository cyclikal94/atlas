# Tasks

Habits and chores are presets over one task model. The canonical contract is
[OpenAPI](../api/openapi.json); all routes use `/api/experimental/v1`.

## Writes and offline use

`POST /task-commands` accepts `{ "command": …, "defaults_revision": … }` and a UUID
`Idempotency-Key` header. One command, its evidence, receipt and authorised sync changes
commit atomically. A retry returns its original `{ "revision": … }` even if permissions
have since changed; reusing its operation ID with different content conflicts. Receipts
contain no replayable private content. Normal content operations can be queued offline.

`POST /task-access-commands` handles online enrolment, rota consent/configuration and creates containing explicit
non-empty sharing policies. Existing `/management-commands` manages policies/defaults.
For offline creates using defaults, include the last `/defaults` revision; a stale
revision returns `defaults_changed` without creating anything. Omitting the guard
accepts current server defaults. An explicit empty policy creates privately.

| Command | Behaviour |
| --- | --- |
| `create_task` | Client supplies task/execution UUIDs, title, definition and optional policy. Creates the first due occurrence and enrols the creator. |
| `revise_task` | Version-checked title/definition edit affects occurrences generated afterwards. Existing targets, windows and participants remain frozen. |
| `materialise` | Generates up to 16 intended periods through a supplied date, at most 31 days ahead of today in the saved zone. Repeated calls continue from durable position. |
| `record` | Adds checkbox, exact quantity, checklist or explicit observation evidence to an occurrence's participant stream. |
| `exclude` | Version-checked personal streak exclusion, permitted only by the occurrence's explicit opt-in policy. |
| `resolve` | Version-checked public workflow closure/reopening. Closing a chore does not fabricate completion or streak credit. |
| `revise_occurrence` | Explicit version-checked correction of one occurrence's goal/date/window. Retains its ID, intended key and participants; refreshes progress summaries. |
| `enrol` | Online self-enrolment, withdrawal, progress policy and aggregate consent. Version zero creates the first enrolment. |
| `create_list`, `edit_list`, `list_item` | Named lists and version-checked task references. Lists do not grant task access. |
| `put_field` | Create/update a typed contribution under a person or task. Each contribution has its own policy and edit version. |
| `set_dependencies` | Replaces explicit occurrence prerequisites using the occurrence version; concurrent cycles are rejected. |
| `complete_dependencies` | Applies a current preview atomically, with an explicit completion mode and optional numeric/checklist root evidence. |
| `start_timer`, `stop_timer`, `cancel_timer` | Records duration sessions, checking authority, dependencies and interval overlap; a rejected stop can be retried or cancelled. |
| `set_rota`, `rota_consent` | Online-only roster configuration and self-consent on `/task-access-commands`; assignments apply prospectively. |
| `archive` | Reversible archive of a task, list, person or field. History, receipts and sync content survive. |

Generic person creation and name edits remain on `/commands`; generic text editing
cannot overwrite typed fields or task/progress internals. Names are parent identities,
so sharing a contribution still requires sharing its person's name. A nickname can
be a private typed text contribution. Account-linked profiles, duplicate suggestions,
merge previews and consented linking are implemented; see the [workflow API](people.md).

## Definitions, windows and stable identity

The definition contains a saved IANA timezone, optional date/time, optional fixed
recurrence, goal, carry policy and participation mode. Supported frequencies are daily,
weekly, monthly and yearly, with interval 1–366. An undated one-off is an inbox task.
Date-only slots retain a date and no UTC instant. Timed slots resolve in the saved zone:
folds choose the first instant; gaps preserve position within the gap. Month-end and
leap-day dates without a valid calendar date are omitted.

An occurrence snapshots its definition, participant roster, intended slot and exclusive
window end. `open_days_before` and `close_days_after` are calendar-day offsets from the
slot's date, resolved at local midnight; the latter must be at least one. A timed due
instant does not itself close the action window. For a three-times-weekly target, use
weekly recurrence, numeric minimum `"3"` and a seven-day window.

Occurrence IDs are UUIDv5 with the task UUID as namespace and UTF-8 name
`atlas-occurrence-v1:` plus the slot key. A one-off key is `once`; recurring keys are
`YYYY-MM-DDTdate` or `YYYY-MM-DDTHH:MM:SS`. Progress IDs use the occurrence UUID as namespace
and `atlas-progress-v1:` plus the account UUID. These public identifiers let clients
queue creation and completion together; they are never credentials. Clients must
retain the server's existing IDs after explicit rescheduling.

Materialisation never moves backwards. Clock rollback, travel or polling again cannot
delete an occurrence or repeat its progress. Template edits affect only periods not yet
materialised; pre-generated future periods keep their snapshots until explicitly
corrected. Participation mode and switching between one-off and recurrence require a
new task, because silently rewriting existing obligations would be ambiguous.

`GET /tasks/{id}` returns identity, authorised execution details and materialisation
position. `pending` and the daily view's `pending_tasks` identify missing periods through
today; clients should catch up before treating history as complete. Continue bounded
materialisation until pending clears. For an explicitly requested future horizon,
compare the returned last date with the next slots from the definition, or continue
until the position stops advancing. Reads do not create work. Archiving stops generation
and excludes the task from the default daily view; explicit task history and streaks
remain readable. Unarchiving resumes catch-up.

## Missed work and corrections

| Carry policy | After an incomplete window |
| --- | --- |
| `close_incomplete` | Keeps the occurrence in history but ends actionability. Suitable for teeth or putting bins out for a specific collection. |
| `retain_one` | Keeps one outstanding chore actionable. Later periods are retained as covered periods without independent progress streams. |
| `accumulate` | Keeps each outstanding occurrence separately actionable. |

Covered periods never earn completion credit. A retained chore advances after completion
is visible to every execution reader, or after an editor explicitly resolves it. This
prevents private progress from changing public scheduling. An overdue chore can be
completed late without retroactively repairing a missed period's streak.

`happened_at` records when the work happened, separately from arrival. Offline work
recorded inside its original window can arrive later. Close-incomplete tasks reject
work that actually happened after closure; retained/accumulating work permits it.
Future evidence is limited to five minutes of clock skew. Correct a mistaken timestamp
by appending a replacement, not by deleting history.

Checkbox/checklist writes require the progress resource's `expected_version`, because
order changes meaning. Numeric and observation appends can omit it, allowing independent
offline writers. A correction's `replaces` must identify an active entry in the same
stream. It inherits the original entry's logical order: correcting an old checkbox
must not override a later checkbox. Correcting an already replaced entry conflicts.

Quantities are decimal strings with up to six fractional digits and exact integer
arithmetic internally. Upper/range goals require explicit observation; silence is
unknown, not a successful zero. Their result remains provisional until window close.
History uses the occurrence's frozen target. Explicit occurrence corrections validate
existing evidence against the new goal and fail atomically if incompatible.

## Participation and privacy

Task/execution access, participation, progress access and aggregate consent are separate.
Editing a shared task does not let someone alter another account's progress. Delegated
progress editing requires an explicit edit grant on that participant's stream.

- `personal`: the creator's progress.
- `anyone`: any occurrence editor can begin their own stream, including offline. Private
  participation does not amend the published roster. A visible, consented completion is
  sufficient; without one the shared result is unknown. Personal scope still exposes
  the caller's own incomplete/missed outcome. Exclusions affect personal scope and cannot
  prove a joint exclusion, because the participant set is open.
- `everyone`: each accepted participant must meet the goal independently.
- `pooled`: accepted participants contribute to one numeric goal.

Enrolment changes take effect by intended date in the saved task zone. Already generated
periods keep their roster and consent; catching up older periods uses enrolment history,
so joining today cannot impose yesterday's obligations. Accounts that can no longer see
execution details are not enrolled into newly generated periods. `GET /tasks/{id}/enrolment`
returns the caller's current policy/version. There are at most eight enrolled participants.

Every ancestor must remain visible. Required hidden or non-consented contributions make
joint results unavailable; no private totals or success counts are returned. A user can
always request their own `scope=personal` history/streak separately. Changing an enrolment
policy affects future streams; use existing resource-policy commands to change access to
an already created stream.

Creation defaults layer application → primary household → personal → explicit policy.
People default to household edit access; fields/tasks/lists default private. Progress
defaults to household read access, gated by the task/execution/occurrence. With no primary
household it is private. Read access is deliberate: a household should be able to see
shared workout progress without automatically recording it for someone else. All five
resource defaults are configurable.

## Reads, history and sync

`GET /tasks`, `/lists`, `/people`, `/fields` and `/progress` return authorised resources,
with optional parent/archive filtering and UUID pagination. A resource `value` is native JSON for typed fields, execution definitions,
occurrence state and progress summaries. All fields use the tagged FieldValue model. List values contain only authorised task IDs,
and change in sync when the caller's task access changes.

`GET /occurrences` supports task, list, day, display timezone, state and personal/joint
scope. `GET /daily` requires an explicit day and display timezone. State is `all`,
`actionable`, `complete`, `missed` or `unknown`. It returns scheduled occurrences plus
currently actionable overdue/undated work relevant to that day. `actionable` describes
what the caller can do now, not a historical reconstruction of the plan. Choosing another
day never means deleting items from offline storage.

Authority and business filters precede UUID ordering/pagination. Send the returned
`revision` alongside `after`; concurrent publication returns `conflict`, requiring a
restart of the view. Sync remains the authoritative offline cache protocol: parents
arrive before children, revocations remove children first, and cached access changes
only on explicit sync removals or a new snapshot.

Progress sync values contain exact action/period summaries, the active entry count, snapshotted
`aggregate_consent` and up to eight recent active entries. Offline clients must respect
that consent when combining other people's streams. `GET /progress/{id}/entries` pages the complete
append-only journal, including corrections, by sequence. Each page rechecks current
permissions. Apply `replaces` and order surviving entries by `logical_order` when rebuilding
state; the short recent list is not the whole history. Synced summaries are evaluated
before closure; clients apply the saved window end to provisional upper/range results.
The server's occurrence view returns the current finalised outcome.

`GET /tasks/{id}/streak?scope=personal|joint` reports current/longest intended-period
streaks. Missing materialisation returns `materialisation_required`; hidden required
history returns null statistics. Open unfinished periods do not erase an established
streak. Missing a closed period breaks it. Exclusion is disabled by default; opting in
allows an excluded period to bridge a streak without incrementing it. There is no extra
skip/miss synonym in the model.

## Bounds

Definitions are limited to 4 KiB, resource values to 8 KiB and active evidence to 10,000
entries per stream. Lists contain at most 100 task references; query pages contain at
most 200 items. The existing 10,000-resource installation ceiling still applies. Task
view evaluation is bounded by that ceiling. Prototype benchmarks are not task
throughput measurements. These limits return explicit errors rather than discarding
old content.

## Worked offline commands

Create a private daily task with client-generated IDs:

<!-- experimental-schema: TaskCommandInput -->
```json
{
  "command": {
    "kind": "create_task",
    "id": "70000000-0000-4000-8000-000000000011",
    "execution_id": "70000000-0000-4000-8000-000000000012",
    "title": "Brush your teeth",
    "definition": {
      "schedule": {
        "start_date": "2026-09-08",
        "time": null,
        "timezone": "Europe/Vienna",
        "repeat": { "frequency": "daily", "interval": 1 }
      },
      "goal": { "kind": "checkbox" },
      "carry": "close_incomplete",
      "participation": "personal",
      "open_days_before": 0,
      "close_days_after": 1,
      "allow_streak_exclusions": false
    },
    "initial_policy": { "grants": [], "exclude_accounts": [] }
  }
}
```

Its first intended date has occurrence ID `4cc26047-d75e-5cb3-ad46-fe81937bde57`.
After creation/materialisation, the participant can queue the following command with
a separate operation UUID. Replace the example account with the authenticated account;
`expected_version` is the progress resource's version, not the task's version.

<!-- experimental-schema: TaskCommandInput -->
```json
{
  "command": {
    "kind": "record",
    "occurrence_id": "4cc26047-d75e-5cb3-ad46-fe81937bde57",
    "subject_account_id": "70000000-0000-4000-8000-000000000001",
    "entry_id": "70000000-0000-4000-8000-000000000013",
    "evidence": { "kind": "checkbox", "complete": true },
    "expected_version": 1,
    "happened_at": 1788861600
  }
}
```

## Dependencies, timers and recurrence

`set_dependencies` on `/task-commands` replaces an occurrence's direct prerequisite
IDs, using that occurrence's current resource version. Dependencies refer to explicit
occurrences, so recurring tasks never guess which period is required. Transitive
cycles are rejected under concurrent edits. Limits are 32 direct prerequisites and
200 nodes in a preview.

`GET /occurrences/{id}/dependencies` returns an authorised preview and token. Hidden
progress does not alter the token. `complete_dependencies` accepts that token and a
mode: `require_satisfied`, `complete_prerequisites`, or `advisory_override`. The cascade
records only the caller's eligible checkbox progress. Optional `root_evidence` lets a
numeric or checklist root record explicit evidence in the same transaction. It never
invents numeric prerequisite evidence or records another person's completion. Strict
roots reject advisory overrides. Failure rolls back the entire operation. Ordinary
partial progress and corrections remain possible without completing prerequisites.

`start_timer`, `stop_timer` and `cancel_timer` use numeric goals whose unit is
`seconds`. Stop records one exact integer duration in the ordinary progress journal.
One active timer per account and checks against historical intervals prevent overlap
across devices. Sessions last at most seven days. Stop/start represents pause/resume;
cancel records no progress and reserves the session ID against reuse. Timers and
cascades support on-demand participation in editable Anyone occurrences, just like
`record`. `GET /occurrences/{id}/timers` pages the caller's own sessions. These commands
can be queued offline, with conflicts resolved on synchronisation. A stop that would
complete a task still requires its prerequisites; rejection rolls back both the stop
and evidence. Retry after satisfying them, submit a valid shorter interval, or cancel
the session to free the account. Backdated sessions can fill gaps before recorded
intervals, but cannot overlap them.

`repeat.frequency = after_completion` uses the interval as calendar days after
completion in the saved task timezone. Each predecessor can issue one deterministic
successor. Corrections and clock rollback cannot remove or move issued work. Switching
between fixed and completion-relative recurrence requires a new task. Shared schedules
advance only from consented evidence, including its timestamp, visible to every
execution reader; a private completion cannot reveal itself through a public date.

Recording, stopping timers, cascades and materialisation reconcile successors. The
server also checks up to 32 pending occurrences per integration tick, deferring each
checked item for 60 seconds. This handles closed upper/range windows and policy changes
without a client command. The main loop ticks every 15 seconds. Backlogs may therefore
delay generation; reads do not promise immediate generation. Already-issued tasks and
history are retained regardless of the clock. Missed `close_incomplete` periods stop
polling until explicit evidence or an occurrence correction; missing a deadline does
not fabricate a successor. A late offline completion with an in-window `happened_at`
can still issue one. Archived tasks pause polling, and unarchiving resumes it. Idle
worker ticks do not acquire the publication lock.

`GET /task-presets?start_date=YYYY-MM-DD&timezone=Europe/Vienna` supplies six editable
ordinary definitions: daily routine, shared chore, weekly quota, joint routine, focus
timer and completion-relative routine. Presets do not override sharing defaults.

## Rotas

Rotas assign responsibility on Anyone tasks; other authorised people can help.
Participants enrol and explicitly accept `rota_consent` before a task editor can put
them in `set_rota`. Both commands use `/task-access-commands` and require a connection.
`GET /tasks/{id}/rota` returns the roster version and the caller's separate consent
version. Execution editors also receive `eligible_participants`: enrolled, consented
accounts that can still see the execution. Other readers receive an empty list. This
lets editors assemble a roster before one exists. Both versions start at zero. The roster supports up to eight unique participants;
an empty roster disables future assignments.

Each generated occurrence snapshots the roster revision, ordinal and assigned account.
The ordinal advances on generation, including covered slots, rather than completion.
Roster changes apply prospectively and start a new ordinal sequence. If the selected
participant withdraws or loses access, new assignments retain the slot with a null
account rather than silently reassigning responsibility. Existing assignments remain
unchanged. A rota does not grant access or authorise recording for someone else.


## Daily planning

| Input or output | Implemented semantics |
| --- | --- |
| Day and timezone | Both explicit. Timed tasks retain their saved scheduling zone; display conversion never rewrites occurrence identity. Date-only work stays date-only. |
| Membership | Scheduled occurrences for the requested day, plus currently actionable overdue/undated work. Clock advancement alone never deletes stored work. |
| State | Select all, actionable, complete, missed or unknown. Actionability is evaluated now; the endpoint does not reconstruct what a past plan looked like. |
| Progress | Personal or joint scope, with frozen goals and participant rosters. Required hidden progress is unknown. Anyone completion needs a visible, consented witness. |
| History | Late evidence updates its original occurrence. Late chore completion can close work without awarding missed-period streak credit. |
| Pagination | Authority and business filtering before UUID order. Continuation includes a revision guard; concurrent publication requires restarting the view. |
| Coverage | `pending_tasks` identifies authorised tasks whose recurrence needs bounded catch-up through today in their saved zone. Reads never silently generate or omit missing periods as though complete. |
| Calendar context | Events, birthdays and source-change review items enrich ordinary tasks. |

The daily view is not the source of truth for an offline cache. Normal edits use
account-wide operation identities and the snapshot/delta protocol. Changing the selected
day must never imply deletion; explicit sync removals or a fresh snapshot determine
current access.


## Clock and source changes

## Atlas tasks do not disappear

An occurrence's identity belongs to its intended schedule slot, not its mutable UTC
timestamp. Recalculating its execution/reminder time updates the same occurrence.
Clock corrections, timezone-database updates and daylight-saving transitions cannot
delete a task, duplicate its progress or silently reset its history.

| Situation | Proposed default |
| --- | --- |
| Date-only task | Keep the local date; no midnight-UTC conversion |
| Vienna 02:30 during the spring-forward gap | Run once at 03:30, shifting by the one-hour gap |
| Vienna 02:30 during autumn's repeated hour | Run once at the first 02:30 |
| Travelling | Continue using the saved task timezone |
| Explicit timezone change | Replan future times, preserve occurrence identity and completed history |
| Server/device clock corrected backwards | Never create a second occurrence or replay completion |
| Reminder was due while disconnected | Apply the configured late-delivery policy; preserve the task |

After the spring exception, the next ordinary occurrence returns to 02:30. A later
follow-me timezone option can exist, but must explicitly specify what happens to an
already open daily window. It is not the default and does not follow device location
silently. Shared tasks and joint streaks need one agreed window/timezone.

Calendar-day offsets and elapsed-hour offsets must be distinct. “One day before at
09:00” preserves local clock intent, whereas “24 hours before” expresses elapsed time.
Persist the chosen rule instead of treating a day as always 86,400 seconds.

## Imported events are evidence, not the lifecycle of a task

Imported ICS interpretation must respect its format. A generated calendar recurrence
at a nonexistent local time is omitted under RFC 5545; explicitly written timestamps
have their own rules. That must not delete an existing Atlas task anchored to it.

Keep the task and last known timing, mark uncertain/changed source state for review,
and preserve completed history. For a source occurrence never generated by a valid
calendar rule, do not invent an external event. Users who need work to occur regardless
of a calendar source should use an Atlas recurrence with optional event enrichment.
The client should make that distinction clear when an event-dependent rule is created.

The adapter must therefore separate Atlas scheduling policy from imported calendar
semantics. The current candidate's gap shift is compatible with the proposed Atlas
policy, but does not by itself establish correct ICS ingestion.

