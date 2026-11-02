"""BE-PEOPLE-ACTIVITY: the People activity timeline against a real server.

`call` validates every request and response against api/openapi.json; in `test_replicas.py` each
successive request is served by the other process. The scenario builds its own people so earlier
smoke modules' state does not matter: alice owns a person shared with bob (edit) and carol (view),
with one shared and one private field. Every read is walked at a small page size and repeated, so
two processes (or two reads) must return the identical ordered list with unique, stable ids. It
returns a redacted example timeline (ids replaced by role names) for the WEB-19 handover.
"""
import uuid

V1 = '/api/experimental/v1'


def _grant(account, edit):
    return {'kind': 'account', 'id': account, 'edit': edit}


def _create(call, token, commands):
    """Sharing creations go to `/access-commands`, private ones to `/commands` (one route per batch)."""
    def shares(command):
        policy = command.get('initial_policy') or {}
        return bool(policy.get('grants') or policy.get('exclude_accounts'))
    for online in (True, False):
        batch = [c for c in commands if shares(c) == online]
        if batch:
            call('POST', V1 + ('/access-commands' if online else '/commands'), token, {'commands': batch},
                 str(uuid.uuid4()))


def _resource(call, token, person, resource):
    if resource == person:
        return call('GET', f'{V1}/people/{person}', token)['person']
    for archived in ('false', 'true'):
        page = call('GET', f'{V1}/fields?parent_id={person}&archived={archived}&limit=200', token)
        found = [item for item in page['items'] if item['id'] == resource]
        if found:
            return found[0]
    raise AssertionError(f'{resource} is not visible')


def _edit(call, token, person, resource, label, value, operation=None):
    current = _resource(call, token, person, resource)
    body = {'commands': [{'kind': 'edit', 'id': resource, 'expected_version': current['version'],
                          'expected_policy_version': current['policy_version'], 'label': label,
                          'value': value}]}
    return call('POST', V1 + '/commands', token, body, operation or str(uuid.uuid4()))


def _archive(call, token, person, resource, archived):
    current = _resource(call, token, person, resource)
    call('POST', V1 + '/task-commands', token, {'command': {
        'kind': 'archive', 'id': resource, 'expected_version': current['version'],
        'archived': archived}}, str(uuid.uuid4()))


def _walk(call, token, person, limit):
    """The whole timeline, following `next_after`; each page is checked against the rules."""
    items, after = [], None
    while True:
        query = f'?limit={limit}' + (f'&after={after}' if after else '')
        page = call('GET', f'{V1}/people/{person}/activity{query}', token)
        assert len(page['items']) <= limit, page
        if page['next_after'] is not None:
            assert len(page['items']) == limit and page['next_after'] == page['items'][-1]['id'], page
        items.extend(page['items'])
        if page['next_after'] is None:
            return items, page
        after = page['next_after']


def _timeline(call, token, person):
    """The timeline read twice at two page sizes: all four reads must agree exactly."""
    first, page = _walk(call, token, person, 2)
    for limit in (3, 100):
        again, _ = _walk(call, token, person, limit)
        assert again == first, (limit, again, first)
    ids = [item['id'] for item in first]
    assert len(set(ids)) == len(ids), ids
    return first, page


def _types(items):
    return [item['type'] for item in items]


def _redact(value, names):
    if isinstance(value, str):
        return names.get(value, value)
    if isinstance(value, list):
        return [_redact(v, names) for v in value]
    if isinstance(value, dict):
        return {k: ('UNIX_SECONDS' if k in ('occurred_at', 'coverage_started_at') else _redact(v, names))
                for k, v in value.items()}
    return value


