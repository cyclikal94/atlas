"""Validate real HTTP traffic and sanitised logs using a disposable local database."""
import json
import datetime
import os
from pathlib import Path
import sqlite3
import subprocess
import tempfile
import time
import urllib.error
import urllib.request
import uuid

from smoke.client import ContractClient
from smoke import calendars, sharing, households, tasks, people, devices

root = Path(__file__).resolve().parents[1]
binary = Path(os.environ.get('ATLAS_TEST_BINARY', root / 'target/debug/atlas-server'))
document = json.loads((root / 'api/openapi.json').read_text())
request_ids = set()
secrets = ['local-smoke-password-123', 'PRIVATE_LOG_SENTINEL', 'secret-field-label']

with tempfile.TemporaryDirectory(prefix='atlas-server-smoke-') as temporary:
    env = {**os.environ, 'ATLAS_DATABASE_URL': f'sqlite://{temporary}/test.sqlite?mode=rwc',
           'ATLAS_BIND': '127.0.0.1:0', 'ATLAS_PUBLIC_ORIGIN':'https://atlas.example'}
    accounts = {}
    for name in ['alice', 'bob', 'carol']:
        result = subprocess.run([str(binary), 'account', name], input=secrets[0].encode(),
                                env=env, check=True, capture_output=True)
        accounts[name] = result.stdout.decode().strip()
    log_path = Path(temporary) / 'server.log'
    with log_path.open('w') as logs:
        server = subprocess.Popen([str(binary)], env=env, stdout=subprocess.DEVNULL, stderr=logs)
        try:
            deadline = time.monotonic() + 15
            while '\n' not in log_path.read_text():
                if server.poll() is not None or time.monotonic() > deadline:
                    raise RuntimeError('Server startup failed or timed out')
                time.sleep(0.02)
            try:
                address = json.loads(log_path.read_text().splitlines()[0])['address']
            except (ValueError, KeyError) as error:
                raise RuntimeError('Server startup failed: ' + log_path.read_text()) from error
            base = 'http://' + address

            call = ContractClient(base, document, request_ids)

            # /health is unauthenticated liveness plus the contract version, and never sets a cookie.
            health = call('GET', '/health')
            if health != {'status': 'ok', 'api_version': document['info']['version']}:
                raise RuntimeError(f'Unexpected /health body: {health}')
            if any(name.lower() == 'set-cookie' for name in call.last_headers):
                raise RuntimeError('/health set a cookie')
            call('GET', '/ready')
            tokens = {}
            for name in accounts:
                tokens[name] = call('POST', '/api/experimental/v1/sessions', body={
                    'username':name, 'password':secrets[0], 'device_id':'phone'})['access_token']
                secrets.append(tokens[name])
            call('GET', '/api/experimental/v1/me', tokens['alice'])
            calendars.run(call, tokens, secrets)
            person, field = sharing.run(call, accounts, tokens, secrets)
            # Two independent clients so the narrowing and the collaborator's save can overlap.
            lanes = [ContractClient(base, document, request_ids) for _ in range(2)]
            print(json.dumps({'scenario': 'policy-precondition-one-process',
                              **sharing.policy_precondition(call, accounts, tokens, lanes)}))
            sharing.alias_edit(call, accounts, tokens)
            households.run(call, accounts, tokens, secrets, person, field)
            workflow = tasks.run(call, accounts, tokens, document)
            people.run(call, accounts, tokens, workflow)
            def seed(account, device, kind):
                """Fixture rows for devices.run: no OpenID provider or push endpoint exists here."""
                expires = int(time.time()) + 3600
                with sqlite3.connect(f'{temporary}/test.sqlite', timeout=10) as database:
                    if kind == 'handoff':
                        database.execute(
                            'INSERT INTO native_handoffs(code_hash,account_id,device_id,challenge,'
                            'configuration_hash,redirect_uri,expires_at) VALUES (?,?,?,?,?,?,?)',
                            (str(uuid.uuid4()), account, device, 'c', 'h', 'r', expires))
                    else:
                        database.execute(
                            'INSERT INTO notification_subscriptions(id,account_id,device_id,transport,'
                            'secret,version,active) VALUES (?,?,?,?,?,1,1)',
                            (str(uuid.uuid4()), account, device, 'ntfy', 'fixture'))
            devices.run(call, tokens, secrets, seed)
            secrets.append('replacement-smoke-password-456')
            call('POST', '/api/experimental/v1/password', tokens['alice'],
                {'current_password':secrets[0], 'new_password':secrets[-1]}, expected=204)
            call('GET', '/api/experimental/v1/sync', tokens['alice'], expected=401)
            call('DELETE', '/api/experimental/v1/sessions/current', tokens['bob'], expected=204)
            call('GET', '/api/experimental/v1/sync', tokens['bob'], expected=401)
        finally:
            server.terminate()
            try:
                server.wait(timeout=15)
            except subprocess.TimeoutExpired:
                server.kill()
                server.wait()
        if server.returncode != 0:
            raise RuntimeError('Server did not shut down cleanly')
    log_text = log_path.read_text()
    if any(secret in log_text for secret in secrets):
        raise RuntimeError('Private content or credentials reached logs')
    events = [json.loads(line) for line in log_text.splitlines()]
    logged_ids = {event['request_id'] for event in events if event['event'] == 'http_request'}
    if request_ids != logged_ids:
        raise RuntimeError('Requests and logs are not correlated')
print('Live contract smoke passed: authentication, sharing, households, tasks, people, calendars, device retirement and sanitised logs')
