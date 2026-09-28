"""BE-B6 over real HTTP: independent per-occurrence timers and the account-wide timer list.

Every request and response is validated against the OpenAPI contract by the caller's client,
including the `oneOf` of the two row shapes. Nothing here mocks a server or a database."""
import time
import uuid

V1 = '/api/experimental/v1'


def occurrence_of(task):
    return str(uuid.uuid5(uuid.UUID(task), 'atlas-occurrence-v1:once'))


def progress_of(occurrence, account):
    return str(uuid.uuid5(uuid.UUID(occurrence), 'atlas-progress-v1:' + account))


def seconds_task(call, owner_token, title='Timed', share_with=None, participation='personal'):
    """A once-only task with a seconds goal; returns (task id, occurrence id)."""
    task = str(uuid.uuid4())
    command = {'kind': 'create_task', 'id': task, 'execution_id': str(uuid.uuid4()), 'title': title,
               'definition': {'schedule': {'start_date': None, 'time': None, 'timezone': 'UTC', 'repeat': None},
                              'goal': {'kind': 'numeric', 'minimum': '60', 'maximum': None, 'unit': 'seconds'},
                              'carry': 'retain_one', 'participation': participation,
                              'open_days_before': 0, 'close_days_after': 1, 'allow_streak_exclusions': False}}
    endpoint = 'task-commands'
    if share_with:
        command['initial_policy'] = {'grants': [{'kind': 'account', 'id': share_with, 'edit': True}],
                                     'exclude_accounts': []}
        endpoint = 'task-access-commands'
    call('POST', f'{V1}/{endpoint}', owner_token, {'command': command}, str(uuid.uuid4()))
    return task, occurrence_of(task)


def start(call, token, occurrence, session, at, operation=None, expected=200):
    return call('POST', V1 + '/task-commands', token, {'command': {
        'kind': 'start_timer', 'occurrence_id': occurrence, 'session_id': session, 'started_at': at}},
        operation or str(uuid.uuid4()), expected=expected)


def stop(call, token, occurrence, session, at, expected=200):
    return call('POST', V1 + '/task-commands', token, {'command': {
        'kind': 'stop_timer', 'occurrence_id': occurrence, 'session_id': session,
        'expected_version': 1, 'stopped_at': at}}, str(uuid.uuid4()), expected=expected)


def listing(call, token, query='', expected=200):
    return call('GET', V1 + '/timer-sessions' + query, token, expected=expected)


def walk(call, token, state=None, limit=1):
    """Every page of the listing, following `next_after` until it is null."""
    ids, pages, after = [], 0, None
    while True:
        query = f'?limit={limit}' + (f'&state={state}' if state else '') + (f'&after={after}' if after else '')
        page = listing(call, token, query)
        assert len(page['items']) <= limit, page
        ids += [item['id'] for item in page['items']]
        pages += 1
        after = page['next_after']
        if after is None:
            return ids, pages
        assert pages < 500, 'the cursor walk must terminate'


def recorded(call, token, occurrence, account):
    entries = call('GET', f'{V1}/progress/{progress_of(occurrence, account)}/entries', token)['entries']
    return [entry['evidence']['amount'] for entry in entries if entry['evidence']['kind'] == 'quantity']


AVAILABLE = {'access', 'id', 'occurrence_id', 'task_id', 'task_title', 'slot_date', 'started_at',
             'stopped_at', 'version', 'can_modify'}
RESTRICTED = {'access', 'id', 'started_at', 'stopped_at', 'version'}


