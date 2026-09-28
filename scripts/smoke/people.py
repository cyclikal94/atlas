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

    # BE-Q16: the recipient of a merge request can preview it even though they cannot see the
    # other, hidden side, and their acceptance must echo a fresh recipient-side preview token.
    hidden_source = str(uuid.uuid4())
    call('POST','/api/experimental/v1/commands',tokens['alice'],{'commands':[
        {'kind':'create_person','id':hidden_source,'name':'Q16 Source'}]},str(uuid.uuid4()))
    visible_target = str(uuid.uuid4())
    call('POST','/api/experimental/v1/access-commands',tokens['bob'],{'commands':[
        {'kind':'create_person','id':visible_target,'name':'Q16 Target',
         'initial_policy':{'grants':[{'kind':'account','id':accounts['alice'],'edit':True}],
                            'exclude_accounts':[]}}]},str(uuid.uuid4()))
    q16_preview = call('POST','/api/experimental/v1/people/merge-preview',tokens['alice'],
        {'source_id':hidden_source,'target_id':visible_target})
    assert q16_preview['requires_approval']
    q16_request = str(uuid.uuid4())
    workflow({'kind':'request_merge','id':q16_request,'source_id':hidden_source,
        'target_id':visible_target,'preview_token':q16_preview['token'],
        'name':'Q16 Merged'},endpoint='people-commands')
    recipient_preview = call('GET',
        '/api/experimental/v1/people/requests/'+q16_request+'/merge-preview',tokens['bob'])
    if recipient_preview['source'] is not None:
        raise RuntimeError('Recipient should not see the sender-only identity')
    if recipient_preview['target'] is None or recipient_preview['target']['id'] != visible_target:
        raise RuntimeError('Recipient should see the identity they own')
    if recipient_preview['stale']:
        raise RuntimeError('A freshly-read preview must not be stale')
    workflow({'kind':'respond_request','id':q16_request,'accept':True,
        'recipient_preview_token':recipient_preview['token']},actor='bob',endpoint='people-commands')
    if call('GET','/api/experimental/v1/people/'+hidden_source,
            tokens['alice'])['person']['id'] != visible_target:
        raise RuntimeError('Accepted merge did not commit')
    # A stale/missing recipient token is rejected on a second, independent request.
    hidden_source2 = str(uuid.uuid4())
    call('POST','/api/experimental/v1/commands',tokens['alice'],{'commands':[
        {'kind':'create_person','id':hidden_source2,'name':'Q16 Source Two'}]},str(uuid.uuid4()))
    visible_target2 = str(uuid.uuid4())
    call('POST','/api/experimental/v1/access-commands',tokens['bob'],{'commands':[
        {'kind':'create_person','id':visible_target2,'name':'Q16 Target Two',
         'initial_policy':{'grants':[{'kind':'account','id':accounts['alice'],'edit':True}],
                            'exclude_accounts':[]}}]},str(uuid.uuid4()))
    preview2 = call('POST','/api/experimental/v1/people/merge-preview',tokens['alice'],
        {'source_id':hidden_source2,'target_id':visible_target2})
    request2 = str(uuid.uuid4())
    workflow({'kind':'request_merge','id':request2,'source_id':hidden_source2,
        'target_id':visible_target2,'preview_token':preview2['token'],
        'name':'Q16 Merged Two'},endpoint='people-commands')
    call('POST','/api/experimental/v1/people-commands',tokens['bob'],
        {'command':{'kind':'respond_request','id':request2,'accept':True}},
        str(uuid.uuid4()),expected=409)
    call('POST','/api/experimental/v1/people-commands',tokens['alice'],
        {'command':{'kind':'cancel_request','id':request2}},str(uuid.uuid4()))
    call('GET',
        '/api/experimental/v1/people/requests/'+request2+'/merge-preview',tokens['bob'],expected=404)

    # Durable sent-history: paginated, all-states, keyed by sender, naming the recipient (R2:
    # a fresh device with no local record of what was sent can still identify each recipient).
    sent = call('GET','/api/experimental/v1/people/requests/sent',tokens['alice'])
    sent_by_id = {item['id']: item for item in sent['items']}
    if sent_by_id.get(q16_request, {}).get('state') != 'accepted':
        raise RuntimeError('Sent-history did not report the accepted request')
    if sent_by_id.get(request2, {}).get('state') != 'cancelled':
        raise RuntimeError('Sent-history did not report the withdrawn request')
    if sent_by_id[q16_request]['recipient_id'] != accounts['bob']:
        raise RuntimeError('Sent-history did not identify the recipient')
    if sent_by_id[q16_request]['recipient_username'] != 'bob':
        raise RuntimeError('Sent-history did not name the recipient')
    call('GET','/api/experimental/v1/people/requests/sent?limit=1',tokens['alice'])

    # R1: a receipt committed under the pre-existing wire format (no `recipient_preview_token`
    # in the request body at all, not merely `null`) must still replay its stored result,
    # rather than failing with `409 operation_conflict`, once the schema gains that field.
    legacy_person = str(uuid.uuid4())
    call('POST','/api/experimental/v1/commands',tokens['alice'],{'commands':[
        {'kind':'create_person','id':legacy_person,'name':'Legacy Wire Format'}]},str(uuid.uuid4()))
    legacy_link = str(uuid.uuid4())
    legacy_detail = call('GET','/api/experimental/v1/people/'+legacy_person,tokens['alice'])
    workflow({'kind':'request_link','id':legacy_link,'person_id':legacy_person,
        'account_id':accounts['bob'],'expected_version':legacy_detail['person']['version']},
        endpoint='people-commands')
    legacy_operation = str(uuid.uuid4())
    legacy_body = {'command':{'kind':'respond_request','id':legacy_link,'accept':True}}
    first = call('POST','/api/experimental/v1/people-commands',tokens['bob'],
        legacy_body,legacy_operation)
    replay = call('POST','/api/experimental/v1/people-commands',tokens['bob'],
        legacy_body,legacy_operation)
    if replay != first:
        raise RuntimeError('Legacy-wire-format replay did not return the original receipt')
