import datetime
import json
import time
import uuid
from jsonschema import Draft202012Validator, FormatChecker

def run(call, accounts, tokens, secrets, person, field):
    household = str(uuid.uuid4())
    def manage(token, command, expected=200):
        return call('POST', '/api/experimental/v1/management-commands', token,
                    {'commands':[command]}, str(uuid.uuid4()), expected)
    manage(tokens['alice'], {'kind':'create_household','id':household,'name':'Home'})
    homes = call('GET', '/api/experimental/v1/households', tokens['alice'])
    if homes[0]['members'][0]['account_id'] != accounts['alice']:
        raise RuntimeError('Creator was not added as a household member')
    defaults = call('GET', '/api/experimental/v1/defaults', tokens['alice'])
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
    registered = call('POST', '/api/experimental/v1/browser-registration', body={
        'token':signup['token'],'username':'charlie','password':secrets[0],'device_id':'browser'},
        extra_headers={'Origin':'https://atlas.example'})
    secrets.append(registered['csrf_token'])
    secrets.append(call.last_headers['set-cookie'].split(';')[0].split('=',1)[1])
    call('POST', '/api/experimental/v1/registration', body={
        'token':signup['token'],'username':'replay','password':secrets[0],'device_id':'phone'}, expected=401)
    unused = call('POST', '/api/experimental/v1/account-invitations', tokens['alice'], {'household_id':household})
    secrets.append(unused['token'])
    call('DELETE', '/api/experimental/v1/account-invitations/'+unused['id'], tokens['alice'], expected=204)
    browser = call('POST', '/api/experimental/v1/browser-sessions', body={
        'username':'alice','password':secrets[0],'device_id':'browser'},
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
    call('DELETE', '/api/experimental/v1/devices/unused-device', tokens['alice'], expected=204)
    sessions = call('GET', '/api/experimental/v1/sessions', tokens['alice'])['sessions']
    browser_id = next(item['id'] for item in sessions if item['device_id']=='browser')
    call('DELETE', '/api/experimental/v1/sessions/'+browser_id, tokens['alice'], expected=204)
    call('GET', '/api/experimental/v1/sync', expected=401, extra_headers=browser_headers)
