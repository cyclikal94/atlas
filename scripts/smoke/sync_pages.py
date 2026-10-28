"""BE-B1 `/sync` page counts against a real server process.

A delta page requested at the `limit` ceiling must carry every pending publication batch, and
`ContractClient` validates each page against the contract, so more than `SyncPage.batches.maxItems`
batches in a valid response would fail here. The session's device is this module's own.
"""
import uuid

V1 = '/api/experimental/v1'
PENDING = 85  # more than the former 80-batch page bound


def run(call, tokens, secrets):
    token = call('POST', V1 + '/sessions', body={
        'username': 'carol', 'password': secrets[0], 'device_id': 'sync-pages'})['access_token']
    secrets.append(token)
    person = str(uuid.uuid4())
    call('POST', V1 + '/commands', token, {'commands': [
        {'kind': 'create_person', 'id': person, 'name': 'Paged'}]}, str(uuid.uuid4()))
    page = call('GET', V1 + '/sync?limit=200', token)
    while page['has_more']:
        page = call('GET', V1 + '/sync?limit=200&cursor=' + page['next_cursor'], token)
    cursor = page['next_cursor']
    revisions = []
    for version in range(1, PENDING + 1):
        revisions.append(call('POST', V1 + '/commands', token, {'commands': [
            {'kind': 'edit', 'id': person, 'expected_version': version, 'expected_policy_version': 1,
             'label': f'Version {version}', 'value': ''}]}, str(uuid.uuid4()))['revision'])
    pages, delivered = [], []
    while True:
        page = call('GET', V1 + '/sync?limit=200&cursor=' + cursor, token)
        assert page['phase'] == 'delta', page['phase']
        pages.append(len(page['batches']))
        delivered.extend(batch['revision'] for batch in page['batches'])
        cursor = page['next_cursor']
        if not page['has_more']:
            break
    assert pages == [PENDING], pages
    assert delivered == revisions, 'delta pages lost, duplicated or reordered a batch'
    for limit in ('0', '201'):
        error = call('GET', V1 + '/sync?limit=' + limit, token, expected=422)
        assert error['code'] == 'invalid_value', error
    return {'scenario': 'sync-pages', 'pending': PENDING, 'pages': pages}
