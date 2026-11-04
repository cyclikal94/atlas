"""BE-Q23 bounded `/sync` delivery against a real server process.

Two new devices of one account read the same revision: one without `max_changes` (one whole
batch) and one at `max_changes=4` (pieces). Every response, segments and continuation cursors
included, is validated against the contract by `ContractClient`. The reassembled pieces must
equal the whole batch, and a third device's fresh snapshot must agree with both. Field values
carry a sentinel in the caller's secret list, so the final log scan proves content never
reaches the logs.
"""
import uuid

V1 = '/api/experimental/v1'
PASSWORD = 'local-smoke-password-123'
SENTINEL = 'BOUNDED_SYNC_SENTINEL'


def _drain(call, token, cursor, query=''):
    pages = []
    while True:
        separator = '&' if query else ''
        page = call('GET', f'{V1}/sync?cursor={cursor}{separator}{query}', token)
        pages.append(page)
        cursor = page['next_cursor']
        if not page['has_more']:
            return pages, cursor


def run(call, accounts, tokens, secrets):
    secrets.append(SENTINEL)
    devices = {}
    for device in ['bounded-whole', 'bounded-pieces', 'bounded-fresh']:
        session = call('POST', V1 + '/sessions', body={
            'username': 'carol', 'password': PASSWORD, 'device_id': device})
        devices[device] = session['access_token']
        secrets.append(session['access_token'])
    heads = {}
    for device in ['bounded-whole', 'bounded-pieces']:
        page = call('GET', V1 + '/sync', devices[device])
        while page['has_more']:
            page = call('GET', f"{V1}/sync?cursor={page['next_cursor']}", devices[device])
        heads[device] = page['next_cursor']
    person = str(uuid.uuid4())
    commands = [{'kind': 'create_person', 'id': person, 'name': 'Bounded smoke'}]
    commands += [{'kind': 'create_field', 'id': str(uuid.uuid4()), 'person_id': person,
                  'label': 'Bounded', 'value': f'{SENTINEL} {i}'} for i in range(12)]
    call('POST', V1 + '/commands', devices['bounded-whole'], {'commands': commands},
         operation=str(uuid.uuid4()))

    whole, _ = _drain(call, devices['bounded-whole'], heads['bounded-whole'])
    assert len(whole) == 1 and len(whole[0]['batches']) == 1, whole
    batch = whole[0]['batches'][0]
    assert 'segment' not in batch and len(batch['changes']) == 13, batch
    assert whole[0]['next_cursor'].startswith('d1.')

    # max_changes outside 4..=800 is invalid_value; the error body is checked by the client.
    call('GET', f"{V1}/sync?cursor={heads['bounded-pieces']}&max_changes=3",
         devices['bounded-pieces'], expected=422)
    pieces, end = _drain(call, devices['bounded-pieces'], heads['bounded-pieces'], 'max_changes=4')
    segments = [page['batches'][0]['segment'] for page in pieces]
    assert segments == [{'index': i, 'last': i == 3} for i in range(4)], segments
    assert all(len(page['batches']) == 1 and len(page['batches'][0]['changes']) <= 4
               for page in pieces), pieces
    cursors = [page['next_cursor'][:2] for page in pieces]
    assert cursors == ['d2', 'd2', 'd2', 'd1'], cursors
    assert end.startswith('d1.')
    reassembled = {}
    for page in pieces:
        changes = page['batches'][0]['changes']
        # Every piece carries the person before its fields.
        assert changes[0]['resource']['id'] == person, changes
        for change in changes:
            reassembled[change['resource']['id']] = change['resource']
    expected = {change['resource']['id']: change['resource'] for change in batch['changes']}
    assert reassembled == expected, 'the pieces reassemble to the whole batch'

    fresh = call('GET', V1 + '/sync', devices['bounded-fresh'])
    snapshot = {}
    while True:
        for change in fresh['batches'][0]['changes']:
            snapshot[change['resource']['id']] = change['resource']
        if not fresh['has_more']:
            break
        fresh = call('GET', f"{V1}/sync?cursor={fresh['next_cursor']}", devices['bounded-fresh'])
    assert all(snapshot[key] == value for key, value in expected.items()), 'a fresh snapshot agrees'
    return {'scenario': 'bounded-sync-one-process', 'whole_changes': 13,
            'pieces': len(pieces), 'max_changes': 4}