def run(call, accounts, tokens, document, leave_running=True):
    """Two timers on two occurrences, the list, redaction, per-occurrence durations, and one timer
    deliberately left running so a later backup holds a running session."""
    alice, bob = tokens['alice'], tokens['bob']
    now = int(time.time())
    (task_a, occ_a), (_, occ_b), (_, occ_c) = [seconds_task(call, alice) for _ in range(3)]
    session_a, session_b, session_c = (str(uuid.uuid4()) for _ in range(3))

    # Two occurrences of one person run at once: this was a conflict for the whole account before 0.27.0.
    operation = str(uuid.uuid4())
    started_a = start(call, alice, occ_a, session_a, now - 600, operation)
    start(call, alice, occ_b, session_b, now - 500)
    # Offline replay: the same operation again is answered from its receipt, not re-evaluated.
    assert start(call, alice, occ_a, session_a, now - 600, operation) == started_a
    # One running timer per occurrence still holds, on every device.
    refused = start(call, alice, occ_a, str(uuid.uuid4()), now - 400, expected=409)
    assert refused['code'] == 'conflict', refused

    page = listing(call, alice)
    mine = {item['id']: item for item in page['items']}
    assert {session_a, session_b} <= set(mine), page
    for session in (session_a, session_b):
        item = mine[session]
        assert item['access'] == 'available' and set(item) == AVAILABLE, item
        assert item['stopped_at'] is None and item['can_modify'] is True, item
    running = [item['id'] for item in page['items'] if item['stopped_at'] is None]
    assert running.index(session_b) < running.index(session_a), 'newest start first'
    assert all(item['stopped_at'] is not None for item in listing(call, alice, '?state=stopped')['items'])

    # Redaction: bob times a task alice shares with him; alice then withdraws the share.
    shared_task, shared_occurrence = seconds_task(call, alice, 'Shared timed', share_with=accounts['bob'],
                                                  participation='anyone')
    session_bob = str(uuid.uuid4())
    start(call, bob, shared_occurrence, session_bob, now - 300)
    seen = next(item for item in listing(call, bob)['items'] if item['id'] == session_bob)
    assert seen['access'] == 'available' and seen['task_title'] == 'Shared timed', seen
    def share(policy_grants):
        """Replace the task's policy (the online-only management route) and return nothing."""
        version = call('GET', f'{V1}/tasks/{shared_task}', alice)['task']['policy_version']
        call('POST', V1 + '/management-commands', alice, {'commands': [{
            'kind': 'replace_policy', 'id': shared_task, 'expected_version': version,
            'policy': {'grants': policy_grants, 'exclude_accounts': []}}]}, str(uuid.uuid4()))
    share([])
    narrowed = listing(call, bob)
    hidden = next(item for item in narrowed['items'] if item['id'] == session_bob)
    assert hidden['access'] == 'restricted' and set(hidden) == RESTRICTED, hidden
    assert shared_task not in str(narrowed) and 'Shared timed' not in str(narrowed), narrowed
    assert len(narrowed['items']) == len(listing(call, bob, '?limit=200')['items'])
    # The owner cannot stop what they can no longer read; the server stops nothing on their behalf.
    denied = stop(call, bob, shared_occurrence, session_bob, now - 100, expected=404)
    assert denied['code'] == 'not_found', denied
    assert session_bob in [i['id'] for i in listing(call, bob, '?state=running')['items']]
    # Alice sees none of bob's timers.
    assert session_bob not in [i['id'] for i in listing(call, alice, '?limit=200')['items']]
    # Access restored: detail returns.
    share([{'kind': 'account', 'id': accounts['bob'], 'edit': True}])
    restored = next(item for item in listing(call, bob)['items'] if item['id'] == session_bob)
    assert restored['access'] == 'available' and restored['can_modify'] is True, restored
    stop(call, bob, shared_occurrence, session_bob, now - 100)

    # Nested intervals: b [now-500, now-300] lies inside a [now-600, now-100]. Each records its own duration.
    stop(call, alice, occ_b, session_b, now - 300)
    assert recorded(call, alice, occ_b, accounts['alice']) == ['200'], 'b records only its own 200 seconds'
    stop(call, alice, occ_a, session_a, now - 100)
    assert recorded(call, alice, occ_a, accounts['alice']) == ['500'], 'a records only its own 500 seconds'

    # Order and paging over a mixed list, across the running/finished boundary.
    start(call, alice, occ_c, session_c, now - 60)
    full = [item['id'] for item in listing(call, alice, '?limit=200')['items']]
    assert full[0] == session_c and full.index(session_c) < full.index(session_a), full
    finished = [item['id'] for item in listing(call, alice, '?state=stopped&limit=200')['items']]
    assert finished[:2] == [session_a, session_b], 'latest finish first'
    walked, pages = walk(call, alice, limit=1)
    assert walked == full and pages == len(full), (walked, full)
    assert walk(call, alice, 'stopped', 1)[0] == finished
    assert walk(call, alice, 'running', 1)[0] == [i for i in full if i not in finished]
    for query, code in (('?limit=0', 422), ('?limit=201', 422), ('?state=bogus', 422),
                        ('?after=garbage', 422), ('?limit=x', 400), ('?unknown=1', 400)):
        listing(call, alice, query, expected=code)
    call('GET', V1 + '/timer-sessions', expected=401)
    if not leave_running:
        stop(call, alice, occ_c, session_c, now - 30)
    return {'scenario': 'independent-timers', 'sessions': len(full), 'left_running': leave_running}
