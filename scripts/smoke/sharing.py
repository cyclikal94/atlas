import concurrent.futures
import datetime
import json
import threading
import time
import uuid
from jsonschema import Draft202012Validator, FormatChecker

def run(call, accounts, tokens, secrets):
    call('GET', '/api/experimental/v1/sync', expected=401)
    person, field = str(uuid.uuid4()), str(uuid.uuid4())
    creation = {'commands':[
        {'kind':'create_person', 'id':person, 'name':'Morgan'},
        {'kind':'create_field', 'id':field, 'person_id':person,
         'label':secrets[2], 'value':secrets[1]}]}
    operation = str(uuid.uuid4())
    first = call('POST', '/api/experimental/v1/commands', tokens['alice'], creation, operation)
    retry = call('POST', '/api/experimental/v1/commands', tokens['alice'], creation, operation)
    if first != retry:
        raise RuntimeError('Idempotent retry changed receipt')
    call('POST', '/api/experimental/v1/commands', tokens['alice'], creation, str(uuid.uuid4()), 409)
    call('POST', '/api/experimental/v1/access-commands', tokens['alice'], {'commands':[
        {'kind':'grant','id':person,'expected_version':1,'account_id':accounts['bob'],'edit':True}]}, str(uuid.uuid4()))
    page = call('GET', '/api/experimental/v1/sync', tokens['bob'])
    if any(secret in json.dumps(page) for secret in secrets):
        raise RuntimeError('Private field reached shared projection')
    call('GET', '/api/experimental/v1/sync?limit=1', tokens['alice'])
    call('POST', '/api/experimental/v1/access-commands', tokens['alice'], {'commands':[
        {'kind':'revoke','id':person,'expected_version':2,'account_id':accounts['bob']}]}, str(uuid.uuid4()))
    delta = call('GET', '/api/experimental/v1/sync?cursor=' + page['next_cursor'], tokens['bob'])
    if delta['batches'][0]['changes'] != [{'kind':'remove', 'id':person}]:
        raise RuntimeError('Revocation did not produce a removal')
    return person, field

def read_resource(call, accounts, tokens, who, resource):
    """The resource as `who` sees it in a fresh snapshot, following every page, or None."""
    base = '/api/experimental/v1/'
    path = base + 'sync?limit=200'
    while True:
        page = call('GET', path, tokens[who])
        for batch in page['batches']:
            for change in batch['changes']:
                if change['kind'] == 'upsert' and change['resource']['id'] == resource:
                    if accounts['alice'] in json.dumps(page) and who != 'alice':
                        raise RuntimeError('A reader was told about another account')
                    return change['resource']
        if not page['has_more']:
            return None
        path = base + 'sync?limit=200&cursor=' + page['next_cursor']

def alias_edit(call, accounts, tokens):
    """An edit is checked against the resource it names, never one it was re-addressed to.

    Reproduces the review's schedule over HTTP. The source person is edited once and shared
    with bob and unshared again, leaving it private at (version, policy_version) = (2, 3);
    the target is shared with bob at (1, 2). Merging the source into the target advances the
    target's counters to (2, 3), so a draft captured from the source has counters that
    equal the canonical person's while the audiences differ."""
    base = '/api/experimental/v1/'
    source, target = str(uuid.uuid4()), str(uuid.uuid4())
    private = {'grants': [], 'exclude_accounts': []}

    def post(who, route, body, expected=200):
        return call('POST', base + route, tokens[who], body, str(uuid.uuid4()), expected)

    def counters(who, resource):
        seen = read_resource(call, accounts, tokens, who, resource)
        return None if seen is None else (seen['version'], seen['policy_version'])

    def edit(resource, version, revision, label):
        return {'commands': [{'kind': 'edit', 'id': resource, 'expected_version': version,
                              'expected_policy_version': revision, 'label': label, 'value': ''}]}

    post('alice', 'commands', {'commands': [
        {'kind': 'create_person', 'id': source, 'name': 'Alias source', 'initial_policy': private},
        {'kind': 'create_person', 'id': target, 'name': 'Alias target', 'initial_policy': private}]})
    post('alice', 'commands', edit(source, 1, 1, 'Alias source'))
    post('alice', 'access-commands', {'commands': [
        {'kind': 'grant', 'id': source, 'expected_version': 1, 'account_id': accounts['bob'], 'edit': False},
        {'kind': 'revoke', 'id': source, 'expected_version': 2, 'account_id': accounts['bob']}]})
    post('alice', 'access-commands', {'commands': [
        {'kind': 'grant', 'id': target, 'expected_version': 1, 'account_id': accounts['bob'], 'edit': False}]})
    captured = counters('alice', source)
    if captured != (2, 3) or counters('alice', target) != (1, 2) or counters('bob', source) is not None:
        raise RuntimeError('Alias fixture is not in the expected state')

    preview = call('POST', base + 'people/merge-preview', tokens['alice'],
                   {'source_id': source, 'target_id': target})
    call('POST', base + 'people-commands', tokens['alice'], {'command': {
        'kind': 'merge', 'source_id': source, 'target_id': target,
        'preview_token': preview['token'], 'name': 'Alias target'}}, str(uuid.uuid4()))
    if counters('alice', target) != captured or counters('bob', target) is None:
        raise RuntimeError('Merge did not produce colliding counters that bob can read')

    # The unsent draft, addressed to the old ID with counters equal to the target's, is refused.
    rejected = post('alice', 'commands', edit(source, captured[0], captured[1], 'Unsent text'), 409)
    if rejected['code'] != 'conflict':
        raise RuntimeError('Alias edit was not reported as a conflict: ' + rejected['code'])
    for who in ('alice', 'bob'):
        if read_resource(call, accounts, tokens, who, target)['label'] != 'Alias target':
            raise RuntimeError('A rejected alias edit changed the canonical person')
    if call('GET', base + 'people/' + source, tokens['alice'])['person']['label'] != 'Alias target':
        raise RuntimeError('The old ID no longer resolves to the unchanged canonical person')

    # After reading the canonical person, an edit addressed to it commits and reaches bob.
    version, revision = counters('alice', target)
    post('alice', 'commands', edit(target, version, revision, 'Reviewed text'))
    if read_resource(call, accounts, tokens, 'bob', target)['label'] != 'Reviewed text':
        raise RuntimeError('An edit of the canonical person did not reach bob')

