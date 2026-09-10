#!/usr/bin/env python3
"""Two actual server processes against a fresh, harness-owned PostgreSQL database."""
import concurrent.futures
import json
import os
from pathlib import Path
import statistics
import subprocess
import tempfile
import time
import uuid
from smoke.client import ContractClient
from smoke import calendars, sharing, households, tasks, people

ROOT = Path(__file__).resolve().parents[1]
SERVER = ROOT / 'target/debug/atlas-server'


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
                clients.append(client)
            accounts = {}
            for name in ['alice', 'bob']:
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
                result = client(*args, **kwargs)
                call.last_headers = client.last_headers
                return result
            calendars.run(call, tokens, secrets)
            person, field = sharing.run(call, accounts, tokens, secrets)
            households.run(call, accounts, tokens, secrets, person, field)
            workflow = tasks.run(call, accounts, tokens, document)
            people.run(call, accounts, tokens, workflow)
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
    print('Two-replica HTTP, concurrent receipt and failover checks passed')


if __name__ == '__main__':
    main()
