import datetime
import json
import time
import uuid
from jsonschema import Draft202012Validator, FormatChecker

def run(call, tokens, secrets):
    source = str(uuid.uuid4())
    call('POST', '/api/experimental/v1/calendar-commands', tokens['alice'], {'command':{
        'kind':'create_source','id':source,'label':'Calendar','timezone':'UTC'}}, str(uuid.uuid4()))
    calendar_import = {'ics':'BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VEVENT\r\nUID:smoke\r\nDTSTART:20260910T090000Z\r\nSUMMARY:PRIVATE_LOG_SENTINEL\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n', 'from':'2026-09-01','through':'2026-12-31'}
    import_op = str(uuid.uuid4())
    import_path = '/api/experimental/v1/calendar-sources/' + source + '/import'
    imported = call('POST', import_path, tokens['alice'], calendar_import, import_op)
    assert call('POST', import_path, tokens['alice'], calendar_import, import_op) == imported
    assert len(call('GET','/api/experimental/v1/events',tokens['alice'])['items']) == 1
    assert call('GET','/api/experimental/v1/events',tokens['bob'])['items'] == []
    call('GET','/api/experimental/v1/calendar-sources',tokens['alice'])
    call('GET','/api/experimental/v1/review-items',tokens['alice'])
    call('GET','/api/experimental/v1/reminders',tokens['alice'])
    call('GET','/api/experimental/v1/notification-capabilities',tokens['alice'])
    call('GET','/api/experimental/v1/notification-subscriptions',tokens['alice'])
    call('GET','/api/experimental/v1/notification-deliveries',tokens['alice'])