def policy_precondition(call, accounts, tokens, lanes=None, rounds=20):
    """A collaborator's writes race the owner's sharing changes over real HTTP.

    Three accounts: alice owns a person and a field, bob and carol may both edit the field.
    The owner then narrows the field's sharing so that carol loses it and bob keeps edit
    access, which is a genuinely smaller audience with the collaborator still authorised.

    Part 1 is serial and asserts exact outcomes. When the caller can tell which server
    process served a request (the replica harness alternates them), it also returns which
    ones handled the sharing change and the stale write. Part 2 issues the narrowing and
    the collaborator's save at the same instant from two independent clients, `lanes`, and
    checks that each answer describes what was stored. Which of the two commits first is
    not controlled there, so both orders are accepted and counted; the ordered proof is the
    held-gate test in the core crate.

    Uses its own people and fields so the version arithmetic of the other steps is untouched."""
    base = '/api/experimental/v1/'

    def post(who, route, body, expected=200, operation=None):
        return call('POST', base + route, tokens[who], body, operation or str(uuid.uuid4()), expected)

    def read(who, field):
        return read_resource(call, accounts, tokens, who, field)

    def edit(field, version, revision, label, value):
        command = {'kind': 'edit', 'id': field, 'expected_version': version,
                   'label': label, 'value': value}
        if revision is not None:
            command['expected_policy_version'] = revision
        return {'commands': [command]}

    def put(person, field, version, revision, text):
        command = {'kind': 'put_field', 'id': field, 'parent_id': person, 'expected_version': version,
                   'label': 'Policy field', 'value': {'kind': 'text', 'text': text}, 'initial_policy': None}
        if revision is not None:
            command['expected_policy_version'] = revision
        return {'command': command}

    def narrowing(field, revision):
        # Carol is removed; bob stays an editor. Replacing a Bob-only policy with itself would
        # advance the revision without changing who can see the field, which proves less.
        return {'commands': [{'kind': 'replace_policy', 'id': field, 'expected_version': revision,
            'policy': {'grants': [{'kind': 'account', 'id': accounts['bob'], 'edit': True}],
                       'exclude_accounts': []}}]}

    def conflict(who, route, body, operation=None):
        rejected = post(who, route, body, 409, operation)
        if rejected['code'] != 'conflict':
            raise RuntimeError('Stale write was not reported as a conflict: ' + rejected['code'])

    def share_with_two_editors():
        person, field = str(uuid.uuid4()), str(uuid.uuid4())
        post('alice', 'commands', {'commands': [
            {'kind': 'create_person', 'id': person, 'name': 'Policy person'},
            {'kind': 'create_field', 'id': field, 'person_id': person, 'label': 'Policy field', 'value': 'one'}]})
        post('alice', 'access-commands', {'commands': [
            {'kind': 'grant', 'id': person, 'expected_version': 1, 'account_id': accounts['bob'], 'edit': False},
            {'kind': 'grant', 'id': person, 'expected_version': 2, 'account_id': accounts['carol'], 'edit': False},
            {'kind': 'grant', 'id': field, 'expected_version': 1, 'account_id': accounts['bob'], 'edit': True},
            {'kind': 'grant', 'id': field, 'expected_version': 2, 'account_id': accounts['carol'], 'edit': True}]})
        return person, field

    # Part 1: serial, exact outcomes.
    person, field = share_with_two_editors()

    # A non-owner collaborator can obtain the revision, but not the policy behind it.
    seen = read('bob', field)
    if seen is None or not isinstance(seen.get('policy_version'), int):
        raise RuntimeError('Non-owner reader did not receive policy_version')
    if read('carol', field) is None:
        raise RuntimeError('Carol should hold the field before the narrowing')
    call('GET', base + 'policies/' + field, tokens['bob'], expected=403)
    post('bob', 'commands', edit(field, seen['version'], seen['policy_version'], 'Policy field', 'two'))

    # The owner narrows sharing from another session; bob remains an editor and carol loses it.
    post('alice', 'management-commands', narrowing(field, seen['policy_version']))
    changed_by = getattr(call, 'process', None)
    if read('carol', field) is not None:
        raise RuntimeError('The narrowing did not remove the field from carol')
    kept = read('bob', field)
    if kept is None or not kept['can_edit']:
        raise RuntimeError('The narrowing removed the collaborator or their edit access')

    # Generic and typed writes made against the old sharing are rejected atomically,
    # as is one that omits the revision altogether.
    retry = str(uuid.uuid4())
    conflict('bob', 'commands', edit(field, seen['version'] + 1, seen['policy_version'], 'Policy field', 'stale'), retry)
    rejected_by = getattr(call, 'process', None)
    conflict('bob', 'commands', edit(field, seen['version'] + 1, None, 'Policy field', 'omitted'))
    conflict('bob', 'task-commands', put(person, field, seen['version'] + 1, seen['policy_version'], 'stale'))
    conflict('bob', 'task-commands', put(person, field, seen['version'] + 1, None, 'omitted'))
    # The owner is gated as well.
    conflict('alice', 'commands', edit(field, seen['version'] + 1, seen['policy_version'], 'Policy field', 'stale'))

    # After refetching, the same operation ID commits the corrected write once.
    now = read('bob', field)
    if now['policy_version'] != seen['policy_version'] + 1 or now['version'] != seen['version'] + 1:
        raise RuntimeError('Refetch did not show the new sharing revision and the unchanged content')
    if now['value'].get('text') != 'two':
        raise RuntimeError('A rejected write changed content')
    post('bob', 'commands', edit(field, now['version'], now['policy_version'], 'Policy field', 'three'), operation=retry)
    post('bob', 'task-commands', put(person, field, now['version'] + 1, now['policy_version'], 'four'))
    owner = read('alice', field)
    post('alice', 'commands', edit(field, owner['version'], owner['policy_version'], 'Policy field', 'five'))
    final = read('bob', field)
    if final['value'].get('text') != 'five' or final['policy_version'] != now['policy_version']:
        raise RuntimeError('Content edits changed the sharing revision or were lost')

    # Part 2: the narrowing and the collaborator's save of an existing field overlap.
    outcome = {'policy_change_process': changed_by, 'stale_write_process': rejected_by}
    if lanes is None:
        return outcome
    owner_lane, collaborator_lane = lanes
    saved = rejected = 0
    for round_number in range(rounds):
        person, field = share_with_two_editors()
        seen = read('bob', field)
        save_operation = str(uuid.uuid4())
        gate = threading.Barrier(2)

        def narrow():
            gate.wait()
            return owner_lane('POST', base + 'management-commands', tokens['alice'],
                              narrowing(field, seen['policy_version']), str(uuid.uuid4()), 200)

        def save():
            gate.wait()
            body = collaborator_lane('POST', base + 'task-commands', tokens['bob'],
                                     put(person, field, seen['version'], seen['policy_version'], 'Saved'),
                                     save_operation, {200, 409})
            return collaborator_lane.last_status, body

        with concurrent.futures.ThreadPoolExecutor(max_workers=2) as pool:
            narrowed, answered = pool.submit(narrow), pool.submit(save)
            narrowed.result()
            status, body = answered.result()
        after = read('bob', field)
        if after is None or not after['can_edit'] or after['policy_version'] != seen['policy_version'] + 1:
            raise RuntimeError(f'Round {round_number}: the narrowing was lost or the collaborator removed')
        if read('carol', field) is not None:
            raise RuntimeError(f'Round {round_number}: carol kept the field')
        if status == 200:
            saved += 1
            if (after['version'], after['value'].get('text')) != (seen['version'] + 1, 'Saved'):
                raise RuntimeError(f'Round {round_number}: an accepted save is not what was stored')
        else:
            rejected += 1
            if body['code'] != 'conflict':
                raise RuntimeError(f'Round {round_number}: rejection was {body["code"]}')
            if (after['version'], after['value'].get('text')) != (seen['version'], 'one'):
                raise RuntimeError(f'Round {round_number}: a rejected save changed content')
            # A rejected write left no receipt: the same operation ID commits once refetched.
            post('bob', 'task-commands', put(person, field, after['version'], after['policy_version'], 'Saved'),
                 operation=save_operation)
    outcome.update({'overlap_rounds': rounds, 'saved_before_narrowing': saved,
                    'rejected_after_narrowing': rejected})
    return outcome
