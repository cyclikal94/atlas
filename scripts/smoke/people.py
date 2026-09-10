import datetime
import json
import time
import uuid
from jsonschema import Draft202012Validator, FormatChecker

def run(call, accounts, tokens, workflow):
    # Merge visible duplicates; the old identity remains an authorised alias.
    duplicate_ids = [str(uuid.uuid4()),str(uuid.uuid4())]
    call('POST','/api/experimental/v1/commands',tokens['alice'],{'commands':[
        {'kind':'create_person','id':p,'name':'Merge Example'} for p in duplicate_ids]},str(uuid.uuid4()))
    candidates = call('GET','/api/experimental/v1/people/'+duplicate_ids[0]+'/duplicates',tokens['alice'])
    assert any(p['id']==duplicate_ids[1] for p in candidates['items'])
    preview = call('POST','/api/experimental/v1/people/merge-preview',tokens['alice'],
        {'source_id':duplicate_ids[0],'target_id':duplicate_ids[1]})
    workflow({'kind':'merge','source_id':duplicate_ids[0],'target_id':duplicate_ids[1],
        'preview_token':preview['token'],'name':'Merge Example'},endpoint='people-commands')
    assert call('GET','/api/experimental/v1/people/'+duplicate_ids[0],tokens['alice'])['person']['id'] == duplicate_ids[1]
    # The subject explicitly accepts linking before gaining profile access.
    link_id = str(uuid.uuid4())
    detail = call('GET','/api/experimental/v1/people/'+duplicate_ids[1],tokens['alice'])
    workflow({'kind':'request_link','id':link_id,'person_id':duplicate_ids[1],
        'account_id':accounts['bob'],'expected_version':detail['person']['version']},endpoint='people-commands')
    requests = call('GET','/api/experimental/v1/people/requests',tokens['bob'])
    assert any(r['id']==link_id for r in requests['items'])
    workflow({'kind':'respond_request','id':link_id,'accept':True},actor='bob',endpoint='people-commands')
    assert call('GET','/api/experimental/v1/people/'+duplicate_ids[1],tokens['bob'])['linked_account_id'] == accounts['bob']
    reference = workflow({'kind':'reference_account','account_id':accounts['bob'],
        'person_id':str(uuid.uuid4())},endpoint='people-commands')
    assert reference['person_id'] == duplicate_ids[1]