def run(call, accounts, tokens):
    alice, bob, carol = tokens['alice'], tokens['bob'], tokens['carol']
    person, shared, private = (str(uuid.uuid4()) for _ in range(3))
    audience = {'grants': [_grant(accounts['bob'], True), _grant(accounts['carol'], False)],
                'exclude_accounts': []}
    _create(call, alice, [
        {'kind': 'create_person', 'id': person, 'name': 'Activity Morgan', 'initial_policy': audience},
        {'kind': 'create_field', 'id': shared, 'person_id': person, 'label': 'Hobby', 'value': 'Surfing',
         'initial_policy': audience},
        {'kind': 'create_field', 'id': private, 'person_id': person, 'label': 'Medical note',
         'value': 'activity-private-value', 'initial_policy': {'grants': [], 'exclude_accounts': []}},
    ])
    _edit(call, bob, person, shared, 'Pastime', 'Rowing')
    _edit(call, alice, person, private, 'Medical', 'activity-private-value-2')
    _edit(call, alice, person, person, 'Activity Morgan Lee', '')
    _archive(call, alice, person, shared, True)
    _archive(call, alice, person, shared, False)
    # A command retried with the same key (each attempt may reach a different process): one event.
    operation = str(uuid.uuid4())
    current = _resource(call, alice, person, shared)
    retried = {'commands': [{'kind': 'edit', 'id': shared, 'expected_version': current['version'],
                             'expected_policy_version': current['policy_version'],
                             'label': 'Pastime', 'value': 'Sailing'}]}
    first = call('POST', V1 + '/commands', alice, retried, operation)
    assert call('POST', V1 + '/commands', alice, retried, operation) == first

    owner, page = _timeline(call, alice, person)
    assert _types(owner) == ['field_edited', 'field_restored', 'field_archived', 'person_renamed',
                             'field_edited', 'field_edited', 'field_created', 'field_created',
                             'person_created'], _types(owner)
    assert page['person_id'] == person and page['coverage_started_at'] > 0
    assert owner[0]['changed'] == ['value'] and owner[0]['actor'] == {'kind': 'you'}
    assert owner[5]['actor'] == {'kind': 'someone_else'}, 'a grantee is not named (D-ACT)'
    assert all(item['target']['label'] in ('Activity Morgan Lee', 'Pastime', 'Medical') for item in owner)

    reader, _ = _timeline(call, carol, person)
    assert _types(reader) == ['field_edited', 'field_restored', 'field_archived', 'person_renamed',
                              'field_edited', 'field_created', 'person_created'], _types(reader)
    assert reader[-1]['actor']['kind'] == 'account' and reader[-1]['actor']['account_id'] == accounts['alice']
    assert reader[4]['actor'] == {'kind': 'someone_else'}
    text = repr(reader)
    for hidden in (private, 'Medical', 'activity-private-value', accounts['bob']):
        assert hidden not in text, hidden
    editor, _ = _timeline(call, bob, person)
    assert [item['id'] for item in editor] == [item['id'] for item in reader]
    assert editor[4]['actor'] == {'kind': 'you'}

    # A merge of a person carol never saw: carol gets the merge without its source or its history.
    source = str(uuid.uuid4())
    _create(call, alice, [{'kind': 'create_person', 'id': source, 'name': 'Activity Source',
                           'initial_policy': {'grants': [], 'exclude_accounts': []}}])
    preview = call('POST', V1 + '/people/merge-preview', alice, {'source_id': source, 'target_id': person})
    call('POST', V1 + '/people-commands', alice, {'command': {
        'kind': 'merge', 'source_id': source, 'target_id': person, 'preview_token': preview['token'],
        'name': 'Activity Morgan Lee'}}, str(uuid.uuid4()))
    merged, _ = _timeline(call, alice, source)
    assert merged[0]['type'] == 'person_merged' and merged[0]['merged_from']['id'] == source, merged[0]
    # The source was created after the target's history, so its own event is the next newest.
    assert merged[1]['type'] == 'person_created'
    assert merged[1]['target'] == {'kind': 'person', 'id': source, 'label': 'Activity Source', 'archived': True,
                                   'state': 'merged', 'canonical_person_id': person}, merged[1]
    assert merged[2:] == owner
    after_merge, _ = _timeline(call, carol, person)
    assert after_merge[0]['type'] == 'person_merged' and 'merged_from' not in after_merge[0]
    assert after_merge[1:] == reader and source not in repr(after_merge)

    # Link to carol and unlink: the accepter is the actor; ownership follows the link.
    linked = str(uuid.uuid4())
    _create(call, alice, [{'kind': 'create_person', 'id': linked, 'name': 'Activity Linked',
                           'initial_policy': {'grants': [], 'exclude_accounts': []}}])
    request = str(uuid.uuid4())
    version = call('GET', f'{V1}/people/{linked}', alice)['person']['version']
    call('POST', V1 + '/people-commands', alice, {'command': {
        'kind': 'request_link', 'id': request, 'person_id': linked, 'account_id': accounts['carol'],
        'expected_version': version}}, str(uuid.uuid4()))
    result = call('POST', V1 + '/people-commands', carol, {'command': {
        'kind': 'respond_request', 'id': request, 'accept': True}}, str(uuid.uuid4()))
    canonical = result['person_id']
    subject, _ = _timeline(call, carol, canonical)
    # Carol may already have a profile from an earlier module, in which case linking merges into it.
    assert subject[0]['type'] in ('person_linked', 'person_merged'), subject[0]
    assert subject[0]['actor'] == {'kind': 'you'}
    detail = call('GET', f'{V1}/people/{canonical}', carol)
    call('POST', V1 + '/people-commands', carol, {'command': {
        'kind': 'unlink', 'person_id': canonical, 'expected_version': detail['person']['version']}},
        str(uuid.uuid4()))
    unlinked, _ = _timeline(call, carol, canonical)
    assert unlinked[0]['type'] == 'person_unlinked' and unlinked[0]['account'] == {'kind': 'you'}, unlinked[0]
    assert unlinked[1:] == subject

    # An invisible person and a missing one are the same not_found as GET /people/{id}.
    hidden = str(uuid.uuid4())
    _create(call, alice, [{'kind': 'create_person', 'id': hidden, 'name': 'Activity Hidden',
                           'initial_policy': {'grants': [], 'exclude_accounts': []}}])
    for subject_id in (hidden, str(uuid.uuid4())):
        error = call('GET', f'{V1}/people/{subject_id}/activity', bob, expected=404)
        detail = call('GET', f'{V1}/people/{subject_id}', bob, expected=404)
        assert {k: v for k, v in error.items() if k != 'request_id'} == \
            {k: v for k, v in detail.items() if k != 'request_id'}
    stale = call('GET', f'{V1}/people/{person}/activity?after={uuid.uuid4()}', carol, expected=422)
    assert stale['code'] == 'invalid_value'
    # A cursor at an event carol cannot see (the private field's, the hidden merge source's) is the
    # same error as a missing one, while the owner can use it (BE-PEOPLE-ACTIVITY review R1).
    without_id = lambda body: {k: v for k, v in body.items() if k != 'request_id'}
    hidden_events = [item['id'] for item in merged if item['target']['id'] in (private, source)]
    assert len(hidden_events) == 3, hidden_events
    for event in hidden_events:
        refused = call('GET', f'{V1}/people/{person}/activity?after={event}', carol, expected=422)
        assert without_id(refused) == without_id(stale), refused
        call('GET', f'{V1}/people/{person}/activity?after={event}', alice)

    names = {person: 'PERSON', shared: 'SHARED_FIELD', source: 'MERGED_SOURCE',
             accounts['alice']: 'ALICE', accounts['bob']: 'BOB', accounts['carol']: 'CAROL'}
    names.update({item['id']: f'EVENT_{i}' for i, item in enumerate(after_merge)})
    return {'people_activity': {'events_for_reader': len(after_merge), 'events_for_owner': len(owner) + 2,
                                'example_reader_page': _redact(after_merge[:4], names)}}
