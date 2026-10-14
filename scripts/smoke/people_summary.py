"""BE-PEOPLE-SUMMARY: viewer-safe People summaries against a real server.

`call` validates every request and response against api/openapi.json. The scenario builds its own
households so earlier smoke modules' state does not matter: alice manages two households with bob,
carol is a direct grantee and later the linked subject of one person. It returns redacted example
bodies (ids replaced by role names) for the WEB-11a handover.
"""
import uuid

V1 = '/api/experimental/v1'


def _manage(call, token, command, expected=200):
    return call('POST', V1 + '/management-commands', token, {'commands': [command]},
                str(uuid.uuid4()), expected)


def _household(call, tokens, name, member, member_id):
    household = str(uuid.uuid4())
    _manage(call, tokens['alice'], {'kind': 'create_household', 'id': household, 'name': name})
    version = next(h for h in call('GET', V1 + '/households', tokens['alice'])
                   if h['id'] == household)['version']
    invitation = str(uuid.uuid4())
    _manage(call, tokens['alice'], {'kind': 'invite_to_household', 'id': invitation,
            'household_id': household, 'recipient_id': member_id, 'expected_version': version})
    _manage(call, tokens[member], {'kind': 'respond_to_household_invitation', 'id': invitation,
            'expected_version': 1, 'accept': True})
    return household


def _set_primary(call, token, household):
    version = call('GET', V1 + '/defaults', token)['preferences_version']
    _manage(call, token, {'kind': 'set_primary_household', 'household_id': household,
            'expected_version': version})


def _person(call, token, name, grants, exclude=()):
    person = str(uuid.uuid4())
    call('POST', V1 + '/access-commands', token, {'commands': [
        {'kind': 'create_person', 'id': person, 'name': name,
         'initial_policy': {'grants': grants, 'exclude_accounts': list(exclude)}}]},
        str(uuid.uuid4()))
    return person


def _field(call, token, person, grants):
    field = str(uuid.uuid4())
    call('POST', V1 + '/access-commands', token, {'commands': [
        {'kind': 'create_field', 'id': field, 'person_id': person, 'label': 'Birthday',
         'value': '1 May', 'initial_policy': {'grants': grants, 'exclude_accounts': []}}]},
        str(uuid.uuid4()))
    return field


def _all(call, token, scope='all'):
    """Every row of `scope`, following `next_after` at a small page size."""
    rows, after = [], None
    while True:
        query = f'?scope={scope}&limit=2' + (f'&after={after}' if after else '')
        page = call('GET', V1 + '/people/summaries' + query, token)
        rows.extend(page['items'])
        if page['next_after'] is None:
            return rows, page['home']
        after = page['next_after']


def _ids(call, token, scope='all'):
    rows, _ = _all(call, token, scope)
    ids = [row['id'] for row in rows]
    assert len(ids) == len(set(ids)) and ids == sorted(ids), ids
    return set(ids)


def _row(call, token, person):
    rows, _ = _all(call, token)
    return next(row for row in rows if row['id'] == person)


def _normalised(value):
    """Blank the members a person's own identity determines (S3 comparison)."""
    if isinstance(value, dict):
        return {k: None if k in ('id', 'label', 'version', 'policy_version') else _normalised(v)
                for k, v in value.items()}
    if isinstance(value, list):
        return [_normalised(v) for v in value]
    return value


def _redact(value, names):
    if isinstance(value, str):
        return names.get(value, value)
    if isinstance(value, dict):
        return {k: _redact(v, names) for k, v in value.items()}
    if isinstance(value, list):
        return [_redact(v, names) for v in value]
    return value


