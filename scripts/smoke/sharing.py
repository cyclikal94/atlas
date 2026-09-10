import datetime
import json
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
