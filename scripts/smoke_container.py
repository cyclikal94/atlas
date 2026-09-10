"""Check a locally built Atlas image using a disposable PostgreSQL database and loopback port."""
import json
import concurrent.futures
import statistics
import os
import subprocess
import sys
import time
import urllib.error
import urllib.request
import uuid

image = sys.argv[1] if len(sys.argv) > 1 else 'atlas-api:test'
suffix = uuid.uuid4().hex
volume = 'atlas-smoke-' + suffix
container = 'atlas-smoke-' + suffix
network = 'atlas-smoke-' + suffix
postgres = 'atlas-pg-' + suffix
password = 'container-test-' + uuid.uuid4().hex

def docker(*arguments, input=None):
    return subprocess.check_output(['docker', *arguments], input=input, stderr=subprocess.PIPE).decode().strip()

def address():
    return 'http://' + docker('port', container, '3000/tcp').splitlines()[0]

def call(base, method, path, body=None, token=None, operation=None):
    headers = {'Content-Type':'application/json'}
    if token:
        headers['Authorization'] = 'Bearer ' + token
    if operation:
        headers['Idempotency-Key'] = operation
    request = urllib.request.Request(base + path, method=method, headers=headers,
        data=None if body is None else json.dumps(body).encode())
    with urllib.request.urlopen(request, timeout=10) as response:
        assert response.headers['Cache-Control'] == 'private, no-store'
        raw = response.read()
        return json.loads(raw) if raw else None

def ready():
    deadline = time.monotonic() + 45
    base = address()
    while True:
        try:
            assert call(base, 'GET', '/ready') == {'status':'ready'}
            return base
        except (OSError, urllib.error.URLError):
            if time.monotonic() >= deadline:
                raise RuntimeError('Container readiness timed out')
            time.sleep(0.1)

