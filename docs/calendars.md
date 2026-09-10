# Calendar enrichment

Atlas ingests ICS files and links to enrich ordinary tasks. It does not edit external
calendars. All routes below use `/api/experimental/v1`.

## Sources and imports

Create a source through `POST /calendar-commands`. File imports work without server
secrets. Sources are private unless an initial policy explicitly shares them. Events
copy that policy when created and remain subject to the source's access gate. Use the
existing policy API to change access. Connection URLs, bearer tokens and HTTP validators
never enter resource projections or sync; connection configuration is owner-only.

<!-- experimental-schema: CalendarCommandInput -->
```json
{"command":{"kind":"create_source","id":"22222222-2222-4222-8222-222222222222","label":"Family calendar","timezone":"Europe/Vienna"}}
```

All calendar and reminder POST operations require a UUID `Idempotency-Key`. Reusing it
with the same command returns its receipt; different content returns `operation_conflict`.
Receipts for settings without sync changes may contain revision zero on an empty server.

`POST /calendar-sources/{id}/import` takes `ics`, `from` and `through`. The coverage range
is inclusive, at most 730 days, with canonical dates between 1900 and 9999. Inputs are
bounded to 1 MiB UTF-8, 20,000 lines, 256 event components and 2,048 expanded instances;
the JSON envelope limit is 2 MiB. Recurrence expansion has a shared iteration budget.
Unsupported imports fail atomically and retain the last good events. Source health and
last-success time show the failure without exposing connection details.

Supported: date-only, UTC, floating dates in the source timezone, IANA timezones,
daily/weekly/monthly/yearly recurrence, interval, count/until, BYDAY, BYMONTH,
BYMONTHDAY, WKST, RDATE, EXDATE and individual RECURRENCE-ID overrides. Generated times
in a daylight-saving gap are omitted; ambiguous times use the first occurrence.
Explicit local times preserve their intended wall-clock value. Embedded VTIMEZONE
rules must agree with the named IANA zone over the imported range. Custom timezones,
RANGE overrides, DURATION, EXRULE and unsupported recurrence parts fail explicitly.
Timed DTEND must use the same timezone as DTSTART. Ordinal yearly BYDAY requires
BYMONTH. A detached override should retain its original recurrence identifier and
zone; changing DTSTART alone must not change RECURRENCE-ID.

A successful covered import marks an absent event `missing`, not confirmed cancelled.
Explicit STATUS:CANCELLED, EXDATE or METHOD:CANCEL can confirm cancellation. Cancellation
messages do not mark unrelated events missing. Lower SEQUENCE values cannot overwrite
newer stored events. Dates outside declared coverage are left alone. HTTP 304 preserves
events; fetch and parsing failures never count as an empty successful import.

`GET /calendar-sources`, `/events`, `/review-items` and `/reminders` return authorised
resource pages, optionally filtered by `parent_id`, with `after` and `limit` pagination.
Their `value` is native structured JSON, like other Atlas projections.
Source/event identity and visibility changes use the existing sync protocol.

## Anchored tasks and review

Supply an optional `anchor` on `create_task`, or use the calendar `set_anchor` command
with the task's current content version. The task's schedule must have no recurrence:
its event series or person date supplies the occurrences. A person date is an existing
typed date field; annual dates may omit the birth year. Leap-day dates produce only
valid anniversaries. The worker maintains the current/adjacent annual years and a
creation horizon of 30 days back to 400 days ahead. Previously stored annual years
remain recognised. The creator accepts this initial backlog; later enrolments remain
prospective.

<!-- experimental-schema: CalendarAnchor -->
```json
{"reference":{"kind":"event_series","source_id":"22222222-2222-4222-8222-222222222222","uid":"trip"},"offset":{"kind":"calendar_days","days":-1,"time":"18:00:00"}}
```

Calendar-day offsets follow the task's saved timezone and preserve date-only values
unless an explicit time is supplied. Elapsed-second offsets require a timed anchor.
Optional weekday and title-substring conditions control eligibility. Event occurrences
retain their IDs and original identity keys when moved; order uses saved dates and
clock times, not opaque IDs.

Unstarted tasks follow source timing when all recipients can see the reference and
relevant progress evidence. Existing work, completed occurrences and resolved history
are preserved. Started work gets a private `work_started` review instead of having its
progress window moved. A move, missing/cancelled source, failed eligibility or unavailable
context creates a durable private review item. Review resolution is explicit and
versioned. It acknowledges the issue; adjust the task/occurrence or binding separately.
Binding changes retain existing occurrences; adding a binding to a task with existing
one-off work can create additional anchored occurrences. Resolve superseded work
explicitly. Disabling a binding stops automation and keeps its existing occurrences.

Sharing a task does not grant access to its source. If access or private progress prevents
safe propagation, Atlas retains the saved timing and creates a private generic review.
It does not reveal the new date or title. `GET /tasks/{id}/anchor` returns null when the
reference is unavailable to the caller. For event-series bindings, readers other than
the binding author must also see an event with that UID; seeing the calendar source
alone cannot reveal private event identifiers. Binding configuration is read online; occurrences,
progress, events and reviews synchronise normally. A cancelled event never deletes tasks.


Annual person-date anchors observe February 29 on February 28 in common years. The
field retains its original date and each year's anchor key stays unchanged. Leap years
use February 29; fixed yearly task schedules and imported ICS rules keep their existing
recurrence semantics. Existing work is retained when the reconciliation horizon moves.
