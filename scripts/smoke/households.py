import base64
import concurrent.futures
import datetime
import hashlib
import json
import threading
import time
import uuid
from jsonschema import Draft202012Validator, FormatChecker

def _verifier():
    """A fresh RFC 7636 verifier and its S256 challenge (BE-Q19 activation grants)."""
    verifier = uuid.uuid4().hex + uuid.uuid4().hex
    challenge = base64.urlsafe_b64encode(hashlib.sha256(verifier.encode()).digest()).rstrip(b'=').decode()
    return verifier, challenge

SNAPSHOT = '/api/experimental/v1/defaults/snapshot'

def _canonical(household):
    """A household with its members in account-id order, so two projections compare equal."""
    return {**household, 'members': sorted(household['members'], key=lambda member: member['account_id'])}

def _roles(household):
    return {member['account_id']: member['role'] for member in household['members']}

def snapshot_race(lanes, tokens, household, member, turns=40):
    """A writer toggles one member's role while readers, each with its own client, read the snapshot.

    Every revision must always come back with the same households, and the readers must have seen
    more than one revision. This is a floor: it can fail only if a tear happens to be sampled. The
    deterministic proof is the paused-read schedule in the Rust tests.
    """
    writer, readers = lanes[0], lanes[1:]
    stop = threading.Event()
    def read(client):
        seen, versions = {}, []
        while True:
            finished = stop.is_set()
            body = client('GET', SNAPSHOT, tokens['alice'])
            listed = [_canonical(h) for h in body['households']]
            revision = body['defaults']['revision']
            if seen.setdefault(revision, listed) != listed:
                raise RuntimeError('One snapshot revision was returned with two different sets of households')
            primary = body['defaults']['primary_household_id']
            if primary is not None and primary not in [h['id'] for h in listed]:
                raise RuntimeError('A snapshot named a primary household it did not list')
            versions.append(next(h['version'] for h in listed if h['id'] == household))
            if finished:
                if versions != sorted(versions):
                    raise RuntimeError('A reader saw a household version go backwards')
                return seen
    def write():
        try:
            for turn in range(turns):
                version = next(h['version'] for h in writer('GET', '/api/experimental/v1/households', tokens['alice'])
                               if h['id'] == household)
                writer('POST', '/api/experimental/v1/management-commands', tokens['alice'],
                       {'commands':[{'kind':'set_household_role','household_id':household,'account_id':member,
                                     'expected_version':version,'manager':turn % 2 == 0}]}, str(uuid.uuid4()))
        finally:
            stop.set()
    with concurrent.futures.ThreadPoolExecutor(max_workers=len(readers) + 1) as pool:
        futures = [pool.submit(read, client) for client in readers]
        pool.submit(write).result()
        merged = {}
        for future in futures:
            for revision, listed in future.result().items():
                if merged.setdefault(revision, listed) != listed:
                    raise RuntimeError('Two readers saw one snapshot revision with different households')
    if len(merged) < 2:
        raise RuntimeError(f'Snapshot readers saw {len(merged)} revision(s): the writer never overlapped them')
    return {'scenario': 'sharing-snapshot-race', 'role_changes': turns, 'revisions_observed': len(merged)}

