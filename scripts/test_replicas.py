#!/usr/bin/env python3
"""Two actual server processes against a fresh, harness-owned PostgreSQL database."""
import base64
import collections
import concurrent.futures
import hashlib
import json
import os
from pathlib import Path
import random
import shutil
import statistics
import subprocess
import tempfile
import threading
import time
import urllib.error
import urllib.request
import uuid
from smoke.client import ContractClient
from smoke import calendars, sharing, households, tasks, people, devices

ROOT = Path(__file__).resolve().parents[1]
# ATLAS_TEST_BINARY names the exact executable under test (the release-shaped build).
SERVER = Path(os.environ.get('ATLAS_TEST_BINARY', ROOT / 'target/debug/atlas-server'))
V1 = '/api/experimental/v1'


def raw(base, method, path, token, headers=None):
    """One request with no status expectation, for races where several answers are lawful."""
    request = urllib.request.Request(base + path, method=method, headers={
        'Authorization': 'Bearer ' + token, 'Content-Type': 'application/json', **(headers or {})})
    try:
        response = urllib.request.urlopen(request, timeout=30)
    except urllib.error.HTTPError as error:
        response = error
    with response:
        body = response.read()
        return response.status, (json.loads(body) if body else None), dict(response.headers)


def psql(sql):
    result = subprocess.run([shutil.which('psql'), os.environ['ATLAS_DATABASE_URL'], '-XAt',
                             '-v', 'ON_ERROR_STOP=1', '-c', sql], capture_output=True, check=True)
    return result.stdout.decode().strip()


def raw_json(base, method, path, body, headers=None):
    """As raw(), for an unauthenticated request with a JSON body (BE-Q19's activation-grant
    endpoints, which carry their own bearer-shaped credential in the body, not Authorization)."""
    request = urllib.request.Request(base + path, method=method, data=json.dumps(body).encode(),
        headers={'Content-Type': 'application/json', **(headers or {})})
    try:
        response = urllib.request.urlopen(request, timeout=30)
    except urllib.error.HTTPError as error:
        response = error
    with response:
        body = response.read()
        return response.status, (json.loads(body) if body else None), dict(response.headers)


def _pkce_pair():
    verifier = uuid.uuid4().hex + uuid.uuid4().hex
    challenge = base64.urlsafe_b64encode(hashlib.sha256(verifier.encode()).digest()).rstrip(b'=').decode()
    return verifier, challenge