def run(call, accounts, tokens):
    alice, bob, carol = tokens['alice'], tokens['bob'], tokens['carol']
    home = _household(call, tokens, 'Summary Home', 'bob', accounts['bob'])
    away = _household(call, tokens, 'Summary Away', 'bob', accounts['bob'])
    _set_primary(call, bob, home)
    in_home = {'kind': 'household', 'id': home, 'edit': False}

    # S2/S3: two people bob cannot tell apart; the first is also shared directly with carol.
    wide = _person(call, alice, 'Summary Twin', [in_home, {'kind': 'account', 'id': accounts['carol'], 'edit': False}])
    narrow = _person(call, alice, 'Summary Twin', [in_home])
    for person in (wide, narrow):
        _field(call, alice, person, [in_home])
    # An exclusion: shared with Home but not with bob.
    excluded = _person(call, alice, 'Summary Excluded', [in_home], [accounts['bob']])
    elsewhere = _person(call, alice, 'Summary Away', [{'kind': 'household', 'id': away, 'edit': False}])

    def view(person):
        return {
            'row': _row(call, bob, person),
            'detail': call('GET', f'{V1}/people/{person}/summary', bob),
            'fields': call('GET', f'{V1}/people/{person}/field-audiences', bob),
        }
    wide_view, narrow_view = view(wide), view(narrow)
    assert wide_view['row']['audience']['disclosure'] == 'partial'
    assert _normalised(wide_view) == _normalised(narrow_view), 'hidden access changed a non-owner view'
    assert wide_view['detail']['shared_with']['households'][0]['members'] is None
    assert wide_view['detail']['shared_with']['excluded_accounts'] is None

    owner = call('GET', f'{V1}/people/{wide}/summary', alice)
    assert owner['shared_with']['disclosure'] == 'complete'
    assert [a['account_id'] for a in owner['shared_with']['accounts']] == [accounts['carol']]
    ex = call('GET', f'{V1}/people/{excluded}/summary', alice)
    members = {m['account_id']: m['excluded'] for m in ex['shared_with']['households'][0]['members']}
    assert members == {accounts['alice']: False, accounts['bob']: True}, members
    assert ex['summary']['audience']['excluded_count'] == 1

    direct = _row(call, carol, wide)
    assert direct['audience']['your_direct_grant'] == {'can_edit': False}
    assert direct['audience']['households'] == []
    assert wide in _ids(call, carol, 'shared_with_me')

    # S9: In Home is bob's primary household only.
    assert {wide, narrow} <= _ids(call, bob, 'in_home')
    assert not ({excluded, elsewhere} & _ids(call, bob, 'in_home'))
    assert _row(call, bob, elsewhere)['audience']['in_home'] is False
    _set_primary(call, bob, away)
    assert elsewhere in _ids(call, bob, 'in_home') and wide not in _ids(call, bob, 'in_home')
    _set_primary(call, bob, home)

    # S6: linking moves ownership to carol but not who added the person.
    linked = _person(call, alice, 'Summary Linked', [in_home])
    request = str(uuid.uuid4())
    version = call('GET', f'{V1}/people/{linked}', alice)['person']['version']
    call('POST', V1 + '/people-commands', alice, {'command': {'kind': 'request_link', 'id': request,
         'person_id': linked, 'account_id': accounts['carol'], 'expected_version': version}},
         str(uuid.uuid4()))
    call('POST', V1 + '/people-commands', carol, {'command': {'kind': 'respond_request',
         'id': request, 'accept': True}}, str(uuid.uuid4()))
    creator = _row(call, alice, linked)
    assert (creator['added_by_you'], creator['linkage'], creator['your_access']) == ('yes', 'other', 'view'), creator
    assert creator['owner']['account_id'] == accounts['carol']
    assert linked not in _ids(call, alice, 'shared_with_me')
    subject = _row(call, carol, linked)
    assert (subject['added_by_you'], subject['linkage'], subject['your_access']) == ('no', 'you', 'owner'), subject
    assert _row(call, bob, linked)['added_by_you'] == 'no'
    assert linked in _ids(call, bob, 'shared_with_me')

    examples = {
        'owner_row': _row(call, alice, wide),
        'partial_row': wide_view['row'],
        'owner_detail': owner,
        'partial_detail': wide_view['detail'],
        'partial_field_audiences': wide_view['fields'],
        'linked_creator_row': creator,
    }

    # S12: removing bob from Away revokes what only Away gave him and clears that primary.
    _set_primary(call, bob, away)
    version = next(h for h in call('GET', V1 + '/households', alice) if h['id'] == away)['version']
    _manage(call, alice, {'kind': 'remove_household_member', 'household_id': away,
            'account_id': accounts['bob'], 'expected_version': version})
    assert elsewhere not in _ids(call, bob)
    call('GET', f'{V1}/people/{elsewhere}/summary', bob, expected=404)
    rows, home_after = _all(call, bob, 'in_home')
    assert rows == [] and home_after is None
    examples['no_primary_household'] = call('GET', V1 + '/people/summaries?scope=in_home', bob)
    _set_primary(call, bob, home)

    names = {accounts['alice']: 'ALICE', accounts['bob']: 'BOB', accounts['carol']: 'CAROL',
             home: 'HOME', away: 'AWAY', wide: 'WIDE_PERSON', linked: 'LINKED_PERSON'}
    for item in wide_view['fields']['items']:
        names[item['id']] = 'FIELD'
    return {'scenario': 'people-summaries', 'examples': _redact(examples, names)}


if __name__ == '__main__':
    raise SystemExit('Run through scripts/smoke_server.py or scripts/test_replicas.py')