def run(call, accounts, tokens, secrets, person, field, lanes):
    """`lanes` are at least four independent clients: one writer and the readers of the snapshot race."""
    household = str(uuid.uuid4())
    def manage(token, command, expected=200):
        return call('POST', '/api/experimental/v1/management-commands', token,
                    {'commands':[command]}, str(uuid.uuid4()), expected)
    manage(tokens['alice'], {'kind':'create_household','id':household,'name':'Home'})
    homes = call('GET', '/api/experimental/v1/households', tokens['alice'])
    if homes[0]['members'][0]['account_id'] != accounts['alice']:
        raise RuntimeError('Creator was not added as a household member')
    defaults = call('GET', '/api/experimental/v1/defaults', tokens['alice'])
    # BE-B5: the snapshot is exactly what the two separate reads say for the same state.
    created = call('GET', SNAPSHOT, tokens['alice'])
    if created['defaults'] != defaults or [_canonical(h) for h in created['households']] != [_canonical(h) for h in homes]:
        raise RuntimeError('The sharing snapshot disagreed with GET /defaults and GET /households')
    household_person = str(uuid.uuid4())
    call('POST', '/api/experimental/v1/commands', tokens['alice'],
         {'defaults_revision':defaults['revision'],'commands':[
             {'kind':'create_person','id':household_person,'name':'Household friend'}]}, str(uuid.uuid4()))
    call('GET', '/api/experimental/v1/defaults/templates', tokens['alice'])
    call('GET', '/api/experimental/v1/defaults/templates?household_id='+household, tokens['alice'])
    invitation = str(uuid.uuid4())
    manage(tokens['alice'], {'kind':'invite_to_household','id':invitation,
           'household_id':household,'recipient_id':accounts['bob'],'expected_version':homes[0]['version']})
    pending = call('GET', '/api/experimental/v1/invitations', tokens['bob'])
    if pending[0]['id'] != invitation:
        raise RuntimeError('Invitation was not delivered to its recipient')
    manage(tokens['alice'], {'kind':'respond_to_household_invitation','id':invitation,
           'expected_version':1,'accept':True}, 404)
    manage(tokens['bob'], {'kind':'respond_to_household_invitation','id':invitation,
           'expected_version':1,'accept':True})
    joined = call('GET', SNAPSHOT, tokens['alice'])
    if _roles(joined['households'][0]) != {accounts['alice']: 'manager', accounts['bob']: 'member'}:
        raise RuntimeError('The snapshot did not list the household with both members after the invitation was accepted')
    if joined['defaults']['revision'] == created['defaults']['revision']:
        raise RuntimeError('A membership change did not move the defaults revision')
    if joined['defaults']['revision'] != call('GET', '/api/experimental/v1/defaults', tokens['alice'])['revision']:
        raise RuntimeError('The snapshot revision differs from GET /defaults for the same state')
    bobs = call('GET', SNAPSHOT, tokens['bob'])
    if [h['id'] for h in bobs['households']] != [household] or bobs['households'][0]['role'] != 'member':
        raise RuntimeError('The new member did not see their household, as a member')
    # BE-Q16: durable, all-states sent-invitation history, keyed by sender.
    revoked_invitation = str(uuid.uuid4())
    manage(tokens['alice'], {'kind':'invite_to_household','id':revoked_invitation,
           'household_id':household,'recipient_id':accounts['carol'],
           'expected_version':call('GET', '/api/experimental/v1/households', tokens['alice'])[0]['version']})
    manage(tokens['alice'], {'kind':'revoke_household_invitation','id':revoked_invitation,
           'expected_version':1})
    sent = call('GET', '/api/experimental/v1/invitations/sent', tokens['alice'])
    sent_by_id = {item['id']: item for item in sent['items']}
    if sent_by_id.get(invitation, {}).get('status') != 'accepted':
        raise RuntimeError('Sent-invitation history did not report the accepted invitation')
    if sent_by_id.get(revoked_invitation, {}).get('status') != 'revoked':
        raise RuntimeError('Sent-invitation history did not report the revoked invitation')
    # BE-Q16 R2: the whole point of this read is that a fresh device with no local record of
    # what was sent can still identify and label each recipient.
    if sent_by_id[invitation]['recipient_id'] != accounts['bob']:
        raise RuntimeError('Sent-invitation history did not identify the accepted recipient')
    if sent_by_id[invitation]['recipient_username'] != 'bob':
        raise RuntimeError('Sent-invitation history did not name the accepted recipient')
    if sent_by_id[revoked_invitation]['recipient_id'] != accounts['carol']:
        raise RuntimeError('Sent-invitation history did not identify the revoked recipient')
    if sent_by_id[revoked_invitation]['recipient_username'] != 'carol':
        raise RuntimeError('Sent-invitation history did not name the revoked recipient')
    call('GET', '/api/experimental/v1/invitations/sent?limit=1', tokens['alice'])
    page = call('GET', '/api/experimental/v1/sync', tokens['bob'])
    visible_ids = {change['resource']['id'] for batch in page['batches'] for change in batch['changes']}
    if household_person not in visible_ids or person in visible_ids or field in visible_ids:
        raise RuntimeError('Joining changed the wrong resource audiences')
    call('GET', '/api/experimental/v1/policies/'+household_person, tokens['alice'])
    call('GET', '/api/experimental/v1/policies/'+household_person, tokens['bob'], expected=403)
    defaults = call('GET', '/api/experimental/v1/defaults', tokens['alice'])
    manage(tokens['alice'], {'kind':'set_defaults','household_id':None,
           'resource_kind':'person','expected_version':0,'template':{'kind':'private'}})
    draft = {'defaults_revision':defaults['revision'],'commands':[
        {'kind':'create_person','id':str(uuid.uuid4()),'name':'Offline draft'}]}
    call('POST', '/api/experimental/v1/commands', tokens['alice'], draft, str(uuid.uuid4()), 409)
    explicit = {'commands':[{'kind':'create_person','id':str(uuid.uuid4()),'name':'Private from creation',
                'initial_policy':{'grants':[],'exclude_accounts':[]}}]}
    call('POST', '/api/experimental/v1/commands', tokens['alice'], explicit, str(uuid.uuid4()))
    call('GET', '/api/experimental/v1/directory', tokens['alice'])
    version = call('GET', '/api/experimental/v1/households', tokens['alice'])[0]['version']
    manage(tokens['alice'], {'kind':'remove_household_member','household_id':household,
           'account_id':accounts['bob'],'expected_version':version})
    gone = call('GET', SNAPSHOT, tokens['bob'])
    if gone['households'] != [] or gone['defaults']['primary_household_id'] is not None:
        raise RuntimeError('A removed member still saw the household in their snapshot')
    removed = call('GET', '/api/experimental/v1/sync?cursor='+page['next_cursor'], tokens['bob'])
    if not any(change == {'kind':'remove','id':household_person} for batch in removed['batches'] for change in batch['changes']):
        raise RuntimeError('Leaving failed to remove household-derived visibility')
    signup = call('POST', '/api/experimental/v1/account-invitations', tokens['alice'],
        {'household_id':household})
    secrets.append(signup['token'])
    call('GET', '/api/experimental/v1/account-invitations', tokens['alice'])
    preview = call('POST', '/api/experimental/v1/registration/preview', body={'token':signup['token']})
    if preview['household']['id'] != household:
        raise RuntimeError('Signup preview named the wrong household')
    before_signup = call('GET', SNAPSHOT, tokens['alice'])
    verifier, challenge = _verifier()
    granted = call('POST', '/api/experimental/v1/browser-registration', body={
        'token':signup['token'],'username':'charlie','password':secrets[0],'device_id':'browser',
        'attempt_challenge':challenge},
        extra_headers={'Origin':'https://atlas.example'})
    if 'set-cookie' in call.last_headers:
        raise RuntimeError('browser-registration set a cookie')
    registered = call('POST', '/api/experimental/v1/browser-sessions/activate', body={
        'grant':granted['grant'],'verifier':verifier},
        extra_headers={'Origin':'https://atlas.example'})
    secrets.append(registered['csrf_token'])
    secrets.append(call.last_headers['set-cookie'].split(';')[0].split('=',1)[1])
    # BE-B5: signing up into a household is a membership change like any other.
    after_signup = call('GET', SNAPSHOT, tokens['alice'])
    if _roles(after_signup['households'][0]) != {accounts['alice']: 'manager', registered['account_id']: 'member'}:
        raise RuntimeError('The snapshot did not list the household member who signed up')
    if after_signup['households'][0]['version'] != before_signup['households'][0]['version'] + 1:
        raise RuntimeError('Signing up into a household did not advance its version by one')
    if after_signup['defaults']['revision'] == before_signup['defaults']['revision']:
        raise RuntimeError('Signing up into a household did not move the defaults revision')
    print(json.dumps(snapshot_race(lanes, tokens, household, registered['account_id'])))
    call('POST', '/api/experimental/v1/registration', body={
        'token':signup['token'],'username':'replay','password':secrets[0],'device_id':'phone'}, expected=401)
    unused = call('POST', '/api/experimental/v1/account-invitations', tokens['alice'], {'household_id':household})
    secrets.append(unused['token'])
    call('DELETE', '/api/experimental/v1/account-invitations/'+unused['id'], tokens['alice'], expected=204)
    verifier, challenge = _verifier()
    grant = call('POST', '/api/experimental/v1/browser-sessions', body={
        'username':'alice','password':secrets[0],'device_id':'browser',
        'attempt_challenge':challenge},
        extra_headers={'Origin':'https://atlas.example'})
    if 'set-cookie' in call.last_headers:
        raise RuntimeError('browser-sessions login set a cookie')
    browser = call('POST', '/api/experimental/v1/browser-sessions/activate', body={
        'grant':grant['grant'],'verifier':verifier},
        extra_headers={'Origin':'https://atlas.example'})
    cookie = call.last_headers['set-cookie'].split(';')[0]
    secrets.extend([cookie.split('=',1)[1], browser['csrf_token']])
    browser_headers = {'Cookie':cookie, 'X-CSRF-Token':browser['csrf_token']}
    restored = call('GET', '/api/experimental/v1/browser-sessions/current',
        extra_headers={'Cookie':cookie,'X-Atlas-Session':'1'})
    if restored != browser:
        raise RuntimeError('Browser session reload changed identity')
    call('GET', '/api/experimental/v1/sync', expected=403, extra_headers={'Cookie':cookie})
    call('GET', '/api/experimental/v1/sync', extra_headers=browser_headers)
    devices = call('GET', '/api/experimental/v1/devices', tokens['alice'])['devices']
    assert any(item['id']=='browser' for item in devices)
    # A device with nothing left is `superseded` (recorded, not applied); the token is not compared.
    ghost = call('DELETE', '/api/experimental/v1/devices/unused-device', tokens['alice'],
        operation=str(uuid.uuid4()), extra_headers={'Atlas-Device-State': 'v1.' + 'A' * 43})
    assert ghost['outcome'] == 'superseded'
    sessions = call('GET', '/api/experimental/v1/sessions', tokens['alice'])['sessions']
    browser_id = next(item['id'] for item in sessions if item['device_id']=='browser')
    call('DELETE', '/api/experimental/v1/sessions/'+browser_id, tokens['alice'], expected=204)
    call('GET', '/api/experimental/v1/sync', expected=401, extra_headers=browser_headers)
