import datetime
import json
import time
import uuid
from jsonschema import Draft202012Validator, FormatChecker

def run(call, accounts, tokens, document):
    task, execution = str(uuid.uuid4()), str(uuid.uuid4())
    today = datetime.datetime.now(datetime.timezone.utc).date().isoformat()
    definition = {'schedule':{'start_date':today,'time':None,'timezone':'UTC','repeat':None},
        'goal':{'kind':'checkbox'},'carry':'retain_one','participation':'personal',
        'open_days_before':0,'close_days_after':1,'allow_streak_exclusions':False}
    task_create = {'command':{'kind':'create_task','id':task,'execution_id':execution,
        'title':'PRIVATE_LOG_SENTINEL','definition':definition,'initial_policy':{'grants':[],'exclude_accounts':[]}}}
    op = str(uuid.uuid4())
    result = call('POST','/api/experimental/v1/task-commands',tokens['alice'],task_create,op)
    assert call('POST','/api/experimental/v1/task-commands',tokens['alice'],task_create,op) == result
    call('GET','/api/experimental/v1/tasks/'+task,tokens['bob'],expected=404)
    detail = call('GET','/api/experimental/v1/tasks/'+task,tokens['alice'])
    assert detail['materialisation']['pending'] is False
    occurrence = str(uuid.uuid5(uuid.UUID(task),'atlas-occurrence-v1:once'))
    progress = str(uuid.uuid5(uuid.UUID(occurrence),'atlas-progress-v1:'+accounts['alice']))
    call('GET','/api/experimental/v1/tasks/'+task+'/enrolment',tokens['alice'])
    record = {'command':{'kind':'record','occurrence_id':occurrence,'subject_account_id':accounts['alice'],
        'entry_id':str(uuid.uuid4()),'evidence':{'kind':'checkbox','complete':True},
        'expected_version':1,'happened_at':int(time.time())}}
    call('POST','/api/experimental/v1/task-commands',tokens['alice'],record,str(uuid.uuid4()))
    daily = call('GET','/api/experimental/v1/daily?day='+today+'&timezone=UTC',tokens['alice'])
    assert next(v for v in daily['items'] if v['id']==occurrence)['outcome']=='complete'
    call('GET','/api/experimental/v1/daily',tokens['alice'],expected=422)
    assert call('GET','/api/experimental/v1/tasks/'+task+'/streak?scope=personal',tokens['alice'])['current']==1
    journal = call('GET','/api/experimental/v1/progress/'+progress+'/entries',tokens['alice'])
    assert len(journal['entries'])==1
    call('GET','/api/experimental/v1/progress/'+progress+'/entries',tokens['bob'],expected=404)
    progress_page = call('GET','/api/experimental/v1/progress?parent_id='+occurrence,tokens['alice'])
    for resource in progress_page['items']:
        progress_state = resource['value']
        Draft202012Validator({**document,'$ref':'#/components/schemas/ProgressState'},format_checker=FormatChecker()).validate(progress_state)
        assert progress_state['aggregate_consent'] is True
    call('GET','/api/experimental/v1/occurrences?task_id='+task,tokens['alice'])
    shared_task = str(uuid.uuid4())
    shared_definition = {**definition,'participation':'anyone'}
    shared_create = {'command':{'kind':'create_task','id':shared_task,'execution_id':str(uuid.uuid4()),
        'title':'Shared chore','definition':shared_definition,'initial_policy':{
            'grants':[{'kind':'account','id':accounts['bob'],'edit':True}],'exclude_accounts':[]}}}
    shared_op = str(uuid.uuid4())
    call('POST','/api/experimental/v1/task-commands',tokens['alice'],shared_create,shared_op,expected=422)
    call('POST','/api/experimental/v1/task-access-commands',tokens['alice'],shared_create,shared_op)
    call('GET','/api/experimental/v1/tasks/'+shared_task,tokens['bob'])
    shared_occurrence = str(uuid.uuid5(uuid.UUID(shared_task),'atlas-occurrence-v1:once'))
    call('POST','/api/experimental/v1/task-commands',tokens['bob'],{'command':{
        'kind':'record','occurrence_id':shared_occurrence,'subject_account_id':accounts['bob'],
        'entry_id':str(uuid.uuid4()),'evidence':{'kind':'checkbox','complete':True},
        'expected_version':1,'happened_at':int(time.time())}},str(uuid.uuid4()))
    assert call('GET','/api/experimental/v1/occurrences?task_id='+shared_task,tokens['bob'])['items'][0]['outcome']=='complete'
    field_id,list_id = str(uuid.uuid4()),str(uuid.uuid4())
    for cmd in [
        {'kind':'put_field','id':field_id,'parent_id':task,'label':'Target','value':{'kind':'quantity','amount':'3','unit':'sessions'}},
        {'kind':'create_list','id':list_id,'name':'Today'},
        {'kind':'list_item','list_id':list_id,'expected_version':1,'task_id':task,'included':True}]:
        call('POST','/api/experimental/v1/task-commands',tokens['alice'],{'command':cmd},str(uuid.uuid4()))
    for path in ['tasks','lists','people','fields?parent_id='+task]:
        call('GET','/api/experimental/v1/'+path,tokens['alice'])
    # M5: live requests and responses are checked against OpenAPI above.
    def workflow(command, actor='alice', endpoint='task-commands'):
        return call('POST','/api/experimental/v1/'+endpoint,tokens[actor],
                    {'command':command},str(uuid.uuid4()))
    presets = call('GET','/api/experimental/v1/task-presets?start_date='+today+'&timezone=UTC',tokens['alice'])
    assert len(presets['items']) == 6
    for command in [
        {'kind':'rota_consent','task_id':shared_task,'expected_version':0,'accepted':True},
        {'kind':'set_rota','task_id':shared_task,'expected_version':0,'participants':[accounts['alice']]}]:
        workflow(command,endpoint='task-access-commands')
    assert call('GET','/api/experimental/v1/tasks/'+shared_task+'/rota',tokens['alice'])['version'] == 1
    # Bob has no pre-created stream on these Anyone tasks. Both cascade
    # and timer start must support the same participation as Record.
    workflow_occurrences = []
    for goal in [{'kind':'checkbox'}, {'kind':'numeric','minimum':'60','maximum':None,'unit':'seconds'}]:
        workflow_task = str(uuid.uuid4())
        workflow({'kind':'create_task','id':workflow_task,'execution_id':str(uuid.uuid4()),
            'title':'Workflow','definition':{**shared_definition,'schedule':{**definition['schedule'],'start_date':None},'goal':goal},
            'initial_policy':{'grants':[{'kind':'account','id':accounts['bob'],'edit':True}],'exclude_accounts':[]}},endpoint='task-access-commands')
        workflow_occurrences.append(str(uuid.uuid5(uuid.UUID(workflow_task),'atlas-occurrence-v1:once')))
    prerequisite, timed = workflow_occurrences
    workflow({'kind':'set_dependencies','occurrence_id':prerequisite,'expected_version':1,
        'prerequisites':[],'strict':False})
    preview = call('GET','/api/experimental/v1/occurrences/'+prerequisite+'/dependencies',tokens['bob'])
    assert preview['items'][0]['can_complete'] is True
    workflow({'kind':'complete_dependencies','occurrence_id':prerequisite,'preview_token':preview['token'],
        'mode':'complete_prerequisites','happened_at':int(time.time())},actor='bob')
    timer = str(uuid.uuid4())
    workflow({'kind':'start_timer','occurrence_id':timed,'session_id':timer,'started_at':int(time.time())-120},actor='bob')
    workflow({'kind':'stop_timer','occurrence_id':timed,'session_id':timer,'expected_version':1,'stopped_at':int(time.time())},actor='bob')
    assert len(call('GET','/api/experimental/v1/occurrences/'+timed+'/timers',tokens['bob'])['items']) == 1
    return workflow