def race_retirements(clients, document, tokens, secrets, rounds):
    """Real cross-process races on PostgreSQL between a retirement and a writer of its member row.

    Each round: a fresh device with one session, a retirement sent to one process (authenticated by
    the account's stable session) and a revoke of that session sent to the other, started together
    with jitter. The interleaving is chosen by the servers and PostgreSQL, so this is statistical
    and complements the deterministic hook schedules; it proves the protocol holds between real
    processes, sockets and connection pools. Invariants, every round:

    * the retirement never reports rejected_stale (the only possible change is the revoke's removal);
    * "confirmed_applied" is never credited when the revoke itself removed the row (ordinary revoke:
      204 means one row deleted), and "superseded" only when it did (a 204 from the logout route is
      not evidence: it is also returned when the request authenticated before the retirement
      committed and deleted nothing);
    * exactly one ledger row per operation ID, recording the reported outcome; no session remains.
    """
    alice = tokens['alice']
    password = secrets[0]
    seen = collections.Counter()
    started = time.monotonic()
    for index in range(rounds):
        # Login is rate limited per source; spacing rounds keeps every process well under it.
        round_started = time.monotonic()
        retiring, writing = clients[index % 2], clients[(index + 1) % 2]
        kind = 'revoke' if index % 2 == 0 else 'logout'
        device = f'race-{index}'
        session = retiring('POST', V1 + '/sessions', body={
            'username': 'alice', 'password': password, 'device_id': device})['access_token']
        secrets.append(session)
        session_id = next(s['id'] for s in retiring('GET', V1 + '/sessions', alice)['sessions']
                          if s['device_id'] == device)
        state = next(d['state_token'] for d in retiring('GET', V1 + '/devices', alice)['devices']
                     if d['id'] == device)
        operation = str(uuid.uuid4())
        barrier = threading.Barrier(2)

        def retire():
            barrier.wait()
            time.sleep(random.uniform(0, 0.004))
            return raw(retiring.base, 'DELETE', f'{V1}/devices/{device}', alice,
                       {'Idempotency-Key': operation, 'Atlas-Device-State': state})

        def write():
            barrier.wait()
            time.sleep(random.uniform(0, 0.004))
            path = f'{V1}/sessions/{session_id}' if kind == 'revoke' else V1 + '/sessions/current'
            return raw(writing.base, 'DELETE', path, session)

        with concurrent.futures.ThreadPoolExecutor(max_workers=2) as pool:
            retirement, writer = pool.submit(retire), pool.submit(write)
            (r_status, r_body, r_headers), (w_status, _, w_headers) = retirement.result(), writer.result()
        assert r_status == 200, (kind, r_status, r_body)
        outcome = r_body['outcome']
        assert outcome in ('confirmed_applied', 'superseded'), r_body
        assert 'set-cookie' not in {k.lower() for k in r_headers | w_headers}
        assert w_status in (204, 404, 401), (kind, w_status)
        if kind == 'revoke':
            allowed = (404, 401) if outcome == 'confirmed_applied' else (204,)
        else:
            allowed = (204, 401) if outcome == 'confirmed_applied' else (204,)
        assert w_status in allowed, f'{kind}: retirement {outcome} with writer {w_status}'
        rows = psql(f"SELECT outcome FROM operation_outcomes WHERE operation_id='{operation}'").splitlines()
        assert rows == [outcome], (rows, outcome)
        assert psql(f"SELECT count(*) FROM sessions WHERE device_id='{device}'") == '0'
        seen[(kind, outcome, w_status)] += 1
        time.sleep(max(0.0, 0.6 - (time.monotonic() - round_started)))

    # The same operation ID sent to both processes at once: one evaluation, one replay, identical.
    for index in range(max(2, rounds // 10)):
        device = f'race-twin-{index}'
        session = clients[0]('POST', V1 + '/sessions', body={
            'username': 'alice', 'password': password, 'device_id': device})['access_token']
        secrets.append(session)
        state = next(d['state_token'] for d in clients[0]('GET', V1 + '/devices', alice)['devices']
                     if d['id'] == device)
        operation = str(uuid.uuid4())
        barrier = threading.Barrier(2)

        def twin(client):
            barrier.wait()
            time.sleep(random.uniform(0, 0.004))
            return raw(client.base, 'DELETE', f'{V1}/devices/{device}', alice,
                       {'Idempotency-Key': operation, 'Atlas-Device-State': state})
        with concurrent.futures.ThreadPoolExecutor(max_workers=2) as pool:
            first, second = [f.result() for f in [pool.submit(twin, c) for c in clients]]
        assert first[0] == second[0] == 200 and first[1] == second[1], (first, second)
        assert first[1]['outcome'] == 'confirmed_applied'
        assert psql(f"SELECT count(*) FROM operation_outcomes WHERE operation_id='{operation}'") == '1'
        time.sleep(0.6)
    return {'rounds': rounds, 'twin_rounds': max(2, rounds // 10),
            'elapsed_seconds': round(time.monotonic() - started, 1),
            'orders_observed': {f'{k[0]}/{k[1]}/writer_{k[2]}': v for k, v in sorted(seen.items())}}


def race_activations(clients, document, tokens, secrets, rounds):
    """Real cross-process races on PostgreSQL involving BE-Q19 activation grants: exactly one
    winner and a consistent post-state every round, exercised between real processes, sockets
    and connection pools rather than only in-process task scheduling (checks (b)/(n)/(r)).

    Each round issues a fresh grant (a real `POST /browser-sessions`, not a fixture) for a new
    device and races either:

    * `activate` against a retirement of that same device, both started from the device's
      approved-state token read just before the race. If activation wins, the grant redeems into
      a session and the retirement — still holding the pre-race token — is `rejected_stale`,
      because the device's membership changed under it (the grant left, a session arrived). If
      the retirement wins, it cancels the still-`issued` grant and reports `confirmed_applied`,
      and `activate` afterwards is `401` with no session created.
    * `activate` against `activate/cancel` of the same grant. Whichever commits first decides the
      outcome the other reports (`not_activated` before any session exists, or `session_revoked`
      after one does), but no live session ever survives the round: `cancel` revokes it if
      `activate` won first.
    """
    alice = tokens['alice']
    password = secrets[0]
    seen = collections.Counter()
    started = time.monotonic()
    origin = {'Origin': 'https://atlas.example'}
    for index in range(rounds):
        round_started = time.monotonic()
        acting, other = clients[index % 2], clients[(index + 1) % 2]
        device = f'grant-race-{index}'
        verifier, challenge = _pkce_pair()
        granted = acting('POST', V1 + '/browser-sessions', body={
            'username': 'alice', 'password': password, 'device_id': device,
            'attempt_challenge': challenge}, extra_headers=origin)
        grant_hash = hashlib.sha256(granted['grant'].encode()).hexdigest()
        state = next(d['state_token'] for d in acting('GET', V1 + '/devices', alice)['devices']
                     if d['id'] == device)
        mode = 'retire' if index % 2 == 0 else 'cancel'
        operation = str(uuid.uuid4())
        barrier = threading.Barrier(2)

        def activate():
            barrier.wait()
            time.sleep(random.uniform(0, 0.004))
            return raw_json(acting.base, 'POST', V1 + '/browser-sessions/activate',
                            {'grant': granted['grant'], 'verifier': verifier}, origin)

        if mode == 'retire':
            def counterpart():
                barrier.wait()
                time.sleep(random.uniform(0, 0.004))
                return raw(other.base, 'DELETE', f'{V1}/devices/{device}', alice,
                           {'Idempotency-Key': operation, 'Atlas-Device-State': state})
        else:
            def counterpart():
                barrier.wait()
                time.sleep(random.uniform(0, 0.004))
                return raw_json(other.base, 'POST', V1 + '/browser-sessions/activate/cancel',
                                {'verifier': verifier}, origin)

        with concurrent.futures.ThreadPoolExecutor(max_workers=2) as pool:
            (a_status, a_body, a_headers), (o_status, o_body, o_headers) = [
                f.result() for f in [pool.submit(activate), pool.submit(counterpart)]]
        assert 'set-cookie' not in {k.lower() for k in o_headers}
        assert a_status in (200, 401), (mode, a_status, a_body)
        won_activate = a_status == 200
        if won_activate:
            assert 'set-cookie' in {k.lower() for k in a_headers}
            session_id = a_body['session_id']
        else:
            assert 'set-cookie' not in {k.lower() for k in a_headers}
        if mode == 'retire':
            # rejected_stale is a committed answer carried on 409, not a plain failure; confirmed
            # outcomes (applied or superseded) are 200. See docs/api.md's operation_response note.
            if won_activate:
                assert o_status == 409 and o_body['outcome'] == 'rejected_stale', (mode, a_status, o_status, o_body)
                assert psql(f"SELECT count(*) FROM sessions WHERE session_id='{session_id}'") == '1'
                # A rejected_stale retirement leaves this session alive by design (that is the
                # point of this branch); clean it up so a long run doesn't exhaust alice's
                # 32-session device capacity across hundreds of rounds.
                psql(f"DELETE FROM sessions WHERE session_id='{session_id}'")
            else:
                assert o_status == 200 and o_body['outcome'] == 'confirmed_applied', (mode, a_status, o_status, o_body)
                assert psql(f"SELECT state FROM activation_grants WHERE grant_hash='{grant_hash}'") == 'cancelled'
                assert psql(f"SELECT count(*) FROM sessions WHERE device_id='{device}'") == '0'
        else:
            assert o_status == 200, (mode, o_status, o_body)
            if won_activate:
                assert o_body['result'] == 'session_revoked', (mode, a_status, o_body)
                assert psql(f"SELECT count(*) FROM sessions WHERE session_id='{session_id}'") == '0'
            else:
                assert o_body['result'] == 'not_activated', (mode, a_status, o_body)
            assert psql(f"SELECT count(*) FROM sessions WHERE device_id='{device}'") == '0'
        seen[(mode, won_activate)] += 1
        # Each round makes up to three source_attempt()-throttled calls (issue, activate,
        # cancel), split across two processes that swap roles every round — up to triple
        # race_retirements' one-call-per-round rate. That budget allows 120 attempts per
        # ~60s per source; pace at 1.5s/round (worst case ~2/s per process) to stay clear
        # of it over long runs instead of tripping a legitimate throttle near the end.
        time.sleep(max(0.0, 1.5 - (time.monotonic() - round_started)))
    return {'rounds': rounds, 'elapsed_seconds': round(time.monotonic() - started, 1),
            'orders_observed': {f'{k[0]}/activate_{"won" if k[1] else "lost"}': v
                                 for k, v in sorted(seen.items())}}


def main():
    env = {**os.environ, 'ATLAS_BIND': '127.0.0.1:0', 'ATLAS_PUBLIC_ORIGIN': 'https://atlas.example'}
    assert env['ATLAS_DATABASE_URL'].startswith(('postgres://', 'postgresql://'))
    document = json.loads((ROOT / 'api/openapi.json').read_text())
    secrets = ['replica-test-password-123', 'PRIVATE_REPLICA_SENTINEL', 'private-replica-field']
    with tempfile.TemporaryDirectory(prefix='atlas-replicas-') as temp:
        root = Path(temp)
        processes, logs, clients = [], [], []
        try:
            # Concurrent startup includes concurrent schema initialisation.
            for number in range(2):
                path = root / f'{number}.log'
                output = path.open('w')
                logs.append((path, output))
                processes.append(subprocess.Popen([str(SERVER)], env=env, stdout=subprocess.DEVNULL, stderr=output))
            for process, (path, _) in zip(processes, logs):
                deadline = time.monotonic() + 20
                while not path.read_text().strip():
                    if process.poll() is not None or time.monotonic() > deadline:
                        raise RuntimeError('Replica failed to start')
                    time.sleep(0.02)
                address = json.loads(path.read_text().splitlines()[0])['address']
                client = ContractClient('http://' + address, document, set())
                client('GET', '/ready')
                # Every replica reports the contract version it was built with.
                health = client('GET', '/health')
                assert health == {'status': 'ok', 'api_version': document['info']['version']}, health
                clients.append(client)
            accounts = {}
            for name in ['alice', 'bob', 'carol']:
                result = subprocess.run([str(SERVER), 'account', name], input=secrets[0].encode(), env=env, check=True, capture_output=True)
                accounts[name] = result.stdout.decode().strip()
            tokens = {name: clients[0]('POST', '/api/experimental/v1/sessions', body={
                'username': name, 'password': secrets[0], 'device_id': 'replica-phone'})['access_token'] for name in accounts}
            secrets.extend(tokens.values())
            # Every successive request goes through a different process.
            index = 0
            def call(*args, **kwargs):
                nonlocal index
                index += 1
                client = clients[index % 2]
                call.process = index % 2
                result = client(*args, **kwargs)
                call.last_headers = client.last_headers
                return result
            calendars.run(call, tokens, secrets)
            person, field = sharing.run(call, accounts, tokens, secrets)
            # The sharing change and the stale write it invalidates reach different processes.
            # One lane per process, so in the overlapping rounds the owner's narrowing and the
            # collaborator's save are served by different processes against one database.
            lanes = [ContractClient(client.base, document, set()) for client in clients]
            race = {**sharing.policy_precondition(call, accounts, tokens, lanes),
                    'overlap_owner_process': 0, 'overlap_collaborator_process': 1}
            assert race['policy_change_process'] is not None and race['policy_change_process'] != race['stale_write_process'], race
            sharing.alias_edit(call, accounts, tokens)
            # The writer is served by process 0; the readers alternate between both processes.
            snapshot_lanes = [ContractClient(clients[number % 2].base, document, set()) for number in range(4)]
            households.run(call, accounts, tokens, secrets, person, field, snapshot_lanes)
            workflow = tasks.run(call, accounts, tokens, document)
            people.run(call, accounts, tokens, workflow)
            def seed(account, device, kind):
                """Fixture rows for devices.run: no OpenID provider or push endpoint exists here."""
                expires = int(time.time()) + 3600
                if kind == 'handoff':
                    psql("INSERT INTO native_handoffs(code_hash,account_id,device_id,challenge,"
                         "configuration_hash,redirect_uri,expires_at) VALUES "
                         f"('{uuid.uuid4()}','{account}','{device}','c','h','r',{expires})")
                elif kind == 'grant':
                    psql("INSERT INTO activation_grants(grant_hash,grant_id,account_id,device_id,"
                         "auth_kind,challenge_hash,state,failed_verifiers,created_at,expires_at) "
                         "VALUES "
                         f"('{uuid.uuid4()}','{uuid.uuid4()}','{account}','{device}','local',"
                         f"'{uuid.uuid4()}','issued',0,{int(time.time())},{expires})")
                else:
                    psql("INSERT INTO notification_subscriptions(id,account_id,device_id,transport,"
                         "secret,version,active) VALUES "
                         f"('{uuid.uuid4()}','{account}','{device}','ntfy','fixture',1,1)")
            devices.run(call, tokens, secrets, seed)
            operation = str(uuid.uuid4())
            body = {'commands': [{'kind': 'create_person', 'id': str(uuid.uuid4()), 'name': 'Concurrent receipt'}]}
            with concurrent.futures.ThreadPoolExecutor(max_workers=2) as pool:
                results = list(pool.map(lambda client: client('POST', '/api/experimental/v1/commands', tokens['alice'], body, operation), clients))
            assert results[0] == results[1]
            def workload(index):
                client = ContractClient(clients[index % 2].base, document, set())
                start = time.monotonic()
                client('POST', '/api/experimental/v1/commands', tokens['alice'], {'commands': [
                    {'kind': 'create_person', 'id': str(uuid.uuid4()), 'name': f'Load {index}'}]}, str(uuid.uuid4()))
                return 1000 * (time.monotonic() - start)
            start = time.monotonic()
            with concurrent.futures.ThreadPoolExecutor(max_workers=8) as pool:
                elapsed = sorted(pool.map(workload, range(100)))
            print(json.dumps({'scenario': 'two-postgres-processes', 'concurrent_clients': 8,
                              'writes': len(elapsed), 'elapsed_seconds': time.monotonic() - start,
                              'write_p50_ms': statistics.median(elapsed), 'write_p95_ms': elapsed[94]}))
            rounds = int(os.environ.get('ATLAS_RACE_ROUNDS', '200'))
            if rounds:
                print(json.dumps({'scenario': 'device-retirement-races', **race_retirements(clients, document, tokens, secrets, rounds)}))
                print(json.dumps({'scenario': 'activation-grant-races', **race_activations(clients, document, tokens, secrets, rounds)}))
            # A surviving replica must serve committed state and the same session.
            processes[0].terminate()
            processes[0].wait(timeout=15)
            clients[1]('GET', '/api/experimental/v1/sync', tokens['alice'])
        finally:
            for process in processes:
                if process.poll() is None:
                    process.terminate()
                try:
                    process.wait(timeout=15)
                except subprocess.TimeoutExpired:
                    process.kill()
                    process.wait()
            for _, output in logs:
                output.close()
        assert all(process.returncode == 0 for process in processes)
        assert not any(secret in path.read_text() for path, _ in logs for secret in secrets)
    print(json.dumps({'scenario': 'policy-precondition-across-processes', **race}))
    print('Two-replica HTTP, concurrent receipt, device-retirement race and failover checks passed')


if __name__ == '__main__':
    main()