try:
    for extra in [[], ['-e', 'ATLAS_DATABASE_URL=sqlite://invalid.sqlite']]:
        rejected = subprocess.run(['docker', 'run', '--rm', *extra, image], capture_output=True)
        assert rejected.returncode != 0
        assert b'Container deployments require a PostgreSQL' in rejected.stderr
    docker('volume', 'create', volume)
    docker('network', 'create', network)
    docker('run', '-d', '--name', postgres, '--network', network,
           '-e', 'POSTGRES_PASSWORD=' + password, '-e', 'POSTGRES_DB=atlas',
           '--mount', f'type=volume,source={volume},target=/var/lib/postgresql/data', 'postgres:17.11')
    deadline = time.monotonic() + 60
    while subprocess.run(['docker', 'exec', postgres, 'pg_isready', '-U', 'postgres', '-d', 'atlas'], capture_output=True).returncode:
        if time.monotonic() > deadline:
            raise RuntimeError('PostgreSQL readiness timed out')
        time.sleep(0.2)
    common = ['--read-only', '--cap-drop=ALL', '--security-opt=no-new-privileges',
              '--network', network, '-e', f'ATLAS_DATABASE_URL=postgres://postgres:{password}@{postgres}:5432/atlas']
    account = docker('run', '--rm', '-i', *common, image, 'account', 'smoke', input=password.encode())
    uuid.UUID(account)
    docker('run', '-d', '--name', container, *common, '-p', '127.0.0.1::3000', image)
    base = ready()
    config = json.loads(docker('inspect', container))[0]
    assert config['Config']['User'] == '10001:10001'
    assert config['HostConfig']['ReadonlyRootfs']
    session = call(base, 'POST', '/api/experimental/v1/sessions',
        {'username':'smoke','password':password,'device_id':'container-test'})
    assert session['account_id'] == account
    token = session['access_token']
    person = str(uuid.uuid4())
    call(base, 'POST', '/api/experimental/v1/commands',
        {'commands':[{'kind':'create_person','id':person,'name':'PRIVATE_CONTAINER_SENTINEL'}]},
        token, str(uuid.uuid4()))
    task, execution = str(uuid.uuid4()), str(uuid.uuid4())
    command = {'command':{'kind':'create_task','id':task,'execution_id':execution,
        'title':'PRIVATE_CONTAINER_SENTINEL','definition':{
            'schedule':{'start_date':None,'time':None,'timezone':'UTC','repeat':None},
            'goal':{'kind':'checkbox'},'carry':'retain_one','participation':'personal',
            'open_days_before':0,'close_days_after':1,'allow_streak_exclusions':False}}}
    operation = str(uuid.uuid4())
    receipt = call(base,'POST','/api/experimental/v1/task-commands',command,token,operation)
    occurrence = str(uuid.uuid5(uuid.UUID(task),'atlas-occurrence-v1:once'))
    call(base,'POST','/api/experimental/v1/task-commands',{'command':{'kind':'record',
        'occurrence_id':occurrence,'subject_account_id':account,'entry_id':str(uuid.uuid4()),
        'evidence':{'kind':'checkbox','complete':True},'expected_version':1,'happened_at':int(time.time())}},token,str(uuid.uuid4()))
    source = str(uuid.uuid4())
    call(base,'POST','/api/experimental/v1/calendar-commands',{'command':{'kind':'create_source','id':source,'label':'Calendar','timezone':'UTC'}},token,str(uuid.uuid4()))
    import_path = '/api/experimental/v1/calendar-sources/' + source + '/import'
    calendar_import = {'ics':'BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VEVENT\r\nUID:smoke\r\nDTSTART:20260910T090000Z\r\nSUMMARY:PRIVATE_CONTAINER_SENTINEL\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n','from':'2026-09-01','through':'2026-12-31'}
    import_op = str(uuid.uuid4())
    import_receipt = call(base,'POST',import_path,calendar_import,token,import_op)
    duplicate = str(uuid.uuid4())
    call(base,'POST','/api/experimental/v1/commands',{'commands':[
        {'kind':'create_person','id':duplicate,'name':'PRIVATE_CONTAINER_SENTINEL'}]},token,str(uuid.uuid4()))
    preview = call(base,'POST','/api/experimental/v1/people/merge-preview',
        {'source_id':duplicate,'target_id':person},token)
    merge = {'command':{'kind':'merge','source_id':duplicate,'target_id':person,
        'preview_token':preview['token'],'name':'PRIVATE_CONTAINER_SENTINEL'}}
    merge_op = str(uuid.uuid4())
    merge_receipt = call(base,'POST','/api/experimental/v1/people-commands',merge,token,merge_op)
    assert len(call(base,'GET','/api/experimental/v1/task-presets?start_date=2026-09-09&timezone=UTC',token=token)['items']) == 6
    assert call(base,'GET','/api/experimental/v1/occurrences/'+occurrence+'/dependencies',token=token)['items'][0]['complete']
    docker('stop', '--time', '15', container)
    assert json.loads(docker('inspect', container))[0]['State']['ExitCode'] == 0
    docker('start', container)
    base = ready()
    page = call(base, 'GET', '/api/experimental/v1/sync', token=token)
    assert any(change.get('resource', {}).get('id') == person
               for batch in page['batches'] for change in batch['changes'])
    assert call(base,'POST','/api/experimental/v1/task-commands',command,token,operation)==receipt
    assert call(base,'GET','/api/experimental/v1/occurrences?task_id='+task,token=token)['items'][0]['outcome']=='complete'
    progress = call(base,'GET','/api/experimental/v1/progress?parent_id='+occurrence,token=token)
    assert progress['items'][0]['value']['aggregate_consent'] is True
    assert call(base,'POST',import_path,calendar_import,token,import_op) == import_receipt
    assert call(base,'POST','/api/experimental/v1/people-commands',merge,token,merge_op) == merge_receipt
    assert call(base,'GET','/api/experimental/v1/people/'+duplicate,token=token)['person']['id'] == person
    assert len(call(base,'GET','/api/experimental/v1/events',token=token)['items']) == 1
    assert call(base,'GET','/api/experimental/v1/notification-capabilities',token=token)['native_local'] is True
    def workload(index):
        started = time.monotonic()
        call(base, 'POST', '/api/experimental/v1/commands', {'commands': [
            {'kind':'create_person','id':str(uuid.uuid4()),'name':f'Load {index}'}]}, token, str(uuid.uuid4()))
        return 1000 * (time.monotonic() - started)
    started = time.monotonic()
    with concurrent.futures.ThreadPoolExecutor(max_workers=8) as pool:
        timings = sorted(pool.map(workload, range(100)))
    print(json.dumps({'scenario':'container-postgres', 'image':image,
        'concurrent_clients':8, 'writes':100, 'elapsed_seconds':time.monotonic()-started,
        'write_p50_ms':statistics.median(timings),'write_p95_ms':timings[94]}))
    notices = docker('exec', container, 'cat', '/usr/share/doc/atlas/THIRD_PARTY_NOTICES.txt')
    assert 'ece ' in notices and 'coarsetime ' in notices and 'Frank Denis' in notices
    docker('exec', container, 'test', '-s', '/usr/share/doc/atlas/rust/COPYRIGHT-library.html')
    docker('exec', container, 'test', '-s', '/usr/share/doc/libssl3/copyright')
    logs = docker('logs', container)
    # Docker separates stdout/stderr; inspect both without printing credentials.
    recorded = subprocess.run(['docker','logs',container], capture_output=True, check=True)
    logs += recorded.stderr.decode()
    assert all(secret not in logs for secret in [password,token,'PRIVATE_CONTAINER_SENTINEL'])
    print('Container smoke passed: non-root/read-only runtime, bootstrap, login, task completion, dependency reads, presets, people merge/alias/replay, calendar import/replay, receipt replay, persistence, restart and sanitised logs')
finally:
    subprocess.run(['docker','rm','-f',container], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    subprocess.run(['docker','rm','-f',postgres], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    subprocess.run(['docker','network','rm',network], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    subprocess.run(['docker','volume','rm',volume], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
